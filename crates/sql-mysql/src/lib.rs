//! The MySQL/MariaDB driver for SplitGrid's SQL pane: connect, run statements
//! (streaming rows as sql_core::Event), cancel, and introspect the catalog.

mod introspect;
mod types;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Conn, Opts, OptsBuilder, QueryResult, SslOpts};
use sql_core::{Canceller, Catalog, Column, ConnectConfig, Connection, Dialect, Event, TableRef, Value};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const BATCH_ROWS: usize = 500;
/// A batch goes out early when rows trickle in, so a slow query still shows progress.
const BATCH_AGE: Duration = Duration::from_millis(100);

/// Open a connection. Must be called inside a tokio runtime.
pub async fn connect(cfg: &ConnectConfig) -> anyhow::Result<Box<dyn Connection>> {
    // mysql_async builds its TLS config from rustls's process default; with
    // ring the only provider compiled in, this is a no-op, but if another crate
    // ever enables aws-lc too, rustls would otherwise refuse to pick one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let opts = opts(cfg);
    let mut conn = open(&opts).await?;
    let raw: Option<String> = conn.query_first("select version()").await?;
    let (version, mariadb) = version_label(raw.as_deref().unwrap_or_default());
    let cancel = Arc::new(Cancel { opts: opts.clone(), id: AtomicU32::new(conn.id()), running: AtomicBool::new(false) });
    Ok(Box::new(MysqlConnection { opts, conn: Some(conn), version, mariadb, cancel }))
}

fn is_local(host: &str) -> bool {
    matches!(host, "" | "localhost" | "127.0.0.1" | "::1")
}

fn opts(cfg: &ConnectConfig) -> Opts {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let host = if cfg.host.is_empty() { "localhost" } else { cfg.host.as_str() };
    let mut b = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(if cfg.port == 0 { Dialect::Mysql.default_port() } else { cfg.port })
        .user(non_empty(&cfg.user))
        .pass(non_empty(&cfg.password))
        .db_name(non_empty(&cfg.database))
        // A local server would otherwise be reached over its unix socket,
        // bypassing the TLS the user asked for.
        .prefer_socket(false);
    if cfg.ssl {
        // As in the Electron driver (rejectUnauthorized off only for local
        // hosts): remote certificates are verified, local self-signed ones pass.
        let local = is_local(&cfg.host);
        b = b.ssl_opts(SslOpts::default().with_danger_accept_invalid_certs(local).with_danger_skip_domain_validation(local));
    }
    b.into()
}

async fn open(opts: &Opts) -> anyhow::Result<Conn> {
    tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts.clone()))
        .await
        .context("timed out connecting to the MySQL server")?
        .map_err(Into::into)
}

/// "MySQL 8.0.36" / "MariaDB 11.2.2" from `version()`, and whether it is MariaDB.
fn version_label(raw: &str) -> (String, bool) {
    let mariadb = raw.contains("MariaDB");
    // MariaDB 10 behind a MySQL 5.5 handshake reports "5.5.5-10.6.12-MariaDB".
    let raw = raw.strip_prefix("5.5.5-").unwrap_or(raw);
    let number = raw.split(['-', ' ']).next().unwrap_or(raw);
    (format!("{} {number}", if mariadb { "MariaDB" } else { "MySQL" }), mariadb)
}

/// What the canceller needs, shared with the connection.
struct Cancel {
    opts: Opts,
    /// The server's thread id of the current session (changes on reconnect).
    id: AtomicU32,
    running: AtomicBool,
}

impl Canceller for Cancel {
    /// `KILL QUERY` from a short-lived second connection: the statement fails
    /// with "Query execution was interrupted" and the session stays usable.
    fn cancel(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.load(Ordering::SeqCst);
        let (opts, id) = (self.opts.clone(), self.id.load(Ordering::SeqCst));
        Box::pin(async move {
            if !running {
                return Ok(());
            }
            let mut killer = open(&opts).await?;
            killer.query_drop(format!("KILL QUERY {id}")).await?;
            killer.disconnect().await?;
            Ok(())
        })
    }
}

struct MysqlConnection {
    opts: Opts,
    /// None once the session died (network, server restart); reopened lazily.
    conn: Option<Conn>,
    version: String,
    mariadb: bool,
    cancel: Arc<Cancel>,
}

/// The live session, reopening it if it was lost.
async fn session<'a>(slot: &'a mut Option<Conn>, opts: &Opts, cancel: &Cancel) -> anyhow::Result<&'a mut Conn> {
    if slot.is_none() {
        let conn = open(opts).await?;
        cancel.id.store(conn.id(), Ordering::SeqCst);
        *slot = Some(conn);
    }
    Ok(slot.as_mut().expect("just filled"))
}

