//! The catalog on PostgreSQL. These run against the server named by the
//! standard PGHOST, PGPORT, PGDATABASE, PGUSER and PGPASSWORD variables and are
//! skipped without PGHOST. Each test works in a schema of its own, dropped
//! afterwards, so the database's own tables are never touched.
use super::server::ServerConfig;
use super::*;
use crate::{develop::Recipe, export::ExportOptions};

fn config() -> Option<ServerConfig> {
    let host = std::env::var("PGHOST").ok()?;
    Some(ServerConfig {
        host,
        port: std::env::var("PGPORT").ok()?.parse().ok()?,
        database: std::env::var("PGDATABASE").ok()?,
        user: std::env::var("PGUSER").ok()?,
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
    })
}

/// A private schema of the server's database, dropped with the guard.
struct Schema {
    config: ServerConfig,
    name: String,
}
impl Schema {
    fn new(config: &ServerConfig) -> Result<Self> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = format!(
            "rm_test_{}_{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        Self::admin(config, &format!("CREATE SCHEMA {name}"))?;
        Ok(Self {
            config: config.clone(),
            name,
        })
    }
    fn admin(config: &ServerConfig, sql: &str) -> Result<()> {
        let mut c = postgres::Config::new();
        c.host(&config.host)
            .port(config.port)
            .user(&config.user)
            .password(&config.password)
            .dbname(&config.database);
        c.connect(postgres::NoTls)?.batch_execute(sql)?;
        Ok(())
    }
    fn open(&self) -> Result<Catalog> {
        Catalog::open_server_in(&self.config, Some(&self.name))
    }
}
impl Drop for Schema {
    fn drop(&mut self) {
        let _ = Self::admin(&self.config, &format!("DROP SCHEMA {} CASCADE", self.name));
    }
}

#[test]
fn server_catalog_is_created_reopened_and_refuses_other_databases() -> Result<()> {
    let Some(config) = config() else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    let cat = schema.open()?;
    assert!(cat.is_server());
    assert!(cat.photos()?.is_empty());
    drop(cat);
    // Reopening finds its tables and adds nothing twice.
    let cat = schema.open()?;
    assert!(cat.roots()?.is_empty());
    drop(cat);
    // Something that is not a catalog is left alone.
    Schema::admin(
        &config,
        &format!(
            "CREATE SCHEMA {0}_x; CREATE TABLE {0}_x.other(a int)",
            schema.name
        ),
    )?;
    let other = Catalog::open_server_in(&config, Some(&format!("{}_x", schema.name)));
    let _ = Schema::admin(&config, &format!("DROP SCHEMA {}_x CASCADE", schema.name));
    assert!(other.is_err());
    Ok(())
}

