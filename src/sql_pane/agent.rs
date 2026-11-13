//! What the `sg-sql` agent bridge (`agent::sql_cmd`) drives a SQL pane through:
//! the Electron app's SqlWorkbenchHandle. An agent's statements go through the
//! pane's own queue, so they surface as result tabs (and in the Output log)
//! like the user's, and the bridge is told the last statement's result once
//! its batch is done.

use std::sync::Arc;

use futures::channel::{mpsc, oneshot};
use gpui_kit::*;
use serde_json::{Value as Json, json};
use sql_core::{Catalog, Dialect, Event, TableRef, Value};

use super::run::Kind;
use super::store::{SavedSql, dialect_name};
use super::tree::ConnStatus;
use super::{Modal, Phase, Purpose, ResultTab, SqlPane, store};

/// Rows of a result the agent gets back (the rest are counted).
pub(crate) const AGENT_ROWS: usize = 100;

/// One statement's outcome, as the agent sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentResult {
    pub sql: String,
    /// Empty when the statement returned no result set.
    pub columns: Vec<String>,
    /// The first [`AGENT_ROWS`] rows.
    pub rows: Vec<Vec<Value>>,
    /// Rows fetched (all of them, up to the pane's cap).
    pub fetched: usize,
    pub affected: Option<u64>,
    /// The pane's row cap cut the result.
    pub capped: bool,
    pub ms: u128,
}

pub(crate) type AgentOutcome = Result<AgentResult, String>;

/// A batch the agent waits on.
pub(super) struct AgentWait {
    tx: oneshot::Sender<AgentOutcome>,
    expected: usize,
    done: usize,
    last: Option<AgentResult>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AgentPhase {
    Idle,
    Connecting,
    Ready,
    /// The saved password can't be opened here: the user has to type it in the pane.
    NeedsPassword,
    Failed(String),
}

/// A saved connection by id, exact label, then label substring (Electron's findSaved).
pub(crate) fn find_saved<'a>(conns: &'a [SavedSql], query: &str) -> Option<&'a SavedSql> {
    let q = query.trim().to_lowercase();
    conns
        .iter()
        .find(|c| c.id == query)
        .or_else(|| conns.iter().find(|c| c.label.to_lowercase() == q))
        .or_else(|| conns.iter().find(|c| !q.is_empty() && c.label.to_lowercase().contains(&q)))
}

fn summary(c: &SavedSql) -> Json {
    json!({ "id": c.id, "label": c.label, "dialect": c.dialect, "host": c.host, "port": c.port, "database": c.database })
}

/// The saved SQL connections (no secrets), fresh from disk.
pub(crate) fn saved_connections(cx: &mut App) -> Vec<Json> {
    let store = store(cx);
    store.borrow_mut().sync_from_disk();
    store.borrow().connections.iter().map(summary).collect()
}

/// Whether the dialect has schemas the user picks between (Electron's supportsSchemas).
pub(crate) fn supports_schemas(d: Dialect) -> bool {
    matches!(d, Dialect::Postgres | Dialect::Mssql)
}

impl SqlPane {
    /// What a pane shows, for `list` / `use` (Electron's SqlPanelInfo).
    pub(crate) fn agent_info(&self) -> Json {
        let d = self.dialect();
        let schema = supports_schemas(d).then(|| self.catalog.as_ref().and_then(|c| c.search_path.first().cloned())).flatten();
        json!({
            "connected": self.session.is_some(),
            "connectionName": self.conn.as_ref().map(|c| c.label.clone()),
            "savedConnectionId": self.conn.as_ref().map(|c| c.id.clone()),
            "dialect": self.conn.as_ref().map(|c| c.dialect.clone()).unwrap_or_else(|| dialect_name(d).into()),
            "database": self.database,
            "schema": schema,
            "supportsSchemas": supports_schemas(d),
        })
    }

    pub(crate) fn agent_phase(&self) -> AgentPhase {
        if let (Some(Modal::Password { id, .. }), Some(c)) = (&self.modal, &self.conn)
            && *id == c.id
        {
            return AgentPhase::NeedsPassword;
        }
        match &self.phase {
            Phase::Idle => AgentPhase::Idle,
            Phase::Connecting => AgentPhase::Connecting,
            Phase::Ready => AgentPhase::Ready,
            Phase::Failed(e) => AgentPhase::Failed(e.clone()),
        }
    }

