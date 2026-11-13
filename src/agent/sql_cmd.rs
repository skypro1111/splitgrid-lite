//! `sg-sql` commands (the Electron app's agent-sql-bridge + useSqlAgentBridge):
//! an agent runs queries, inspects schema and exports or imports tables
//! through the SQL pane of ITS OWN workspace, via `$SPLITGRID_SQL_CLI`.
//!
//! Targeting is Electron's: the workspace's first SQL pane (`targetedMultiple`
//! says when there are more); with none, commands that need one open it in the
//! caller's zone (`--zone/--at/--split`). Statements run through the pane's
//! queue, so they surface as its result tabs, and the agent gets the last
//! statement's result as a text table capped at 100 rows.
//!
//! Read-only unless the user allows writes: every statement of a batch is
//! classified, conservatively (anything not positively a pure read is a
//! write), and a batch with any write is refused whole. As in Electron, there
//! is no server-side read-only transaction; the classifier is the gate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::oneshot;
use gpui_kit::{AsyncApp, Context, Entity, WeakEntity, Window};
use serde_json::{Map, Value as Json, json};
use sql_core::{Catalog, Dialect, Event, TableKind, TableRef, Value};
use sql_lang::StatementKind;

use super::AgentConfig;
use super::browser_cmd::Answer;
use super::server::Request;
use super::terminal_cmd::{ZoneFlags, fail, label_of, ok, reply, take_zone_flags, valid_zone_name, zone_panes};
use super::zones::{self, DEFAULT_ZONE, OpenArgs, ZoneOwner, ZoneSplit};
use crate::app::AppView;
use crate::model::workspace::{Container, ContainerContent, ContentType, new_id};
use crate::sql_pane::SqlPane;
use crate::sql_pane::agent::{self, AgentOutcome, AgentPhase, AgentResult};

/// Connecting (or introspecting) a pane, before a command gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Below the helper's 35s budget, so our own timeout answers first.
const RUN_TIMEOUT: Duration = Duration::from_secs(33);
const MAX_CELL: usize = 40;
/// Excel's limit for one cell's text.
const XLSX_MAX_TEXT: usize = 32_767;

const WRITE_BLOCKED: &str = "This statement modifies data/schema. Ask the user to enable \"Allow agents to modify data\" in Settings.";

// ─── Gate + env ──────────────────────────────────────────────────────────────

/// (SQL control, writes allowed), fresh from `app-settings.json`: Electron's
/// initAgentIntegrations (control needs the master switch, writes need control).
pub fn switches(cfg: &AgentConfig) -> (bool, bool) {
    let v: Json = std::fs::read(&cfg.settings_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let on = |k: &str| v.get(k).and_then(Json::as_bool).unwrap_or(false);
    let control = on("agentIntegrations") && on("agentSqlControl");
    (control, control && on("agentSqlWrite"))
}

/// The SPLITGRID_SQL_* env of a new terminal (Electron's names). `_WRITE` is
/// only a hint; the gate is checked on every command.
pub fn add_env(cfg: &AgentConfig, env: &mut HashMap<String, String>) {
    let (control, write) = switches(cfg);
    if !control {
        return;
    }
    env.insert("SPLITGRID_SQL_ENDPOINT".into(), format!("http://127.0.0.1:{}/sql", cfg.port));
    env.insert("SPLITGRID_SQL_CLI".into(), cfg.helpers.join("splitgrid-sql.sh").to_string_lossy().into_owned());
    env.insert("SPLITGRID_SQL_TOKEN".into(), cfg.token.clone());
    if write {
        env.insert("SPLITGRID_SQL_WRITE".into(), "1".into());
    }
}

/// The main-side gate (agent-sql-bridge's processSqlCommand): refused without
/// SQL control even with a valid token, which every bridge shares.
pub fn gate(control: bool, argv: &[String]) -> Result<(), Json> {
    if !control {
        return Err(fail("sql_disabled", "SQL agent access is disabled. Ask the user to enable it in Settings → Agent integrations."));
    }
    if argv.is_empty() {
        return Err(fail("empty_command", "usage: splitgrid-sql <cmd> [args]"));
    }
    Ok(())
}

fn help() -> Json {
    ok(json!({
        "usage": "splitgrid-sql <command> [args]   (acts on the SQL pane in YOUR workspace)",
        "commands": [
            "list                         — SQL panes in this workspace + which is targeted",
            "connections                  — saved SQL connections (label, dialect, host, db)",
            "use <conn> [database]        — switch the target pane to a saved connection",
            "open <conn>                  — create a SQL pane if none, then `use <conn>`",
            "     [--zone <name>] [--split right|down] [--at <pane>]  — where a created pane goes in your zone",
            "tables [schema]              — tables/views in a schema (default current)",
            "columns <table> [schema]     — columns with type / nullable / PK",
            "ddl <table> [schema]         — the table's DDL text",
            "query <sql>                  — run SQL (surfaces in a tab); write-gated",
            "explain <sql>                — EXPLAIN the SQL (read)",
            "export <table> <format> <path> [schema]  — export a table to a file (csv|json|sql|xlsx)",
            "import <table> <csvpath> [schema]        — import a CSV into a table (write-gated)",
        ],
        "notSupported": ["screenshot (SplitGrid Lite can't capture its window for agents)"],
    }))
}

// ─── Read / write classification ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

const READ_LEADERS: [&str; 9] = ["select", "show", "explain", "describe", "desc", "pragma", "use", "values", "table"];
const WRITE_LEADERS: [&str; 36] = [
    "insert", "update", "delete", "merge", "replace", "upsert", "create", "alter", "drop", "truncate", "rename", "comment", "grant", "revoke", "call", "do",
    "exec", "execute", "set", "reset", "begin", "start", "commit", "rollback", "savepoint", "vacuum", "analyze", "cluster", "reindex", "copy", "load",
    "lock", "attach", "detach", "release", "end",
];
/// DML / DDL anywhere in a SELECT or CTE makes it a write (over-matches on purpose).
const EMBEDDED_WRITES: [&str; 11] = ["insert", "update", "delete", "merge", "truncate", "create", "alter", "drop", "grant", "revoke", "call"];

/// The statement past leading whitespace and comments.
fn strip_leading_trivia(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            return rest;
        }
    }
}

/// Lowercase words (`[A-Za-z0-9_]+`), as JavaScript's `\b` sees them.
fn words(sql: &str) -> Vec<String> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect()
}

/// SELECT … INTO, locking reads, and DML/DDL in CTE bodies (Electron's selectHasWriteClause).
fn select_has_write_clause(w: &[String]) -> bool {
    let at = |word: &str| w.iter().position(|x| x == word);
    if let (Some(s), Some(i)) = (at("select"), w.iter().rposition(|x| x == "into"))
        && s < i
    {
        return true;
    }
    let locking = w.windows(2).any(|p| p[0] == "for" && matches!(p[1].as_str(), "update" | "share" | "no" | "key"));
    locking || w.iter().any(|x| EMBEDDED_WRITES.contains(&x.as_str()))
}