#[test]
fn server_catalog_reads_and_writes_like_a_local_one() -> Result<()> {
    let Some(config) = config() else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    let dir = tempfile::tempdir()?;
    let folder = dir.path().join("photos");
    std::fs::create_dir_all(folder.join("sub"))?;
    std::fs::write(folder.join("image.ARW"), b"synthetic raw identity")?;
    std::fs::write(folder.join("sub/other.ARW"), b"another raw")?;
    let mut cat = schema.open()?;
    assert_eq!(cat.add_folder(&folder)?, 2);
    assert_eq!(cat.add_folder(&folder)?, 0);
    let photos = cat.photos()?;
    assert_eq!(photos.len(), 2);
    assert_eq!(cat.folders()?.iter().map(|f| f.count).sum::<usize>(), 2);
    let original = photos
        .iter()
        .find(|p| p.filename == "image.ARW")
        .unwrap()
        .clone();
    let path = original.path.clone();

    cat.set_metadata(original.id, 3, 1, "Red")?;
    assert!(cat.set_metadata(original.id, 9, 0, "").is_err());
    let after = cat.photos()?;
    let now = after.iter().find(|p| p.id == original.id).unwrap();
    assert_eq!((now.rating, now.flag, now.label.as_str()), (3, 1, "Red"));

    // Keywords: a hierarchy, shared by name, and removal.
    cat.add_keywords(
        &[original.id],
        &[vec!["Places".into(), "City".into()], vec!["Places".into()]],
    )?;
    let keywords = cat.keywords(original.id)?;
    assert_eq!(keywords.len(), 2);
    assert_eq!(keywords[1].path, ["Places", "City"]);
    cat.remove_keyword(&[original.id], keywords[1].id)?;
    assert_eq!(cat.keywords(original.id)?.len(), 1);

    // Descriptive metadata round trip, with its undo snapshot.
    let before = cat.metadata_snapshot(&[original.id])?;
    cat.set_text(&[original.id], TextField::Title, "Harbour")?;
    cat.set_creators(&[original.id], &["Ana".into(), "Bo".into()])?;
    cat.set_capture(
        original.id,
        &Capture {
            captured: "2021-06-06T10:00:00".into(),
            subsec: None,
            offset: Some("+02:00".into()),
        },
    )?;
    let d = cat.descriptive(original.id)?;
    assert!(d.title.is_some() && d.creator.is_some());
    assert_eq!(
        d.capture.as_ref().unwrap().offset.as_deref(),
        Some("+02:00")
    );
    cat.restore_metadata(&before)?;
    assert_eq!(cat.descriptive(original.id)?, Default::default());

    // Edits, with spots and a history, and their protection.
    let edit = Recipe {
        exposure: 1.25,
        ..Default::default()
    };
    cat.save_edit(
        original.id,
        &path,
        &edit,
        &ExportOptions::default(),
        HistoryUpdate::Keep,
    )?;
    assert_eq!(cat.load_edit(original.id, &path)?.unwrap().recipe, edit);
    assert!(cat.edit_times()?.contains_key(&original.id));
    let (recipe, lightroom) = cat.edit_texts(original.id)?;
    assert!(recipe.is_some() && lightroom.is_none());
    let stamp = cat.edit_stamp(original.id)?;
    cat.save_edit(
        original.id,
        &path,
        &Recipe {
            exposure: -1.,
            ..Default::default()
        },
        &ExportOptions::default(),
        HistoryUpdate::Keep,
    )?;
    assert_ne!(stamp, cat.edit_stamp(original.id)?);

    // Virtual copies, promoted and removed.
    let first = cat.create_virtual_copy(original.id)?;
    let second = cat.create_virtual_copy(first)?;
    let copies = cat.photos()?;
    let copy = copies.iter().find(|p| p.id == second).unwrap();
    assert_eq!(
        (copy.master, copy.copy_name.as_str()),
        (Some(original.id), "Copy 2")
    );
    assert_eq!(cat.load_edit(first, &path)?.unwrap().recipe.exposure, -1.);
    cat.set_copy_as_master(second)?;
    assert_eq!(
        cat.photos()?
            .iter()
            .find(|p| p.id == original.id)
            .unwrap()
            .master,
        Some(second)
    );
    cat.remove_virtual_copy(first)?;
    assert_eq!(cat.photos()?.len(), 3);

    // Snapshots, collections, bitmaps, photo info and relinking.
    let snapshot = cat.add_snapshot(original.id, "Warm", &edit)?;
    cat.rename_snapshot(snapshot, "Cool")?;
    assert_eq!(cat.snapshots(original.id)?[0].name, "Cool");
    cat.delete_snapshot(snapshot)?;
    assert!(cat.snapshots(original.id)?.is_empty());
    let quick = cat.quick_collection()?;
    assert_eq!(quick, cat.quick_collection()?);
    cat.change_collection(quick, &[original.id], &[])?;
    cat.change_collection(quick, &[original.id], &[])?;
    assert_eq!(cat.collection_members(quick)?.len(), 1);
    cat.fill_photo_info(&[(
        original.id,
        Some(PhotoInfo {
            camera: Some("Nikon Z6".into()),
            dimensions: Some((6000, 4000)),
            ..Default::default()
        }),
    )])?;
    assert_eq!(cat.cameras()?, ["Nikon Z6"]);
    assert!((cat.aspect_ratios()?[&original.id] - 1.5).abs() < 1e-6);
    assert_eq!(
        cat.photo_info(original.id)?.unwrap().dimensions,
        Some((6000, 4000))
    );
    let root = cat.roots()?[0].0;
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere)?;
    cat.relink_root(root, &elsewhere)?;
    assert!(cat.photos()?[0].path.starts_with(&elsewhere));
    cat.fill_capture_times(&[(original.id, "2020-01-01T00:00:00".into())])?;
    // An edit is protected once its file changes.
    std::fs::write(&path, b"changed raw")?;
    assert!(cat.load_edit(original.id, &path).is_err());
    Ok(())
}

#[test]
fn a_lightroom_import_copies_to_the_server_with_the_same_ids_and_bytes() -> Result<()> {
    let Some(config) = config() else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.lrcat");
    super::tests::fixture(&source)?;
    let local_file = dir.path().join("Photos.rawmakase");
    super::lightroom::import_lightroom(&source, &local_file)?;
    let local = Catalog::open(&local_file)?;
    // Lightroom keeps a history step as compressed bytes, not text.
    let step = vec![0u8, 0, 0, 9, 0x78, 0x9c, 0xff, 0xfe, 0x00];
    local.db_for_tests().execute(
        "INSERT INTO lightroom_history(photo, position, name, created, text) VALUES (40, 1, 'Open', 1.5, ?)",
        [&step],
    )?;

    let mut server = schema.open()?;
    assert_eq!(local.copy_to_server(&server)?, 2);
    // A second copy has nowhere to go.
    assert!(local.copy_to_server(&server).is_err());

    assert_eq!(
        format!("{:?}", local.photos()?),
        format!("{:?}", server.photos()?)
    );
    assert_eq!(
        format!("{:?}", local.folders()?),
        format!("{:?}", server.folders()?)
    );
    assert_eq!(
        format!("{:?}", local.collections()?),
        format!("{:?}", server.collections()?)
    );
    assert_eq!(local.roots()?, server.roots()?);
    assert_eq!(local.collection_photos()?, server.collection_photos()?);
    assert_eq!(local.keywords(40)?[0].path, server.keywords(40)?[0].path);
    assert_eq!(local.lightroom_develop(40)?, server.lightroom_develop(40)?);
    let archive: Vec<u8> =
        server
            .db
            .query_row("SELECT original_catalog FROM sources", (), |r| r.get(0))?;
    assert_eq!(archive, std::fs::read(&source)?);
    let copied: Vec<u8> = server.db.query_row(
        "SELECT text FROM lightroom_history WHERE photo = 40",
        (),
        |r| r.get(0),
    )?;
    assert_eq!(copied, step);
    // Nothing is left to recover from the archive.
    assert_eq!(server.backfill_lightroom_history()?, 0);

    // New rows are numbered after the imported ones.
    let copy = server.create_virtual_copy(40)?;
    assert!(copy > 41);
    let photos = dir.path().join("more");
    std::fs::create_dir(&photos)?;
    std::fs::write(photos.join("new.NEF"), b"new raw")?;
    server.add_folder(&photos)?;
    assert!(
        server
            .photos()?
            .iter()
            .any(|p| p.filename == "new.NEF" && p.id > copy)
    );
    Ok(())
}

