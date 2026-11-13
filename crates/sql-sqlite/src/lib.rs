//! The sqlite driver for SplitGrid's SQL pane: connect, run statements (streaming
//! rows as sql_core::Event), cancel, and introspect the catalog.
//!
//! rusqlite is blocking, so every call runs on tokio's blocking pool with the
//! connection behind a mutex; cancel goes through SQLite's interrupt handle,
//! which is safe to call from any thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Batch, InterruptHandle, ToSql};
use sql_core::{
    Canceller, Catalog, Column, ColumnInfo, ConnectConfig, Connection, Dialect, Event, ForeignKey,
    Index, Schema, Table, TableKind, TableRef, Value,
};

/// Rows per `Event::Rows`, and how long a partial batch may wait.
const BATCH: usize = 500;
const FLUSH_AFTER: Duration = Duration::from_millis(100);

/// Open a connection. Must be called inside a tokio runtime.
///
/// Like the Electron driver, a missing file is created.
pub async fn connect(cfg: &ConnectConfig) -> anyhow::Result<Box<dyn Connection>> {
    let path = cfg
        .file_path
        .clone()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(cfg.database.trim()));
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "SQLite requires a database file path."
    );
    let open = path.clone();
    let conn = tokio::task::spawn_blocking(move || -> rusqlite::Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(open)?;
        // better-sqlite3's default: wait for another writer instead of failing at once.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    })
    .await??;
    Ok(Box::new(SqliteConnection {
        interrupt: Arc::new(Interrupt(conn.get_interrupt_handle())),
        conn: Arc::new(Mutex::new(conn)),
        path: path.to_string_lossy().into_owned(),
    }))
}

struct Interrupt(InterruptHandle);

impl Canceller for Interrupt {
    fn cancel(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        self.0.interrupt();
        futures::future::ready(Ok(())).boxed()
    }
}

struct SqliteConnection {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<Interrupt>,
    path: String,
}

impl SqliteConnection {
    /// Runs `f` with the connection on the blocking pool.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&lock(&conn))).await?
    }
}

fn lock(conn: &Mutex<rusqlite::Connection>) -> MutexGuard<'_, rusqlite::Connection> {
    conn.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait::async_trait]
impl Connection for SqliteConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn server_version(&self) -> String {
        format!("SQLite {}", rusqlite::version())
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        events: UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.blocking(move |conn| run(conn, &sql, &params, &events))
            .await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.interrupt.clone()
    }

    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.path.clone()])
    }

    async fn introspect(&mut self) -> anyhow::Result<Catalog> {
        let path = self.path.clone();
        self.blocking(move |conn| catalog(conn, path)).await
    }

    async fn ddl(&mut self, table: &TableRef) -> anyhow::Result<String> {
        let table = table.clone();
        self.blocking(move |conn| ddl(conn, &table)).await
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Binds a pane value the way SQLite stores it (no date or decimal classes).
struct Param<'a>(&'a Value);

impl ToSql for Param<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        use rusqlite::types::Value as V;
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Owned(V::Null),
            Value::Bool(b) => ToSqlOutput::Owned(V::Integer(*b as i64)),
            Value::Int(i) => ToSqlOutput::Owned(V::Integer(*i)),
            Value::Float(f) => ToSqlOutput::Owned(V::Real(*f)),
            Value::Bytes(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Value::Json(j) => ToSqlOutput::Owned(V::Text(j.to_string())),
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Uuid(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::Timestamp(s)
            | Value::Other(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
        })
    }
}

