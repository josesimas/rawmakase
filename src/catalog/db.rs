//! The database behind a catalog: a local SQLite file or a PostgreSQL server.
//!
//! Catalog code is written once, in SQL both engines understand, against
//! [`Db`]. Statements use `?` (or `?1`) placeholders and return ids with
//! `RETURNING id`; the few dialect differences are translated for PostgreSQL
//! here, or branch on [`Db::is_postgres`].
use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use std::{cell::RefCell, collections::HashMap};

/// One value in or out of a statement, as SQLite names its storage classes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A Rust value a statement can bind.
pub trait ToValue {
    fn to_value(&self) -> Value;
}
macro_rules! int_value {
    ($($t:ty),*) => {$(impl ToValue for $t {
        fn to_value(&self) -> Value { Value::Int(*self as i64) }
    })*};
}
int_value!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);
impl ToValue for bool {
    fn to_value(&self) -> Value {
        Value::Int(i64::from(*self))
    }
}
impl ToValue for f64 {
    fn to_value(&self) -> Value {
        Value::Real(*self)
    }
}
impl ToValue for f32 {
    fn to_value(&self) -> Value {
        Value::Real(f64::from(*self))
    }
}
impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Text(self.into())
    }
}
impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}
impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToValue for Vec<u8> {
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
}
impl ToValue for [u8] {
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        self.as_ref().map_or(Value::Null, T::to_value)
    }
}

/// The parameters of a statement.
pub trait Params {
    fn values(&self) -> Vec<Value>;
}
impl Params for () {
    fn values(&self) -> Vec<Value> {
        Vec::new()
    }
}
impl<T: ToValue, const N: usize> Params for [T; N] {
    fn values(&self) -> Vec<Value> {
        self.iter().map(ToValue::to_value).collect()
    }
}
impl Params for &[&dyn ToValue] {
    fn values(&self) -> Vec<Value> {
        self.iter().map(|v| v.to_value()).collect()
    }
}
impl Params for Vec<Value> {
    fn values(&self) -> Vec<Value> {
        self.clone()
    }
}
/// Mixed-type parameters: `params![id, name]`.
macro_rules! params {
    () => { () };
    ($($p:expr),+ $(,)?) => {
        &[$(&$p as &dyn $crate::catalog::db::ToValue),+] as &[&dyn $crate::catalog::db::ToValue]
    };
}
pub(crate) use params;

/// A Rust value a column can be read as.
pub trait FromValue: Sized {
    fn from_value(v: Value) -> Result<Self>;
}
macro_rules! int_from {
    ($($t:ty),*) => {$(impl FromValue for $t {
        fn from_value(v: Value) -> Result<Self> {
            match v {
                Value::Int(i) => <$t>::try_from(i).map_err(|_| anyhow!("Integer {i} out of range")),
                Value::Real(f) if f.fract() == 0. => Ok(f as $t),
                other => Err(anyhow!("Expected an integer, found {other:?}")),
            }
        }
    })*};
}
int_from!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);
impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Int(i) => Ok(i != 0),
            other => Err(anyhow!("Expected a flag, found {other:?}")),
        }
    }
}
impl FromValue for f64 {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Real(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            other => Err(anyhow!("Expected a number, found {other:?}")),
        }
    }
}
impl FromValue for f32 {
    fn from_value(v: Value) -> Result<Self> {
        f64::from_value(v).map(|f| f as f32)
    }
}
impl FromValue for String {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Text(s) => Ok(s),
            Value::Int(i) => Ok(i.to_string()),
            Value::Real(f) => Ok(f.to_string()),
            other => Err(anyhow!("Expected text, found {other:?}")),
        }
    }
}
impl FromValue for Vec<u8> {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Blob(b) => Ok(b),
            Value::Text(s) => Ok(s.into_bytes()),
            other => Err(anyhow!("Expected bytes, found {other:?}")),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Null => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

/// One result row.
pub struct Row(Vec<Value>);
impl Row {
    pub fn get<T: FromValue>(&self, i: usize) -> Result<T> {
        let v = self
            .0
            .get(i)
            .with_context(|| format!("No column {i}"))?
            .clone();
        T::from_value(v).with_context(|| format!("Column {i}"))
    }
    pub fn values(&self) -> Vec<Value> {
        self.0.clone()
    }
}

/// The connection: SQLite or PostgreSQL. One user at a time, like a rusqlite
/// `Connection`.
pub struct Db(Kind);
enum Kind {
    Sqlite(Connection),
    Postgres(Box<Pg>),
}
struct Pg {
    client: RefCell<postgres::Client>,
    statements: RefCell<HashMap<String, postgres::Statement>>,
    /// Makes a new connection when the server dropped this one (the computer
    /// slept, the network changed), between transactions only.
    reconnect: Box<dyn Fn() -> Result<postgres::Client> + Send>,
    in_transaction: std::cell::Cell<bool>,
    last_used: std::cell::Cell<std::time::Instant>,
}
/// After this long unused, a connection is tested before the next statement:
/// a computer that slept, or a router that forgot the connection, leaves one
/// that only fails when used.
const IDLE_CHECK: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(100)
} else {
    std::time::Duration::from_secs(5)
};

