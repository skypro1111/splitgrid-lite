//! The SQL Server driver for SplitGrid's SQL pane: connect, run statements
//! (streaming rows as sql_core::Event), cancel, and introspect the catalog.
//!
//! Built on tiberius 0.12 (TDS over tokio, TLS through rustls on ring).

mod introspect;
mod tap;
mod types;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::TryStreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use sql_core::{Canceller, Catalog, Column, ConnectConfig, Connection, Dialect, Event, TableRef, Value};
use tiberius::{AuthMethod, ColumnType, Config, EncryptionLevel, Query, QueryItem, ToSql};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};
use tracing::instrument::WithSubscriber as _;

use crate::introspect::Rows;
use crate::tap::{Tap, Tapped};

type Client = tiberius::Client<Compat<TcpStream>>;
type TdsError = tiberius::error::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a cancel waits for its `KILL` before giving up on it.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
const BATCH_ROWS: usize = 500;
/// A batch goes out early when rows trickle in, so a slow query still shows progress.
const BATCH_AGE: Duration = Duration::from_millis(100);

const CANCELLED: &str = "Query cancelled. SQL Server ended the session to stop it, so a new one was opened: \
     temporary tables, SET options and any open transaction (rolled back) are gone.";

/// Open a connection. Must be called inside a tokio runtime.
pub async fn connect(cfg: &ConnectConfig) -> anyhow::Result<Box<dyn Connection>> {
    let s = open_session(cfg, &cfg.database).await?;
    let cancel = Arc::new(Cancel {
        cfg: cfg.clone(),
        spid: AtomicU32::new(s.spid),
        running: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        abort: Notify::new(),
    });
    let tap = Arc::new(Tap::default());
    Ok(Box::new(MssqlConnection {
        cfg: cfg.clone(),
        client: Some(s.client),
        database: s.database,
        version: s.version,
        dispatch: tap.dispatch(),
        tap,
        cancel,
    }))
}

fn is_local(host: &str) -> bool {
    matches!(host, "" | "localhost" | "127.0.0.1" | "::1")
}

/// As in the Electron driver: `ssl` asks for full encryption with the
/// certificate verified, except on local hosts; without it only the login is
/// encrypted (SQL Server's default) and the certificate is not checked.
fn config(cfg: &ConnectConfig, database: &str) -> Config {
    let mut c = Config::new();
    c.host(if cfg.host.is_empty() { "localhost" } else { cfg.host.as_str() });
    c.port(if cfg.port == 0 { Dialect::Mssql.default_port() } else { cfg.port });
    c.authentication(AuthMethod::sql_server(&cfg.user, &cfg.password));
    if !database.is_empty() {
        c.database(database);
    }
    c.application_name("SplitGrid");
    c.encryption(if cfg.ssl { EncryptionLevel::Required } else { EncryptionLevel::Off });
    if !cfg.ssl || is_local(&cfg.host) {
        c.trust_cert();
    }
    c
}

async fn open(cfg: &ConnectConfig, database: &str) -> anyhow::Result<Client> {
    async fn dial(config: Config) -> Result<Client, TdsError> {
        let tcp = TcpStream::connect(config.get_addr()).await?;
        tcp.set_nodelay(true)?;
        Client::connect(config, tcp.compat_write()).await
    }
    let attempt = async {
        match dial(config(cfg, database)).await {
            // Azure SQL gateways redirect to the node holding the database.
            Err(TdsError::Routing { host, port }) => {
                let mut redirected = config(cfg, database);
                redirected.host(host);
                redirected.port(port);
                dial(redirected).await
            }
            other => other,
        }
    };
    Ok(tokio::time::timeout(CONNECT_TIMEOUT, attempt).await.context("timed out connecting to SQL Server")??)
}

struct Session {
    client: Client,
    spid: u32,
    version: String,
    database: String,
}

