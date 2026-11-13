//! The postgres driver for SplitGrid's SQL pane: connect, run statements (streaming
//! rows as sql_core::Event), cancel, and introspect the catalog.
//!
//! Statements run over the simple query protocol: every cell arrives as the
//! server's text, so no type can make a query fail to decode, and several
//! statements in one string yield several result sets. A Describe of the same
//! statement beforehand supplies the column types and source tables.

mod introspect;
mod tls;
mod values;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use sql_core::{
    Canceller, Catalog, Column, ConnectConfig, Connection, Dialect, Event, TableRef, Value,
};
use tokio_postgres::config::SslMode;
use tokio_postgres::{AsyncMessage, CancelToken, Client, NoTls, SimpleQueryMessage};

use tls::MakeRustls;
use values::{Decode, scan, type_name};

/// Rows per `Event::Rows`.
const BATCH: usize = 500;

/// Open a connection. Must be called inside a tokio runtime.
pub async fn connect(cfg: &ConnectConfig) -> anyhow::Result<Box<dyn Connection>> {
    let host = if cfg.host.trim().is_empty() {
        "localhost"
    } else {
        cfg.host.trim()
    };
    let mut pg = tokio_postgres::Config::new();
    pg.host(host)
        .port(if cfg.port == 0 {
            Dialect::Postgres.default_port()
        } else {
            cfg.port
        })
        .user(&cfg.user)
        .password(&cfg.password)
        .dbname(if cfg.database.is_empty() {
            "postgres"
        } else {
            &cfg.database
        })
        .application_name("SplitGrid")
        .connect_timeout(Duration::from_secs(15))
        .ssl_mode(if cfg.ssl {
            SslMode::Require
        } else {
            SslMode::Disable
        });

    let tls = if cfg.ssl {
        Tls::Rustls(MakeRustls::new(host)?)
    } else {
        Tls::None
    };
    let notices: NoticeSink = Arc::new(Mutex::new(None));
    let (client, version) = match &tls {
        Tls::None => spawn(pg.connect(NoTls).await.map_err(pg_err)?, notices.clone()),
        Tls::Rustls(make) => spawn(
            pg.connect(make.clone()).await.map_err(pg_err)?,
            notices.clone(),
        ),
    };

    // ISO dates whatever the server's default, so Date/Timestamp text is parseable.
    client
        .batch_execute("SET DateStyle = 'ISO, MDY'")
        .await
        .map_err(pg_err)?;
    let version = match version {
        Some(v) => v,
        None => client
            .query_one("show server_version", &[])
            .await
            .map_err(pg_err)?
            .get(0),
    };
    let version = version
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let major = version
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);

    Ok(Box::new(PgConnection {
        canceller: Arc::new(PgCanceller {
            token: client.cancel_token(),
            tls,
        }),
        client,
        version,
        major,
        notices,
        origins: HashMap::new(),
    }))
}

/// Where NOTICEs go: the events of the statement currently running, if any.
type NoticeSink = Arc<Mutex<Option<UnboundedSender<Event>>>>;

/// Drives the connection on its own task, forwarding NOTICEs. Returns the
/// client and the server_version the server reported at startup.
fn spawn<S, T>(
    conn: (Client, tokio_postgres::Connection<S, T>),
    notices: NoticeSink,
) -> (Client, Option<String>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (client, mut connection) = conn;
    let version = connection.parameter("server_version").map(str::to_owned);
    tokio::spawn(async move {
        let mut messages = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        // An error here means the connection is gone; the client's next call reports it.
        while let Some(Ok(msg)) = messages.next().await {
            if let AsyncMessage::Notice(n) = msg {
                let sink = notices.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(events) = sink.as_ref() {
                    let _ = events.unbounded_send(Event::Notice(format!(
                        "{}: {}",
                        n.severity(),
                        n.message()
                    )));
                }
            }
        }
    });
    (client, version)
}

/// The server's error as psql prints it (severity, message, detail, hint),
/// rather than tokio-postgres's "db error: …" wrapper.
pub(crate) fn pg_err(e: tokio_postgres::Error) -> anyhow::Error {
    let Some(db) = e.as_db_error() else {
        return anyhow::anyhow!(e);
    };
    let mut msg = format!("{}: {}", db.severity(), db.message());
    if let Some(d) = db.detail() {
        msg.push_str(&format!("\nDETAIL: {d}"));
    }
    if let Some(h) = db.hint() {
        msg.push_str(&format!("\nHINT: {h}"));
    }
    anyhow::anyhow!(msg)
}

#[derive(Clone)]
enum Tls {
    None,
    Rustls(MakeRustls),
}

struct PgCanceller {
    token: CancelToken,
    tls: Tls,
}

impl Canceller for PgCanceller {
    fn cancel(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let (token, tls) = (self.token.clone(), self.tls.clone());
        async move {
            match tls {
                Tls::None => token.cancel_query(NoTls).await,
                Tls::Rustls(make) => token.cancel_query(make).await,
            }
            .map_err(pg_err)
        }
        .boxed()
    }
}

/// A result column as Describe reported it.
struct Described {
    name: String,
    type_name: String,
    decode: Decode,
    origin: Option<TableRef>,
}

struct PgConnection {
    client: Client,
    canceller: Arc<PgCanceller>,
    version: String,
    major: u32,
    notices: NoticeSink,
    /// Table OID → name, for `Column::origin`.
    origins: HashMap<u32, TableRef>,
}