impl Db {
    pub fn sqlite(conn: Connection) -> Self {
        Db(Kind::Sqlite(conn))
    }
    pub fn postgres(
        client: postgres::Client,
        reconnect: Box<dyn Fn() -> Result<postgres::Client> + Send>,
    ) -> Self {
        Db(Kind::Postgres(Box::new(Pg {
            client: RefCell::new(client),
            statements: RefCell::new(HashMap::new()),
            reconnect,
            in_transaction: std::cell::Cell::new(false),
            last_used: std::cell::Cell::new(std::time::Instant::now()),
        })))
    }
    pub fn is_postgres(&self) -> bool {
        matches!(self.0, Kind::Postgres(_))
    }
    /// The SQLite connection, for what only SQLite does (Lightroom's own
    /// catalogs, pragmas) and for tests.
    pub fn sqlite_connection(&self) -> Option<&Connection> {
        match &self.0 {
            Kind::Sqlite(c) => Some(c),
            Kind::Postgres(_) => None,
        }
    }
    pub fn postgres_client(&self) -> Option<&RefCell<postgres::Client>> {
        match &self.0 {
            Kind::Postgres(p) => Some(&p.client),
            Kind::Sqlite(_) => None,
        }
    }

    /// Runs one statement; the rows it changed.
    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        let values = params.values();
        match &self.0 {
            Kind::Sqlite(c) => {
                let mut stmt = c.prepare_cached(sql)?;
                Ok(stmt.execute(rusqlite::params_from_iter(values.iter().map(sqlite_value)))?)
            }
            Kind::Postgres(p) => {
                let stmt = p.statement(sql)?;
                let args = pg_args(&values);
                let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    args.iter().map(|a| a as _).collect();
                Ok(p.client.borrow_mut().execute(&stmt, &refs)? as usize)
            }
        }
    }
    /// Runs statements without parameters.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.0 {
            Kind::Sqlite(c) => Ok(c.execute_batch(sql)?),
            Kind::Postgres(p) => {
                p.live()?;
                Ok(p.client.borrow_mut().batch_execute(&translate(sql))?)
            }
        }
    }
    /// Every row of a query, each mapped by `f`.
    pub fn query_all<T>(
        &self,
        sql: &str,
        params: impl Params,
        mut f: impl FnMut(&Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        let values = params.values();
        match &self.0 {
            Kind::Sqlite(c) => {
                let mut stmt = c.prepare_cached(sql)?;
                let columns = stmt.column_count();
                let mut rows =
                    stmt.query(rusqlite::params_from_iter(values.iter().map(sqlite_value)))?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    let row = Row(sqlite_row(r, columns)?);
                    out.push(f(&row)?);
                }
                Ok(out)
            }
            Kind::Postgres(p) => {
                let stmt = p.statement(sql)?;
                let args = pg_args(&values);
                let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    args.iter().map(|a| a as _).collect();
                let rows = p.client.borrow_mut().query(&stmt, &refs)?;
                rows.iter().map(|r| f(&Row(pg_row(r)?))).collect()
            }
        }
    }
    /// The first row of a query, or `None`.
    pub fn query_opt<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut f = Some(f);
        let mut rows = self.query_all(sql, params, |r| Ok(f.take().map(|f| f(r))))?;
        rows.drain(..).next().flatten().transpose()
    }
    /// The first row of a query; an error without one.
    pub fn query_row<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<T> {
        self.query_opt(sql, params, f)?
            .context("Query returned no rows")
    }
    /// The id a statement with `RETURNING id` made.
    pub fn insert(&self, sql: &str, params: impl Params) -> Result<i64> {
        self.query_row(sql, params, |r| r.get(0))
    }

    fn left_transaction(&self) {
        if let Kind::Postgres(p) = &self.0 {
            p.in_transaction.set(false);
        }
    }
    /// Starts a transaction, rolled back unless committed.
    pub fn transaction(&self) -> Result<Tx<'_>> {
        self.execute_batch("BEGIN")?;
        if let Kind::Postgres(p) = &self.0 {
            p.in_transaction.set(true);
        }
        Ok(Tx {
            db: self,
            savepoint: None,
            done: false,
        })
    }
}

