//! The database explorer (ConnectionTree.tsx, DataGrip's Database Explorer):
//! every saved connection → its databases → schemas → Tables / Views /
//! Routines / Sequences → tables → columns, indexes, foreign keys. What is
//! loaded lives in a `Forest`; `TreeState` (expanded nodes, selection, filter)
//! flattens it into the rows the virtual list draws, and moves through them
//! from the keyboard. Plain data, no gpui: the pane does the loading.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::UniformListScrollHandle;
use sql_core::{Catalog, ColumnInfo, Routine, Schema, Table, TableKind, TableRef};

/// Joins the parts of a node key (names may hold any printable character).
const SEP: char = '\u{1f}';

pub fn key(parts: &[&str]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(SEP);
        }
        out.push_str(p);
    }
    out
}

pub fn conn_key(id: &str) -> String {
    key(&["c", id])
}

pub fn db_key(id: &str, db: &str) -> String {
    key(&["d", id, db])
}

/// The parts of a key: its kind first.
pub fn parts(k: &str) -> Vec<&str> {
    k.split(SEP).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Load<T> {
    Loading,
    Failed(String),
    Ready(T),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnStatus {
    Idle,
    Connecting,
    Failed(String),
    Open,
}

/// One database of an open connection.
#[derive(Clone, Debug, PartialEq)]
pub enum DbState {
    Connecting,
    Introspecting,
    Ready { catalog: Arc<Catalog>, refreshing: bool },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnEntry {
    pub id: String,
    pub label: String,
    pub dialect: String,
    pub address: String,
    /// SQLite: no database level, the catalog hangs off the connection.
    pub flat: bool,
    pub status: ConnStatus,
    /// The database the connection's own session is on.
    pub default_db: String,
    pub databases: Load<Vec<String>>,
    pub dbs: HashMap<String, DbState>,
}

impl ConnEntry {
    pub fn new(id: &str, label: &str, dialect: &str, address: &str, flat: bool, default_db: &str) -> Self {
        ConnEntry {
            id: id.into(),
            label: label.into(),
            dialect: dialect.into(),
            address: address.into(),
            flat,
            status: ConnStatus::Idle,
            default_db: default_db.into(),
            databases: Load::Loading,
            dbs: HashMap::new(),
        }
    }

    pub fn catalog(&self, db: &str) -> Option<&Arc<Catalog>> {
        match self.dbs.get(db) {
            Some(DbState::Ready { catalog, .. }) => Some(catalog),
            _ => None,
        }
    }

    /// The databases the tree lists: the server's, with the default one first
    /// when the server didn't list it (or listing failed).
    pub fn listed(&self) -> Vec<String> {
        let mut out = match &self.databases {
            Load::Ready(list) => list.clone(),
            _ => Vec::new(),
        };
        if !self.default_db.is_empty() && !out.contains(&self.default_db) {
            out.insert(0, self.default_db.clone());
        }
        out
    }

    pub fn reset(&mut self) {
        self.status = ConnStatus::Idle;
        self.databases = Load::Loading;
        self.dbs.clear();
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Forest {
    pub conns: Vec<ConnEntry>,
}

impl Forest {
    pub fn conn(&self, id: &str) -> Option<&ConnEntry> {
        self.conns.iter().find(|c| c.id == id)
    }

    pub fn conn_mut(&mut self, id: &str) -> Option<&mut ConnEntry> {
        self.conns.iter_mut().find(|c| c.id == id)
    }

    pub fn catalog(&self, id: &str, db: &str) -> Option<&Arc<Catalog>> {
        self.conn(id)?.catalog(db)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Tables,
    Views,
    Routines,
    Sequences,
}

impl Group {
    fn key(self) -> &'static str {
        match self {
            Group::Tables => "tables",
            Group::Views => "views",
            Group::Routines => "routines",
            Group::Sequences => "sequences",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Group::Tables => "Tables",
            Group::Views => "Views",
            Group::Routines => "Routines",
            Group::Sequences => "Sequences",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Folder {
    Indexes,
    ForeignKeys,
}

impl Folder {
    pub fn label(self) -> &'static str {
        match self {
            Folder::Indexes => "Indexes",
            Folder::ForeignKeys => "Foreign keys",
        }
    }
}

/// Where a node's objects live: a connection and one of its databases.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    pub conn: String,
    pub db: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Conn { id: String, label: String, dialect: String, address: String, status: ConnStatus, active: bool },
    Database { scope: Scope, default: bool, active: bool, loading: bool },
    Schema { scope: Scope, name: String, tables: usize, default: bool },
    Group { group: Group, count: usize },
    Table { scope: Scope, table: TableRef, kind: TableKind, columns: usize, rows: Option<u64>, comment: Option<String> },
    Column { column: ColumnInfo, pk: bool, fk: bool },
    Folder { folder: Folder, count: usize },
    Index { name: String, columns: Vec<String>, unique: bool },
    ForeignKey { name: String, columns: Vec<String>, target: String },
    Routine(Routine),
    Sequence(String),
    /// Connecting…, an error (with what Retry does), "Not connected".
    Message { text: String, error: bool, busy: bool, retry: Option<(String, Option<String>)> },
}

impl Node {
    /// The name type-to-search matches.
    pub fn name(&self) -> &str {
        match self {
            Node::Conn { label, .. } => label,
            Node::Database { scope, .. } => &scope.db,
            Node::Schema { name, .. } | Node::Index { name, .. } | Node::ForeignKey { name, .. } | Node::Sequence(name) => name,
            Node::Group { group, .. } => group.label(),
            Node::Table { table, .. } => &table.name,
            Node::Column { column, .. } => &column.name,
            Node::Folder { folder, .. } => folder.label(),
            Node::Routine(r) => &r.name,
            Node::Message { .. } => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub key: String,
    pub depth: usize,
    pub node: Node,
    pub expandable: bool,
    pub expanded: bool,
}

/// Type-to-search: the typed prefix, forgotten after a second of quiet.
#[derive(Default)]
pub struct TypeAhead {
    prefix: String,
    at: Option<Instant>,
}

const TYPE_AHEAD_RESET: Duration = Duration::from_millis(1000);

#[derive(Default)]
pub struct TreeState {
    pub expanded: HashSet<String>,
    pub selected: Option<String>,
    pub filter: String,
    pub scroll: UniformListScrollHandle,
    pub type_ahead: TypeAhead,
}

/// What a key press in the tree asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Select(String),
    Expand(String),
    Collapse(String),
    /// Enter / double-click: the node's default action.
    Activate(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavKey {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Space,
}

impl NavKey {
    pub fn parse(key: &str) -> Option<NavKey> {
        Some(match key {
            "up" => NavKey::Up,
            "down" => NavKey::Down,
            "left" => NavKey::Left,
            "right" => NavKey::Right,
            "home" => NavKey::Home,
            "end" => NavKey::End,
            "pageup" => NavKey::PageUp,
            "pagedown" => NavKey::PageDown,
            "enter" => NavKey::Enter,
            "space" => NavKey::Space,
            _ => return None,
        })
    }
}

/// How a table of `schema` is referred to: unqualified in the default schema.
pub fn table_ref(catalog: &Catalog, schema: &str, table: &str) -> TableRef {
    let default = catalog.search_path.first().map(String::as_str);
    TableRef::new((default != Some(schema)).then_some(schema), table)
}

fn is_view(t: &Table) -> bool {
    matches!(t.kind, TableKind::View | TableKind::MaterializedView)
}

/// The filter, lowercased; `matches` with an empty one is always true.
struct Filter(String);

impl Filter {
    fn on(&self) -> bool {
        !self.0.is_empty()
    }

    fn matches(&self, name: &str) -> bool {
        self.0.is_empty() || name.to_lowercase().contains(&self.0)
    }

    fn schema_has(&self, s: &Schema) -> bool {
        s.tables.iter().any(|t| self.matches(&t.name)) || s.routines.iter().any(|r| self.matches(&r.name)) || s.sequences.iter().any(|q| self.matches(q))
    }

    fn catalog_has(&self, c: &Catalog) -> bool {
        self.on() && c.schemas.iter().any(|s| self.schema_has(s))
    }
}

impl TreeState {
    #[cfg(test)]
    pub fn toggle(&mut self, key: &str) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
        }
    }

    pub fn expand(&mut self, key: String) {
        self.expanded.insert(key);
    }

    /// A catalog seen for the first time in its scope: open the default schema and its tables.
    pub fn on_catalog(&mut self, conn: &str, db: &str, catalog: &Catalog) {
        let prefixes = [key(&["s", conn, db, ""]), key(&["g", conn, db, ""])];
        if self.expanded.iter().any(|k| prefixes.iter().any(|p| k.starts_with(p.as_str()))) {
            return;
        }
        let first = catalog.search_path.iter().find(|s| catalog.schema(s).is_some()).or(catalog.schemas.first().map(|s| &s.name));
        if let Some(s) = first {
            self.expanded.insert(key(&["s", conn, db, s]));
            self.expanded.insert(key(&["g", conn, db, s, Group::Tables.key()]));
        }
    }

    /// Forget the expansion of nodes inside a connection (it was deleted).
    pub fn forget(&mut self, conn: &str) {
        self.expanded.retain(|k| parts(k).get(1) != Some(&conn));
    }

    /// The rows to draw. `active`: the connection and database the console runs on.
    pub fn rows(&self, forest: &Forest, active: Option<(&str, &str)>) -> Vec<TreeRow> {
        let f = Filter(self.filter.trim().to_lowercase());
        let mut out = Vec::new();
        for c in &forest.conns {
            let has_match = f.on() && c.dbs.values().any(|d| matches!(d, DbState::Ready { catalog, .. } if f.catalog_has(catalog)));
            if f.on() && !has_match && !f.matches(&c.label) {
                continue;
            }
            let ckey = conn_key(&c.id);
            let open = self.expanded.contains(&ckey) || has_match;
            let is_active = active.is_some_and(|(id, _)| id == c.id);
            out.push(TreeRow {
                key: ckey.clone(),
                depth: 0,
                node: Node::Conn { id: c.id.clone(), label: c.label.clone(), dialect: c.dialect.clone(), address: c.address.clone(), status: c.status.clone(), active: is_active },
                expandable: true,
                expanded: open,
            });
            if !open {
                continue;
            }
            let msg = |text: &str, error: bool, busy: bool, retry: Option<(String, Option<String>)>| TreeRow {
                key: key(&["m", &ckey]),
                depth: 1,
                node: Node::Message { text: text.into(), error, busy, retry },
                expandable: false,
                expanded: false,
            };
            match &c.status {
                ConnStatus::Idle => out.push(msg("Not connected — Enter to connect", false, false, Some((c.id.clone(), None)))),
                ConnStatus::Connecting => out.push(msg("Connecting…", false, true, None)),
                ConnStatus::Failed(e) => out.push(msg(e, true, false, Some((c.id.clone(), None)))),
                ConnStatus::Open if c.flat => self.db_children(c, &c.default_db, 1, &f, &mut out),
                ConnStatus::Open => self.databases(c, active, &f, &mut out),
            }
        }
        out
    }

    fn databases(&self, c: &ConnEntry, active: Option<(&str, &str)>, f: &Filter, out: &mut Vec<TreeRow>) {
        if c.databases == Load::Loading && c.default_db.is_empty() {
            out.push(TreeRow { key: key(&["m", &conn_key(&c.id)]), depth: 1, node: Node::Message { text: "Loading…".into(), error: false, busy: true, retry: None }, expandable: false, expanded: false });
            return;
        }
        if let (Load::Failed(e), true) = (&c.databases, c.default_db.is_empty()) {
            out.push(TreeRow { key: key(&["m", &conn_key(&c.id)]), depth: 1, node: Node::Message { text: e.clone(), error: true, busy: false, retry: None }, expandable: false, expanded: false });
            return;
        }
        for db in c.listed() {
            let state = c.dbs.get(&db);
            let has_match = matches!(state, Some(DbState::Ready { catalog, .. }) if f.catalog_has(catalog));
            if f.on() && !has_match {
                continue;
            }
            let dkey = db_key(&c.id, &db);
            let open = self.expanded.contains(&dkey) || has_match;
            let scope = Scope { conn: c.id.clone(), db: db.clone() };
            let loading = matches!(state, Some(DbState::Connecting | DbState::Introspecting | DbState::Ready { refreshing: true, .. }));
            let is_active = active == Some((c.id.as_str(), db.as_str()));
            out.push(TreeRow { key: dkey, depth: 1, node: Node::Database { scope, default: db == c.default_db, active: is_active, loading }, expandable: true, expanded: open });
            if open {
                self.db_children(c, &db, 2, f, out);
            }
        }
    }

    fn db_children(&self, c: &ConnEntry, db: &str, depth: usize, f: &Filter, out: &mut Vec<TreeRow>) {
        let msg = |text: &str, error: bool, busy: bool, retry: Option<(String, Option<String>)>| TreeRow {
            key: key(&["m", &db_key(&c.id, db)]),
            depth,
            node: Node::Message { text: text.into(), error, busy, retry },
            expandable: false,
            expanded: false,
        };
        let retry = Some((c.id.clone(), Some(db.to_owned())));
        match c.dbs.get(db) {
            None => out.push(msg("Not connected — Enter to connect", false, false, retry)),
            Some(DbState::Connecting) => out.push(msg("Connecting…", false, true, None)),
            Some(DbState::Introspecting) => out.push(msg("Loading schema…", false, true, None)),
            Some(DbState::Failed(e)) => out.push(msg(e, true, false, retry)),
            Some(DbState::Ready { catalog, .. }) => {
                let scope = Scope { conn: c.id.clone(), db: db.to_owned() };
                self.catalog(&scope, catalog, depth, f, out);
            }
        }
    }

    fn catalog(&self, scope: &Scope, catalog: &Catalog, depth: usize, f: &Filter, out: &mut Vec<TreeRow>) {
        // One schema (SQLite's main, a MySQL database): no level for it.
        let flat = catalog.schemas.len() == 1;
        let default = catalog.search_path.first();
        for schema in &catalog.schemas {
            if f.on() && !f.schema_has(schema) {
                continue;
            }
            let base = if flat {
                depth
            } else {
                let skey = key(&["s", &scope.conn, &scope.db, &schema.name]);
                let open = self.expanded.contains(&skey) || f.on();
                out.push(TreeRow {
                    key: skey,
                    depth,
                    node: Node::Schema { scope: scope.clone(), name: schema.name.clone(), tables: schema.tables.len(), default: default == Some(&schema.name) },
                    expandable: true,
                    expanded: open,
                });
                if !open {
                    continue;
                }
                depth + 1
            };
            self.groups(scope, catalog, schema, base, f, out);
        }
    }

    fn groups(&self, scope: &Scope, catalog: &Catalog, schema: &Schema, depth: usize, f: &Filter, out: &mut Vec<TreeRow>) {
        let tables: Vec<&Table> = schema.tables.iter().filter(|t| !is_view(t) && f.matches(&t.name)).collect();
        let views: Vec<&Table> = schema.tables.iter().filter(|t| is_view(t) && f.matches(&t.name)).collect();
        let routines: Vec<&Routine> = schema.routines.iter().filter(|r| f.matches(&r.name)).collect();
        let sequences: Vec<&String> = schema.sequences.iter().filter(|s| f.matches(s)).collect();
        for (group, count) in [(Group::Tables, tables.len()), (Group::Views, views.len()), (Group::Routines, routines.len()), (Group::Sequences, sequences.len())] {
            if count == 0 && (group != Group::Tables || f.on()) {
                continue;
            }
            let gkey = key(&["g", &scope.conn, &scope.db, &schema.name, group.key()]);
            let open = count > 0 && (self.expanded.contains(&gkey) || f.on());
            out.push(TreeRow { key: gkey, depth, node: Node::Group { group, count }, expandable: count > 0, expanded: open });
            if !open {
                continue;
            }
            match group {
                Group::Tables => self.tables(scope, catalog, schema, &tables, depth + 1, out),
                Group::Views => self.tables(scope, catalog, schema, &views, depth + 1, out),
                Group::Routines => {
                    for r in &routines {
                        out.push(TreeRow {
                            key: key(&["r", &scope.conn, &scope.db, &schema.name, &format!("{}{}", r.name, r.signature)]),
                            depth: depth + 1,
                            node: Node::Routine((*r).clone()),
                            expandable: false,
                            expanded: false,
                        });
                    }
                }
                Group::Sequences => {
                    for s in &sequences {
                        out.push(TreeRow { key: key(&["q", &scope.conn, &scope.db, &schema.name, s]), depth: depth + 1, node: Node::Sequence((*s).clone()), expandable: false, expanded: false });
                    }
                }
            }
        }
    }

    fn tables(&self, scope: &Scope, catalog: &Catalog, schema: &Schema, tables: &[&Table], depth: usize, out: &mut Vec<TreeRow>) {
        for t in tables {
            let tkey = key(&["t", &scope.conn, &scope.db, &schema.name, &t.name]);
            let open = self.expanded.contains(&tkey);
            out.push(TreeRow {
                key: tkey.clone(),
                depth,
                node: Node::Table { scope: scope.clone(), table: table_ref(catalog, &schema.name, &t.name), kind: t.kind, columns: t.columns.len(), rows: t.row_estimate, comment: t.comment.clone() },
                expandable: !t.columns.is_empty() || !t.indexes.is_empty() || !t.foreign_keys.is_empty(),
                expanded: open,
            });
            if !open {
                continue;
            }
            for c in &t.columns {
                let pk = t.primary_key.contains(&c.name);
                let fk = t.foreign_keys.iter().any(|f| f.columns.contains(&c.name));
                out.push(TreeRow { key: format!("{tkey}{SEP}col{SEP}{}", c.name), depth: depth + 1, node: Node::Column { column: c.clone(), pk, fk }, expandable: false, expanded: false });
            }
            for (folder, count) in [(Folder::Indexes, t.indexes.len()), (Folder::ForeignKeys, t.foreign_keys.len())] {
                if count == 0 {
                    continue;
                }
                let fkey = format!("{tkey}{SEP}{}", if folder == Folder::Indexes { "ix" } else { "fk" });
                let open = self.expanded.contains(&fkey);
                out.push(TreeRow { key: fkey.clone(), depth: depth + 1, node: Node::Folder { folder, count }, expandable: true, expanded: open });
                if !open {
                    continue;
                }
                match folder {
                    Folder::Indexes => {
                        for ix in &t.indexes {
                            out.push(TreeRow {
                                key: format!("{fkey}{SEP}{}", ix.name),
                                depth: depth + 2,
                                node: Node::Index { name: ix.name.clone(), columns: ix.columns.clone(), unique: ix.unique },
                                expandable: false,
                                expanded: false,
                            });
                        }
                    }
                    Folder::ForeignKeys => {
                        for (i, fk) in t.foreign_keys.iter().enumerate() {
                            let name = if fk.name.is_empty() { format!("fk_{}", fk.columns.join("_")) } else { fk.name.clone() };
                            let target = format!("{}({})", sql_qualified(&fk.references), fk.ref_columns.join(", "));
                            out.push(TreeRow {
                                key: format!("{fkey}{SEP}{i}"),
                                depth: depth + 2,
                                node: Node::ForeignKey { name, columns: fk.columns.clone(), target },
                                expandable: false,
                                expanded: false,
                            });
                        }
                    }
                }
            }
        }
    }
}

fn sql_qualified(t: &TableRef) -> String {
    match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn position(rows: &[TreeRow], selected: Option<&str>) -> Option<usize> {
    let sel = selected?;
    rows.iter().position(|r| r.key == sel)
}

/// The row a key press moves to, or what it asks for.
pub fn nav(rows: &[TreeRow], selected: Option<&str>, key: NavKey, page: usize) -> Option<Cmd> {
    if rows.is_empty() {
        return None;
    }
    let select = |i: usize| Some(Cmd::Select(rows[i].key.clone()));
    let Some(i) = position(rows, selected) else {
        // Nothing (visible) selected: the first arrow press picks an end.
        return match key {
            NavKey::Up | NavKey::End | NavKey::PageUp => select(rows.len() - 1),
            NavKey::Down | NavKey::Home | NavKey::PageDown | NavKey::Right | NavKey::Left => select(0),
            NavKey::Enter | NavKey::Space => None,
        };
    };
    let row = &rows[i];
    match key {
        NavKey::Up => select(i.saturating_sub(1)),
        NavKey::Down => select((i + 1).min(rows.len() - 1)),
        NavKey::Home => select(0),
        NavKey::End => select(rows.len() - 1),
        NavKey::PageUp => select(i.saturating_sub(page.max(1))),
        NavKey::PageDown => select((i + page.max(1)).min(rows.len() - 1)),
        NavKey::Right if row.expandable && !row.expanded => Some(Cmd::Expand(row.key.clone())),
        NavKey::Right => match rows.get(i + 1) {
            Some(next) if row.expanded && next.depth > row.depth => select(i + 1),
            _ => None,
        },
        NavKey::Left if row.expandable && row.expanded => Some(Cmd::Collapse(row.key.clone())),
        NavKey::Left => (0..i).rev().find(|&j| rows[j].depth < row.depth).and_then(select),
        NavKey::Space if row.expandable => Some(if row.expanded { Cmd::Collapse(row.key.clone()) } else { Cmd::Expand(row.key.clone()) }),
        NavKey::Space => None,
        NavKey::Enter => Some(Cmd::Activate(row.key.clone())),
    }
}

impl TypeAhead {
    /// Whether a search is being typed (so a space belongs to it).
    pub fn active(&self, now: Instant) -> bool {
        !self.prefix.is_empty() && self.at.is_some_and(|at| now.duration_since(at) <= TYPE_AHEAD_RESET)
    }

    /// A typed character: the next row whose name starts with the prefix typed so far.
    pub fn push(&mut self, ch: &str, now: Instant, rows: &[TreeRow], selected: Option<&str>) -> Option<String> {
        if self.at.is_none_or(|at| now.duration_since(at) > TYPE_AHEAD_RESET) {
            self.prefix.clear();
        }
        self.at = Some(now);
        self.prefix.push_str(&ch.to_lowercase());
        if rows.is_empty() {
            return None;
        }
        let cur = position(rows, selected);
        // The same letter again ("ss"): cycle through the rows starting with it.
        let mut chars = self.prefix.chars();
        let first = chars.next()?;
        let repeat = self.prefix.chars().count() > 1 && chars.all(|c| c == first);
        let find = |prefix: &str, from: usize| (0..rows.len()).map(|k| (from + k) % rows.len()).find(|&j| rows[j].node.name().to_lowercase().starts_with(prefix));
        // A longer prefix may still match the current row; a new one moves on.
        let from = match cur {
            Some(c) if self.prefix.chars().count() > 1 => c,
            Some(c) => c + 1,
            None => 0,
        };
        let hit = find(&self.prefix, from).or_else(|| repeat.then(|| find(&first.to_string(), cur.map_or(0, |c| c + 1))).flatten());
        hit.map(|j| rows[j].key.clone())
    }
}

/// "~1.2k rows" for the tree's dim estimate.
pub fn rows_estimate(n: u64) -> String {
    match n {
        0..1000 => format!("{n} rows"),
        1000..1_000_000 => format!("~{:.1}k rows", n as f64 / 1e3),
        _ => format!("~{:.1}M rows", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sql_core::{ForeignKey, Index};

    fn catalog(schemas: &[&str]) -> Catalog {
        let col = |n: &str| ColumnInfo { name: n.into(), type_name: "int".into(), ..Default::default() };
        let orders = Table {
            name: "orders".into(),
            columns: vec![col("id"), col("user_id")],
            primary_key: vec!["id".into()],
            foreign_keys: vec![ForeignKey { name: "orders_user".into(), columns: vec!["user_id".into()], references: TableRef::new(None, "users"), ref_columns: vec!["id".into()] }],
            indexes: vec![Index { name: "orders_pkey".into(), columns: vec!["id".into()], unique: true }],
            row_estimate: Some(4200),
            ..Default::default()
        };
        let users = Table { name: "users".into(), columns: vec![col("id")], primary_key: vec!["id".into()], ..Default::default() };
        let view = Table { name: "big_orders".into(), kind: TableKind::View, columns: vec![col("id")], ..Default::default() };
        Catalog {
            database: "app".into(),
            search_path: vec![schemas[0].into()],
            schemas: schemas
                .iter()
                .map(|s| Schema { name: (*s).into(), tables: vec![orders.clone(), users.clone(), view.clone()], sequences: vec!["orders_id_seq".into()], ..Default::default() })
                .collect(),
        }
    }

    /// Two connections: a Postgres one open on `app` (also listing `other`), a SQLite one idle.
    fn forest(schemas: &[&str]) -> Forest {
        let mut pg = ConnEntry::new("pg", "Prod", "postgres", "u@h/app", false, "app");
        pg.status = ConnStatus::Open;
        pg.databases = Load::Ready(vec!["app".into(), "other".into()]);
        pg.dbs.insert("app".into(), DbState::Ready { catalog: Arc::new(catalog(schemas)), refreshing: false });
        let lite = ConnEntry::new("lite", "Local", "sqlite", "~/x.db", true, "/x.db");
        Forest { conns: vec![pg, lite] }
    }

    fn keys(rows: &[TreeRow]) -> Vec<String> {
        rows.iter().map(|r| r.key.replace(SEP, "/")).collect()
    }

    #[test]
    fn every_connection_is_a_root_and_expanding_walks_down() {
        let f = forest(&["main"]);
        let mut st = TreeState::default();
        assert_eq!(keys(&st.rows(&f, None)), ["c/pg", "c/lite"], "collapsed: just the connections");

        st.expand(conn_key("pg"));
        let rows = st.rows(&f, Some(("pg", "app")));
        assert_eq!(keys(&rows), ["c/pg", "d/pg/app", "d/pg/other", "c/lite"]);
        assert!(matches!(&rows[0].node, Node::Conn { active: true, .. }));
        assert!(matches!(&rows[1].node, Node::Database { default: true, active: true, .. }));
        assert!(matches!(&rows[2].node, Node::Database { default: false, active: false, .. }));

        st.expand(db_key("pg", "app"));
        st.on_catalog("pg", "app", f.catalog("pg", "app").unwrap());
        st.expand(key(&["t", "pg", "app", "main", "orders"]));
        let rows = st.rows(&f, None);
        assert_eq!(
            keys(&rows),
            [
                "c/pg",
                "d/pg/app",
                "g/pg/app/main/tables",
                "t/pg/app/main/orders",
                "t/pg/app/main/orders/col/id",
                "t/pg/app/main/orders/col/user_id",
                "t/pg/app/main/orders/ix",
                "t/pg/app/main/orders/fk",
                "t/pg/app/main/users",
                "g/pg/app/main/views",
                "g/pg/app/main/sequences",
                "d/pg/other",
                "c/lite",
            ],
            "one schema: no schema level; empty routines hidden"
        );
        let cols: Vec<(String, bool, bool)> = rows
            .iter()
            .filter_map(|r| match &r.node {
                Node::Column { column, pk, fk } => Some((column.name.clone(), *pk, *fk)),
                _ => None,
            })
            .collect();
        assert_eq!(cols, [("id".into(), true, false), ("user_id".into(), false, true)]);
        let orders = rows.iter().find(|r| r.key.ends_with("orders")).unwrap();
        assert_eq!(orders.depth, 3);
        assert!(matches!(&orders.node, Node::Table { table, rows: Some(4200), .. } if table == &TableRef::new(None, "orders")));

        st.expand(key(&["t", "pg", "app", "main", "orders", "fk"]));
        let fk = st.rows(&f, None).into_iter().find(|r| matches!(r.node, Node::ForeignKey { .. })).unwrap();
        assert!(matches!(&fk.node, Node::ForeignKey { name, target, .. } if name == "orders_user" && target == "users(id)"));
    }

    #[test]
    fn unloaded_nodes_say_why_and_offer_a_retry() {
        let mut f = forest(&["main"]);
        let mut st = TreeState::default();
        st.expand(conn_key("lite"));
        st.expand(db_key("pg", "other"));
        st.expand(conn_key("pg"));
        let rows = st.rows(&f, None);
        let other = rows.iter().find(|r| r.key == key(&["m", &db_key("pg", "other")])).unwrap();
        assert!(matches!(&other.node, Node::Message { retry: Some((c, Some(d))), .. } if c == "pg" && d == "other"));
        let lite = rows.last().unwrap();
        assert!(matches!(&lite.node, Node::Message { retry: Some((c, None)), error: false, .. } if c == "lite"));

        f.conn_mut("lite").unwrap().status = ConnStatus::Failed("no such file".into());
        f.conn_mut("pg").unwrap().dbs.insert("other".into(), DbState::Connecting);
        let rows = st.rows(&f, None);
        assert!(matches!(&rows.last().unwrap().node, Node::Message { error: true, text, .. } if text == "no such file"));
        assert!(rows.iter().any(|r| matches!(&r.node, Node::Message { busy: true, .. })));
        assert!(rows.iter().any(|r| matches!(&r.node, Node::Database { loading: true, .. })));

        // A SQLite file has no database level.
        let lite = f.conn_mut("lite").unwrap();
        lite.status = ConnStatus::Open;
        lite.dbs.insert("/x.db".into(), DbState::Ready { catalog: Arc::new(catalog(&["main"])), refreshing: false });
        let rows = st.rows(&f, None);
        let i = rows.iter().position(|r| r.key == conn_key("lite")).unwrap();
        assert_eq!(rows[i + 1].key.replace(SEP, "/"), "g/lite//x.db/main/tables");
        assert_eq!(rows[i + 1].depth, 1);
    }

    #[test]
    fn schemas_nest_and_qualify_outside_the_search_path() {
        let f = forest(&["public", "audit"]);
        let mut st = TreeState::default();
        st.expand(conn_key("pg"));
        st.expand(db_key("pg", "app"));
        st.on_catalog("pg", "app", f.catalog("pg", "app").unwrap());
        st.toggle(&key(&["s", "pg", "app", "audit"]));
        st.toggle(&key(&["g", "pg", "app", "audit", "tables"]));
        let rows = st.rows(&f, None);
        assert!(matches!(&rows[2].node, Node::Schema { name, default: true, .. } if name == "public"));
        let audit_orders = rows.iter().find(|r| r.key == key(&["t", "pg", "app", "audit", "orders"])).unwrap();
        assert_eq!(audit_orders.depth, 4);
        assert!(matches!(&audit_orders.node, Node::Table { table, .. } if table.schema.as_deref() == Some("audit")));
        // A second catalog of the same scope doesn't re-open what the user closed.
        st.toggle(&key(&["s", "pg", "app", "public"]));
        st.on_catalog("pg", "app", f.catalog("pg", "app").unwrap());
        assert!(!st.expanded.contains(&key(&["s", "pg", "app", "public"])));
        assert_eq!(rows_estimate(999), "999 rows");
        assert_eq!(rows_estimate(4200), "~4.2k rows");
        assert_eq!(rows_estimate(3_500_000), "~3.5M rows");
    }

    #[test]
    fn the_filter_keeps_the_path_to_matches_and_opens_it() {
        let f = forest(&["public", "audit"]);
        let mut st = TreeState { filter: "USER".into(), ..Default::default() };
        let rows = st.rows(&f, None);
        assert_eq!(
            keys(&rows),
            ["c/pg", "d/pg/app", "s/pg/app/public", "g/pg/app/public/tables", "t/pg/app/public/users", "s/pg/app/audit", "g/pg/app/audit/tables", "t/pg/app/audit/users"],
            "collapsed nodes open to show matches; the idle connection doesn't match"
        );
        st.filter = "loc".into();
        assert_eq!(keys(&st.rows(&f, None)), ["c/lite"], "connections match by label");
        st.filter = "seq".into();
        assert!(keys(&st.rows(&f, None)).contains(&"q/pg/app/public/orders_id_seq".to_owned()));
        st.filter = "zzz".into();
        assert!(st.rows(&f, None).is_empty());
        st.filter = "  ".into();
        assert_eq!(st.rows(&f, None).len(), 2, "blank is no filter");
    }

    #[test]
    fn arrows_move_expand_and_climb() {
        let f = forest(&["main"]);
        let mut st = TreeState::default();
        st.expand(conn_key("pg"));
        let rows = st.rows(&f, None);
        let k = |i: usize| rows[i].key.clone();
        assert_eq!(nav(&rows, None, NavKey::Down, 10), Some(Cmd::Select(k(0))));
        assert_eq!(nav(&rows, None, NavKey::Up, 10), Some(Cmd::Select(k(3))));
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::Down, 10), Some(Cmd::Select(k(1))));
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::Up, 10), Some(Cmd::Select(k(0))), "stays at the top");
        assert_eq!(nav(&rows, Some(&k(3)), NavKey::Down, 10), Some(Cmd::Select(k(3))), "stays at the bottom");
        // → on an open node goes to its first child; on a closed one expands it.
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::Right, 10), Some(Cmd::Select(k(1))));
        assert_eq!(nav(&rows, Some(&k(1)), NavKey::Right, 10), Some(Cmd::Expand(k(1))));
        // ← on an open node collapses; on a leaf / closed node goes to the parent.
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::Left, 10), Some(Cmd::Collapse(k(0))));
        assert_eq!(nav(&rows, Some(&k(2)), NavKey::Left, 10), Some(Cmd::Select(k(0))));
        assert_eq!(nav(&rows, Some(&k(3)), NavKey::Left, 10), None, "a closed root has no parent");
        assert_eq!(nav(&rows, Some(&k(1)), NavKey::Space, 10), Some(Cmd::Expand(k(1))));
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::Space, 10), Some(Cmd::Collapse(k(0))));
        assert_eq!(nav(&rows, Some(&k(2)), NavKey::Enter, 10), Some(Cmd::Activate(k(2))));
        assert_eq!(nav(&rows, Some(&k(0)), NavKey::PageDown, 2), Some(Cmd::Select(k(2))));
        assert_eq!(nav(&rows, Some(&k(1)), NavKey::End, 2), Some(Cmd::Select(k(3))));
        assert_eq!(nav(&rows, Some("gone"), NavKey::Down, 2), Some(Cmd::Select(k(0))), "a hidden selection restarts at the top");
        assert_eq!(NavKey::parse("pagedown"), Some(NavKey::PageDown));
        assert_eq!(NavKey::parse("a"), None);
    }

    #[test]
    fn typing_jumps_by_prefix_and_resets_after_a_pause() {
        let f = forest(&["main"]);
        let mut st = TreeState::default();
        st.expand(conn_key("pg"));
        st.expand(db_key("pg", "app"));
        st.on_catalog("pg", "app", f.catalog("pg", "app").unwrap());
        let rows = st.rows(&f, None);
        let find = |name: &str| rows.iter().find(|r| r.node.name() == name).unwrap().key.clone();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut ta = TypeAhead::default();
        let orders = ta.push("o", t0, &rows, None);
        assert_eq!(orders, Some(find("orders")), "\"other\" comes later; the first match from the top");
        assert_eq!(ta.push("t", ms(200), &rows, orders.as_deref()), Some(find("other")), "\"ot\" moves on to other");
        // After a pause it is a new search, starting after the selection.
        let users = ta.push("u", ms(2000), &rows, Some(&find("other")));
        assert_eq!(users, Some(find("users")));
        // Repeating a letter cycles through its matches.
        let mut ta = TypeAhead::default();
        let first = ta.push("o", t0, &rows, None).unwrap();
        let second = ta.push("o", ms(100), &rows, Some(&first)).unwrap();
        assert_eq!((first, second), (find("orders"), find("other")));
        assert_eq!(TypeAhead::default().push("z", t0, &rows, None), None);
    }
}