#[test]
fn a_dropped_connection_is_replaced_between_transactions_only() -> Result<()> {
    let Some(config) = config() else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    let mut cat = schema.open()?;
    let quick = cat.quick_collection()?;
    let drop_connection = |cat: &Catalog| -> Result<()> {
        let pid: i64 = cat
            .db
            .query_row("SELECT pg_backend_pid()", (), |r| r.get(0))?;
        Schema::admin(&config, &format!("SELECT pg_terminate_backend({pid})"))?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(())
    };
    // The server closed it while idle: the next call connects again.
    drop_connection(&cat)?;
    assert_eq!(cat.quick_collection()?, quick);
    // Closed in the middle of a transaction, the change fails rather than
    // silently committing nothing.
    let tx = cat.db.transaction()?;
    tx.execute("INSERT INTO roots(original_path) VALUES ('/a')", ())?;
    drop_connection(&cat)?;
    assert!(
        tx.execute("INSERT INTO roots(original_path) VALUES ('/b')", ())
            .is_err()
    );
    drop(tx);
    assert!(cat.roots()?.is_empty());
    cat.change_collection(quick, &[], &[])?;
    Ok(())
}

#[test]
fn a_first_revision_server_catalog_is_upgraded_when_opened() -> Result<()> {
    let Some(config) = config() else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    drop(schema.open()?);
    // As revision 1 made it: history text as TEXT.
    Schema::admin(
        &config,
        &format!(
            "SET search_path={0}; ALTER TABLE lightroom_history ALTER COLUMN text TYPE TEXT USING convert_from(text, 'UTF8');
             UPDATE meta SET value='1' WHERE key='schema'",
            schema.name
        ),
    )?;
    let cat = schema.open()?;
    cat.db.execute(
        "INSERT INTO lightroom_history(photo, position, name, text) VALUES (1, 1, '', ?)",
        [&vec![0u8, 255, 0]],
    )?;
    let kind: String = cat.db.query_row(
        "SELECT data_type FROM information_schema.columns WHERE table_schema = current_schema()
         AND table_name = 'lightroom_history' AND column_name = 'text'",
        (),
        |r| r.get(0),
    )?;
    assert_eq!(kind, "bytea");
    Ok(())
}

/// Copies a real catalog (RAWMAKASE_TEST_CATALOG, a copy of it) to a scratch
/// schema; for checking a large catalog by hand.
#[test]
#[ignore = "needs RAWMAKASE_TEST_CATALOG and a server"]
fn a_large_local_catalog_copies_to_the_server() -> Result<()> {
    let (Some(config), Ok(file)) = (config(), std::env::var("RAWMAKASE_TEST_CATALOG")) else {
        return Ok(());
    };
    let schema = Schema::new(&config)?;
    let local = Catalog::open(std::path::Path::new(&file))?;
    let server = schema.open()?;
    let started = std::time::Instant::now();
    let photos = local.copy_to_server(&server)?;
    eprintln!("{photos} photos in {:?}", started.elapsed());
    assert_eq!(local.photos()?.len(), server.photos()?.len());
    assert_eq!(
        format!("{:?}", local.folders()?),
        format!("{:?}", server.folders()?)
    );
    let (here, there) = (local.edit_times()?, server.edit_times()?);
    let different: Vec<_> = here
        .iter()
        .filter(|(id, t)| there.get(id) != Some(t))
        .take(5)
        .map(|(id, t)| (id, t, there.get(id)))
        .collect();
    assert!(
        here.len() == there.len() && different.is_empty(),
        "{} vs {}: {different:?}",
        here.len(),
        there.len()
    );
    assert_eq!(local.aspect_ratios()?.len(), server.aspect_ratios()?.len());
    Ok(())
}