impl Pg {
    /// Replaces a connection the server closed, unless a transaction was
    /// open on it: its changes are gone, and carrying on would commit nothing.
    fn live(&self) -> Result<()> {
        let idle = self.last_used.replace(std::time::Instant::now()).elapsed() > IDLE_CHECK;
        let mut client = self.client.borrow_mut();
        let healthy = !client.is_closed()
            && (!idle
                || self.in_transaction.get()
                || client.is_valid(std::time::Duration::from_secs(3)).is_ok());
        drop(client);
        if healthy {
            return Ok(());
        }
        if self.in_transaction.get() {
            self.in_transaction.set(false);
            anyhow::bail!("The connection to the server was lost; the change was not saved");
        }
        *self.client.borrow_mut() = (self.reconnect)()?;
        self.statements.borrow_mut().clear();
        Ok(())
    }
    fn statement(&self, sql: &str) -> Result<postgres::Statement> {
        self.live()?;
        if let Some(s) = self.statements.borrow().get(sql) {
            return Ok(s.clone());
        }
        let stmt = self.client.borrow_mut().prepare(&translate(sql))?;
        self.statements
            .borrow_mut()
            .insert(sql.into(), stmt.clone());
        Ok(stmt)
    }
}

/// A transaction, or a savepoint inside one. Dropping it undoes its changes.
pub struct Tx<'a> {
    db: &'a Db,
    savepoint: Option<String>,
    done: bool,
}
impl<'a> Tx<'a> {
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        match &self.savepoint {
            None => {
                let committed = self.db.execute_batch("COMMIT");
                self.db.left_transaction();
                committed
            }
            Some(name) => self.db.execute_batch(&format!("RELEASE {name}")),
        }
    }
    /// A part of this transaction that can be undone on its own.
    pub fn savepoint(&self) -> Result<Tx<'a>> {
        let name = format!(
            "sp{}",
            SAVEPOINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.db.execute_batch(&format!("SAVEPOINT {name}"))?;
        Ok(Tx {
            db: self.db,
            savepoint: Some(name),
            done: false,
        })
    }
}
static SAVEPOINTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
impl std::ops::Deref for Tx<'_> {
    type Target = Db;
    fn deref(&self) -> &Db {
        self.db
    }
}
impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _ = match &self.savepoint {
            None => {
                let rolled = self.db.execute_batch("ROLLBACK");
                self.db.left_transaction();
                rolled
            }
            Some(name) => self
                .db
                .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}")),
        };
    }
}