/// A dead connection, as opposed to an SQL error the session survives.
fn is_fatal(e: &mysql_async::Error) -> bool {
    matches!(e, mysql_async::Error::Io(_) | mysql_async::Error::Driver(_))
}

impl MysqlConnection {
    async fn conn(&mut self) -> anyhow::Result<&mut Conn> {
        session(&mut self.conn, &self.opts, &self.cancel).await
    }

    /// Runs a metadata query and reads every cell as text.
    async fn text_rows(&mut self, sql: &str, param: Option<&str>) -> anyhow::Result<introspect::Rows> {
        let conn = self.conn().await?;
        let rows: Vec<mysql_async::Row> = match param {
            Some(p) => conn.exec(sql, (p,)).await,
            None => conn.query(sql).await,
        }
        .inspect_err(|e| {
            if is_fatal(e) {
                self.conn = None;
            }
        })?;
        Ok(rows.into_iter().map(|r| r.unwrap().into_iter().map(types::text).collect()).collect())
    }
}

/// Rows collected for one `Event::Rows`.
struct Batch {
    rows: Vec<Vec<Value>>,
    since: Instant,
}

impl Batch {
    fn new() -> Self {
        Batch { rows: Vec::new(), since: Instant::now() }
    }

    /// False once the receiver is gone.
    fn push(&mut self, row: Vec<Value>, tx: &UnboundedSender<Event>) -> bool {
        self.rows.push(row);
        if self.rows.len() >= BATCH_ROWS || self.since.elapsed() >= BATCH_AGE { self.flush(tx) } else { true }
    }

    fn flush(&mut self, tx: &UnboundedSender<Event>) -> bool {
        self.since = Instant::now();
        self.rows.is_empty() || tx.unbounded_send(Event::Rows(std::mem::take(&mut self.rows))).is_ok()
    }
}

/// How a statement's results ended.
#[derive(Default)]
struct Outcome {
    /// The receiver was dropped: stop without reading the rest.
    stopped: bool,
    warnings: bool,
}

fn result_columns(cols: &[mysql_async::Column]) -> Vec<Column> {
    cols.iter()
        .map(|c| {
            let table = c.org_table_str();
            let schema = c.schema_str();
            Column {
                name: c.name_str().into_owned(),
                type_name: types::type_name(c),
                origin: (!table.is_empty())
                    .then(|| TableRef { schema: (!schema.is_empty()).then(|| schema.into_owned()), name: table.into_owned() }),
            }
        })
        .collect()
}

/// Streams every result set of a statement (a `CALL` may return several).
async fn stream<P: Protocol>(r: &mut QueryResult<'_, 'static, P>, tx: &UnboundedSender<Event>) -> Result<Outcome, mysql_async::Error> {
    let stopped = Outcome { stopped: true, warnings: false };
    let mut out = Outcome::default();
    let mut had_rows = false;
    loop {
        match r.columns() {
            None => break,
            Some(cols) if cols.is_empty() => {
                let affected = r.affected_rows();
                out.warnings |= r.warnings() > 0;
                r.next().await?; // consumes the OK packet, moves to the next set
                // A procedure ends with a status-only result; after its rows that is noise.
                if !(had_rows && r.is_empty()) && tx.unbounded_send(Event::ResultEnd { affected: Some(affected) }).is_err() {
                    return Ok(stopped);
                }
            }
            Some(cols) => {
                if tx.unbounded_send(Event::ResultStart(result_columns(&cols))).is_err() {
                    return Ok(stopped);
                }
                let mut batch = Batch::new();
                while let Some(row) = r.next().await? {
                    let cells = row.unwrap().into_iter().zip(cols.iter()).map(|(v, c)| types::cell(c, v)).collect();
                    if !batch.push(cells, tx) {
                        return Ok(stopped);
                    }
                }
                out.warnings |= r.warnings() > 0;
                if !batch.flush(tx) || tx.unbounded_send(Event::ResultEnd { affected: None }).is_err() {
                    return Ok(stopped);
                }
                had_rows = true;
            }
        }
        if r.is_empty() {
            break;
        }
    }
    Ok(out)
}