/// One statement: a read only when positively recognised as one. `kind` is
/// sql-lang's, which also catches a WITH whose CTE modifies data.
pub fn classify_statement(sql: &str, kind: StatementKind) -> Access {
    let body = strip_leading_trivia(sql);
    let w = words(body);
    let Some(first) = w.first().map(String::as_str) else { return Access::Read };
    if WRITE_LEADERS.contains(&first) {
        return Access::Write;
    }
    let read = match first {
        "select" | "with" | "table" | "values" => kind == StatementKind::Select && !select_has_write_clause(&w),
        // EXPLAIN ANALYZE runs the statement.
        "explain" => !w.iter().any(|x| x == "analyze"),
        // `PRAGMA user_version = 3` writes; `PRAGMA table_info(t)` reads.
        "pragma" => !body.contains('='),
        _ => READ_LEADERS.contains(&first),
    };
    if read { Access::Read } else { Access::Write }
}

/// A batch is a write when ANY statement is (refused whole, never half-run).
pub fn batch_access(sql: &str, d: Dialect) -> Access {
    let write = sql_lang::split(sql, d).iter().any(|s| classify_statement(&s.text, s.kind) == Access::Write);
    if write { Access::Write } else { Access::Read }
}

// ─── Text table ──────────────────────────────────────────────────────────────

/// A cell as the agent's text table shows it.
pub fn cell_text(v: &Value) -> String {
    let s = v.display().replace(['\n', '\r'], " ");
    if s.chars().count() > MAX_CELL {
        let mut out: String = s.chars().take(MAX_CELL - 1).collect();
        out.push('…');
        out
    } else {
        s
    }
}

/// Electron's renderTextTable: `rows` are the first rows of `total`.
pub fn render_text_table(columns: &[String], rows: &[Vec<String>], total: usize) -> String {
    if columns.is_empty() {
        return "(no columns)".into();
    }
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for r in rows {
        for (i, t) in r.iter().enumerate().take(widths.len()) {
            widths[i] = widths[i].max(t.chars().count());
        }
    }
    let sep = |ch: char| widths.iter().map(|w| ch.to_string().repeat(w + 2)).collect::<Vec<_>>().join("+");
    let line = |cells: &[String]| {
        let parts: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let t = cells.get(i).map(String::as_str).unwrap_or("");
                format!(" {t}{} ", " ".repeat(w - t.chars().count()))
            })
            .collect();
        format!("|{}|", parts.join("|"))
    };
    let mut out = vec![sep('-'), line(columns), sep('=')];
    out.extend(rows.iter().map(|r| line(r)));
    out.push(sep('-'));
    if total > rows.len() {
        out.push(format!("… {} more rows (showing {} of {total})", total - rows.len(), rows.len()));
    }
    out.join("\n")
}

fn result_table(r: &AgentResult) -> String {
    let rows: Vec<Vec<String>> = r.rows.iter().map(|row| row.iter().map(cell_text).collect()).collect();
    render_text_table(&r.columns, &rows, r.fetched)
}

/// The statement's verb, as the Postgres driver's `command` (SELECT, INSERT…).
fn command_of(sql: &str) -> String {
    words(strip_leading_trivia(sql)).first().map(|w| w.to_ascii_uppercase()).unwrap_or_default()
}

/// Rows of a result set, else the rows a write touched.
fn row_count(r: &AgentResult) -> u64 {
    if r.columns.is_empty() { r.affected.unwrap_or(0) } else { r.fetched as u64 }
}

// ─── Export ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Sql,
    Xlsx,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        Some(match s.to_ascii_lowercase().as_str() {
            "csv" => Format::Csv,
            "json" => Format::Json,
            "sql" => Format::Sql,
            "xlsx" => Format::Xlsx,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Sql => "sql",
            Format::Xlsx => "xlsx",
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A cell in full, as text (CSV, XLSX strings).
pub fn full_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bytes(b) => format!("0x{}", hex(b)),
        Value::Json(j) => j.to_string(),
        other => other.display(),
    }
}

/// RFC 4180: quoted when it holds the delimiter, a quote, CR or LF.
pub fn csv_quote(text: &str, delimiter: char) -> String {
    if text.contains([delimiter, '"', '\n', '\r']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_owned() }
}

pub fn csv_line(cells: impl IntoIterator<Item = String>) -> String {
    cells.into_iter().map(|c| csv_quote(&c, ',')).collect::<Vec<_>>().join(",")
}

pub fn json_value(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Json(j) => j.clone(),
        other => Json::String(full_text(other)),
    }
}

/// An identifier, always quoted (Electron's quoteIdentFor).
pub fn quote_ident(name: &str, d: Dialect) -> String {
    let (open, close) = match d {
        Dialect::Mysql => ('`', '`'),
        Dialect::Mssql => ('[', ']'),
        Dialect::Postgres | Dialect::Sqlite => ('"', '"'),
    };
    format!("{open}{}{close}", name.replace(close, &format!("{close}{close}")))
}

/// `schema.table`, the schema only where the dialect has them (Electron's tableRefFor).
pub fn table_sql(schema: Option<&str>, table: &str, d: Dialect) -> String {
    match schema.filter(|s| !s.is_empty() && agent::supports_schemas(d)) {
        Some(s) => format!("{}.{}", quote_ident(s, d), quote_ident(table, d)),
        None => quote_ident(table, d),
    }
}

fn string_literal(text: &str, d: Dialect) -> String {
    let quoted = text.replace('\'', "''");
    match d {
        Dialect::Mysql => format!("'{}'", quoted.replace('\\', "\\\\")),
        Dialect::Mssql => format!("N'{quoted}'"),
        _ => format!("'{quoted}'"),
    }
}

/// A value as a SQL literal (Electron's sqlValueLiteral).
pub fn sql_literal(v: &Value, d: Dialect) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match d {
            Dialect::Sqlite | Dialect::Mssql => if *b { "1" } else { "0" }.into(),
            _ => if *b { "TRUE" } else { "FALSE" }.into(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(_) => "NULL".into(),
        Value::Bytes(_) => sql_lang::literal(v, d),
        other => string_literal(&full_text(other), d),
    }
}

pub fn sql_insert(table: &str, columns: &[String], row: &[Value], d: Dialect) -> String {
    let cols = columns.iter().map(|c| quote_ident(c, d)).collect::<Vec<_>>().join(", ");
    let vals = row.iter().map(|v| sql_literal(v, d)).collect::<Vec<_>>().join(", ");
    format!("INSERT INTO {table} ({cols}) VALUES ({vals});")
}

/// CSV / JSON / SQL, written as rows stream in (Electron's export sinks).
pub struct TextSink<W: std::io::Write> {
    out: W,
    format: Format,
    columns: Vec<String>,
    /// For SQL: the INSERTs' target.
    table: String,
    dialect: Dialect,
    wrote_any: bool,
}

impl<W: std::io::Write> TextSink<W> {
    pub fn new(out: W, format: Format, table: String, dialect: Dialect) -> Self {
        TextSink { out, format, columns: Vec::new(), table, dialect, wrote_any: false }
    }

    pub fn begin(&mut self, columns: Vec<String>) -> std::io::Result<()> {
        self.columns = columns;
        match self.format {
            Format::Csv => {
                let header = csv_line(self.columns.clone());
                self.line(&header)
            }
            Format::Json => self.out.write_all(b"["),
            _ => Ok(()),
        }
    }

    fn line(&mut self, text: &str) -> std::io::Result<()> {
        if self.wrote_any {
            self.out.write_all(b"\r\n")?;
        }
        self.wrote_any = true;
        self.out.write_all(text.as_bytes())
    }