async fn open_session(cfg: &ConnectConfig, database: &str) -> anyhow::Result<Session> {
    let mut client = open(cfg, database).await?;
    let rows = text_rows(&mut client, "select @@spid, cast(serverproperty('ProductVersion') as nvarchar(128)), db_name()", &[]).await?;
    let row = rows.into_iter().next().unwrap_or_default();
    let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    Ok(Session { spid: cell(0).parse().unwrap_or(0), version: version_label(&cell(1)), database: cell(2), client })
}

/// "SQL Server 16.0.4135" from ProductVersion "16.0.4135.4".
fn version_label(product_version: &str) -> String {
    let parts: Vec<&str> = product_version.split('.').take(3).collect();
    format!("SQL Server {}", parts.join("."))
}

/// Runs a query and reads the first result set's cells as text.
async fn text_rows(client: &mut Client, sql: &str, params: &[&dyn ToSql]) -> Result<Rows, TdsError> {
    let stream = if params.is_empty() { client.simple_query(sql).await? } else { client.query(sql, params).await? };
    let rows = stream.into_first_result().await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let tys: Vec<ColumnType> = row.columns().iter().map(|c| c.column_type()).collect();
            row.into_iter().zip(tys).map(|(d, ty)| types::text(types::cell(ty, d))).collect()
        })
        .collect())
}

/// A dead connection, as opposed to an SQL error the session survives.
fn is_fatal(e: &TdsError) -> bool {
    matches!(e, TdsError::Io { .. } | TdsError::Protocol(_) | TdsError::Tls(_) | TdsError::Routing { .. })
}

/// Stops a running statement.
///
/// tiberius 0.12 cannot send a TDS attention (0.13 can, but only once the
/// result stream is dropped, and it pulls in the aws-lc crypto provider), so
/// the only server-side stop is `KILL <spid>` from a second connection. That
/// ends the whole session: `execute` then reports the cancel, drops the dead
/// connection and opens a fresh one, so the next statement just works, minus
/// temp tables, SET options and the open transaction (rolled back by the
/// server). KILL needs the ALTER ANY CONNECTION permission; without it the
/// cancel still abandons the statement locally (closing the socket, which the
/// server notices when it next writes) and reports that the KILL failed.
struct Cancel {
    cfg: ConnectConfig,
    spid: AtomicU32,
    running: AtomicBool,
    /// Set by a cancel, so `execute` reports its failure as a cancel.
    cancelled: AtomicBool,
    /// Wakes `execute` to drop the connection when KILL did not end it.
    abort: Notify,
}

/// The `Canceller` handed out; `Cancel` itself stays shared with the connection.
struct Stopper(Arc<Cancel>);