async fn run(conn: &mut Conn, sql: &str, params: &[Value], tx: &UnboundedSender<Event>) -> Result<(), mysql_async::Error> {
    // Plain text protocol without parameters: it accepts every statement
    // (prepared statements reject some) and is what the mysql CLI does.
    let out = if params.is_empty() {
        let mut r = conn.query_iter(sql).await?;
        stream(&mut r, tx).await?
    } else {
        let mut r = conn.exec_iter(sql, params.iter().map(types::param).collect::<Vec<_>>()).await?;
        stream(&mut r, tx).await?
    };
    if out.warnings && !out.stopped {
        let rows: Vec<mysql_async::Row> = conn.query("SHOW WARNINGS").await?;
        for row in rows {
            let cells: Vec<String> = row.unwrap().into_iter().map(|v| types::text(v).unwrap_or_default()).collect();
            let [level, code, message] = cells.as_slice() else { continue };
            if tx.unbounded_send(Event::Notice(format!("{level} {code}: {message}"))).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Clears the running flag however `execute` ends, so a late cancel cannot
/// kill the next statement.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

#[async_trait::async_trait]
impl Connection for MysqlConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Mysql
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    async fn execute(&mut self, sql: &str, params: &[Value], events: UnboundedSender<Event>) -> anyhow::Result<()> {
        if self.conn.is_none() {
            self.conn().await?;
            let _ = events.unbounded_send(Event::Notice(
                "Reconnected: the previous session was lost (session variables, temporary tables and any open transaction are gone).".into(),
            ));
        }
        let cancel = self.cancel.clone();
        let conn = self.conn().await?;
        cancel.running.store(true, Ordering::SeqCst);
        let _running = Running(&cancel.running);
        let result = run(conn, sql, params, &events).await;
        if let Err(e) = &result
            && is_fatal(e)
        {
            self.conn = None;
        }
        result.map_err(Into::into)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.cancel.clone()
    }

    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let rows = self.text_rows("SHOW DATABASES", None).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter(|d| !introspect::SYSTEM_DATABASES.contains(&d.to_ascii_lowercase().as_str()))
            .collect())
    }

    async fn introspect(&mut self) -> anyhow::Result<Catalog> {
        let current = self.text_rows("select database()", None).await?;
        let db = current.into_iter().next().and_then(|r| r.into_iter().next().flatten());
        let db = db.as_deref();
        let mut fetch = async |sql: &str| self.text_rows(&introspect::scoped(sql, db), db).await;
        let fetched = introspect::Fetched {
            tables: fetch(introspect::TABLES).await?,
            columns: fetch(introspect::COLUMNS).await?,
            indexes: fetch(introspect::INDEXES).await?,
            foreign_keys: fetch(introspect::FOREIGN_KEYS).await?,
            routines: fetch(introspect::ROUTINES).await?,
            parameters: fetch(introspect::PARAMETERS).await?,
        };
        Ok(introspect::build(db, self.mariadb, fetched))
    }

    /// `SHOW CREATE TABLE`, which also answers for views (with their definition).
    async fn ddl(&mut self, table: &TableRef) -> anyhow::Result<String> {
        let target = match &table.schema {
            Some(s) => format!("{}.{}", quote(s), quote(&table.name)),
            None => quote(&table.name),
        };
        let rows = self.text_rows(&format!("SHOW CREATE TABLE {target}"), None).await?;
        rows.into_iter()
            .next()
            .and_then(|r| r.into_iter().nth(1).flatten())
            .with_context(|| format!("no DDL for {target}"))
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        if let Some(conn) = self.conn {
            conn.disconnect().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_versions_read_like_the_product() {
        assert_eq!(version_label("8.0.36"), ("MySQL 8.0.36".into(), false));
        assert_eq!(version_label("8.4.0-commercial"), ("MySQL 8.4.0".into(), false));
        assert_eq!(version_label("11.2.2-MariaDB-1:11.2.2+maria~ubu2204"), ("MariaDB 11.2.2".into(), true));
        assert_eq!(version_label("5.5.5-10.6.12-MariaDB-log"), ("MariaDB 10.6.12".into(), true));
    }

    #[test]
    fn identifiers_are_backtick_quoted() {
        assert_eq!(quote("we`ird"), "`we``ird`");
    }

    #[test]
    fn tls_verifies_remote_hosts_only() {
        let cfg = |host: &str| ConnectConfig { host: host.into(), ssl: true, ..Default::default() };
        assert!(opts(&cfg("localhost")).ssl_opts().unwrap().accept_invalid_certs());
        assert!(!opts(&cfg("db.example.com")).ssl_opts().unwrap().accept_invalid_certs());
        assert!(opts(&ConnectConfig::default()).ssl_opts().is_none());
    }
}
