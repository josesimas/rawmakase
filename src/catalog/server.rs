//! A catalog kept on a PostgreSQL server instead of in a file.
//!
//! Preferences > Catalog chooses between the two and holds the connection
//! details. A server catalog is identified like a file one, by a `path`:
//! `postgres://user@host:port/database`, which `Catalog::open` accepts. The
//! password is never part of it; it is read from the saved [`Settings`].
use super::{Catalog, db::Db};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const SCHEME: &str = "postgres://";
/// Marks a database as a RAWmakase catalog, in its `meta` table.
const FORMAT: &str = "rawmakase";
/// Which version of `schema_pg.sql` a database has; raised whenever that file
/// gains a table, so only then is it run again on open.
const SCHEMA_REVISION: &str = "2";
/// Held while a catalog's tables are made or brought up to date, so two
/// computers opening a new server catalog together do not both create them.
const SCHEMA_LOCK: i64 = 0x4f4d_4152_5047;

/// Which kind of catalog the application uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// A `.rawmakase` file on this computer.
    #[default]
    Local,
    /// The PostgreSQL catalog in [`Settings::server`].
    Server,
}

/// How to reach a PostgreSQL server and the database that holds the catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 5432,
            database: String::new(),
            user: String::new(),
            password: String::new(),
        }
    }
}

impl ServerConfig {
    /// Whether enough is filled in to try connecting.
    pub fn is_complete(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.database.trim().is_empty()
            && !self.user.trim().is_empty()
            && self.port != 0
    }
    /// The catalog's identity, without the password.
    pub fn location(&self) -> PathBuf {
        PathBuf::from(format!(
            "{SCHEME}{}@{}:{}/{}",
            self.user.trim(),
            self.host.trim(),
            self.port,
            self.database.trim()
        ))
    }
    /// What the server named by `location` needs from `self` to be the one:
    /// the same user, host, port and database.
    pub fn is_for(&self, location: &Path) -> bool {
        self.location() == location
    }
    fn connect_config(&self, schema: Option<&str>) -> postgres::Config {
        let mut c = postgres::Config::new();
        c.host(self.host.trim())
            .port(self.port)
            .user(self.user.trim())
            .password(&self.password)
            .dbname(self.database.trim())
            .connect_timeout(Duration::from_secs(8))
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(30))
            .application_name("RAWmakase");
        if let Some(schema) = schema {
            c.options(&format!("-c search_path={schema}"));
        }
        c
    }
    fn connect(&self, schema: Option<&str>) -> Result<postgres::Client> {
        ensure!(self.is_complete(), "Fill in the server, database and user");
        self.connect_config(schema)
            .connect(postgres::NoTls)
            .map_err(|e| anyhow::anyhow!("{}", describe(&e)))
            .with_context(|| format!("Cannot connect to {}", self.location().display()))
    }
    /// Connects and reports the server's version; for Preferences' Test.
    pub fn test(&self) -> Result<String> {
        let mut client = self.connect(None)?;
        let row = client.query_one("SHOW server_version", &[])?;
        let version: String = row.get(0);
        let tables: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_tables WHERE schemaname = current_schema()",
                &[],
            )?
            .get(0);
        let is_catalog = client
            .query_opt(
                "SELECT 1 FROM pg_tables WHERE schemaname = current_schema() AND tablename = 'meta'",
                &[],
            )?
            .is_some();
        Ok(if is_catalog {
            format!("Connected: PostgreSQL {version}, holding a RAWmakase catalog")
        } else if tables == 0 {
            format!(
                "Connected: PostgreSQL {version}; the database is empty and will become a catalog"
            )
        } else {
            bail!("The database already holds other tables; use an empty database")
        })
    }
}

/// The message a connection error carries, which `postgres::Error` hides
/// behind "db error" when the server refused.
fn describe(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{} ({})", db.message(), db.code().code()),
        None => {
            let mut text = e.to_string();
            let mut source = std::error::Error::source(e);
            while let Some(s) = source {
                text.push_str(": ");
                text.push_str(&s.to_string());
                source = s.source();
            }
            text
        }
    }
}

/// Whether `path` names a server catalog rather than a file.
pub fn is_location(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.starts_with(SCHEME))
}
/// The catalog's name for status lines and titles: "database on host" for a
/// server, else the file's name.
pub fn display_name(path: &Path) -> String {
    match path.to_str().and_then(|p| p.strip_prefix(SCHEME)) {
        Some(rest) => {
            let (_, rest) = rest.split_once('@').unwrap_or(("", rest));
            let (host, database) = rest.split_once('/').unwrap_or((rest, ""));
            let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
            format!("{database} on {host}")
        }
        None => path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    }
}

/// The saved choice of catalog and server, in `catalog-server.json` of the
/// data folder. Readable only by its user: it holds the password.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    pub server: ServerConfig,
    /// The local catalog last open, which choosing Local returns to.
    pub last_local: Option<PathBuf>,
}
impl Settings {
    fn file() -> PathBuf {
        crate::storage::data_dir().join("catalog-server.json")
    }
    pub fn load() -> Self {
        crate::storage::read_json_or_default(&Self::file())
    }
    pub fn save(&self) -> Result<()> {
        crate::storage::atomic_json(&Self::file(), self)
    }
    /// The server catalog, when that is what is chosen.
    pub fn active_server(&self) -> Option<&ServerConfig> {
        (self.mode == Mode::Server && self.server.is_complete()).then_some(&self.server)
    }
}

