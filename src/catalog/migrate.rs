//! Moving a local catalog onto a server: a Lightroom import made in a
//! temporary file and copied across, or an existing `.rawmakase` file.
use super::{Catalog, db::FromValue, db::PgValue, server::ServerConfig};
use anyhow::{Context, Result, ensure};
use postgres::binary_copy::BinaryCopyInWriter;
use std::path::{Path, PathBuf};

/// Every table, parents before the tables that refer to them.
const TABLES: [&str; 22] = [
    "sources",
    "roots",
    "folders",
    "photos",
    "collections",
    "keywords",
    "collection_photos",
    "photo_keywords",
    "folder_mappings",
    "lightroom_history",
    "local_edits",
    "develop_history",
    "develop_snapshots",
    "bitmaps",
    "photo_info",
    "meta",
    "photo_fields",
    "photo_text",
    "photo_creators",
    "photo_capture",
    "photo_location",
    "keyword_export",
];
/// The tables whose `id` the server numbers itself.
const IDENTITIES: [&str; 7] = [
    "sources",
    "roots",
    "folders",
    "photos",
    "collections",
    "keywords",
    "develop_snapshots",
];

impl Catalog {
    /// Copies this local catalog into `target`, a server catalog that has no
    /// photos yet, in one transaction: all of it, or none. Ids are kept, so
    /// the copy refers to the same photos. Returns the photos copied.
    pub fn copy_to_server(&self, target: &Catalog) -> Result<usize> {
        ensure!(!self.is_server(), "The catalog is already on a server");
        ensure!(
            target.is_server(),
            "The destination is not a server catalog"
        );
        let used: i64 = target.db.query_row(
            "SELECT (SELECT count(*) FROM photos) + (SELECT count(*) FROM roots)",
            (),
            |r| r.get(0),
        )?;
        ensure!(
            used == 0,
            "The server catalog already has photos; copy into an empty database"
        );
        let tx = target.db.transaction()?;
        let client = target.db.postgres_client().expect("a server catalog");
        for table in TABLES {
            let columns: Vec<String> = self.db.query_all(
                &format!("SELECT name FROM pragma_table_info('{table}')"),
                (),
                |r| r.get(0),
            )?;
            let quoted = columns
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let rows = self
                .db
                .query_all(&format!("SELECT {quoted} FROM {table}"), (), |r| {
                    Ok(r.values())
                })?;
            let expected = rows.len();
            if table == "meta" {
                // The server catalog's own format and version stay.
                for row in rows {
                    let key = String::from_value(row[0].clone())?;
                    if key != "format" && key != "version" {
                        super::set_meta(&tx, &key, &String::from_value(row[1].clone())?)?;
                    }
                }
                continue;
            }
            if rows.is_empty() {
                continue;
            }
            let mut client = client.borrow_mut();
            let types: Vec<_> = client
                .prepare(&format!("SELECT {quoted} FROM {table}"))?
                .columns()
                .iter()
                .map(|c| c.type_().clone())
                .collect();
            let writer = client.copy_in(&format!("COPY {table} ({quoted}) FROM STDIN BINARY"))?;
            let mut writer = BinaryCopyInWriter::new(writer, &types);
            for row in &rows {
                let values: Vec<PgValue<'_>> = row.iter().map(PgValue).collect();
                let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    values.iter().map(|v| v as _).collect();
                writer
                    .write(&refs)
                    .with_context(|| format!("Copying {table}"))?;
            }
            let written = writer
                .finish()
                .with_context(|| format!("Copying {table}"))?;
            ensure!(
                written as usize == expected,
                "Copied {written} of {expected} rows of {table}"
            );
        }
        for table in IDENTITIES {
            target.db.execute(
                &format!(
                    "SELECT setval(pg_get_serial_sequence('{table}', 'id'),
                            COALESCE((SELECT max(id) FROM {table}), 0) + 1, false)"
                ),
                (),
            )?;
        }
        let photos: i64 = tx.query_row("SELECT count(*) FROM photos", (), |r| r.get(0))?;
        tx.commit()?;
        let _ = target.db.execute_batch("ANALYZE");
        Ok(photos as usize)
    }
}

/// Imports a closed Lightroom catalog straight onto the server: built in a
/// temporary file, then copied. Returns the server catalog's location.
pub fn import_lightroom_to_server(source: &Path, server: &ServerConfig) -> Result<PathBuf> {
    // Checked first, so a wrong password does not wait for the import.
    let target = Catalog::open_server(server)?;
    let dir = tempfile::tempdir()?;
    let local = super::lightroom::import_lightroom(source, &dir.path().join("import.rawmakase"))?;
    let local = Catalog::open(&local)?;
    local.copy_to_server(&target)?;
    Ok(target.path.clone())
}

/// Copies an existing `.rawmakase` file onto the server's empty catalog.
pub fn copy_file_to_server(file: &Path, server: &ServerConfig) -> Result<PathBuf> {
    let target = Catalog::open_server(server)?;
    Catalog::open(file)?.copy_to_server(&target)?;
    Ok(target.path.clone())
}
