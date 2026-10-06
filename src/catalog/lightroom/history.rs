//! Lightroom's develop history, imported with the catalog and shown under
//! "From Lightroom" in the History panel, and the stored Lightroom develop
//! settings a photo opens with.
use crate::catalog::Catalog;
use crate::catalog::db::Db;
use anyhow::Result;

/// Set in `meta` once Lightroom history has been recovered from the stored catalog.
const HISTORY_BACKFILLED: &str = "lightroom_history_backfilled";
/// Copies history steps from an attached Lightroom catalog named `lr`.
pub(super) const COPY_LIGHTROOM_HISTORY: &str =
    "INSERT OR IGNORE INTO lightroom_history(photo,position,name,created,text)
    SELECT image, row_number() OVER (PARTITION BY image ORDER BY dateCreated, id_local),
           COALESCE(name,''), dateCreated, text
    FROM lr.Adobe_libraryImageDevelopHistoryStep
    WHERE text IS NOT NULL AND image IN (SELECT id FROM photos);";
/// Largest history snapshot accepted. A step's text is a develop-settings string
/// of a few kilobytes; the cap is far above that and only exists so a corrupt
/// catalog cannot name a length the allocation would follow.
const MAX_TEXT_BYTES: usize = 1 << 20;
/// Lightroom stores history snapshots either as text or as a 4-byte
/// big-endian length followed by a zlib stream.
pub(in crate::catalog) fn decode_history_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > 6 && bytes[4] == 0x78 {
        use std::io::Read;
        // The prefix declares the decompressed length, so it bounds the read and
        // a snapshot that expands past it is corrupt and refused.
        let expected = u32::from_be_bytes(bytes[..4].try_into().ok()?) as usize;
        if expected > MAX_TEXT_BYTES {
            return None;
        }
        let mut text = String::new();
        flate2::read::ZlibDecoder::new(&bytes[4..])
            .take(expected as u64 + 1)
            .read_to_string(&mut text)
            .ok()?;
        return (text.len() <= expected).then_some(text);
    }
    String::from_utf8(bytes.to_vec()).ok()
}
/// One Lightroom history step.
#[derive(Clone, Debug)]
pub struct HistoryStep {
    pub name: String,
    /// Seconds since 2001-01-01 (Lightroom's epoch).
    pub created: Option<f64>,
    pub text: String,
}
impl Catalog {
    /// Lightroom's history for a photo, oldest step first.
    pub fn lightroom_history(&self, id: i64) -> Result<Vec<HistoryStep>> {
        let rows = self.db.query_all(
            "SELECT name, created, text FROM lightroom_history WHERE photo=? ORDER BY position",
            [id],
            |r| {
                let text = r.get::<Option<Vec<u8>>>(2)?.unwrap_or_default();
                Ok((r.get::<String>(0)?, r.get::<Option<f64>>(1)?, text))
            },
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(name, created, bytes)| {
                Some(HistoryStep {
                    name,
                    created,
                    text: decode_history_text(&bytes)?,
                })
            })
            .collect())
    }
    /// Catalogs imported before history was kept still hold the original
    /// Lightroom catalog; copy its history steps once. Returns steps added.
    pub fn backfill_lightroom_history(&mut self) -> Result<usize> {
        // Once is enough: without history to recover, the stored catalog would
        // otherwise be written out and attached on every open. History that
        // came with the import leaves nothing to recover, so it is not.
        if self.meta(HISTORY_BACKFILLED)?.is_none() && self.has_lightroom_history()? {
            self.set_meta(HISTORY_BACKFILLED, "1")?;
        }
        self.backfill_once(HISTORY_BACKFILLED, copy_history)
    }
    fn has_lightroom_history(&self) -> Result<bool> {
        let have: i64 = self
            .db
            .query_row("SELECT count(*) FROM lightroom_history", (), |r| r.get(0))?;
        Ok(have > 0)
    }
    pub fn lightroom_develop(&self, id: i64) -> Result<Option<String>> {
        self.db.query_row(
            "SELECT lightroom_develop FROM photos WHERE id=?",
            [id],
            |r| r.get(0),
        )
    }
}

/// Copies history steps from a Lightroom catalog attached as `lr` that has them.
fn copy_history(db: &Db) -> Result<usize> {
    if !super::has_table(db, "lr", "Adobe_libraryImageDevelopHistoryStep")? {
        return Ok(0);
    }
    db.execute(COPY_LIGHTROOM_HISTORY, ())
}
