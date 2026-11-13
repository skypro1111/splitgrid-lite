//! SQL text intelligence for the SQL pane, without a server: splitting scripts
//! into statements, the statement under the caret, schema-aware completion,
//! parameters, dangerous-statement checks, and the SQL the data editor
//! generates (quoting, literals, staged row edits, table browsing).
//!
//! Pure functions over text and a `sql_core::Catalog`; every dialect's
//! lexical rules (quotes, comments, dollar quotes, `GO`, `DELIMITER`) live here.
//!
//! API CONTRACT: the SQL pane (src/sql_pane) is written against these
//! signatures. Additions are fine; changing a signature means changing its callers.

mod check;
mod complete;
mod edit;
mod ident;
mod keywords;
mod lexer;
mod matcher;
mod params;
mod scope;
mod split;

use std::collections::HashMap;
use std::ops::Range;

pub use sql_core::{Catalog, Dialect, TableRef, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    /// CREATE / ALTER / DROP / TRUNCATE / COMMENT …
    Ddl,
    /// BEGIN / COMMIT / ROLLBACK / SAVEPOINT / START TRANSACTION
    Transaction,
    /// EXPLAIN, SHOW, SET, USE, PRAGMA, CALL/EXEC, WITH … that isn't a plain select, …
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    /// Byte range in the script, trimmed of surrounding whitespace and the terminator.
    pub range: Range<usize>,
    pub text: String,
    pub kind: StatementKind,
}

/// The statements of a script, in order. Empty statements and pure comments are dropped.
///
/// `;` inside strings, comments, dollar quotes and routine `BEGIN … END` bodies
/// doesn't split; MySQL `DELIMITER` lines and MSSQL `GO` lines do. An MSSQL batch
/// that declares variables or defines a routine stays one statement up to `GO`,
/// since its parts can't run separately.
pub fn split(sql: &str, dialect: Dialect) -> Vec<Statement> {
    split::split(sql, dialect)
}

/// The statement the caret is in, or the nearest one before it on the same
/// or previous non-blank line (DataGrip's "current statement").
pub fn statement_at(sql: &str, offset: usize, dialect: Dialect) -> Option<Statement> {
    split::statement_at(sql, offset, dialect)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
    Function,
    Alias,
    /// A whole join condition from a foreign key: `orders o ON o.user_id = u.id`.
    Join,
    Snippet,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// What the list shows.
    pub label: String,
    pub kind: CompletionKind,
    /// What replaces `CompletionResult::replace` (quoted where needed).
    pub insert: String,
    /// Shown dim on the right: a column's type, a table's schema, a function's signature.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompletionResult {
    /// The byte range the chosen item replaces (the partial word at the caret).
    pub replace: Range<usize>,
    /// Best first.
    pub items: Vec<Completion>,
}

/// Completion at `offset` in `sql` (the whole editor text; the statement is found inside it).
///
/// The whole statement is analysed, so tables after the caret count. Items are
/// ranked by kind relevance for the position, then match quality (prefix,
/// camel/snake humps, substring last), then name.
pub fn complete(sql: &str, offset: usize, dialect: Dialect, catalog: &Catalog) -> CompletionResult {
    complete::complete(sql, offset, dialect, catalog)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    /// As written: `:name`, `@name` (not variables the statement declares or
    /// assigns), `$1`; each bare `?` is numbered `?1`, `?2`… since each takes its own value.
    pub name: String,
    pub range: Range<usize>,
}

/// The parameters a statement needs values for, each name once, in order of first use.
pub fn params(stmt: &str, dialect: Dialect) -> Vec<Param> {
    params::params(stmt, dialect)
}

/// Rewrite named parameters into the driver's native placeholders ($1 / ? / @P1)
/// and order the values to match. `values` is keyed by `Param::name`; a missing
/// value binds NULL.
pub fn bind(stmt: &str, dialect: Dialect, values: &HashMap<String, Value>) -> (String, Vec<Value>) {
    params::bind(stmt, dialect, values)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Danger {
    UpdateWithoutWhere,
    DeleteWithoutWhere,
    Drop,
    Truncate,
}

/// What to confirm before running a statement.
pub fn dangers(stmt: &str, dialect: Dialect) -> Vec<Danger> {
    check::dangers(stmt, dialect)
}

/// Where a result's rows come from, when they can be edited in place.
#[derive(Clone, Debug, PartialEq)]
pub struct EditSource {
    pub table: TableRef,
    /// The table's primary key (or a unique not-null index).
    pub key_columns: Vec<String>,
    /// For each result column, the table column it shows (None = computed / not editable).
    pub column_map: Vec<Option<String>>,
}

/// A result is editable when it is a plain single-table SELECT (no joins,
/// grouping, DISTINCT or set operations) whose columns include the table's key.
pub fn editable_source(
    stmt: &str,
    dialect: Dialect,
    catalog: &Catalog,
    result_columns: &[String],
) -> Option<EditSource> {
    check::editable_source(stmt, dialect, catalog, result_columns)
}

pub fn quote_ident(name: &str, dialect: Dialect) -> String {
    ident::quote_ident(name, dialect)
}

/// `schema.table`, each part quoted only when needed.
pub fn qualified(table: &TableRef, dialect: Dialect) -> String {
    ident::qualified(table, dialect)
}

/// A value as a SQL literal in the dialect.
pub fn literal(value: &Value, dialect: Dialect) -> String {
    ident::literal(value, dialect)
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowEdit {
    Update {
        key: Vec<(String, Value)>,
        set: Vec<(String, Value)>,
    },
    Insert {
        values: Vec<(String, Value)>,
    },
    Delete {
        key: Vec<(String, Value)>,
    },
}

/// The statements that apply staged edits, one per edit, for preview and to run in a transaction.
/// Values are inlined as literals. An UPDATE/DELETE without key columns (it would
/// hit every row) or an UPDATE with nothing to set produces no statement.
pub fn build_edits(table: &TableRef, edits: &[RowEdit], dialect: Dialect) -> Vec<String> {
    edit::build_edits(table, edits, dialect)
}

/// The query behind a table's data view, with the user's WHERE / ORDER BY
/// text (either may be empty) and a row limit in the dialect's syntax.
pub fn browse_query(
    table: &TableRef,
    filter: &str,
    order_by: &str,
    limit: usize,
    dialect: Dialect,
) -> String {
    edit::browse_query(table, filter, order_by, limit, dialect)
}

/// EXPLAIN for a statement (`EXPLAIN (FORMAT JSON) …`, `EXPLAIN FORMAT=JSON …`,
/// `EXPLAIN QUERY PLAN …`); None where the pane can't show a plan (MSSQL for now).
pub fn explain_query(stmt: &str, dialect: Dialect, analyze: bool) -> Option<String> {
    edit::explain_query(stmt, dialect, analyze)
}
