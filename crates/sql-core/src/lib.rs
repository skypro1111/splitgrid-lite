//! What every SQL driver and the SQL pane share: the dialects, connection
//! settings, values, the streamed result events, the driver trait, and the
//! catalog model (schemas, tables, columns, keys) that completion and the
//! schema tree are built from.
//!
//! The drivers live in their own crates (sql-postgres, sql-mysql, sql-sqlite,
//! sql-mssql); each exposes `pub async fn connect(&ConnectConfig) ->
//! anyhow::Result<Box<dyn Connection>>`, called inside a tokio runtime.

use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
}

impl Dialect {
    pub const ALL: [Dialect; 4] = [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite, Dialect::Mssql];

    pub fn label(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::Mysql => "MySQL",
            Dialect::Sqlite => "SQLite",
            Dialect::Mssql => "SQL Server",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Dialect::Postgres => 5432,
            Dialect::Mysql => 3306,
            Dialect::Sqlite => 0,
            Dialect::Mssql => 1433,
        }
    }

    /// File-based: `file_path` instead of host/port/user.
    pub fn is_file(self) -> bool {
        self == Dialect::Sqlite
    }
}

/// How to reach a database (the Electron app's SQLConnectionConfig).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectConfig {
    pub dialect: Option<Dialect>,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub database: String,
    pub ssl: bool,
    pub file_path: Option<PathBuf>,
}

/// One cell. Temporal and other exotic types arrive as their text form;
/// `Decimal` keeps full precision as text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    Uuid(String),
    Date(String),
    Time(String),
    Timestamp(String),
    /// Anything else, as the server prints it (arrays, intervals, geometry…).
    Other(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The cell as the grid shows it (bytes as hex, NULL as "NULL").
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bytes(b) => {
                let mut s = String::with_capacity(2 + b.len() * 2);
                s.push_str("0x");
                for byte in b.iter().take(64) {
                    s.push_str(&format!("{byte:02x}"));
                }
                if b.len() > 64 {
                    s.push('…');
                }
                s
            }
            Value::Json(j) => j.to_string(),
            Value::Decimal(s) | Value::Text(s) | Value::Uuid(s) | Value::Date(s) | Value::Time(s) | Value::Timestamp(s) | Value::Other(s) => s.clone(),
        }
    }
}

/// A result column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// The server's type name, lowercase where the server allows (`int4`, `varchar`, `datetime2`…).
    pub type_name: String,
    /// The table it comes from, when the server says (Postgres and MySQL do).
    pub origin: Option<TableRef>,
}

pub type Row = Vec<Value>;

/// What a running statement streams back. A statement may produce several
/// result sets (MSSQL batches, MySQL procedures): each is
/// `ResultStart`, `Rows`*, `ResultEnd`. A statement without rows sends just
/// `ResultEnd { affected }`.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    ResultStart(Vec<Column>),
    /// A batch of rows (drivers send batches of up to ~500).
    Rows(Vec<Row>),
    ResultEnd { affected: Option<u64> },
    /// Server messages (Postgres NOTICE, MSSQL PRINT, MySQL warnings).
    Notice(String),
}

/// Stops the statement running on a connection, from another task.
pub trait Canceller: Send + Sync {
    fn cancel(&self) -> BoxFuture<'static, anyhow::Result<()>>;
}

/// An open connection. One statement at a time; `execute` streams into `events`.
/// When the receiver is dropped the driver stops fetching and returns `Ok`.
#[async_trait::async_trait]
pub trait Connection: Send {
    fn dialect(&self) -> Dialect;

    /// e.g. "PostgreSQL 16.4".
    fn server_version(&self) -> String;

    /// Run ONE statement (the pane splits scripts with sql-lang). `params` bind
    /// the dialect's native placeholders ($1 / ? / @P1).
    async fn execute(&mut self, sql: &str, params: &[Value], events: UnboundedSender<Event>) -> anyhow::Result<()>;

    /// Cancels whatever `execute` is running on this connection.
    fn canceller(&self) -> Arc<dyn Canceller>;

    /// The databases on the server (one entry, the file, for SQLite).
    async fn databases(&mut self) -> anyhow::Result<Vec<String>>;

    /// The catalog of the current database: every schema the user can see
    /// (system schemas excluded), with tables, views, columns, keys, indexes and routines.
    async fn introspect(&mut self) -> anyhow::Result<Catalog>;

    /// The CREATE statement of a table or view, as close to the server's own as it allows.
    async fn ddl(&mut self, table: &TableRef) -> anyhow::Result<String>;

    async fn close(self: Box<Self>) -> anyhow::Result<()>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TableRef {
    /// None in SQLite (or when the server's default schema applies).
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: Option<&str>, name: &str) -> Self {
        TableRef { schema: schema.map(str::to_owned), name: name.to_owned() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub database: String,
    /// Where unqualified names resolve, in order (Postgres search_path; `dbo` in MSSQL;
    /// the database itself in MySQL; `main` in SQLite).
    pub search_path: Vec<String>,
    pub schemas: Vec<Schema>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub tables: Vec<Table>,
    pub routines: Vec<Routine>,
    pub sequences: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableKind {
    #[default]
    Table,
    View,
    MaterializedView,
    Foreign,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub indexes: Vec<Index>,
    pub row_estimate: Option<u64>,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// Serial / identity / AUTO_INCREMENT / rowid alias: leave it out of INSERTs.
    pub generated: bool,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub references: TableRef,
    pub ref_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutineKind {
    #[default]
    Function,
    Procedure,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    pub name: String,
    pub kind: RoutineKind,
    /// e.g. "(user_id integer, since date)".
    pub signature: String,
    pub returns: Option<String>,
}

impl Catalog {
    pub fn schema(&self, name: &str) -> Option<&Schema> {
        self.schemas.iter().find(|s| s.name == name)
    }

    /// A table by reference; an unqualified name resolves through `search_path`.
    pub fn table(&self, r: &TableRef) -> Option<(&Schema, &Table)> {
        fn find<'a>(s: &'a Schema, name: &str) -> Option<(&'a Schema, &'a Table)> {
            s.tables.iter().find(|t| t.name == name).map(|t| (s, t))
        }
        match &r.schema {
            Some(s) => self.schema(s).and_then(|s| find(s, &r.name)),
            None => self.search_path.iter().filter_map(|n| self.schema(n)).find_map(|s| find(s, &r.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unqualified_names_resolve_through_the_search_path() {
        let t = |n: &str| Table { name: n.into(), ..Default::default() };
        let cat = Catalog {
            database: "db".into(),
            search_path: vec!["app".into(), "public".into()],
            schemas: vec![
                Schema { name: "public".into(), tables: vec![t("users"), t("orders")], ..Default::default() },
                Schema { name: "app".into(), tables: vec![t("users")], ..Default::default() },
            ],
        };
        assert_eq!(cat.table(&TableRef::new(None, "users")).unwrap().0.name, "app");
        assert_eq!(cat.table(&TableRef::new(None, "orders")).unwrap().0.name, "public");
        assert_eq!(cat.table(&TableRef::new(Some("public"), "users")).unwrap().0.name, "public");
        assert!(cat.table(&TableRef::new(Some("app"), "orders")).is_none());
    }

    #[test]
    fn values_display_like_the_grid() {
        assert_eq!(Value::Null.display(), "NULL");
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).display(), "0xdead");
        assert_eq!(Value::Json(serde_json::json!({"a": 1})).display(), r#"{"a":1}"#);
    }
}