impl PgConnection {
    /// Column types and source tables of a single statement. A failed Describe
    /// isn't fatal: the statement runs anyway and its columns read as text.
    async fn describe(&mut self, sql: &str) -> Option<Vec<Described>> {
        let stmt = self.client.prepare(sql).await.ok()?;
        let missing: Vec<u32> = stmt
            .columns()
            .iter()
            .filter_map(|c| c.table_oid())
            .filter(|o| *o != 0 && !self.origins.contains_key(o))
            .collect();
        if !missing.is_empty()
            && let Ok(rows) = self
                .client
                .query(
                    "select c.oid, n.nspname::text, c.relname::text from pg_class c
                     join pg_namespace n on n.oid = c.relnamespace where c.oid = any($1)",
                    &[&missing],
                )
                .await
        {
            for r in rows {
                self.origins.insert(
                    r.get(0),
                    TableRef {
                        schema: Some(r.get(1)),
                        name: r.get(2),
                    },
                );
            }
        }
        Some(
            stmt.columns()
                .iter()
                .map(|c| Described {
                    name: c.name().to_owned(),
                    type_name: type_name(c.type_()),
                    decode: Decode::for_type(c.type_()),
                    origin: c.table_oid().and_then(|o| self.origins.get(&o).cloned()),
                })
                .collect(),
        )
    }

    async fn run(
        &mut self,
        sql: &str,
        params: &[Value],
        events: &UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        let scanned = scan(sql, params)?;
        // Describing a multi-statement string fails, and in a transaction that
        // failure would abort it; such strings just come back as text.
        let mut described = if scanned.multi {
            None
        } else {
            self.describe(sql).await
        };

        let mut stream = std::pin::pin!(
            self.client
                .simple_query_raw(&scanned.sql)
                .await
                .map_err(pg_err)?
        );
        let mut decoders: Vec<Decode> = Vec::new();
        let mut batch: Vec<Vec<Value>> = Vec::new();
        loop {
            // Rows already received go out before waiting on the network, so a
            // slow trickle still shows up without waiting for a full batch.
            let msg = match stream.next().now_or_never() {
                Some(msg) => msg,
                None => {
                    if !flush(&mut batch, events) {
                        return self.abandon().await;
                    }
                    stream.next().await
                }
            };
            match msg.transpose().map_err(pg_err)? {
                None => return Ok(()),
                Some(SimpleQueryMessage::RowDescription(cols)) => {
                    let names_match = |d: &[Described]| {
                        d.len() == cols.len()
                            && d.iter().zip(cols.iter()).all(|(d, c)| d.name == c.name())
                    };
                    let columns: Vec<(Column, Decode)> =
                        match described.take().filter(|d| names_match(d)) {
                            Some(d) => d
                                .into_iter()
                                .map(|d| {
                                    (
                                        Column {
                                            name: d.name,
                                            type_name: d.type_name,
                                            origin: d.origin,
                                        },
                                        d.decode,
                                    )
                                })
                                .collect(),
                            None => cols
                                .iter()
                                .map(|c| {
                                    (
                                        Column {
                                            name: c.name().to_owned(),
                                            type_name: "text".into(),
                                            origin: None,
                                        },
                                        Decode::Text,
                                    )
                                })
                                .collect(),
                        };
                    let (columns, decode): (Vec<Column>, Vec<Decode>) = columns.into_iter().unzip();
                    decoders = decode;
                    if events.unbounded_send(Event::ResultStart(columns)).is_err() {
                        return self.abandon().await;
                    }
                }
                Some(SimpleQueryMessage::Row(row)) => {
                    batch.push(
                        decoders
                            .iter()
                            .enumerate()
                            .map(|(i, d)| d.value(row.get(i)))
                            .collect(),
                    );
                    if batch.len() >= BATCH && !flush(&mut batch, events) {
                        return self.abandon().await;
                    }
                }
                Some(SimpleQueryMessage::CommandComplete(n)) => {
                    if !flush(&mut batch, events)
                        || events
                            .unbounded_send(Event::ResultEnd { affected: Some(n) })
                            .is_err()
                    {
                        return self.abandon().await;
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// Nobody is listening any more: stop the server's work. The stream is
    /// dropped by the caller and the connection discards what is still in flight.
    async fn abandon(&self) -> anyhow::Result<()> {
        let _ = self.canceller.cancel().await;
        Ok(())
    }
}

/// Sends the pending rows; false when the receiver is gone.
fn flush(batch: &mut Vec<Vec<Value>>, events: &UnboundedSender<Event>) -> bool {
    batch.is_empty()
        || events
            .unbounded_send(Event::Rows(std::mem::take(batch)))
            .is_ok()
}

#[async_trait::async_trait]
impl Connection for PgConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    fn server_version(&self) -> String {
        format!("PostgreSQL {}", self.version)
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        events: UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        *self.notices.lock().unwrap_or_else(|e| e.into_inner()) = Some(events.clone());
        let result = self.run(sql, params, &events).await;
        *self.notices.lock().unwrap_or_else(|e| e.into_inner()) = None;
        result
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.canceller.clone()
    }

    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "select datname::text from pg_database where not datistemplate order by datname",
                &[],
            )
            .await
            .map_err(pg_err)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn introspect(&mut self) -> anyhow::Result<Catalog> {
        introspect::catalog(&self.client, self.major).await
    }

    async fn ddl(&mut self, table: &TableRef) -> anyhow::Result<String> {
        introspect::ddl(&self.client, self.major, table).await
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        // Dropping the client sends Terminate and ends the connection task.
        Ok(())
    }
}

#[cfg(test)]
mod pg_tests;
