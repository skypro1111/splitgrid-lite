//! A live database connection, owned by one task on the tokio runtime.
//!
//! `Connection` runs one statement at a time and needs `&mut`, so the task
//! takes jobs from a queue in order; the pane talks to it through `Session`
//! and awaits the answers on the gpui side. Cancelling goes around the queue
//! (the driver's `Canceller`). Dropping the `Session` closes the connection.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use sql_core::{Canceller, Catalog, ConnectConfig, Connection, Dialect, Event, TableRef, Value};

enum Job {
    Execute { sql: String, params: Vec<Value>, events: mpsc::UnboundedSender<Event>, done: oneshot::Sender<Result<()>> },
    Introspect(oneshot::Sender<Result<Catalog>>),
    Ddl(TableRef, oneshot::Sender<Result<String>>),
    Databases(oneshot::Sender<Result<Vec<String>>>),
}

pub struct Session {
    jobs: mpsc::UnboundedSender<Job>,
    canceller: Arc<dyn Canceller>,
    pub dialect: Dialect,
    pub server_version: String,
}

async fn open(cfg: &ConnectConfig) -> Result<Box<dyn Connection>> {
    match cfg.dialect {
        Some(Dialect::Postgres) => sql_postgres::connect(cfg).await,
        Some(Dialect::Mysql) => sql_mysql::connect(cfg).await,
        Some(Dialect::Sqlite) => sql_sqlite::connect(cfg).await,
        Some(Dialect::Mssql) => sql_mssql::connect(cfg).await,
        None => Err(anyhow!("unknown database type")),
    }
}

/// Run a future on the runtime the drivers live on, and await it from anywhere.
async fn on_runtime<T: Send + 'static>(fut: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    crate::ssh::runtime().spawn(fut).await.map_err(|e| anyhow!("database task failed: {e}"))?
}

impl Session {
    pub async fn connect(cfg: ConnectConfig) -> Result<Session> {
        let (jobs, mut rx) = mpsc::unbounded::<Job>();
        let (ready_tx, ready_rx) = oneshot::channel();
        crate::ssh::runtime().spawn(async move {
            let mut conn = match open(&cfg).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok((conn.canceller(), conn.dialect(), conn.server_version())));
            while let Some(job) = rx.next().await {
                match job {
                    Job::Execute { sql, params, events, done } => {
                        let _ = done.send(conn.execute(&sql, &params, events).await);
                    }
                    Job::Introspect(done) => {
                        let _ = done.send(conn.introspect().await);
                    }
                    Job::Ddl(table, done) => {
                        let _ = done.send(conn.ddl(&table).await);
                    }
                    Job::Databases(done) => {
                        let _ = done.send(conn.databases().await);
                    }
                }
            }
            if let Err(e) = conn.close().await {
                log::warn!("closing a database connection: {e:#}");
            }
        });
        let (canceller, dialect, server_version) = ready_rx.await.map_err(|_| anyhow!("the connection task stopped"))??;
        Ok(Session { jobs, canceller, dialect, server_version })
    }

    /// Check that a configuration connects; the server's version on success.
    pub async fn test(cfg: ConnectConfig) -> Result<String> {
        on_runtime(async move {
            let conn = open(&cfg).await?;
            let version = conn.server_version();
            let _ = conn.close().await;
            Ok(version)
        })
        .await
    }

    fn send<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Job) -> oneshot::Receiver<Result<T>> {
        let (tx, rx) = oneshot::channel();
        // A closed queue drops `tx`, which the receiver reads as "connection closed".
        let _ = self.jobs.unbounded_send(make(tx));
        rx
    }

    /// Run one statement; its results stream into the returned receiver, and
    /// the second future resolves when it is done. Dropping the receiver stops the fetch.
    pub fn execute(&self, sql: String, params: Vec<Value>) -> (mpsc::UnboundedReceiver<Event>, impl Future<Output = Result<()>> + use<>) {
        let (events, rx) = mpsc::unbounded();
        let done = self.send(|done| Job::Execute { sql, params, events, done });
        (rx, flatten(done))
    }

    pub fn introspect(&self) -> impl Future<Output = Result<Catalog>> + use<> {
        flatten(self.send(Job::Introspect))
    }

    pub fn ddl(&self, table: TableRef) -> impl Future<Output = Result<String>> + use<> {
        flatten(self.send(|d| Job::Ddl(table, d)))
    }

    pub fn databases(&self) -> impl Future<Output = Result<Vec<String>>> + use<> {
        flatten(self.send(Job::Databases))
    }

    pub fn cancel(&self) {
        let fut = self.canceller.cancel();
        crate::ssh::runtime().spawn(async move {
            if let Err(e) = fut.await {
                log::warn!("cancelling a statement: {e:#}");
            }
        });
    }
}

async fn flatten<T>(rx: oneshot::Receiver<Result<T>>) -> Result<T> {
    rx.await.map_err(|_| anyhow!("the connection is closed"))?
}