impl Catalog {
    /// Opens the catalog in a server's database, making its tables the first
    /// time. Refuses a database that holds something else.
    pub fn open_server(config: &ServerConfig) -> Result<Self> {
        Self::open_server_in(config, None)
    }
    /// `open_server`, in the PostgreSQL schema `schema` rather than the
    /// default one: tests keep their catalogs apart that way.
    pub(crate) fn open_server_in(config: &ServerConfig, schema: Option<&str>) -> Result<Self> {
        let (again, schema_name) = (config.clone(), schema.map(String::from));
        let db = Db::postgres(
            config.connect(schema)?,
            Box::new(move || again.connect(schema_name.as_deref())),
        );
        // `execute`, as the lock call returns void, which a row cannot hold.
        db.execute("SELECT pg_advisory_lock(?)", [SCHEMA_LOCK])?;
        let made = prepare_tables(&db);
        let _ = db.execute("SELECT pg_advisory_unlock(?)", [SCHEMA_LOCK]);
        made?;
        Ok(Self {
            path: config.location(),
            db,
        })
    }
}

/// Makes the tables of a new database, or checks that the database is a
/// catalog this release can use and adds the tables added since.
fn prepare_tables(db: &Db) -> Result<()> {
    let tables: i64 = db.query_row(
        "SELECT count(*) FROM pg_tables WHERE schemaname = current_schema()",
        (),
        |r| r.get(0),
    )?;
    let has_meta = db
        .query_opt(
            "SELECT 1 FROM pg_tables WHERE schemaname = current_schema() AND tablename = 'meta'",
            (),
            |r| r.get::<i64>(0),
        )?
        .is_some();
    if has_meta {
        let format: Option<String> =
            db.query_opt("SELECT value FROM meta WHERE key = 'format'", (), |r| {
                r.get(0)
            })?;
        ensure!(
            format.as_deref() == Some(FORMAT),
            "This database is not a RAWmakase catalog"
        );
        let version: Option<String> =
            db.query_opt("SELECT value FROM meta WHERE key = 'version'", (), |r| {
                r.get(0)
            })?;
        ensure!(
            version.as_deref() == Some(&super::VERSION.to_string()),
            "Unsupported RAWmakase catalog version; database left unchanged"
        );
    } else {
        ensure!(
            tables == 0,
            "This database already holds other tables; use an empty database for the catalog"
        );
    }
    let current: Option<String> = if has_meta {
        db.query_opt("SELECT value FROM meta WHERE key = 'schema'", (), |r| {
            r.get(0)
        })?
    } else {
        None
    };
    if current.as_deref() == Some(SCHEMA_REVISION) {
        return Ok(());
    }
    let tx = db.transaction()?;
    tx.execute_batch(include_str!("schema_pg.sql"))?;
    super::set_meta(&tx, "format", FORMAT)?;
    super::set_meta(&tx, "version", &super::VERSION.to_string())?;
    super::set_meta(&tx, "schema", SCHEMA_REVISION)?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ServerConfig {
        ServerConfig {
            host: " db.local ".into(),
            port: 5433,
            database: "Catalog".into(),
            user: "ana".into(),
            password: "secret".into(),
        }
    }

    #[test]
    fn a_server_catalog_is_identified_without_its_password() {
        let location = config().location();
        assert_eq!(location, Path::new("postgres://ana@db.local:5433/Catalog"));
        assert!(is_location(&location));
        assert!(!is_location(Path::new("/photos/Photos.rawmakase")));
        assert!(!location.to_string_lossy().contains("secret"));
        assert!(config().is_for(&location));
        let other = ServerConfig {
            port: 5432,
            ..config()
        };
        assert!(!other.is_for(&location));
    }

    #[test]
    fn catalogs_are_named_for_their_status_line() {
        assert_eq!(display_name(&config().location()), "Catalog on db.local");
        assert_eq!(
            display_name(Path::new("/photos/Holiday.rawmakase")),
            "Holiday"
        );
    }

    #[test]
    fn details_need_a_server_database_user_and_port() {
        assert!(config().is_complete());
        assert!(!ServerConfig::default().is_complete());
        assert!(
            !ServerConfig {
                port: 0,
                ..config()
            }
            .is_complete()
        );
        assert!(
            !ServerConfig {
                user: " ".into(),
                ..config()
            }
            .is_complete()
        );
    }

    #[test]
    fn settings_read_older_or_partial_files_and_pick_a_catalog_by_mode() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.mode, Mode::Local);
        assert!(settings.active_server().is_none());
        let settings: Settings = serde_json::from_str(
            r#"{"mode":"server","server":{"host":"h","database":"d","user":"u"}}"#,
        )
        .unwrap();
        assert_eq!(settings.server.port, 5432);
        assert_eq!(settings.active_server().unwrap().host, "h");
        let local = Settings {
            mode: Mode::Local,
            ..settings
        };
        assert!(local.active_server().is_none());
    }

    #[test]
    fn opening_a_server_catalog_needs_matching_saved_details() {
        let nothing = Catalog::open(Path::new("postgres://nobody@nowhere.invalid:1/none"));
        assert!(nothing.is_err());
    }
}
