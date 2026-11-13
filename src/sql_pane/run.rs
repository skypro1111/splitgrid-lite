//! What runs next on a pane's connection: the queue of statements and the
//! transaction state (DataGrip's Auto / Manual commit).
//!
//! In Manual mode the first statement opens a transaction: the queue hands out
//! a BEGIN before it, so a script that commits half-way gets a new one for the
//! rest. Statements the user types (BEGIN, COMMIT, ROLLBACK) move the state too.
//! Jobs come in batches (one script, one Submit); a failure drops the rest of
//! its batch, and a batch that meant to commit rolls back instead.

use std::collections::VecDeque;

use sql_core::{Dialect, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxMode {
    #[default]
    Auto,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxVerb {
    Begin,
    /// COMMIT or ROLLBACK (not to a savepoint).
    End,
}

/// The words of a statement, uppercased, past leading comments.
fn words(sql: &str, n: usize) -> Vec<String> {
    let mut rest = sql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| c.is_whitespace() || c == ';').filter(|w| !w.is_empty()).take(n).map(str::to_uppercase).collect()
}

/// Whether a statement starts or ends a transaction.
pub fn tx_verb(sql: &str) -> Option<TxVerb> {
    let w = words(sql, 3);
    let second = w.get(1).map(String::as_str);
    match w.first().map(String::as_str)? {
        // Not MSSQL's BEGIN TRY / BEGIN … END blocks, nor PL/pgSQL.
        "BEGIN" => matches!(second, None | Some("TRAN" | "TRANSACTION" | "WORK" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE" | "ISOLATION")).then_some(TxVerb::Begin),
        "START" => (second == Some("TRANSACTION")).then_some(TxVerb::Begin),
        "COMMIT" => Some(TxVerb::End),
        "END" => matches!(second, None | Some("TRANSACTION" | "WORK")).then_some(TxVerb::End),
        "ROLLBACK" => {
            let to = |w: Option<&String>| w.is_some_and(|w| w == "TO");
            (!to(w.get(1)) && !to(w.get(2))).then_some(TxVerb::End)
        }
        _ => None,
    }
}

pub fn begin_sql(d: Dialect) -> &'static str {
    match d {
        Dialect::Postgres | Dialect::Sqlite => "BEGIN",
        Dialect::Mysql => "START TRANSACTION",
        Dialect::Mssql => "BEGIN TRANSACTION",
    }
}

pub const COMMIT: &str = "COMMIT";
pub const ROLLBACK: &str = "ROLLBACK";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tx {
    pub mode: TxMode,
    pub open: bool,
}

impl Tx {
    /// A statement that has to wait for a BEGIN (Manual mode, nothing open yet).
    pub fn needs_begin(&self, sql: &str) -> bool {
        self.mode == TxMode::Manual && !self.open && tx_verb(sql).is_none()
    }

    /// A statement succeeded.
    pub fn applied<P>(&mut self, kind: &Kind<P>, sql: &str) {
        match kind {
            Kind::Begin => self.open = true,
            Kind::Commit | Kind::Rollback => self.open = false,
            Kind::Run(_) => match tx_verb(sql) {
                Some(TxVerb::Begin) => self.open = true,
                Some(TxVerb::End) => self.open = false,
                None => {}
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind<P> {
    Begin,
    Commit,
    Rollback,
    Run(P),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job<P> {
    pub batch: u64,
    pub sql: String,
    pub params: Vec<Value>,
    pub kind: Kind<P>,
}

#[derive(Debug)]
pub struct Queue<P> {
    jobs: VecDeque<Job<P>>,
    batches: u64,
}

impl<P> Default for Queue<P> {
    fn default() -> Self {
        Self { jobs: VecDeque::new(), batches: 0 }
    }
}

impl<P> Queue<P> {
    pub fn new_batch(&mut self) -> u64 {
        self.batches += 1;
        self.batches
    }

    pub fn push(&mut self, batch: u64, sql: impl Into<String>, params: Vec<Value>, kind: Kind<P>) {
        self.jobs.push_back(Job { batch, sql: sql.into(), params, kind });
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// Whether jobs of `batch` are still waiting.
    pub fn has_batch(&self, batch: u64) -> bool {
        self.jobs.iter().any(|j| j.batch == batch)
    }

    /// The next job, or the BEGIN it has to wait for.
    pub fn next(&mut self, tx: &Tx, dialect: Dialect) -> Option<Job<P>> {
        let front = self.jobs.front()?;
        if matches!(front.kind, Kind::Run(_)) && tx.needs_begin(&front.sql) {
            return Some(Job { batch: front.batch, sql: begin_sql(dialect).into(), params: Vec::new(), kind: Kind::Begin });
        }
        self.jobs.pop_front()
    }

    /// A job of `batch` failed: drop the rest of the batch. True when the batch
    /// was going to commit (it opened its own transaction), so it should roll back.
    pub fn fail(&mut self, batch: u64) -> bool {
        let mut committed = false;
        self.jobs.retain(|j| {
            let ours = j.batch == batch;
            committed |= ours && matches!(j.kind, Kind::Commit);
            !ours
        });
        committed
    }

    pub fn clear(&mut self) {
        self.jobs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_transaction_statements() {
        for s in ["BEGIN", "begin;", "BEGIN TRANSACTION", "begin tran", "START TRANSACTION READ ONLY", "-- go\nBEGIN", "/* x */ begin work"] {
            assert_eq!(tx_verb(s), Some(TxVerb::Begin), "{s}");
        }
        for s in ["COMMIT", "commit tran", "ROLLBACK", "rollback transaction", "END", "end transaction"] {
            assert_eq!(tx_verb(s), Some(TxVerb::End), "{s}");
        }
        for s in ["BEGIN TRY SELECT 1 END TRY", "ROLLBACK TO SAVEPOINT a", "ROLLBACK TRANSACTION TO x", "SELECT 1", "", "-- only a comment", "END LOOP"] {
            assert_eq!(tx_verb(s), None, "{s}");
        }
    }

    #[test]
    fn manual_mode_opens_a_transaction_before_the_first_statement() {
        let mut tx = Tx { mode: TxMode::Manual, open: false };
        let mut q: Queue<&str> = Queue::default();
        let b = q.new_batch();
        q.push(b, "UPDATE t SET a = 1", vec![], Kind::Run("u"));
        q.push(b, "COMMIT", vec![], Kind::Run("c"));
        q.push(b, "DELETE FROM t WHERE a = 2", vec![], Kind::Run("d"));

        let mut ran = Vec::new();
        while let Some(job) = q.next(&tx, Dialect::Mysql) {
            tx.applied(&job.kind, &job.sql);
            ran.push(job.sql);
        }
        assert_eq!(ran, ["START TRANSACTION", "UPDATE t SET a = 1", "COMMIT", "START TRANSACTION", "DELETE FROM t WHERE a = 2"]);
        assert!(tx.open, "the last statement left a transaction open");

        // Auto mode never adds a BEGIN, but tracks one the user types.
        let mut auto = Tx::default();
        let b = q.new_batch();
        q.push(b, "SELECT 1", vec![], Kind::Run("s"));
        assert_eq!(q.next(&auto, Dialect::Postgres).unwrap().sql, "SELECT 1");
        auto.applied(&Kind::Run(()), "begin");
        assert!(auto.open);
        auto.applied(&Kind::<()>::Rollback, "ROLLBACK");
        assert!(!auto.open);
    }

    #[test]
    fn a_failure_drops_the_rest_of_its_batch_and_asks_for_a_rollback() {
        let mut q: Queue<u8> = Queue::default();
        let edits = q.new_batch();
        q.push(edits, "BEGIN", vec![], Kind::Begin);
        q.push(edits, "UPDATE a", vec![], Kind::Run(1));
        q.push(edits, "UPDATE b", vec![], Kind::Run(2));
        q.push(edits, "COMMIT", vec![], Kind::Commit);
        let later = q.new_batch();
        q.push(later, "SELECT 1", vec![], Kind::Run(3));

        let tx = Tx::default();
        assert_eq!(q.next(&tx, Dialect::Sqlite).unwrap().kind, Kind::Begin);
        assert_eq!(q.next(&tx, Dialect::Sqlite).unwrap().sql, "UPDATE a");
        assert!(q.fail(edits), "the batch was going to commit");
        assert_eq!(q.len(), 1);
        assert_eq!(q.next(&tx, Dialect::Sqlite).unwrap().batch, later);

        // A script without its own commit doesn't roll back.
        let script = q.new_batch();
        q.push(script, "SELECT 2", vec![], Kind::Run(4));
        assert!(!q.fail(script));
        assert!(q.is_empty());
    }
}
