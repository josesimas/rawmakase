//! Photo info: camera settings and size, copied from the Lightroom catalog a
//! photo was imported from, or read from the file for photos added from
//! folders (`fill_photo_info`).
use super::db::{Db, params};
use super::{Catalog, PhotoInfo};
use anyhow::Result;

/// Set in `meta` once photo info has been copied from the stored catalog.
pub(super) const INFO_BACKFILLED: &str = "lightroom_info_backfilled";

impl Catalog {
    /// Each photo's width over height, as shown, where it is known; a
    /// virtual copy has its master's.
    pub fn aspect_ratios(&self) -> Result<std::collections::HashMap<i64, f32>> {
        let rows = self.db.query_all(
            "SELECT p.id, i.width, i.height FROM photos p
             JOIN photo_info i ON i.photo = COALESCE(p.master_id, p.id)
             WHERE i.width > 0 AND i.height > 0",
            (),
            |r| {
                let (width, height): (f64, f64) = (r.get(1)?, r.get(2)?);
                Ok((r.get(0)?, (width / height) as f32))
            },
        )?;
        Ok(rows.into_iter().collect())
    }
    /// A photo's info; a virtual copy has its master's.
    pub fn photo_info(&self, id: i64) -> Result<Option<PhotoInfo>> {
        self.db.query_opt(
            "SELECT camera, lens, focal, aperture, exposure, iso, width, height
                 FROM photo_info
                 WHERE photo = (SELECT COALESCE(master_id, id) FROM photos WHERE id = ?)",
            [id],
            |r| {
                let size: (Option<u32>, Option<u32>) = (r.get(6)?, r.get(7)?);
                Ok(PhotoInfo {
                    camera: r.get(0)?,
                    lens: r.get(1)?,
                    focal: r.get(2)?,
                    aperture: r.get(3)?,
                    exposure: r.get(4)?,
                    iso: r.get(5)?,
                    dimensions: size.0.zip(size.1),
                })
            },
        )
    }
    /// The cameras the catalog's photos were taken with, as photo info names
    /// them, in alphabetical order.
    pub fn cameras(&self) -> Result<Vec<String>> {
        self.db.query_all(
            "SELECT camera FROM photo_info
             WHERE camera IS NOT NULL AND camera != ''
             GROUP BY camera ORDER BY lower(camera), camera",
            (),
            |r| r.get(0),
        )
    }
    /// Masters with no info yet, whose files may have it.
    pub fn photos_without_info(&self) -> Result<Vec<i64>> {
        self.db.query_all(
            "SELECT id FROM photos
             WHERE master_id IS NULL AND id NOT IN (SELECT photo FROM photo_info)",
            (),
            |r| r.get(0),
        )
    }
    /// Records info read from files, in one transaction; `None` records that
    /// a file had none, so it is not read again.
    pub fn fill_photo_info(&mut self, infos: &[(i64, Option<PhotoInfo>)]) -> Result<()> {
        let tx = self.db.transaction()?;
        for (id, info) in infos {
            // Under the photo's master now, in case it became a copy while
            // being read.
            // A photo removed meanwhile has nothing to keep.
            let master: Option<i64> = tx.query_opt(
                "SELECT COALESCE(master_id, id) FROM photos WHERE id = ?",
                [id],
                |r| r.get(0),
            )?;
            if let Some(master) = master {
                insert(&tx, master, info.as_ref().unwrap_or(&PhotoInfo::default()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    /// Catalogs imported before photo info was kept still hold the original
    /// Lightroom catalog; copy its info once.
    pub fn backfill_lightroom_info(&mut self) -> Result<usize> {
        self.backfill_once(INFO_BACKFILLED, copy_lightroom_info)
    }
}

fn insert(db: &Db, id: i64, info: &PhotoInfo) -> Result<()> {
    db.execute(
        "INSERT INTO photo_info
         (photo, camera, lens, focal, aperture, exposure, iso, width, height)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(photo) DO UPDATE SET camera=excluded.camera, lens=excluded.lens,
            focal=excluded.focal, aperture=excluded.aperture, exposure=excluded.exposure,
            iso=excluded.iso, width=excluded.width, height=excluded.height",
        params![
            id,
            info.camera,
            info.lens,
            info.focal,
            info.aperture,
            info.exposure,
            info.iso,
            info.dimensions.map(|d| d.0),
            info.dimensions.map(|d| d.1),
        ],
    )?;
    Ok(())
}

/// Copies photo info from a Lightroom catalog attached as `lr`. Lightroom
/// stores aperture and shutter speed as APEX values: aperture 2.0 is f/2,
/// shutter speed 4.64 is 1/25 s. Returns the photos copied.
pub(super) fn copy_lightroom_info(db: &Db) -> Result<usize> {
    let has = |table: &str| super::lightroom::has_table(db, "lr", table);
    let interned = has("AgInternedExifCameraModel")? && has("AgInternedExifLens")?;
    if !has("AgHarvestedExifMetadata")? || !interned {
        return Ok(0);
    }
    let rows: Vec<(i64, PhotoInfo)> = db.query_all(
        "SELECT i.id_local, c.value, l.value, e.focalLength, e.aperture, e.shutterSpeed,
                    e.isoSpeedRating, i.fileWidth, i.fileHeight, i.orientation
             FROM lr.Adobe_images i
             LEFT JOIN lr.AgHarvestedExifMetadata e ON e.image = i.id_local
             LEFT JOIN lr.AgInternedExifCameraModel c ON c.id_local = e.cameraModelRef
             LEFT JOIN lr.AgInternedExifLens l ON l.id_local = e.lensRef
             WHERE i.id_local IN (SELECT id FROM photos)",
        (),
        |r| {
            let (width, height): (Option<f64>, Option<f64>) = (r.get(7)?, r.get(8)?);
            let orientation: Option<String> = r.get(9)?;
            // Quarter turns, mirrored or not: the photo shows taller than stored.
            let turned = matches!(orientation.as_deref(), Some("BC" | "DA" | "AD" | "CB"));
            let dimensions = width.zip(height).map(|(w, h)| {
                let (w, h) = (w as u32, h as u32);
                if turned { (h, w) } else { (w, h) }
            });
            Ok((
                r.get(0)?,
                PhotoInfo {
                    camera: r.get(1)?,
                    lens: r.get(2)?,
                    focal: r.get(3)?,
                    aperture: r.get::<Option<f64>>(4)?.map(|av| 2f64.powf(av / 2.)),
                    exposure: r.get::<Option<f64>>(5)?.map(|tv| 2f64.powf(-tv)),
                    iso: r.get(6)?,
                    dimensions,
                },
            ))
        },
    )?;
    for (id, info) in &rows {
        insert(db, *id, info)?;
    }
    Ok(rows.len())
}