/// A cell by storage class, refined by the declared type where SQLite has no
/// class of its own (BOOLEAN, JSON). Dates stay the text (or number) stored.
fn value(v: ValueRef<'_>, decl: &str) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) if decl.contains("bool") && (i == 0 || i == 1) => Value::Bool(i == 1),
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => {
            let s = String::from_utf8_lossy(t);
            match decl
                .contains("json")
                .then(|| serde_json::from_str(&s).ok())
                .flatten()
            {
                Some(j) => Value::Json(j),
                None => Value::Text(s.into_owned()),
            }
        }
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

/// `main` is the default schema, so it stays implicit in table references.
fn table_ref(db: &str, name: &str) -> TableRef {
    TableRef {
        schema: (db != "main").then(|| db.to_owned()),
        name: name.to_owned(),
    }
}

/// Runs each statement in `sql` (the pane sends one, but a script still works:
/// one result set per statement). Parameters bind to every statement that takes them.
fn run(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Value],
    events: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let mut batch = Batch::new(conn, sql);
    while let Some(mut stmt) = batch.next()? {
        for i in 1..=stmt.parameter_count() {
            let v = params
                .get(i - 1)
                .ok_or_else(|| anyhow::anyhow!("no value for parameter {i}"))?;
            stmt.raw_bind_parameter(i, Param(v))?;
        }
        if stmt.column_count() == 0 {
            // changes() keeps the last INSERT/UPDATE/DELETE's count through DDL;
            // total_changes() only moves when this statement changed rows.
            let before = conn.total_changes();
            stmt.raw_execute()?;
            let affected = if conn.total_changes() == before {
                0
            } else {
                conn.changes()
            };
            if events
                .unbounded_send(Event::ResultEnd {
                    affected: Some(affected),
                })
                .is_err()
            {
                return Ok(());
            }
            continue;
        }

        let decls: Vec<String> = stmt
            .columns()
            .iter()
            .map(|c| c.decl_type().unwrap_or_default().to_lowercase())
            .collect();
        let columns = stmt
            .columns_with_metadata()
            .iter()
            .zip(&decls)
            .map(|(m, decl)| Column {
                name: m.name().to_owned(),
                type_name: decl.clone(),
                origin: m
                    .table_name()
                    .map(|t| table_ref(m.database_name().unwrap_or("main"), t)),
            })
            .collect();
        if events.unbounded_send(Event::ResultStart(columns)).is_err() {
            return Ok(());
        }

        let mut rows = stmt.raw_query();
        let mut pending = Vec::new();
        let mut last_flush = Instant::now();
        while let Some(row) = rows.next()? {
            pending.push(
                decls
                    .iter()
                    .enumerate()
                    .map(|(i, d)| row.get_ref(i).map(|v| value(v, d)))
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            // A slow query still shows its first rows without waiting for a full batch.
            if pending.len() >= BATCH || last_flush.elapsed() >= FLUSH_AFTER {
                if events
                    .unbounded_send(Event::Rows(std::mem::take(&mut pending)))
                    .is_err()
                {
                    return Ok(());
                }
                last_flush = Instant::now();
            }
        }
        if !pending.is_empty() && events.unbounded_send(Event::Rows(pending)).is_err() {
            return Ok(());
        }
        if events
            .unbounded_send(Event::ResultEnd { affected: None })
            .is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `main` and every attached database (not `temp`), each a schema.
fn catalog(conn: &rusqlite::Connection, path: String) -> anyhow::Result<Catalog> {
    let dbs: Vec<String> = conn
        .prepare("select name from pragma_database_list where name <> 'temp' order by seq")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let schemas = dbs
        .iter()
        .map(|db| schema(conn, db))
        .collect::<anyhow::Result<_>>()?;
    Ok(Catalog {
        database: path,
        search_path: dbs,
        schemas,
    })
}

fn schema(conn: &rusqlite::Connection, db: &str) -> anyhow::Result<Schema> {
    let rels: Vec<(String, String, bool)> = conn
        .prepare(
            "select name, type, wr from pragma_table_list where schema = ?1
             and type in ('table', 'view', 'virtual') and name not like 'sqlite\\_%' escape '\\' order by name",
        )?
        .query_map([db], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // Row counts would be a full scan per table; ANALYZE's estimates are free.
    let estimate = |table: &str| -> Option<u64> {
        conn.query_row(
            &format!(
                "select max(cast(stat as integer)) from {}.sqlite_stat1 where tbl = ?1",
                quote(db)
            ),
            [table],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .and_then(|n| u64::try_from(n).ok())
    };

    let mut tables = Vec::with_capacity(rels.len());
    for (name, kind, without_rowid) in rels {
        let kind = if kind == "view" {
            TableKind::View
        } else {
            TableKind::Table
        };
        let mut table = Table {
            row_estimate: (kind == TableKind::Table)
                .then(|| estimate(&name))
                .flatten(),
            name,
            kind,
            ..Default::default()
        };
        // A virtual table whose module isn't loaded can't be described; list it bare.
        let _ = describe(conn, db, &mut table, without_rowid);
        tables.push(table);
    }

    // An FK without target columns references the parent's primary key.
    let pks: std::collections::HashMap<String, Vec<String>> = tables
        .iter()
        .map(|t| (t.name.clone(), t.primary_key.clone()))
        .collect();
    for fk in tables.iter_mut().flat_map(|t| t.foreign_keys.iter_mut()) {
        if fk.ref_columns.iter().any(String::is_empty)
            && let Some(pk) = pks.get(&fk.references.name)
        {
            fk.ref_columns = pk.clone();
        }
    }
    Ok(Schema {
        name: db.to_owned(),
        tables,
        ..Default::default()
    })
}

fn describe(
    conn: &rusqlite::Connection,
    db: &str,
    t: &mut Table,
    without_rowid: bool,
) -> rusqlite::Result<()> {
    let mut pk: Vec<(i64, String)> = Vec::new();
    let mut stmt = conn.prepare(r#"select name, type, "notnull", dflt_value, pk, hidden from pragma_table_xinfo(?1, ?2) order by cid"#)?;
    let mut rows = stmt.query([&t.name, db])?;
    while let Some(r) = rows.next()? {
        let hidden: i64 = r.get(5)?;
        if hidden == 1 {
            continue; // a virtual table's hidden column
        }
        let name: String = r.get(0)?;
        let pk_pos: i64 = r.get(4)?;
        if pk_pos > 0 {
            pk.push((pk_pos, name.clone()));
        }
        t.columns.push(ColumnInfo {
            name,
            type_name: r.get(1)?,
            nullable: r.get::<_, i64>(2)? == 0,
            default: r.get(3)?,
            generated: hidden >= 2,
            comment: None,
        });
    }
    pk.sort();
    t.primary_key = pk.into_iter().map(|(_, n)| n).collect();
    // INTEGER PRIMARY KEY aliases the rowid: SQLite assigns it.
    if let [only] = t.primary_key.as_slice()
        && !without_rowid
        && let Some(c) = t
            .columns
            .iter_mut()
            .find(|c| &c.name == only && c.type_name.eq_ignore_ascii_case("integer"))
    {
        c.generated = true;
    }

    let mut stmt = conn.prepare(
        r#"select id, "table", "from", "to" from pragma_foreign_key_list(?1, ?2) order by id, seq"#,
    )?;
    let mut rows = stmt.query([&t.name, db])?;
    let mut last_id = None;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if last_id != Some(id) {
            last_id = Some(id);
            let target: String = r.get(1)?;
            t.foreign_keys.push(ForeignKey {
                name: format!("fk_{}_{id}", t.name),
                references: table_ref(db, &target),
                ..Default::default()
            });
        }
        if let Some(fk) = t.foreign_keys.last_mut() {
            fk.columns.push(r.get(2)?);
            fk.ref_columns
                .push(r.get::<_, Option<String>>(3)?.unwrap_or_default());
        }
    }

    let indexes: Vec<(String, bool)> = conn
        .prepare(r#"select name, "unique" from pragma_index_list(?1, ?2) order by name"#)?
        .query_map([&t.name, db], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut cols = conn.prepare(
        "select coalesce(name, '(expression)') from pragma_index_info(?1, ?2) order by seqno",
    )?;
    for (name, unique) in indexes {
        let columns = cols
            .query_map([&name, db], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        t.indexes.push(Index {
            name,
            columns,
            unique,
        });
    }
    Ok(())
}

/// The stored CREATE statement, followed by the table's indexes and triggers.
fn ddl(conn: &rusqlite::Connection, t: &TableRef) -> anyhow::Result<String> {
    let master = format!(
        "{}.sqlite_schema",
        quote(t.schema.as_deref().unwrap_or("main"))
    );
    let create: String = conn
        .query_row(
            &format!("select sql from {master} where name = ?1 and type in ('table', 'view')"),
            [&t.name],
            |r| r.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow::anyhow!("{} not found", t.name),
            e => e.into(),
        })?;
    let mut out = format!("{create};\n");
    let extras: Vec<String> = conn
        .prepare(&format!("select sql from {master} where tbl_name = ?1 and type in ('index', 'trigger') and sql is not null order by type, name"))?
        .query_map([&t.name], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !extras.is_empty() {
        out.push('\n');
        for sql in extras {
            out.push_str(&format!("{sql};\n"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