impl Canceller for Stopper {
    fn cancel(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let this = self.0.clone();
        Box::pin(async move {
            if !this.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            this.cancelled.store(true, Ordering::SeqCst);
            // KILL first, while our session still holds its spid: once the
            // socket is closed the spid can be reused by someone else's session.
            let spid = this.spid.load(Ordering::SeqCst);
            let killed = tokio::time::timeout(KILL_TIMEOUT, async {
                let mut killer = open(&this.cfg, "").await?;
                let res = match killer.simple_query(format!("KILL {spid}")).await {
                    Ok(stream) => stream.into_results().await.map(drop),
                    Err(e) => Err(e),
                };
                let _ = killer.close().await;
                match res {
                    // 6106: the session is already gone, which is the goal.
                    Err(e) if e.code() == Some(6106) => Ok(()),
                    other => other.map_err(anyhow::Error::from),
                }
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
            this.abort.notify_waiters();
            killed.context("stopped waiting for the statement, but could not KILL it on the server (it may still be running)")
        })
    }
}

struct MssqlConnection {
    cfg: ConnectConfig,
    /// None once the session died or was killed; reopened lazily.
    client: Option<Client>,
    /// Tracked through `USE`, so a reopened session lands in the same database.
    database: String,
    version: String,
    cancel: Arc<Cancel>,
    tap: Arc<Tap>,
    dispatch: tracing::Dispatch,
}

impl MssqlConnection {
    async fn client(&mut self) -> anyhow::Result<&mut Client> {
        if self.client.is_none() {
            let s = open_session(&self.cfg, &self.database).await?;
            self.cancel.spid.store(s.spid, Ordering::SeqCst);
            self.client = Some(s.client);
        }
        Ok(self.client.as_mut().expect("just filled"))
    }

    async fn rows(&mut self, sql: &str, params: &[&dyn ToSql]) -> anyhow::Result<Rows> {
        let result = text_rows(self.client().await?, sql, params).await;
        if let Err(e) = &result
            && is_fatal(e)
        {
            self.client = None;
        }
        Ok(result?)
    }
}

/// Turns the stream (plus what the tap saw) into events.
struct Results<'a> {
    tx: &'a UnboundedSender<Event>,
    /// Column types of the open result set.
    open: Option<Vec<ColumnType>>,
    rows: Vec<Vec<Value>>,
    since: Instant,
    emitted: bool,
    /// The database a `USE` switched to.
    database: Option<String>,
}

impl Results<'_> {
    /// Each returns false once the receiver is gone.
    fn send(&mut self, e: Event) -> bool {
        self.emitted = true;
        self.tx.unbounded_send(e).is_ok()
    }

    fn flush(&mut self) -> bool {
        self.since = Instant::now();
        self.rows.is_empty() || self.tx.unbounded_send(Event::Rows(std::mem::take(&mut self.rows))).is_ok()
    }

    fn close(&mut self) -> bool {
        if self.open.take().is_none() {
            return true;
        }
        self.flush() && self.send(Event::ResultEnd { affected: None })
    }

    fn drain(&mut self, tapped: Vec<Tapped>) -> bool {
        tapped.into_iter().all(|t| match t {
            Tapped::Message(m) => self.send(Event::Notice(m)),
            Tapped::Database(db) => {
                self.database = Some(db);
                true
            }
            // A DONE ends the open result set; otherwise it is a statement
            // without rows, worth an event when the server counted rows.
            Tapped::Done { rows } => match (self.open.is_some(), rows) {
                (false, Some(n)) => self.send(Event::ResultEnd { affected: Some(n) }),
                _ => self.close(),
            },
        })
    }

    fn start(&mut self, cols: &[tiberius::Column]) -> bool {
        if !self.close() {
            return false;
        }
        let columns = cols
            .iter()
            .map(|c| Column { name: c.name().to_owned(), type_name: types::type_name(c.column_type()).to_owned(), origin: None })
            .collect();
        self.open = Some(cols.iter().map(|c| c.column_type()).collect());
        self.send(Event::ResultStart(columns))
    }

    fn row(&mut self, row: tiberius::Row) -> bool {
        let tys = self.open.as_deref().unwrap_or_default();
        let cells = row.into_iter().enumerate().map(|(i, d)| types::cell(tys.get(i).copied().unwrap_or(ColumnType::Null), d)).collect();
        self.rows.push(cells);
        if self.rows.len() >= BATCH_ROWS || self.since.elapsed() >= BATCH_AGE { self.flush() } else { true }
    }

    fn finish(&mut self) {
        if self.close() && !self.emitted {
            self.send(Event::ResultEnd { affected: None });
        }
    }
}

/// Runs one statement. Without parameters it goes as a plain batch, so `USE`,
/// temp tables and SET options persist (sp_executesql would scope them).
async fn run(client: &mut Client, sql: &str, params: &[Value], out: &mut Results<'_>, tap: &Tap) -> Result<(), TdsError> {
    let stream = if params.is_empty() {
        client.simple_query(sql).await
    } else {
        let mut q = Query::new(sql);
        for p in params {
            types::bind(&mut q, p);
        }
        q.query(client).await
    };
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            out.drain(tap.take());
            return Err(e);
        }
    };
    loop {
        let item = stream.try_next().await;
        // Messages and DONEs seen while fetching this item came before it.
        if !out.drain(tap.take()) {
            return Ok(());
        }
        let alive = match item? {
            None => break,
            Some(QueryItem::Metadata(meta)) => out.start(meta.columns()),
            Some(QueryItem::Row(row)) => out.row(row),
        };
        if !alive {
            return Ok(());
        }
    }
    out.finish();
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