    pub fn write(&mut self, row: &[Value]) -> std::io::Result<()> {
        match self.format {
            Format::Csv => {
                let line = csv_line(row.iter().map(full_text));
                self.line(&line)
            }
            Format::Json => {
                let obj: Map<String, Json> = self.columns.iter().cloned().zip(row.iter().map(json_value)).collect();
                let sep = if self.wrote_any { "," } else { "" };
                self.wrote_any = true;
                write!(self.out, "{sep}\n  {}", Json::Object(obj))
            }
            Format::Sql => writeln!(self.out, "{}", sql_insert(&self.table, &self.columns, row, self.dialect)),
            Format::Xlsx => Ok(()),
        }
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        if self.format == Format::Json {
            self.out.write_all(if self.wrote_any { b"\n]\n" } else { b"]\n" })?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

enum Sink {
    Text(TextSink<std::io::BufWriter<std::fs::File>>),
    Xlsx { book: Box<rust_xlsxwriter::Workbook>, row: u32 },
}

fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> String {
    e.to_string()
}

impl Sink {
    fn open(path: &Path, format: Format, table: String, d: Dialect) -> Result<Sink, String> {
        if format == Format::Xlsx {
            let mut book = Box::new(rust_xlsxwriter::Workbook::new());
            book.add_worksheet().set_name("Export").map_err(xlsx_err)?;
            return Ok(Sink::Xlsx { book, row: 0 });
        }
        let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Sink::Text(TextSink::new(std::io::BufWriter::new(file), format, table, d)))
    }

    fn sheet(book: &mut rust_xlsxwriter::Workbook) -> Result<&mut rust_xlsxwriter::Worksheet, String> {
        book.worksheet_from_index(0).map_err(xlsx_err)
    }

    fn begin(&mut self, columns: Vec<String>) -> Result<(), String> {
        match self {
            Sink::Text(t) => t.begin(columns).map_err(|e| e.to_string()),
            Sink::Xlsx { book, row } => {
                if columns.is_empty() {
                    return Ok(());
                }
                let bold = rust_xlsxwriter::Format::new().set_bold();
                let sheet = Self::sheet(book)?;
                for (i, c) in columns.iter().enumerate() {
                    sheet.write_string_with_format(0, i as u16, c, &bold).map_err(xlsx_err)?;
                }
                *row = 1;
                Ok(())
            }
        }
    }

    fn write(&mut self, values: &[Value]) -> Result<(), String> {
        match self {
            Sink::Text(t) => t.write(values).map_err(|e| e.to_string()),
            Sink::Xlsx { book, row } => {
                let r = *row;
                let sheet = Self::sheet(book)?;
                for (i, v) in values.iter().enumerate() {
                    let c = i as u16;
                    match v {
                        Value::Null => continue,
                        Value::Int(n) => sheet.write_number(r, c, *n as f64),
                        Value::Float(f) if f.is_finite() => sheet.write_number(r, c, *f),
                        Value::Bool(b) => sheet.write_boolean(r, c, *b),
                        other => {
                            let text: String = full_text(other).chars().take(XLSX_MAX_TEXT).collect();
                            sheet.write_string(r, c, text)
                        }
                    }
                    .map_err(xlsx_err)?;
                }
                *row += 1;
                Ok(())
            }
        }
    }

    fn finish(self, path: &Path) -> Result<(), String> {
        match self {
            Sink::Text(t) => t.finish().map(|_| ()).map_err(|e| e.to_string()),
            Sink::Xlsx { mut book, .. } => book.save(path).map_err(xlsx_err),
        }
    }
}

/// Stream one SELECT's rows into `path`; the row count.
async fn write_export(
    mut events: futures::channel::mpsc::UnboundedReceiver<Event>,
    done: impl Future<Output = anyhow::Result<()>>,
    path: PathBuf,
    format: Format,
    table: String,
    d: Dialect,
) -> Result<usize, String> {
    let mut sink = Sink::open(&path, format, table, d)?;
    let mut begun = false;
    let mut count = 0usize;
    let mut failure = None;
    while let Some(ev) = events.next().await {
        let step = match ev {
            Event::ResultStart(cols) if !begun => {
                begun = true;
                sink.begin(cols.into_iter().map(|c| c.name).collect())
            }
            Event::Rows(rows) if begun => rows.iter().try_for_each(|r| {
                count += 1;
                sink.write(r)
            }),
            _ => Ok(()),
        };
        if let Err(e) = step {
            failure = Some(e);
            break;
        }
    }
    drop(events);
    let result = match (failure, done.await) {
        (Some(e), _) => Err(e),
        (None, Err(e)) => Err(format!("{e:#}")),
        (None, Ok(())) => {
            if !begun {
                sink.begin(Vec::new())?;
            }
            sink.finish(&path)
        }
    };
    match result {
        Ok(()) => Ok(count),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

// ─── Import ──────────────────────────────────────────────────────────────────

/// The likeliest delimiter of the first non-empty line (Electron's detectCsvDelimiter).
pub fn detect_delimiter(text: &str) -> char {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut best = (',', -1i64);
    for d in [',', ';', '\t', '|'] {
        let mut in_quotes = false;
        let mut n = 0i64;
        for ch in first.chars() {
            if ch == '"' {
                in_quotes = !in_quotes;
            } else if ch == d && !in_quotes {
                n += 1;
            }
        }
        if n > best.1 {
            best = (d, n);
        }
    }
    best.0
}

/// RFC 4180 into rows of cells (Electron's parseCsv): quoted fields, doubled
/// quotes, CRLF / LF / CR records; a final empty line is dropped.
pub fn parse_csv(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' | '\n' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    if rows.last().is_some_and(|r: &Vec<String>| r.len() == 1 && r[0].is_empty()) {
        rows.pop();
    }
    rows
}

fn placeholder(d: Dialect, n: usize) -> String {
    match d {
        Dialect::Postgres => format!("${n}"),
        Dialect::Mssql => format!("@P{n}"),
        Dialect::Mysql | Dialect::Sqlite => "?".into(),
    }
}

/// Most bind values one statement may carry (Electron's MAX_PARAMS).
fn max_params(d: Dialect) -> usize {
    match d {
        Dialect::Postgres | Dialect::Mysql => 65_535,
        Dialect::Sqlite => 32_766,
        Dialect::Mssql => 2_100,
    }
}

/// Batched multi-row parameterised INSERTs (values are never spliced into the SQL).
pub fn insert_batches(table: &str, columns: &[String], rows: &[Vec<String>], d: Dialect) -> Vec<(String, Vec<Value>)> {
    if columns.is_empty() {
        return Vec::new();
    }
    let size = (max_params(d) / columns.len()).clamp(1, 200);
    let cols = columns.iter().map(|c| quote_ident(c, d)).collect::<Vec<_>>().join(", ");
    rows.chunks(size)
        .map(|chunk| {
            let mut params = Vec::with_capacity(chunk.len() * columns.len());
            let tuples: Vec<String> = chunk
                .iter()
                .map(|row| {
                    let cells: Vec<String> = (0..columns.len())
                        .map(|i| {
                            params.push(Value::Text(row.get(i).cloned().unwrap_or_default()));
                            placeholder(d, params.len())
                        })
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            (format!("INSERT INTO {table} ({cols}) VALUES {}", tuples.join(", ")), params)
        })
        .collect()
}

// ─── Schema lookups ──────────────────────────────────────────────────────────

/// The schema unqualified names resolve to, where the dialect has schemas.
fn current_schema(catalog: &Catalog, d: Dialect) -> Option<String> {
    agent::supports_schemas(d).then(|| catalog.search_path.first().cloned()).flatten()
}

fn kind_name(k: TableKind) -> &'static str {
    match k {
        TableKind::Table => "table",
        TableKind::View => "view",
        TableKind::MaterializedView => "materializedView",
        TableKind::Foreign => "foreignTable",
    }
}

pub fn list_tables(catalog: &Catalog, schema: Option<&str>, d: Dialect) -> Vec<Json> {
    let wanted = schema.map(str::to_owned).or_else(|| current_schema(catalog, d));
    catalog
        .schemas
        .iter()
        .filter(|s| wanted.as_ref().is_none_or(|w| *w == s.name))
        .flat_map(|s| {
            s.tables.iter().map(move |t| {
                let mut v = json!({ "schema": s.name, "name": t.name, "kind": kind_name(t.kind) });
                if let Some(n) = t.row_estimate {
                    v["rowEstimate"] = n.into();
                }
                v
            })
        })
        .collect()
}

/// A table of the catalog: (its schema, its name, its key columns, its columns).
struct Found {
    schema: String,
    name: String,
    key: Vec<String>,
    columns: Vec<sql_core::ColumnInfo>,
}

fn find_table(catalog: &Catalog, table: &str, schema: Option<&str>) -> Option<Found> {
    let (s, t) = catalog.table(&TableRef::new(schema, table)).or_else(|| {
        // Forgiving like a user would be: any schema, then any case.
        let all = || catalog.schemas.iter().filter(|s| schema.is_none_or(|w| w == s.name)).flat_map(|s| s.tables.iter().map(move |t| (s, t)));
        all().find(|(_, t)| t.name == table).or_else(|| all().find(|(_, t)| t.name.eq_ignore_ascii_case(table)))
    })?;
    Some(Found { schema: s.name.clone(), name: t.name.clone(), key: t.primary_key.clone(), columns: t.columns.clone() })
}

// ─── Running ─────────────────────────────────────────────────────────────────

type Failure = (&'static str, String);

fn failure(note: &Map<String, Json>, (error, message): Failure) -> Json {
    let mut data = note.clone();
    data.insert("message".into(), message.into());
    reply(false, Some(error), Json::Object(data))
}

fn success(note: &Map<String, Json>, more: Json) -> Json {
    let mut data = note.clone();
    if let Json::Object(m) = more {
        data.extend(m);
    }
    ok(Json::Object(data))
}

async fn sleep(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// Wait until the pane is connected (and, with `catalog`, introspected).
async fn wait_ready(pane: &Entity<SqlPane>, catalog: bool, cx: &mut AsyncApp) -> Result<(), Failure> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let (phase, has_catalog) = pane.read_with(cx, |p, _| (p.agent_phase(), p.agent_catalog().is_some()));
        match phase {
            AgentPhase::Ready if !catalog || has_catalog => return Ok(()),
            AgentPhase::Idle => return Err(("not_connected", "the SQL pane has no connection; run: use <connection>  (see: connections)".into())),
            AgentPhase::NeedsPassword => {
                return Err((
                    "connect_failed",
                    "the saved password can't be read by SplitGrid Lite; ask the user to type it in the SQL pane's prompt, then retry".into(),
                ));
            }
            AgentPhase::Failed(e) => return Err(("connect_failed", e)),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(("timeout", "the SQL pane is still connecting; retry shortly".into()));
        }
        sleep(cx, 50).await;
    }
}

/// The schema changed: re-introspect, and give it a moment so `tables` sees it.
async fn refresh_catalog(pane: &Entity<SqlPane>, cx: &mut AsyncApp) {
    let before = pane.read_with(cx, |p, _| p.agent_catalog());
    pane.update(cx, |p, cx| p.agent_refresh_catalog(cx));
    for _ in 0..100 {
        let now = pane.read_with(cx, |p, _| p.agent_catalog());
        let changed = match (&before, &now) {
            (Some(a), Some(b)) => !std::sync::Arc::ptr_eq(a, b),
            _ => true,
        };
        if changed {
            return;
        }
        sleep(cx, 50).await;
    }
}

/// Await a batch the pane runs, past which the agent is told it's still running.
async fn await_run(rx: oneshot::Receiver<AgentOutcome>, cx: &mut AsyncApp) -> Result<AgentResult, Failure> {
    let timer = cx.background_executor().timer(RUN_TIMEOUT);
    match futures::future::select(rx, timer).await {
        futures::future::Either::Left((Ok(Ok(r)), _)) => Ok(r),
        futures::future::Either::Left((Ok(Err(e)), _)) => Err(("", e)),
        futures::future::Either::Left((Err(_), _)) => Err(("", "the SQL pane closed".into())),
        futures::future::Either::Right(_) => Err(("timeout", "still running in the SQL pane (it keeps going; its result will show there)".into())),
    }
}

/// `code` unless the failure carries its own (a timeout).
fn or_code(f: Failure, code: &'static str) -> Failure {
    (if f.0.is_empty() { code } else { f.0 }, f.1)
}

fn resolve_path(raw: &str, cwd: Option<&str>) -> PathBuf {
    let p = PathBuf::from(crate::grid::expand_tilde(raw.trim()));
    match cwd {
        Some(dir) if p.is_relative() => Path::new(dir).join(p),
        _ => p,
    }
}

impl AppView {
    /// `POST /sql`: the token, the gate, then the command (answered when done).
    pub(crate) fn handle_sql_request(&mut self, req: Request, cfg: &AgentConfig, window: &mut Window, cx: &mut Context<Self>) {
        if !cfg.is_token(&req.body["token"]) {
            req.respond(json!({ "ok": false, "error": "unauthorized" }));
            return;
        }
        let (control, write) = switches(cfg);
        let argv: Vec<String> = req.body["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        if let Err(v) = gate(control, &argv) {
            req.respond(v);
            return;
        }
        let terminal = req.body["terminal"].as_str().unwrap_or_default().to_owned();
        match self.run_sql_command(&terminal, &argv, write, window, cx) {
            Answer::Now(v) => req.respond(v),
            Answer::Later(task) => cx.spawn(async move |_, _| req.respond(task.await)).detach(),
        }
    }

    /// Run one `sg-sql` command for the agent in pane `terminal`.
    pub(crate) fn run_sql_command(&mut self, terminal: &str, argv: &[String], write_allowed: bool, window: &mut Window, cx: &mut Context<Self>) -> Answer {
        let now = Answer::Now;
        if argv.is_empty() {
            return now(fail("empty_command", "usage: splitgrid-sql <cmd> [args]"));
        }
        let cmd = argv[0].to_lowercase();
        // Zone flags place a created pane; never parsed out of SQL text.
        let (flags, rest) = if matches!(cmd.as_str(), "query" | "explain") { (ZoneFlags::default(), argv[1..].to_vec()) } else { take_zone_flags(&argv[1..]) };
        if matches!(cmd.as_str(), "help" | "--help" | "-h" | "") {
            return now(help());
        }
        if cmd == "screenshot" {
            return now(fail("not_supported", "screenshot isn't available in SplitGrid Lite"));
        }
        let Some(ws_ix) = self.state.workspaces.iter().position(|w| w.container(terminal).is_some()) else {
            return now(fail("no_workspace", "cannot locate the calling terminal's workspace"));
        };
        let panes: Vec<String> = self.state.workspaces[ws_ix].containers.iter().filter(|c| c.content.kind == ContentType::Sql).map(|c| c.id.clone()).collect();
        match cmd.as_str() {
            "list" => return now(self.sql_list(ws_ix, &panes, cx)),
            "connections" | "conns" => return now(ok(json!({ "connections": agent::saved_connections(cx) }))),
            _ => {}
        }

        // Arguments first, so a bad call doesn't open a pane.
        let arg = |i: usize| rest.get(i).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_owned);
        let usage = |error: &str, message: &str| now(fail(error, message));
        let op = match cmd.as_str() {
            "open" => match Some(rest.join(" ").trim().to_owned()).filter(|s| !s.is_empty()) {
                Some(conn) => Op::Use(conn, None),
                None => return usage("missing_connection", "usage: open <connection name|id>  (run: connections)"),
            },
            "use" => match arg(0) {
                Some(conn) => Op::Use(conn, arg(1)),
                None => return usage("missing_connection", "usage: use <connection name|id> [database]"),
            },
            "tables" => Op::Tables(arg(0)),
            "columns" | "ddl" => match arg(0) {
                Some(t) if cmd == "columns" => Op::Columns(t, arg(1)),
                Some(t) => Op::Ddl(t, arg(1)),
                None => return usage("missing_table", &format!("usage: {cmd} <table> [schema]")),
            },
            "query" | "explain" => {
                let sql = rest.join(" ").trim().to_owned();
                if sql.is_empty() {
                    return usage("missing_sql", &format!("usage: {cmd} <sql>"));
                }
                if cmd == "query" { Op::Query(sql) } else { Op::Explain(sql) }
            }
            "export" => {
                let (Some(table), Some(format), Some(path)) = (arg(0), arg(1), arg(2)) else {
                    return usage("usage", "usage: export <table> <csv|json|sql|xlsx> <path> [schema]");
                };
                let Some(format) = Format::parse(&format) else { return usage("bad_format", "format must be csv|json|sql|xlsx") };
                Op::Export { table, format, path: resolve_path(&path, self.sql_cwd(terminal).as_deref()), schema: arg(3) }
            }
            "import" => {
                let (Some(table), Some(path)) = (arg(0), arg(1)) else { return usage("usage", "usage: import <table> <csvpath> [schema]") };
                Op::Import { table, path: resolve_path(&path, self.sql_cwd(terminal).as_deref()), schema: arg(2) }
            }
            _ => return usage("unknown_command", &format!("unknown command: {cmd}")),
        };

        // The pane: the workspace's first, else a new one in the caller's zone.
        let (id, created) = match panes.first() {
            Some(id) => (id.clone(), false),
            None => match self.sql_open_pane(ws_ix, terminal, &flags, cx) {
                Ok(id) => (id, true),
                Err(e) => return now(e),
            },
        };
        let mut note = Map::new();
        if panes.len() > 1 && cmd != "open" {
            note.insert("targetedMultiple".into(), panes.len().into());
        }
        note.insert("surface".into(), id.clone().into());
        if cmd == "open" {
            note.insert("created".into(), created.into());
        }
        let Some(pane) = self.sql_pane_view(&id, window, cx) else {
            return now(failure(&note, ("pane_not_ready", "opened a SQL pane; retry the command".into())));
        };

        // Gate writes before anything runs (the dialect is re-checked once connected).
        let write_blocked = |note: &Map<String, Json>, message: &str| now(failure(note, ("write_blocked", message.into())));
        match &op {
            Op::Query(sql) if !write_allowed && batch_access(sql, pane.read(cx).agent_dialect()) == Access::Write => return write_blocked(&note, WRITE_BLOCKED),
            Op::Import { .. } if !write_allowed => {
                return write_blocked(&note, "Importing modifies data. Ask the user to enable \"Allow agents to modify data\" in Settings.");
            }
            _ => {}
        }
        if let Op::Use(conn, db) = &op
            && let Err(message) = pane.update(cx, |p, cx| p.agent_use(conn, db.clone(), window, cx))
        {
            return now(failure(&note, ("connect_failed", message)));
        }
        Answer::Later(cx.spawn(async move |_: WeakEntity<AppView>, cx: &mut AsyncApp| run_op(op, pane, note, write_allowed, cx).await))
    }

    /// The calling terminal's working directory (relative export/import paths).
    fn sql_cwd(&self, terminal: &str) -> Option<String> {
        self.panes
            .get(terminal)
            .and_then(|p| p.process.as_ref()?.cwd.clone())
            .or_else(|| self.state.workspaces.iter().find_map(|w| w.container(terminal)).and_then(|c| c.content.cwd.clone()))
    }

    fn sql_list(&self, ws_ix: usize, panes: &[String], cx: &Context<Self>) -> Json {
        let ws = &self.state.workspaces[ws_ix];
        let target = panes.first();
        let list: Vec<Json> = panes
            .iter()
            .map(|id| {
                let mut v = json!({ "id": id, "targeted": Some(id) == target });
                match self.sql_panes.get(id) {
                    Some((pane, _)) => {
                        let info = pane.read(cx).agent_info();
                        v["connected"] = info["connected"].clone();
                        v["connection"] = info["connectionName"].clone();
                        v["dialect"] = info["dialect"].clone();
                        v["database"] = info["database"].clone();
                        v["schema"] = info["schema"].clone();
                    }
                    // Never shown yet: what it was last on.
                    None => {
                        let state = ws.container(id).and_then(|c| c.content.extra.get("sqlState")).cloned().unwrap_or_default();
                        v["connected"] = false.into();
                        v["connection"] = state.get("connectionName").cloned().unwrap_or_default();
                        v["dialect"] = Json::Null;
                        v["database"] = state.get("database").cloned().unwrap_or_default();
                        v["schema"] = Json::Null;
                    }
                }
                v
            })
            .collect();
        ok(json!({ "workspace": ws.name, "targeted": target, "panes": list }))
    }

    /// A new SQL pane in the caller's zone; its container id.
    fn sql_open_pane(&mut self, ws_ix: usize, terminal: &str, flags: &ZoneFlags, cx: &mut Context<Self>) -> Result<String, Json> {
        if let Some(z) = &flags.zone
            && !valid_zone_name(z)
        {
            return Err(fail("bad_zone", "--zone takes a short name: letters, digits, \"-\", \"_\" or \".\" (up to 40)"));
        }
        let split = match ZoneSplit::parse(flags.split.as_deref()) {
            None => None,
            Some(Some(s)) => Some(s),
            Some(None) => return Err(fail("bad_split", "--split takes right or down")),
        };
        let owner = ZoneOwner::new(terminal, flags.zone.as_deref());
        let ws = &self.state.workspaces[ws_ix];
        if let Some(at) = &flags.at
            && !zones::zone_layout(ws.layout_tree.as_ref(), &owner).main.contains(at)
        {
            let panes = zone_panes(ws, &owner);
            let message = if panes.as_array().is_some_and(|a| !a.is_empty()) { "--at must be one of your zone's regular panes" } else { "your zone has no panes yet; open one without --at" };
            return Err(reply(false, Some("not_in_zone"), json!({ "message": message, "zone": owner.name, "panes": panes })));
        }
        let caller_label = ws.container(terminal).map(|c| label_of(c, self.process_name(terminal))).unwrap_or_else(|| "Agent".into());
        let zone_label = if owner.name == DEFAULT_ZONE { caller_label } else { format!("{caller_label} · {}", owner.name) };
        let id = new_id("c");
        let ws = &mut self.state.workspaces[ws_ix];
        let args = OpenArgs { owner: &owner, label: &zone_label, new_id: &id, anchor_id: Some(terminal), at: flags.at.as_deref(), split, background: false };
        match zones::open_in_zone(ws.layout_tree.take(), &args) {
            Ok(tree) => ws.layout_tree = Some(tree),
            Err(message) => return Err(fail("not_in_zone", message)),
        }
        let content = ContainerContent { kind: ContentType::Sql, label: Some("SQL".into()), ..ContainerContent::empty() };
        ws.containers.push(Container { id: id.clone(), content });
        ws.updated_at = crate::model::workspace::now_ms();
        self.changed(cx);
        Ok(id)
    }
}

/// A command for the targeted pane, its arguments checked.
enum Op {
    Use(String, Option<String>),
    Tables(Option<String>),
    Columns(String, Option<String>),
    Ddl(String, Option<String>),
    Query(String),
    Explain(String),
    Export { table: String, format: Format, path: PathBuf, schema: Option<String> },
    Import { table: String, path: PathBuf, schema: Option<String> },
}

async fn run_op(op: Op, pane: Entity<SqlPane>, note: Map<String, Json>, write_allowed: bool, cx: &mut AsyncApp) -> Json {
    let fail_with = |f: Failure| failure(&note, f);
    match op {
        Op::Use(..) => {
            if let Err(f) = wait_ready(&pane, false, cx).await {
                return fail_with(or_code(f, "connect_failed"));
            }
            // The schema comes with the catalog; don't hold the reply long for it.
            for _ in 0..100 {
                if pane.read_with(cx, |p, _| p.agent_catalog().is_some()) {
                    break;
                }
                sleep(cx, 50).await;
            }
            success(&note, pane.read_with(cx, |p, _| p.agent_info()))
        }
        Op::Tables(schema) => {
            if let Err(f) = wait_ready(&pane, true, cx).await {
                return fail_with(f);
            }
            let (catalog, d) = pane.read_with(cx, |p, _| (p.agent_catalog().unwrap_or_default(), p.agent_dialect()));
            let tables = list_tables(&catalog, schema.as_deref(), d);
            let schema = schema.or_else(|| current_schema(&catalog, d));
            success(&note, json!({ "schema": schema, "count": tables.len(), "tables": tables }))
        }
        Op::Columns(table, schema) => {
            if let Err(f) = wait_ready(&pane, true, cx).await {
                return fail_with(f);
            }
            let catalog = pane.read_with(cx, |p, _| p.agent_catalog().unwrap_or_default());
            let Some(found) = find_table(&catalog, &table, schema.as_deref()) else {
                return success(&note, json!({ "table": table, "count": 0, "columns": [], "output": "(no columns)", "message": "no such table in the pane's schema (run: tables)" }));
            };
            let cols: Vec<Json> = found
                .columns
                .iter()
                .map(|c| json!({ "column": c.name, "dataType": c.type_name, "nullable": c.nullable, "primaryKey": found.key.contains(&c.name) }))
                .collect();
            let rows: Vec<Vec<String>> = found
                .columns
                .iter()
                .map(|c| vec![c.name.clone(), c.type_name.clone(), c.nullable.to_string(), found.key.contains(&c.name).to_string()])
                .map(|r| r.into_iter().map(|t| cell_text(&Value::Text(t))).collect())
                .collect();
            let header: Vec<String> = ["column", "type", "nullable", "pk"].map(String::from).to_vec();
            let output = render_text_table(&header, &rows, rows.len());
            success(&note, json!({ "table": table, "count": cols.len(), "columns": cols, "output": output }))
        }
        Op::Ddl(table, schema) => {
            if let Err(f) = wait_ready(&pane, true, cx).await {
                return fail_with(or_code(f, "ddl_failed"));
            }
            let catalog = pane.read_with(cx, |p, _| p.agent_catalog().unwrap_or_default());
            let target = match find_table(&catalog, &table, schema.as_deref()) {
                // Unqualified in the default schema, as the explorer asks for it.
                Some(f) => TableRef::new((catalog.search_path.first() != Some(&f.schema)).then_some(f.schema.as_str()), &f.name),
                None => TableRef::new(schema.as_deref(), &table),
            };
            let Some(fut) = pane.read_with(cx, |p, _| p.agent_ddl(target)) else {
                return fail_with(("ddl_failed", "not connected".into()));
            };
            match fut.await {
                Ok(ddl) => success(&note, json!({ "table": table, "ddl": ddl })),
                Err(e) => fail_with(("ddl_failed", format!("{e:#}"))),
            }
        }
        Op::Query(sql) => {
            if let Err(f) = wait_ready(&pane, false, cx).await {
                return fail_with(or_code(f, "query_failed"));
            }
            let d = pane.read_with(cx, |p, _| p.agent_dialect());
            if !write_allowed && batch_access(&sql, d) == Access::Write {
                return fail_with(("write_blocked", WRITE_BLOCKED.into()));
            }
            let split = sql_lang::split(&sql, d);
            let ddl = split.iter().any(|s| s.kind == StatementKind::Ddl);
            let stmts: Vec<String> = split.into_iter().map(|s| s.text).collect();
            if stmts.is_empty() {
                return fail_with(("missing_sql", "usage: query <sql>  (only comments were given)".into()));
            }
            let rx = match pane.update(cx, |p, cx| p.agent_run(stmts, false, cx)) {
                Ok(rx) => rx,
                Err(e) => return fail_with(("query_failed", e)),
            };
            match await_run(rx, cx).await {
                Ok(r) => {
                    let mut out = json!({
                        "rowCount": row_count(&r),
                        "durationMs": r.ms as u64,
                        "columns": r.columns,
                        "command": command_of(&r.sql),
                        "output": result_table(&r),
                    });
                    if r.capped {
                        out["truncated"] = true.into();
                    }
                    if ddl {
                        refresh_catalog(&pane, cx).await;
                    }
                    success(&note, out)
                }
                Err(f) => fail_with(or_code(f, "query_failed")),
            }
        }
        Op::Explain(sql) => {
            if let Err(f) = wait_ready(&pane, false, cx).await {
                return fail_with(or_code(f, "explain_failed"));
            }
            let d = pane.read_with(cx, |p, _| p.agent_dialect());
            let stmts = sql_lang::split(&sql, d);
            let stmt = match stmts.as_slice() {
                [one] => one.text.clone(),
                [] => return fail_with(("missing_sql", "usage: explain <sql>".into())),
                _ => return fail_with(("explain_failed", "explain takes one statement".into())),
            };
            let Some(query) = sql_lang::explain_query(&stmt, d, false) else {
                return fail_with(("explain_failed", format!("EXPLAIN isn't available for {}", d.label())));
            };
            // `explain "EXPLAIN ANALYZE …"` would run it.
            if !write_allowed && batch_access(&query, d) == Access::Write {
                return fail_with(("write_blocked", WRITE_BLOCKED.into()));
            }
            let rx = match pane.update(cx, |p, cx| p.agent_run(vec![query], true, cx)) {
                Ok(rx) => rx,
                Err(e) => return fail_with(("explain_failed", e)),
            };
            match await_run(rx, cx).await {
                Ok(r) => {
                    let mut out = json!({ "rowCount": row_count(&r), "durationMs": r.ms as u64, "columns": r.columns, "output": result_table(&r) });
                    // A plan in one cell (Postgres / MySQL JSON): in full, the table clips it.
                    if let [row] = r.rows.as_slice()
                        && let [cell] = row.as_slice()
                    {
                        out["plan"] = full_text(cell).into();
                    }
                    success(&note, out)
                }
                Err(f) => fail_with(or_code(f, "explain_failed")),
            }
        }
        Op::Export { table, format, path, schema } => {
            if let Err(f) = wait_ready(&pane, true, cx).await {
                return fail_with(or_code(f, "export_failed"));
            }
            let (catalog, d) = pane.read_with(cx, |p, _| (p.agent_catalog().unwrap_or_default(), p.agent_dialect()));
            let (sch, name, key) = match find_table(&catalog, &table, schema.as_deref()) {
                Some(f) => (Some(f.schema), f.name, f.key),
                None => (schema.or_else(|| current_schema(&catalog, d)), table.clone(), Vec::new()),
            };
            let target = table_sql(sch.as_deref(), &name, d);
            let mut select = format!("SELECT * FROM {target}");
            if !key.is_empty() {
                select.push_str(&format!(" ORDER BY {}", key.iter().map(|k| quote_ident(k, d)).collect::<Vec<_>>().join(", ")));
            }
            let Some((events, done)) = pane.read_with(cx, |p, _| p.agent_stream(select)) else {
                return fail_with(("export_failed", "not connected".into()));
            };
            let file = path.clone();
            let written = cx.background_executor().spawn(write_export(events, done, file, format, target, d)).await;
            match written {
                Ok(n) => success(&note, json!({ "table": table, "format": format.name(), "rowCount": n, "filePath": path.to_string_lossy() })),
                Err(e) => fail_with(("export_failed", e)),
            }
        }
        Op::Import { table, path, schema } => {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return fail_with(("import_failed", format!("{}: {e}", path.display()))),
            };
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let mut matrix = parse_csv(text, detect_delimiter(text));
            if matrix.is_empty() {
                return fail_with(("import_failed", "CSV is empty".into()));
            }
            let headers = matrix.remove(0);
            if let Err(f) = wait_ready(&pane, true, cx).await {
                return fail_with(or_code(f, "import_failed"));
            }
            let (catalog, d) = pane.read_with(cx, |p, _| (p.agent_catalog().unwrap_or_default(), p.agent_dialect()));
            let (sch, name) = match find_table(&catalog, &table, schema.as_deref()) {
                Some(f) => (Some(f.schema), f.name),
                None => (schema.or_else(|| current_schema(&catalog, d)), table.clone()),
            };
            let stmts = insert_batches(&table_sql(sch.as_deref(), &name, d), &headers, &matrix, d);
            if stmts.is_empty() {
                return success(&note, json!({ "table": table, "imported": 0 }));
            }
            let (rx, committed) = match pane.update(cx, |p, cx| p.agent_insert(stmts, cx)) {
                Ok(v) => v,
                Err(e) => return fail_with(("import_failed", e)),
            };
            match await_run(rx, cx).await {
                Ok(_) => {
                    let reload = TableRef::new(sch.as_deref(), &name);
                    pane.update(cx, |p, cx| p.agent_reload_table(&reload, cx));
                    let mut out = json!({ "table": table, "imported": matrix.len() });
                    if !committed {
                        out["committed"] = false.into();
                        out["note"] = "the pane is in Manual commit mode: the rows are in its open transaction until the user commits".into();
                    }
                    success(&note, out)
                }
                Err(f) => fail_with(or_code(f, "import_failed")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(sql: &str) -> Access {
        batch_access(sql, Dialect::Postgres)
    }

    #[test]
    fn reads_and_writes_like_electron() {
        for sql in [
            "select * from users",
            "-- note\nSELECT 1",
            "/* x */ with t as (select 1) select * from t",
            "show tables",
            "explain select 1",
            "describe users",
            "values (1)",
            "pragma table_info(users)",
            "select updated_at, last_update from t",
            "",
        ] {
            assert_eq!(class(sql), Access::Read, "{sql}");
        }
        for sql in [
            "insert into t values (1)",
            "update t set a = 1",
            "delete from t",
            "create table x (a int)",
            "drop table x",
            "truncate t",
            "grant select on t to u",
            "call proc()",
            "explain analyze select 1",
            "EXPLAIN (ANALYZE, BUFFERS) select 1",
            "select * into newt from t",
            "select * from t for update",
            "select * from t for share",
            "with d as (delete from t returning *) select * from d",
            "begin",
            "commit",
            "set search_path = x",
            "vacuum",
            "pragma user_version = 3",
            "frobnicate the db",
            "select 1; delete from t",
        ] {
            assert_eq!(class(sql), Access::Write, "{sql}");
        }
    }

    #[test]
    fn text_tables_cap_and_clip() {
        let cols: Vec<String> = vec!["id".into(), "name".into()];
        let rows = vec![vec!["1".into(), "ann".into()], vec!["22".into(), "bo".into()]];
        assert_eq!(
            render_text_table(&cols, &rows, 5),
            "----+------\n| id | name |\n====+======\n| 1  | ann  |\n| 22 | bo   |\n----+------\n… 3 more rows (showing 2 of 5)"
        );
        assert_eq!(render_text_table(&[], &[], 0), "(no columns)");
        let long = cell_text(&Value::Text("x".repeat(50)));
        assert_eq!(long.chars().count(), MAX_CELL);
        assert!(long.ends_with('…'));
        assert_eq!(cell_text(&Value::Text("a\nb".into())), "a b");
        assert_eq!(cell_text(&Value::Null), "NULL");
    }

    fn export(format: Format, rows: &[Vec<Value>]) -> String {
        let mut sink = TextSink::new(Vec::new(), format, table_sql(Some("public"), "t", Dialect::Postgres), Dialect::Postgres);
        sink.begin(vec!["id".into(), "note".into()]).unwrap();
        for r in rows {
            sink.write(r).unwrap();
        }
        String::from_utf8(sink.finish().unwrap()).unwrap()
    }

    #[test]
    fn export_formats() {
        let rows = vec![vec![Value::Int(1), Value::Text("a,\"b\"".into())], vec![Value::Int(2), Value::Null]];
        assert_eq!(export(Format::Csv, &rows), "id,note\r\n1,\"a,\"\"b\"\"\"\r\n2,");
        assert_eq!(export(Format::Json, &rows), "[\n  {\"id\":1,\"note\":\"a,\\\"b\\\"\"},\n  {\"id\":2,\"note\":null}\n]\n");
        assert_eq!(export(Format::Json, &[]), "[]\n");
        assert_eq!(
            export(Format::Sql, &rows),
            "INSERT INTO \"public\".\"t\" (\"id\", \"note\") VALUES (1, 'a,\"b\"');\nINSERT INTO \"public\".\"t\" (\"id\", \"note\") VALUES (2, NULL);\n"
        );
        assert_eq!(table_sql(Some("main"), "t", Dialect::Sqlite), "\"t\"");
        assert_eq!(table_sql(Some("db"), "a`b", Dialect::Mysql), "`a``b`");
        assert_eq!(sql_literal(&Value::Text("it's \\".into()), Dialect::Mysql), "'it''s \\\\'");
        assert_eq!(sql_literal(&Value::Text("й".into()), Dialect::Mssql), "N'й'");
        assert_eq!(sql_literal(&Value::Bool(true), Dialect::Sqlite), "1");
        assert_eq!(sql_literal(&Value::Float(f64::NAN), Dialect::Postgres), "NULL");
        assert_eq!(json_value(&Value::Json(json!({"a": [1]}))), json!({"a": [1]}));
        assert_eq!(full_text(&Value::Bytes(vec![0xde, 0xad])), "0xdead");
    }

    #[test]
    fn xlsx_export_writes_a_workbook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.xlsx");
        let mut sink = Sink::open(&path, Format::Xlsx, String::new(), Dialect::Sqlite).unwrap();
        sink.begin(vec!["id".into(), "name".into()]).unwrap();
        sink.write(&[Value::Int(1), Value::Text("x".into())]).unwrap();
        sink.write(&[Value::Null, Value::Bool(true)]).unwrap();
        sink.finish(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], b"PK", "a zip container");
    }

    #[test]
    fn csv_parsing() {
        assert_eq!(detect_delimiter("a;b;c\n1;2;3"), ';');
        assert_eq!(detect_delimiter("\"a,b\"\tc\n"), '\t');
        assert_eq!(detect_delimiter(""), ',');
        let rows = parse_csv("id,name\r\n1,\"a, \"\"b\"\"\"\n2,\"multi\nline\"\n", ',');
        assert_eq!(rows, vec![vec!["id", "name"], vec!["1", "a, \"b\""], vec!["2", "multi\nline"]]);
        assert_eq!(parse_csv("a\rb", ','), vec![vec!["a"], vec!["b"]]);
        assert!(parse_csv("", ',').is_empty());
    }

    #[test]
    fn import_batches_are_parameterised() {
        let cols: Vec<String> = vec!["id".into(), "name".into()];
        let rows = vec![vec!["1".into(), "a'b".into()], vec!["2".into()]];
        let b = insert_batches("\"t\"", &cols, &rows, Dialect::Postgres);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, "INSERT INTO \"t\" (\"id\", \"name\") VALUES ($1, $2), ($3, $4)");
        assert_eq!(b[0].1, vec![Value::Text("1".into()), Value::Text("a'b".into()), Value::Text("2".into()), Value::Text(String::new())]);
        assert_eq!(insert_batches("[t]", &cols, &rows, Dialect::Mssql)[0].0, "INSERT INTO [t] ([id], [name]) VALUES (@P1, @P2), (@P3, @P4)");
        // SQL Server's 2100 parameters: 1050 two-column rows per statement at most, and 200 by default.
        let many: Vec<Vec<String>> = (0..450).map(|i| vec![i.to_string(), "x".into()]).collect();
        assert_eq!(insert_batches("t", &cols, &many, Dialect::Sqlite).len(), 3);
        let wide: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
        let wide_rows: Vec<Vec<String>> = (0..100).map(|_| vec!["v".into(); 30]).collect();
        let w = insert_batches("t", &wide, &wide_rows, Dialect::Mssql);
        assert!(w.iter().all(|(_, p)| p.len() <= 2100), "70 rows × 30 columns per statement");
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn schema_lookups() {
        use sql_core::{ColumnInfo, Schema, Table};
        let t = |n: &str, kind| Table { name: n.into(), kind, columns: vec![ColumnInfo { name: "id".into(), type_name: "int".into(), ..Default::default() }], primary_key: vec!["id".into()], ..Default::default() };
        let cat = Catalog {
            database: "d".into(),
            search_path: vec!["public".into()],
            schemas: vec![
                Schema { name: "public".into(), tables: vec![t("users", TableKind::Table), t("v", TableKind::View)], ..Default::default() },
                Schema { name: "audit".into(), tables: vec![t("log", TableKind::Table)], ..Default::default() },
            ],
        };
        let names = |v: Vec<Json>| v.iter().map(|t| format!("{}.{}:{}", t["schema"].as_str().unwrap(), t["name"].as_str().unwrap(), t["kind"].as_str().unwrap())).collect::<Vec<_>>();
        assert_eq!(names(list_tables(&cat, None, Dialect::Postgres)), ["public.users:table", "public.v:view"]);
        assert_eq!(names(list_tables(&cat, Some("audit"), Dialect::Postgres)), ["audit.log:table"]);
        assert_eq!(list_tables(&cat, None, Dialect::Sqlite).len(), 3, "no schemas to pick: all of them");
        assert_eq!(find_table(&cat, "log", None).unwrap().schema, "audit");
        assert_eq!(find_table(&cat, "USERS", None).unwrap().name, "users");
        assert!(find_table(&cat, "log", Some("public")).is_none());
    }

    #[test]
    fn env_and_gate_follow_the_switches() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::new(19568, dir.path());
        let settings = |json: &str| std::fs::write(dir.path().join("app-settings.json"), json).unwrap();
        let env = || {
            let mut e = HashMap::new();
            add_env(&cfg, &mut e);
            e
        };
        assert!(env().is_empty(), "no settings: off");
        settings(r#"{"agentSqlControl":true,"agentSqlWrite":true}"#);
        assert_eq!((switches(&cfg), env().len()), ((false, false), 0), "the master switch gates it");
        settings(r#"{"agentIntegrations":true,"agentSqlControl":true}"#);
        let e = env();
        assert_eq!(e["SPLITGRID_SQL_ENDPOINT"], "http://127.0.0.1:19568/sql");
        assert!(e["SPLITGRID_SQL_CLI"].ends_with("splitgrid-sql.sh"));
        assert_eq!(e["SPLITGRID_SQL_TOKEN"], cfg.token);
        assert!(!e.contains_key("SPLITGRID_SQL_WRITE"));
        settings(r#"{"agentIntegrations":true,"agentSqlControl":true,"agentSqlWrite":true}"#);
        assert_eq!(env()["SPLITGRID_SQL_WRITE"], "1");
        assert_eq!(switches(&cfg), (true, true));

        let argv = vec!["list".to_owned()];
        assert_eq!(gate(false, &argv).unwrap_err()["error"], "sql_disabled");
        assert_eq!(gate(true, &[]).unwrap_err()["error"], "empty_command");
        assert!(gate(true, &argv).is_ok());
    }

    #[test]
    fn reply_shapes() {
        let mut note = Map::new();
        note.insert("surface".into(), "c-1".into());
        assert_eq!(failure(&note, ("write_blocked", "no".into())), json!({ "ok": false, "surface": "c-1", "message": "no", "error": "write_blocked" }));
        assert_eq!(success(&note, json!({ "count": 2 })), json!({ "ok": true, "surface": "c-1", "count": 2 }));
        assert_eq!(or_code(("", "x".into()), "query_failed").0, "query_failed");
        assert_eq!(or_code(("timeout", "x".into()), "query_failed").0, "timeout");
        let r = AgentResult { sql: " select 1".into(), columns: vec![], affected: Some(3), ..Default::default() };
        assert_eq!((row_count(&r), command_of(&r.sql)), (3, "SELECT".into()));
        assert_eq!(resolve_path("out.csv", Some("/tmp/x")), PathBuf::from("/tmp/x/out.csv"));
        assert_eq!(resolve_path("/abs.csv", Some("/tmp/x")), PathBuf::from("/abs.csv"));
    }
}