fn sqlite_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Int(i) => rusqlite::types::Value::Integer(*i),
        Value::Real(f) => rusqlite::types::Value::Real(*f),
        Value::Text(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Blob(b) => rusqlite::types::Value::Blob(b.clone()),
    }
}
fn sqlite_row(r: &rusqlite::Row<'_>, columns: usize) -> Result<Vec<Value>> {
    use rusqlite::types::ValueRef;
    (0..columns)
        .map(|i| {
            Ok(match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::Int(i),
                ValueRef::Real(f) => Value::Real(f),
                ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Value::Blob(b.to_vec()),
            })
        })
        .collect()
}
/// SQLite's values as PostgreSQL columns of the types a statement expects.
#[derive(Debug)]
pub struct PgValue<'a>(pub &'a Value);
fn pg_args(values: &[Value]) -> Vec<PgValue<'_>> {
    values.iter().map(PgValue).collect()
}
impl postgres::types::ToSql for PgValue<'_> {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use postgres::types::{IsNull, Type};
        match (self.0, ty) {
            (Value::Null, _) => Ok(IsNull::Yes),
            (Value::Int(i), t) if *t == Type::INT8 => i.to_sql(t, out),
            (Value::Int(i), t) if *t == Type::INT4 => i32::try_from(*i)?.to_sql(t, out),
            (Value::Int(i), t) if *t == Type::INT2 => i16::try_from(*i)?.to_sql(t, out),
            (Value::Int(i), t) if *t == Type::FLOAT8 => (*i as f64).to_sql(t, out),
            (Value::Int(i), t) if *t == Type::BOOL => (*i != 0).to_sql(t, out),
            (Value::Int(i), t) => i.to_string().to_sql(t, out),
            (Value::Real(f), t) if *t == Type::FLOAT4 => (*f as f32).to_sql(t, out),
            (Value::Real(f), t) if *t == Type::FLOAT8 => f.to_sql(t, out),
            (Value::Real(f), t) if *t == Type::INT8 && f.fract() == 0. => {
                (*f as i64).to_sql(t, out)
            }
            (Value::Real(f), t) => f.to_string().to_sql(t, out),
            (Value::Text(s), t) if *t == Type::BYTEA => s.as_bytes().to_sql(t, out),
            (Value::Text(s), t) => s.as_str().to_sql(t, out),
            (Value::Blob(b), t) if *t == Type::BYTEA => b.as_slice().to_sql(t, out),
            (Value::Blob(b), t) => String::from_utf8(b.clone())?.to_sql(t, out),
        }
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
    postgres::types::to_sql_checked!();
}
fn pg_row(r: &postgres::Row) -> Result<Vec<Value>> {
    use postgres::types::Type;
    (0..r.len())
        .map(|i| {
            let ty = r.columns()[i].type_().clone();
            Ok(if ty == Type::INT8 {
                r.try_get::<_, Option<i64>>(i)?
                    .map_or(Value::Null, Value::Int)
            } else if ty == Type::INT4 {
                r.try_get::<_, Option<i32>>(i)?
                    .map_or(Value::Null, |v| Value::Int(v.into()))
            } else if ty == Type::INT2 {
                r.try_get::<_, Option<i16>>(i)?
                    .map_or(Value::Null, |v| Value::Int(v.into()))
            } else if ty == Type::FLOAT8 {
                r.try_get::<_, Option<f64>>(i)?
                    .map_or(Value::Null, Value::Real)
            } else if ty == Type::FLOAT4 {
                r.try_get::<_, Option<f32>>(i)?
                    .map_or(Value::Null, |v| Value::Real(v.into()))
            } else if ty == Type::BOOL {
                r.try_get::<_, Option<bool>>(i)?
                    .map_or(Value::Null, |v| Value::Int(v.into()))
            } else if ty == Type::BYTEA {
                r.try_get::<_, Option<Vec<u8>>>(i)?
                    .map_or(Value::Null, Value::Blob)
            } else if ty == Type::TEXT
                || ty == Type::VARCHAR
                || ty == Type::BPCHAR
                || ty == Type::NAME
            {
                r.try_get::<_, Option<String>>(i)?
                    .map_or(Value::Null, Value::Text)
            } else {
                anyhow::bail!("Unsupported column type {ty}")
            })
        })
        .collect()
}

/// A statement in the catalog's common SQL as PostgreSQL runs it: `?` and
/// `?N` placeholders become `$N`, and SQLite's `CURRENT_TIMESTAMP` text (UTC,
/// "YYYY-MM-DD HH:MM:SS") is spelled out.
fn translate(sql: &str) -> String {
    const NOW: &str = "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')";
    let sql = sql.replace("CURRENT_TIMESTAMP", NOW);
    let mut out = String::with_capacity(sql.len() + 8);
    let mut next = 1;
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                out.push(c)
            }
            (Some(_), c) => out.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                out.push(c)
            }
            (None, '?') => {
                let mut digits = String::new();
                while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                    digits.push(*d);
                    chars.next();
                }
                if digits.is_empty() {
                    out.push_str(&format!("${next}"));
                    next += 1;
                } else {
                    out.push_str(&format!("${digits}"));
                }
            }
            (None, c) => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_numbered_outside_quotes() {
        assert_eq!(
            translate("UPDATE t SET a=?, b='?' WHERE c=? AND d=?1"),
            "UPDATE t SET a=$1, b='?' WHERE c=$2 AND d=$1"
        );
    }
    #[test]
    fn sqlite_roundtrip_and_savepoints() -> Result<()> {
        let db = Db::sqlite(Connection::open_in_memory()?);
        db.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")?;
        let tx = db.transaction()?;
        let id = tx.insert("INSERT INTO t(v) VALUES (?) RETURNING id", ["a"])?;
        {
            let sp = tx.savepoint()?;
            sp.execute("INSERT INTO t(v) VALUES (?)", params!["b"])?;
        }
        tx.commit()?;
        let all = db.query_all("SELECT id, v FROM t", (), |r| {
            Ok((r.get::<i64>(0)?, r.get::<String>(1)?))
        })?;
        assert_eq!(all, vec![(id, "a".to_string())]);
        assert!(
            db.query_opt("SELECT v FROM t WHERE id=?", [99], |r| r.get::<String>(0))?
                .is_none()
        );
        Ok(())
    }
}