    pub(crate) fn agent_dialect(&self) -> Dialect {
        self.dialect()
    }

    /// The active database's catalog, once introspected.
    pub(crate) fn agent_catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.clone()
    }

    /// Switch the pane to a saved connection (and database), as the explorer does.
    /// Connecting goes on in the background: watch [`Self::agent_phase`].
    pub(crate) fn agent_use(&mut self, query: &str, database: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.sync_connections(cx);
        let conns = store(cx).borrow().connections.clone();
        let saved = find_saved(&conns, query).cloned().ok_or_else(|| format!("no saved connection matches \"{query}\" (run: connections)"))?;
        let same = self.conn.as_ref().is_some_and(|c| c.id == saved.id) && database.as_ref().is_none_or(|d| self.database.as_ref() == Some(d));
        if !same && self.tx.open && self.session.is_some() {
            return Err("the pane has an open transaction; ask the user to commit or roll it back first".into());
        }
        // A failed attempt is retried.
        if let Some(e) = self.forest.conn_mut(&saved.id)
            && matches!(e.status, ConnStatus::Failed(_))
        {
            e.status = ConnStatus::Idle;
        }
        self.activate(&saved.id, database, window, cx);
        Ok(())
    }

    /// The CREATE statement of a table, from the active session.
    pub(crate) fn agent_ddl(&self, table: TableRef) -> Option<impl Future<Output = anyhow::Result<String>> + use<>> {
        Some(self.session.as_ref()?.ddl(table))
    }

    /// Run one statement on the active session outside the pane's queue (an export's read).
    #[allow(clippy::type_complexity)]
    pub(crate) fn agent_stream(&self, sql: String) -> Option<(mpsc::UnboundedReceiver<Event>, impl Future<Output = anyhow::Result<()>> + use<>)> {
        Some(self.session.as_ref()?.execute(sql, Vec::new()))
    }

    /// Queue the agent's statements as one batch, as if run from the console
    /// (`explain`: already wrapped in EXPLAIN; they open a plan tab). The
    /// receiver gets the last statement's result, or the first error.
    pub(crate) fn agent_run(&mut self, stmts: Vec<String>, explain: bool, cx: &mut Context<Self>) -> Result<oneshot::Receiver<AgentOutcome>, String> {
        if self.session.is_none() {
            return Err("the pane isn't connected (run: use <connection>)".into());
        }
        let batch = self.queue.new_batch();
        let expected = stmts.len();
        for sql in stmts {
            let purpose = if explain { Purpose::Explain } else { Purpose::Console { range: None } };
            self.queue.push(batch, sql, Vec::new(), Kind::Run(purpose));
        }
        Ok(self.agent_wait(batch, expected, cx))
    }

    /// Queue parameterised INSERTs in a transaction of their own (Auto commit),
    /// or in the open one (Manual), like submitting edits. True: committed here.
    pub(crate) fn agent_insert(&mut self, stmts: Vec<(String, Vec<Value>)>, cx: &mut Context<Self>) -> Result<(oneshot::Receiver<AgentOutcome>, bool), String> {
        if self.session.is_none() {
            return Err("the pane isn't connected (run: use <connection>)".into());
        }
        let batch = self.queue.new_batch();
        let own = self.tx.mode == super::run::TxMode::Auto && !self.tx.open;
        if own {
            self.queue.push(batch, super::run::begin_sql(self.dialect()), Vec::new(), Kind::Begin);
        }
        let expected = stmts.len();
        for (sql, params) in stmts {
            self.queue.push(batch, sql, params, Kind::Run(Purpose::Console { range: None }));
        }
        if own {
            self.queue.push(batch, super::run::COMMIT, Vec::new(), Kind::Commit);
        }
        Ok((self.agent_wait(batch, expected, cx), own))
    }

    fn agent_wait(&mut self, batch: u64, expected: usize, cx: &mut Context<Self>) -> oneshot::Receiver<AgentOutcome> {
        let (tx, rx) = oneshot::channel();
        self.agent_waits.insert(batch, AgentWait { tx, expected, done: 0, last: None, error: None });
        self.pump(cx);
        rx
    }

    /// Re-introspect the active database (after the agent changed its schema);
    /// the old catalog stays until the new one is in.
    pub(crate) fn agent_refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if let Some((id, db)) = self.active_pair() {
            self.load_catalog(&id, &db, cx);
        }
    }

    /// Reload a table's open data view (after an import).
    pub(crate) fn agent_reload_table(&mut self, table: &TableRef, cx: &mut Context<Self>) {
        let same = |t: &TableRef| t.name == table.name && (t.schema.is_none() || table.schema.is_none() || t.schema == table.schema);
        let ids: Vec<u64> = self.tabs.iter().filter(|t| t.grid().and_then(|g| g.view.as_ref()).is_some_and(|v| same(&v.table))).map(|t| t.id).collect();
        for id in ids {
            self.browse(id, cx);
        }
    }

    /// A job finished (`finish`): note what the agent waiting on its batch gets.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn agent_record(&mut self, batch: u64, kind: &Kind<Purpose>, sql: &str, tab: Option<u64>, fetched: usize, affected: Option<u64>, capped: bool, ms: u128, error: Option<String>, cx: &App) {
        if !self.agent_waits.contains_key(&batch) {
            return;
        }
        let result = (error.is_none() && matches!(kind, Kind::Run(_))).then(|| {
            let mut r = AgentResult { sql: sql.to_owned(), fetched, affected, capped, ms, ..Default::default() };
            if let Some(grid) = tab.and_then(|id| self.tabs.iter().find(|t| t.id == id)).and_then(ResultTab::grid).and_then(|g| g.grid.clone()) {
                let g = grid.read(cx);
                r.columns = g.columns().iter().map(|c| c.name.clone()).collect();
                r.rows = (0..g.row_count().min(AGENT_ROWS)).map(|i| (0..r.columns.len()).map(|j| g.cell(i, j).cloned().unwrap_or(Value::Null)).collect()).collect();
            }
            r
        });
        let Some(w) = self.agent_waits.get_mut(&batch) else { return };
        match (error, result) {
            (Some(e), _) => w.error = w.error.take().or(Some(e)),
            (None, Some(r)) => {
                w.done += 1;
                w.last = Some(r);
            }
            _ => {}
        }
    }

    /// Answer the agents whose batches are over (done, failed or dropped).
    pub(super) fn agent_settle(&mut self) {
        if self.agent_waits.is_empty() {
            return;
        }
        let running = self.running.as_ref().map(|r| r.job.batch);
        let over: Vec<u64> = self.agent_waits.keys().copied().filter(|b| Some(*b) != running && !self.queue.has_batch(*b)).collect();
        for b in over {
            let Some(w) = self.agent_waits.remove(&b) else { continue };
            let outcome = match (w.error, w.last) {
                (Some(e), _) => Err(e),
                (None, Some(r)) if w.done >= w.expected => Ok(r),
                _ => Err(format!("the run was stopped after {} of {} statements (cancelled, or the connection changed)", w.done, w.expected)),
            };
            let _ = w.tx.send(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_saved;
    use crate::sql_pane::store::SavedSql;

    #[test]
    fn saved_connections_match_by_id_label_then_substring() {
        let c = |id: &str, label: &str| SavedSql { id: id.into(), label: label.into(), ..Default::default() };
        let list = vec![c("a1", "Prod PG"), c("b2", "prod-pg-replica"), c("c3", "Local")];
        assert_eq!(find_saved(&list, "b2").unwrap().id, "b2");
        assert_eq!(find_saved(&list, "prod pg").unwrap().id, "a1");
        assert_eq!(find_saved(&list, "replica").unwrap().id, "b2");
        assert_eq!(find_saved(&list, "loc").unwrap().id, "c3");
        assert!(find_saved(&list, "nope").is_none());
        assert!(find_saved(&list, " ").is_none());
    }
}