#[async_trait::async_trait]
impl Connection for MssqlConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Mssql
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    async fn execute(&mut self, sql: &str, params: &[Value], events: UnboundedSender<Event>) -> anyhow::Result<()> {
        if self.client.is_none() {
            self.client().await?;
            let _ = events.unbounded_send(Event::Notice(
                "Reconnected: the previous session was lost (temporary tables, SET options and any open transaction are gone).".into(),
            ));
        }
        let cancel = self.cancel.clone();
        let abort = cancel.abort.notified();
        tokio::pin!(abort);
        abort.as_mut().enable();
        cancel.cancelled.store(false, Ordering::SeqCst);
        cancel.running.store(true, Ordering::SeqCst);
        let running = Running(&cancel.running);

        let mut out = Results { tx: &events, open: None, rows: Vec::new(), since: Instant::now(), emitted: false, database: None };
        let client = self.client.as_mut().expect("connected above");
        let work = run(client, sql, params, &mut out, &self.tap).with_subscriber(self.dispatch.clone());
        let result = tokio::select! {
            r = work => r.map_err(anyhow::Error::from).map_err(|e| (e, false)),
            _ = &mut abort => Err((anyhow::anyhow!(CANCELLED), true)),
        };
        drop(running);
        if let Some(db) = out.database.take() {
            self.database = db;
        }
        self.tap.take();

        let Err((err, aborted)) = result else { return Ok(()) };
        if aborted || cancel.cancelled.load(Ordering::SeqCst) {
            self.client = None;
            // Reopen now so the next statement does not pay for it; if this
            // fails, the next call retries.
            let _ = self.client().await;
            return Err(anyhow::anyhow!(CANCELLED));
        }
        if err.downcast_ref::<TdsError>().is_some_and(is_fatal) {
            self.client = None;
        }
        Err(err)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(Stopper(self.cancel.clone()))
    }

    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let rows = self.rows("select name from sys.databases where database_id > 4 and has_dbaccess(name) = 1 order by name", &[]).await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
    }

    async fn introspect(&mut self) -> anyhow::Result<Catalog> {
        use introspect::*;
        let context = self.rows(CONTEXT, &[]).await?;
        let context = context.into_iter().next().unwrap_or_default();
        let cell = |i: usize| context.get(i).cloned().flatten().unwrap_or_default();
        let (schema, database) = (cell(0), cell(1));
        let fetched = Fetched {
            schemas: self.rows(SCHEMAS, &[]).await?,
            tables: self.rows(&filtered(TABLES, false), &[]).await?,
            columns: self.rows(&filtered(COLUMNS, false), &[]).await?,
            indexes: self.rows(&filtered(INDEXES, false), &[]).await?,
            foreign_keys: self.rows(&filtered(FOREIGN_KEYS, false), &[]).await?,
            routines: self.rows(ROUTINES, &[]).await?,
            sequences: self.rows(SEQUENCES, &[]).await?,
        };
        Ok(build(&database, if schema.is_empty() { "dbo" } else { &schema }, fetched))
    }

    /// Views (and routines) return their own source; tables are rebuilt from
    /// the catalog views, as SSMS's "Script as CREATE" does.
    async fn ddl(&mut self, table: &TableRef) -> anyhow::Result<String> {
        use introspect::*;
        let target = match &table.schema {
            Some(s) => format!("{}.{}", quote(s), quote(&table.name)),
            None => quote(&table.name),
        };
        let p: [&dyn ToSql; 1] = [&target];
        let object = self.rows(OBJECT, &p).await?;
        let row = object.into_iter().next().with_context(|| format!("no table or view {target}"))?;
        let cell = |i: usize| row.get(i).cloned().flatten();
        if cell(0).unwrap_or_default().trim() != "U" {
            return cell(1).with_context(|| format!("the definition of {target} is not readable (encrypted?)"));
        }
        let resolved = TableRef { schema: cell(2), name: table.name.clone() };
        let cols = columns(&self.rows(&filtered(COLUMNS, true), &p).await?);
        let ixs = indexes(&self.rows(&filtered(INDEXES, true), &p).await?);
        let fks = foreign_keys(&self.rows(&filtered(FOREIGN_KEYS, true), &p).await?);
        let checks = self.rows(CHECKS, &p).await?;
        Ok(create_table(&resolved, &cols, &ixs, &fks, &checks))
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        if let Some(client) = self.client {
            client.close().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_version_reads_like_the_product() {
        assert_eq!(version_label("16.0.4135.4"), "SQL Server 16.0.4135");
        assert_eq!(version_label("15.0.2000"), "SQL Server 15.0.2000");
    }

    fn collect(f: impl FnOnce(&mut Results<'_>)) -> Vec<Event> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut out = Results { tx: &tx, open: None, rows: Vec::new(), since: Instant::now(), emitted: false, database: None };
        f(&mut out);
        drop(tx);
        futures::executor::block_on(futures::StreamExt::collect(rx))
    }

    fn col(name: &str) -> tiberius::Column {
        tiberius::Column::new(name.into(), ColumnType::Intn)
    }

    #[test]
    fn dml_reports_affected_rows_and_prints_become_notices() {
        let events = collect(|out| {
            assert!(out.drain(vec![Tapped::Message("hi".into()), Tapped::Done { rows: Some(3) }]));
            out.finish();
        });
        assert_eq!(events, vec![Event::Notice("hi".into()), Event::ResultEnd { affected: Some(3) }]);
    }

    #[test]
    fn result_sets_end_at_their_done_or_the_next_metadata() {
        let events = collect(|out| {
            out.start(&[col("a")]);
            out.drain(vec![Tapped::Done { rows: Some(0) }]);
            out.start(&[col("b")]);
            // No DONE seen (e.g. trace events compiled out): the next set closes it.
            out.start(&[col("c")]);
            out.finish();
        });
        let starts = events.iter().filter(|e| matches!(e, Event::ResultStart(_))).count();
        let ends = events.iter().filter(|e| matches!(e, Event::ResultEnd { affected: None })).count();
        assert_eq!((starts, ends), (3, 3));
        assert!(matches!(events[1], Event::ResultEnd { affected: None }));
    }

    #[test]
    fn a_statement_with_nothing_to_say_still_ends() {
        let events = collect(|out| {
            out.drain(vec![Tapped::Done { rows: None }, Tapped::Database("app".into())]);
            assert_eq!(out.database.as_deref(), Some("app"));
            out.finish();
        });
        assert_eq!(events, vec![Event::ResultEnd { affected: None }]);
    }

    #[test]
    fn encryption_and_trust_follow_the_electron_driver() {
        let cfg = |host: &str, ssl| ConnectConfig { host: host.into(), ssl, ..Default::default() };
        // Config has no getters; its Debug shows the settings (the password as <HIDDEN>).
        let dbg = |c: &ConnectConfig| format!("{:?}", config(c, ""));
        assert!(dbg(&cfg("db.example.com", true)).contains("Required"));
        assert!(!dbg(&cfg("db.example.com", true)).contains("TrustAll"));
        assert!(dbg(&cfg("localhost", true)).contains("TrustAll"));
        assert!(dbg(&cfg("db.example.com", false)).contains("TrustAll"));
        assert!(dbg(&cfg("db.example.com", false)).contains("Off"));
    }
}
