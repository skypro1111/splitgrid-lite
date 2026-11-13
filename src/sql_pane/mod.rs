//! The SQL pane (SqlWorkbench in the Electron app), behaving like DataGrip: a
//! schema tree on the left; on the right the console (top) and the results
//! (bottom). ⌘↩ runs the statement at the caret (or the selection), ⇧⌘↩ the
//! whole script; each run opens a result tab, editable when it is a plain
//! single-table SELECT; an Output tab keeps the run log. Transactions are Auto
//! or Manual, with Commit / Rollback in the toolbar.
//!
//! The explorer on the left lists every saved connection (shared with the
//! Electron app) as a tree; the console runs on the active connection and
//! database. The active one, the expanded nodes, the console text and the
//! split sizes persist in the container's `sqlState`.

pub(crate) mod agent;
pub mod grid;

mod completion;
mod dialog;
mod explorer;
mod plan;
mod run;
mod session;
mod state;
mod store;
mod tree;
mod tree_view;
mod ui;
mod view;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt as _;
use gpui_kit::component::input::{EditorState, InputEvent, InputState, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle};
use gpui_kit::*;
use serde_json::Value as Json;
use sql_core::{Catalog, Column, Dialect, Event, ForeignKey, Row, TableRef, Value};
use sql_lang::EditSource;

use self::grid::{GridEvent, ResultGrid};
use self::run::{Job, Kind, Queue, Tx, TxMode};
use self::session::Session;
use self::state::SqlState;
use self::store::{SavedSql, SqlConnectionStore};
use crate::theme::*;

actions!(sql_pane, [RunStatement, RunScript, CancelRun]);

/// Results past this many rows are dropped (the driver stops fetching).
const ROW_CAP: usize = 100_000;
/// Rows a table's data view loads.
const BROWSE_LIMIT: usize = 500;
const TREE_W: f32 = 240.;
const EDITOR_H: f32 = 220.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Inside the editor its own ⌘↩ (insert a line) has to lose.
        KeyBinding::new("cmd-enter", RunStatement, Some("SqlEditor > Input")),
        KeyBinding::new("cmd-enter", RunStatement, Some("SqlPane")),
        KeyBinding::new("shift-cmd-enter", RunScript, Some("SqlPane")),
        KeyBinding::new("cmd-.", CancelRun, Some("SqlPane")),
    ]);
}

/// The saved connections, one store for every window.
struct SqlStore(Rc<RefCell<SqlConnectionStore>>);

impl Global for SqlStore {}

fn store(cx: &mut App) -> Rc<RefCell<SqlConnectionStore>> {
    if !cx.has_global::<SqlStore>() {
        let dir = crate::model::store::app_support_dir("SplitGrid");
        cx.set_global(SqlStore(Rc::new(RefCell::new(SqlConnectionStore::open(&dir)))));
    }
    cx.global::<SqlStore>().0.clone()
}

pub enum SqlEvent {
    /// The container's new `sqlState`.
    State(Json),
    /// Closing was put off to settle an open transaction; it is settled now.
    Close,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// No active connection: the console waits for one.
    Idle,
    Connecting,
    Ready,
    Failed(String),
}

/// What a queued statement is for.
#[derive(Clone, Debug, PartialEq)]
enum Purpose {
    /// From the console; `range` is highlighted while it runs.
    Console { range: Option<Range<usize>> },
    /// Fills the data view tab `tab`.
    Browse { tab: u64 },
    Explain,
    /// A staged edit of result tab `tab`.
    Edit { tab: u64 },
}

/// A statement picked to run, before dangers and parameters are settled.
#[derive(Clone, Debug)]
struct Stmt {
    sql: String,
    range: Option<Range<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RunMode {
    Run,
    Explain { analyze: bool },
}

#[derive(Clone, Debug)]
struct Pending {
    stmts: Vec<Stmt>,
    mode: RunMode,
}

/// What to do once the open transaction is settled.
#[derive(Clone, Debug, PartialEq)]
enum AfterTx {
    ClosePane,
    /// Close this connection's sessions.
    Disconnect(String),
    /// Make this connection + database the active one.
    Activate(String, String),
}

enum Modal {
    Dialog(Entity<dialog::ConnectionDialog>),
    /// The saved password can't be opened here (Keychain-sealed): ask for it,
    /// then open the connection (or its database `db`).
    Password { id: String, db: Option<String>, input: Entity<InputState> },
    Danger { pending: Pending, lines: Vec<String> },
    Params { pending: Pending, inputs: Vec<(String, Entity<InputState>)> },
    Preview { tab: u64, sql: String },
    OpenTx(AfterTx),
    Delete { id: String, label: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabClass {
    Result,
    Data,
    Ddl,
    Plan,
}

#[derive(Clone, Debug, PartialEq)]
enum TabStatus {
    Running,
    Done { ms: u128, rows: usize, affected: Option<u64>, capped: bool },
    Failed(String),
}

/// A table's data view: the WHERE / ORDER BY fields above its grid.
struct DataView {
    table: TableRef,
    filter: String,
    order: String,
    /// Created at first render (inputs need the window).
    inputs: Option<(Entity<InputState>, Entity<InputState>, [Subscription; 2])>,
}

struct GridTab {
    grid: Option<Entity<ResultGrid>>,
    _sub: Option<Subscription>,
    editable: Option<EditSource>,
    pending: usize,
    view: Option<DataView>,
    /// Submitting the staged edits failed; they stay staged.
    error: Option<String>,
}

enum Body {
    Grid(GridTab),
    Text(Entity<EditorState>),
    Plan(plan::Plan),
}

struct ResultTab {
    id: u64,
    batch: u64,
    class: TabClass,
    title: String,
    pinned: bool,
    sql: String,
    status: TabStatus,
    notices: Vec<String>,
    body: Body,
}

impl ResultTab {
    fn grid(&self) -> Option<&GridTab> {
        match &self.body {
            Body::Grid(g) => Some(g),
            _ => None,
        }
    }

    fn grid_mut(&mut self) -> Option<&mut GridTab> {
        match &mut self.body {
            Body::Grid(g) => Some(g),
            _ => None,
        }
    }
}

struct LogEntry {
    at: String,
    sql: String,
    outcome: Result<String, String>,
    ms: Option<u128>,
}

struct Running {
    job: Job<Purpose>,
    started: Instant,
    /// The tab the current result set fills, and its rows so far.
    tab: Option<u64>,
    rows: usize,
    affected: Option<u64>,
    capped: bool,
    _task: Task<()>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Tree,
    Editor,
    Value,
}

pub struct SqlPane {
    focus: FocusHandle,
    state: SqlState,
    /// The active connection (the console's) and its database.
    conn: Option<SavedSql>,
    database: Option<String>,
    /// The active session and catalog, mirrored from `sessions` / `forest`.
    session: Option<Rc<Session>>,
    phase: Phase,
    catalog: Option<Arc<Catalog>>,
    completion: completion::Shared,
    /// Every saved connection and what is loaded of it.
    forest: tree::Forest,
    /// The open sessions, by (connection, database).
    sessions: HashMap<(String, String), Rc<Session>>,
    /// Passwords typed this session for Keychain-sealed connections.
    passwords: HashMap<String, String>,
    tree: tree::TreeState,
    tree_focus: FocusHandle,
    tree_filter: Option<Entity<InputState>>,
    /// The rows drawn last (what the keyboard moves through).
    tree_rows: Rc<Vec<tree::TreeRow>>,
    debug_done: bool,
    editor: Entity<EditorState>,
    running_mark: RangeDecorationCollection,
    tabs: Vec<ResultTab>,
    /// None: the Output tab.
    active: Option<u64>,
    next_tab: u64,
    result_seq: usize,
    log: Vec<LogEntry>,
    queue: Queue<Purpose>,
    running: Option<Running>,
    tx: Tx,
    /// Edit batches → their tab: committed in the grid when the batch completes.
    edit_batches: HashMap<u64, u64>,
    param_values: HashMap<String, String>,
    modal: Option<Modal>,
    menu: Option<(Point<Pixels>, view::Menu)>,
    tree_w: f32,
    editor_h: f32,
    value_w: f32,
    drag: Option<Drag>,
    /// The bounds the splits are dragged within (recorded at paint).
    body_bounds: Rc<Cell<Bounds<Pixels>>>,
    value_panel: bool,
    /// The value panel's editor and the cell it shows (tab, row, col).
    value_editor: Option<((u64, usize, usize), Entity<EditorState>)>,
    /// Batches an agent (sg-sql) waits on.
    agent_waits: HashMap<u64, agent::AgentWait>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SqlEvent> for SqlPane {}

impl Focusable for SqlPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SqlPane {
    pub fn new(state: Option<&Json>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut state: SqlState = state.and_then(Json::as_object).cloned().unwrap_or_default();
        state::ensure_shape(&mut state);
        let completion = completion::Shared { catalog: Rc::new(RefCell::new(Arc::new(Catalog::default()))), dialect: Rc::new(Cell::new(Dialect::Postgres)) };
        let text = state::console_text(&state);
        let provider = Rc::new(completion::SqlCompletion(completion.clone()));
        let editor = cx.new(|cx| {
            let mut e = EditorState::new(window, cx).language("sql").default_value(text);
            e.lsp_mut().completion_provider = Some(provider);
            e
        });
        let running_mark = editor.update(cx, |e, cx| e.create_range_decorations_collection(Vec::new(), cx));
        let subs = vec![cx.subscribe(&editor, |this, ed, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = ed.read(cx).value().to_string();
                state::set_console_text(&mut this.state, &text);
                this.persist(cx);
            }
        })];
        let tree_w = state::size(&state, "treeWidth").unwrap_or(TREE_W);
        let editor_h = state::size(&state, "editorHeight").unwrap_or(EDITOR_H);
        let saved = state::saved_connection_id(&state);
        let database = state.get("database").and_then(Json::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let tree = tree::TreeState { expanded: state::tree_expanded(&state).into_iter().collect(), ..Default::default() };
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables"));
        let filter_sub = cx.subscribe_in(&filter, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => {
                this.tree.filter = input.read(cx).value().to_string();
                cx.notify();
            }
            // Enter goes on to the tree, to walk the matches.
            InputEvent::PressEnter { .. } => {
                this.tree_focus.focus(window, cx);
                if this.tree.selected.is_none()
                    && let Some(first) = this.tree_rows.iter().find(|r| matches!(r.node, tree::Node::Table { .. })).map(|r| r.key.clone())
                {
                    this.select_node(first);
                }
                cx.notify();
            }
            _ => {}
        });
        let mut subs = subs;
        subs.push(filter_sub);
        let mut pane = Self {
            focus: cx.focus_handle(),
            state,
            conn: None,
            database: None,
            session: None,
            phase: Phase::Idle,
            catalog: None,
            completion,
            forest: tree::Forest::default(),
            sessions: HashMap::new(),
            passwords: HashMap::new(),
            tree,
            tree_focus: cx.focus_handle(),
            tree_filter: Some(filter),
            tree_rows: Rc::new(Vec::new()),
            debug_done: false,
            editor,
            running_mark,
            tabs: Vec::new(),
            active: None,
            next_tab: 1,
            result_seq: 0,
            log: Vec::new(),
            queue: Queue::default(),
            running: None,
            tx: Tx::default(),
            edit_batches: HashMap::new(),
            param_values: HashMap::new(),
            modal: None,
            menu: None,
            tree_w,
            editor_h,
            value_w: 300.,
            drag: None,
            body_bounds: Default::default(),
            value_panel: false,
            value_editor: None,
            agent_waits: HashMap::new(),
            _subs: subs,
        };
        // Restored: reconnect the active connection (to the database it was on).
        pane.restore(saved, database, window, cx);
        pane
    }

    /// The header title: the connection's label (and database, when not the saved one).
    pub fn title(&self) -> String {
        match &self.conn {
            Some(c) => match &self.database {
                Some(db) if !db.is_empty() && *db != c.database && c.dialect().is_none_or(|d| !d.is_file()) => format!("{} · {db}", c.label),
                _ => c.label.clone(),
            },
            None => "SQL".into(),
        }
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        state::set_size(&mut self.state, "treeWidth", self.tree_w);
        state::set_size(&mut self.state, "editorHeight", self.editor_h);
        state::set_tree_expanded(&mut self.state, &self.tree.expanded);
        cx.emit(SqlEvent::State(Json::Object(self.state.clone())));
    }

    fn dialect(&self) -> Dialect {
        self.session.as_ref().map(|s| s.dialect).or_else(|| self.conn.as_ref().and_then(SavedSql::dialect)).unwrap_or(Dialect::Postgres)
    }

    /// Development aid for screenshots: SPLITGRID_DEBUG_SQL=manual,script,commit,table:<name>,explain,value
    /// switches to Manual commit, runs the console, commits, opens a table, explains the
    /// first statement, shows the value panel — in the order given.
    fn debug_run(&mut self, cx: &mut Context<Self>) {
        let Ok(spec) = std::env::var("SPLITGRID_DEBUG_SQL") else { return };
        let text = self.editor_text(cx);
        for step in spec.split(',') {
            let stmts: Vec<Stmt> = sql_lang::split(&text, self.dialect()).into_iter().map(|s| Stmt { sql: s.text, range: Some(s.range) }).collect();
            match step.split_once(':') {
                Some(("table", name)) => self.open_table(TableRef::new(None, name), String::new(), cx),
                _ if step == "script" => self.enqueue(Pending { stmts, mode: RunMode::Run }, &HashMap::new(), cx),
                _ if step == "explain" => self.enqueue(Pending { stmts: stmts.into_iter().take(1).collect(), mode: RunMode::Explain { analyze: true } }, &HashMap::new(), cx),
                _ if step == "value" => self.value_panel = true,
                _ if step == "manual" => self.tx.mode = TxMode::Manual,
                _ if step == "commit" => self.end_tx(true, cx),
                _ => {}
            }
        }
    }

    // ── Closing ─────────────────────────────────────────────────────────────

    /// Whether the pane can close now; with an open transaction it asks first
    /// (and emits `Close` once the user commits or rolls back).
    pub fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.tx.open && self.session.is_some() {
            self.modal = Some(Modal::OpenTx(AfterTx::ClosePane));
            cx.notify();
            return false;
        }
        true
    }

    fn settle_tx(&mut self, commit: Option<bool>, after: AfterTx, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        let Some(commit) = commit else {
            cx.notify();
            return;
        };
        self.end_tx(commit, cx);
        // Settled once the queue drains (the COMMIT is let through); closing drops the session either way.
        self.tx.open = false;
        match after {
            AfterTx::ClosePane => cx.emit(SqlEvent::Close),
            AfterTx::Disconnect(id) => self.disconnect_conn(&id, cx),
            AfterTx::Activate(id, db) => self.activate(&id, Some(db), window, cx),
        }
    }

    // ── Running ─────────────────────────────────────────────────────────────

    fn editor_text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// The selection's statements, else the one at the caret.
    fn current_statements(&self, cx: &App) -> Vec<Stmt> {
        let d = self.dialect();
        let text = self.editor_text(cx);
        let ed = self.editor.read(cx);
        let sel = ed.selected_range();
        if sel.start < sel.end && sel.end <= text.len() {
            return sql_lang::split(&text[sel.clone()], d)
                .into_iter()
                .map(|s| Stmt { sql: s.text, range: Some(s.range.start + sel.start..s.range.end + sel.start) })
                .collect();
        }
        sql_lang::statement_at(&text, ed.cursor().min(text.len()), d).map(|s| Stmt { sql: s.text, range: Some(s.range) }).into_iter().collect()
    }

    fn run_statement(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) {
        let stmts = self.current_statements(cx);
        self.prepare(Pending { stmts, mode: RunMode::Run }, window, cx);
    }

    fn run_script(&mut self, _: &RunScript, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor_text(cx);
        let stmts = sql_lang::split(&text, self.dialect()).into_iter().map(|s| Stmt { sql: s.text, range: Some(s.range) }).collect();
        self.prepare(Pending { stmts, mode: RunMode::Run }, window, cx);
    }

    fn explain(&mut self, analyze: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut stmts = self.current_statements(cx);
        stmts.truncate(1);
        self.prepare(Pending { stmts, mode: RunMode::Explain { analyze } }, window, cx);
    }

    fn cancel_run(&mut self, _: &CancelRun, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel(cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        if self.running.is_some()
            && let Some(s) = &self.session
        {
            s.cancel();
        }
        cx.notify();
    }

    /// Confirm dangerous statements, then ask for parameters, then queue.
    fn prepare(&mut self, pending: Pending, window: &mut Window, cx: &mut Context<Self>) {
        if pending.stmts.is_empty() || self.session.is_none() {
            return;
        }
        let d = self.dialect();
        let lines: Vec<String> = pending
            .stmts
            .iter()
            .flat_map(|s| sql_lang::dangers(&s.sql, d).into_iter().map(move |danger| (danger, s)))
            .map(|(danger, s)| {
                let what = match danger {
                    sql_lang::Danger::UpdateWithoutWhere => "UPDATE without WHERE changes every row",
                    sql_lang::Danger::DeleteWithoutWhere => "DELETE without WHERE removes every row",
                    sql_lang::Danger::Drop => "DROP removes the object and its data",
                    sql_lang::Danger::Truncate => "TRUNCATE removes every row",
                };
                format!("{what}:  {}", first_line(&s.sql, 70))
            })
            .collect();
        if !lines.is_empty() {
            self.modal = Some(Modal::Danger { pending, lines });
            cx.notify();
            return;
        }
        self.ask_params(pending, window, cx);
    }

    fn ask_params(&mut self, pending: Pending, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.dialect();
        let mut names: Vec<String> = Vec::new();
        for s in &pending.stmts {
            for p in sql_lang::params(&s.sql, d) {
                if !names.contains(&p.name) {
                    names.push(p.name);
                }
            }
        }
        if names.is_empty() {
            self.enqueue(pending, &HashMap::new(), cx);
            return;
        }
        let inputs: Vec<(String, Entity<InputState>)> = names
            .into_iter()
            .map(|name| {
                let last = self.param_values.get(&name).cloned().unwrap_or_default();
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("value, 'text' or NULL").default_value(last));
                let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.submit_params(cx);
                    }
                });
                self._subs.push(sub);
                (name, input)
            })
            .collect();
        inputs[0].1.update(cx, |i, cx| i.focus(window, cx));
        self.modal = Some(Modal::Params { pending, inputs });
        cx.notify();
    }

    fn submit_params(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::Params { pending, inputs }) = self.modal.take() else { return };
        let mut values = HashMap::new();
        for (name, input) in inputs {
            let text = input.read(cx).value().to_string();
            values.insert(name.clone(), param_value(&text));
            self.param_values.insert(name, text);
        }
        self.enqueue(pending, &values, cx);
    }

    fn confirm_danger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Danger { pending, .. }) = self.modal.take() else { return };
        self.ask_params(pending, window, cx);
    }

    fn enqueue(&mut self, pending: Pending, values: &HashMap<String, Value>, cx: &mut Context<Self>) {
        let d = self.dialect();
        let batch = self.queue.new_batch();
        for s in pending.stmts {
            let (sql, params) = if values.is_empty() { (s.sql, Vec::new()) } else { sql_lang::bind(&s.sql, d, values) };
            match pending.mode {
                RunMode::Run => self.queue.push(batch, sql, params, Kind::Run(Purpose::Console { range: s.range })),
                RunMode::Explain { analyze } => match sql_lang::explain_query(&sql, d, analyze) {
                    Some(q) => self.queue.push(batch, q, params, Kind::Run(Purpose::Explain)),
                    None => {
                        self.log(first_line(&sql, 80), Err(format!("Explain isn't available for {}", d.label())), None);
                        self.active = None;
                    }
                },
            }
        }
        self.pump(cx);
    }

    /// Start the next queued statement, if nothing is running.
    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        self.agent_settle();
        let Some(session) = self.session.clone() else {
            self.queue.clear();
            self.agent_settle();
            return;
        };
        let Some(job) = self.queue.next(&self.tx, session.dialect) else {
            self.running_mark.clear(cx);
            cx.notify();
            return;
        };
        match &job.kind {
            Kind::Run(Purpose::Console { range: Some(r) }) => {
                let len = self.editor.read(cx).text().len();
                let r = r.start.min(len)..r.end.min(len);
                self.running_mark.set(vec![RangeDecoration::new(r).with_style(RangeDecorationStyle::Fill).with_color(c(hexa(0x228df2, 0.16)))], cx);
            }
            _ => self.running_mark.clear(cx),
        }
        if let Kind::Run(Purpose::Browse { tab }) = &job.kind
            && let Some(t) = self.tabs.iter_mut().find(|t| t.id == *tab)
        {
            t.status = TabStatus::Running;
            t.sql = job.sql.clone();
        }
        let (mut events, done) = session.execute(job.sql.clone(), job.params.clone());
        let task = cx.spawn(async move |this, cx| {
            while let Some(first) = events.next().await {
                // Take what has piled up, so a fast result is one update, not hundreds.
                let mut batch = vec![first];
                while batch.len() < 64
                    && let Ok(ev) = events.try_recv()
                {
                    batch.push(ev);
                }
                let keep = this.update(cx, |this, cx| this.on_events(batch, cx)).unwrap_or(false);
                if !keep {
                    break;
                }
            }
            // Dropping the receiver tells the driver to stop fetching.
            drop(events);
            let result = done.await;
            let _ = this.update(cx, |this, cx| this.finish(result, cx));
        });
        self.running = Some(Running { job, started: Instant::now(), tab: None, rows: 0, affected: None, capped: false, _task: task });
        cx.notify();
    }

    /// Results streaming in. False once the row cap is reached.
    fn on_events(&mut self, events: Vec<Event>, cx: &mut Context<Self>) -> bool {
        for ev in events {
            match ev {
                Event::ResultStart(columns) => self.start_result(columns, cx),
                Event::Rows(rows) => {
                    if !self.append_rows(rows, cx) {
                        cx.notify();
                        return false;
                    }
                }
                Event::ResultEnd { affected } => {
                    if let Some(r) = self.running.as_mut() {
                        r.affected = affected.or(r.affected);
                    }
                }
                Event::Notice(text) => {
                    let tab = self.running.as_ref().and_then(|r| r.tab);
                    match tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
                        Some(t) => t.notices.push(text),
                        None => self.log(text, Ok(String::new()), None),
                    }
                }
            }
        }
        cx.notify();
        true
    }

    fn start_result(&mut self, columns: Vec<Column>, cx: &mut Context<Self>) {
        let Some(running) = self.running.as_ref() else { return };
        let (batch, purpose) = match &running.job.kind {
            Kind::Run(p) => (running.job.batch, p.clone()),
            // BEGIN / COMMIT don't return rows.
            _ => return,
        };
        let sql = running.job.sql.clone();
        let grid = cx.new(|cx| ResultGrid::new(columns, cx));
        let id = match purpose {
            Purpose::Browse { tab } => {
                let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
                if let Some(g) = t.grid_mut() {
                    g.grid = Some(grid.clone());
                    g.pending = 0;
                    g.error = None;
                    g.editable = None;
                }
                tab
            }
            Purpose::Edit { .. } => return,
            Purpose::Console { .. } | Purpose::Explain => {
                let class = if purpose == Purpose::Explain { TabClass::Plan } else { TabClass::Result };
                self.result_seq += 1;
                let title = if class == TabClass::Plan { "Plan".into() } else { format!("Result {}", self.result_seq) };
                let body = Body::Grid(GridTab { grid: Some(grid.clone()), _sub: None, editable: None, pending: 0, view: None, error: None });
                self.add_tab(ResultTab { id: 0, batch, class, title, pinned: false, sql, status: TabStatus::Running, notices: Vec::new(), body })
            }
        };
        let sub = cx.subscribe(&grid, move |this, _, ev: &GridEvent, cx| this.on_grid_event(id, ev, cx));
        if let Some(g) = self.tabs.iter_mut().find(|t| t.id == id).and_then(ResultTab::grid_mut) {
            g._sub = Some(sub);
        }
        if let Some(r) = self.running.as_mut() {
            r.tab = Some(id);
            r.rows = 0;
        }
    }

    /// False once the cap is reached.
    fn append_rows(&mut self, mut rows: Vec<Row>, cx: &mut Context<Self>) -> bool {
        let Some(r) = self.running.as_mut() else { return false };
        let Some(grid) = r.tab.and_then(|id| self.tabs.iter().find(|t| t.id == id)).and_then(ResultTab::grid).and_then(|g| g.grid.clone()) else {
            return true;
        };
        let room = ROW_CAP.saturating_sub(r.rows);
        if rows.len() >= room {
            rows.truncate(room);
            r.capped = true;
        }
        r.rows += rows.len();
        let capped = r.capped;
        grid.update(cx, |g, cx| g.append_rows(rows, cx));
        !capped
    }

    fn finish(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        let Some(running) = self.running.take() else { return };
        let ms = running.started.elapsed().as_millis();
        let Job { batch, sql, kind, .. } = running.job;
        let tab = running.tab;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        self.agent_record(batch, &kind, &sql, tab, running.rows, running.affected, running.capped, ms, error, cx);
        match result {
            Ok(()) => {
                self.tx.applied(&kind, &sql);
                let control = !matches!(kind, Kind::Run(_)) || run::tx_verb(&sql).is_some();
                let summary = match (tab, running.affected) {
                    _ if control => "done".into(),
                    (Some(_), _) => format!("{} row{}{}", ui::count(running.rows), if running.rows == 1 { "" } else { "s" }, if running.capped { " (first 100,000)" } else { "" }),
                    (None, Some(n)) => format!("{n} row{} affected", if n == 1 { "" } else { "s" }),
                    (None, None) => "done".into(),
                };
                if let Some(t) = tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
                    t.status = TabStatus::Done { ms, rows: running.rows, affected: running.affected, capped: running.capped };
                }
                match &kind {
                    Kind::Run(Purpose::Console { .. }) => {
                        if let Some(id) = tab {
                            self.make_editable(id, &sql, cx);
                        }
                    }
                    Kind::Run(Purpose::Browse { tab }) => self.make_editable(*tab, &sql, cx),
                    Kind::Run(Purpose::Explain) => {
                        if let Some(id) = tab {
                            self.show_plan(id, cx);
                        }
                    }
                    _ => {}
                }
                // A statement without rows: the Output tab says what it did.
                if tab.is_none() && matches!(kind, Kind::Run(Purpose::Console { .. })) && self.queue.is_empty() && !self.batch_has_tab(batch) {
                    self.active = None;
                }
                // Edits are committed in the grid once their whole batch (and its COMMIT) succeeded.
                if let Some(&edit_tab) = self.edit_batches.get(&batch)
                    && !self.queue.has_batch(batch)
                {
                    self.edit_batches.remove(&batch);
                    if let Some(g) = self.tabs.iter().find(|t| t.id == edit_tab).and_then(ResultTab::grid).and_then(|g| g.grid.clone()) {
                        g.update(cx, |g, cx| g.commit_edits(cx));
                    }
                    if let Some(g) = self.tabs.iter_mut().find(|t| t.id == edit_tab).and_then(ResultTab::grid_mut) {
                        g.error = None;
                    }
                }
                self.log(first_line(&sql, 100), Ok(summary), Some(ms));
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if let Some(t) = tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
                    t.status = TabStatus::Failed(msg.clone());
                }
                if let Kind::Run(Purpose::Browse { tab }) = &kind
                    && let Some(t) = self.tabs.iter_mut().find(|t| t.id == *tab)
                {
                    t.status = TabStatus::Failed(msg.clone());
                }
                if let Some(edit_tab) = self.edit_batches.remove(&batch)
                    && let Some(g) = self.tabs.iter_mut().find(|t| t.id == edit_tab).and_then(ResultTab::grid_mut)
                {
                    g.error = Some(msg.clone());
                }
                // A failed statement still runs inside an open transaction (Postgres aborts it).
                if self.queue.fail(batch) && self.tx.open {
                    let b = self.queue.new_batch();
                    self.queue.push(b, run::ROLLBACK, Vec::new(), Kind::Rollback);
                }
                if matches!(kind, Kind::Run(Purpose::Console { .. } | Purpose::Explain)) && tab.is_none() {
                    self.active = None;
                }
                self.log(first_line(&sql, 100), Err(msg), Some(ms));
            }
        }
        cx.notify();
        self.pump(cx);
    }

    fn batch_has_tab(&self, batch: u64) -> bool {
        self.tabs.iter().any(|t| t.batch == batch)
    }

    fn log(&mut self, sql: String, outcome: Result<String, String>, ms: Option<u128>) {
        log::debug!("sql: {sql} → {outcome:?} ({ms:?} ms)");
        self.log.push(LogEntry { at: ui::clock(), sql, outcome, ms });
        if self.log.len() > 1000 {
            self.log.drain(..100);
        }
    }

    /// A new tab replaces the unpinned ones of its kind from earlier runs.
    fn add_tab(&mut self, mut tab: ResultTab) -> u64 {
        tab.id = self.next_tab;
        self.next_tab += 1;
        let (class, batch) = (tab.class, tab.batch);
        self.tabs.retain(|t| t.pinned || t.class != class || t.batch == batch || class == TabClass::Data);
        let id = tab.id;
        self.tabs.push(tab);
        self.active = Some(id);
        if let Some((key, _)) = &self.value_editor
            && !self.tabs.iter().any(|t| t.id == key.0)
        {
            self.value_editor = None;
        }
        id
    }

    fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        self.tabs.retain(|t| t.id != id);
        if self.active == Some(id) {
            self.active = self.tabs.last().map(|t| t.id);
        }
        cx.notify();
    }

    /// After a SELECT: editable in place when it reads one table with its key.
    fn make_editable(&mut self, id: u64, sql: &str, cx: &mut Context<Self>) {
        let (Some(catalog), d) = (self.catalog.clone(), self.dialect()) else { return };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        let data_table = t.grid().and_then(|g| g.view.as_ref()).map(|v| v.table.clone());
        let Some(grid) = t.grid().and_then(|g| g.grid.clone()) else { return };
        let names: Vec<String> = grid.read(cx).columns().iter().map(|c| c.name.clone()).collect();
        let source = match &data_table {
            // A data view reads its table as is: map columns by name.
            Some(table) => data_view_source(&catalog, table, &names),
            None => sql_lang::editable_source(sql, d, &catalog, &names),
        };
        let refs = source.as_ref().map(|s| references(&catalog, s)).unwrap_or_default();
        log::debug!("sql: result {id} editable: {:?}, {} references", source.as_ref().map(|s| &s.table), refs.len());
        if let Some(s) = &source
            && data_table.is_none()
        {
            t.title = s.table.name.clone();
        }
        if let Some(g) = t.grid_mut() {
            g.editable = source.clone();
        }
        grid.update(cx, |g, cx| g.set_edit_source(source, refs, cx));
    }

    fn show_plan(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.dialect() != Dialect::Postgres {
            return;
        }
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        let Some(grid) = t.grid().and_then(|g| g.grid.clone()) else { return };
        let plan = grid.read(cx).cell(0, 0).and_then(plan::parse_postgres);
        if let Some(plan) = plan {
            t.body = Body::Plan(plan);
        }
    }

    fn on_grid_event(&mut self, tab: u64, ev: &GridEvent, cx: &mut Context<Self>) {
        match ev {
            GridEvent::EditsChanged(n) => {
                if let Some(g) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(ResultTab::grid_mut) {
                    g.pending = *n;
                }
            }
            GridEvent::OpenReference { table, filter } => {
                let d = self.dialect();
                let clause = filter
                    .iter()
                    .map(|(col, v)| match v {
                        Value::Null => format!("{} IS NULL", sql_lang::quote_ident(col, d)),
                        v => format!("{} = {}", sql_lang::quote_ident(col, d), sql_lang::literal(v, d)),
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                self.open_table(table.clone(), clause, cx);
            }
            // Deselected: the value panel lets go of the old cell.
            GridEvent::Selected(None) => self.value_editor = None,
            GridEvent::Selected(Some(_)) => {}
        }
        cx.notify();
    }

    // ── Data views, DDL, snippets ───────────────────────────────────────────

    /// Open a table's data (reusing its tab), filtered by `filter`.
    fn open_table(&mut self, table: TableRef, filter: String, cx: &mut Context<Self>) {
        self.menu = None;
        let existing = self.tabs.iter().find(|t| t.grid().and_then(|g| g.view.as_ref()).is_some_and(|v| v.table == table)).map(|t| t.id);
        let id = match existing {
            Some(id) => {
                self.active = Some(id);
                id
            }
            None => {
                let view = DataView { table: table.clone(), filter: filter.clone(), order: String::new(), inputs: None };
                let body = Body::Grid(GridTab { grid: None, _sub: None, editable: None, pending: 0, view: Some(view), error: None });
                let batch = self.queue.new_batch();
                self.add_tab(ResultTab { id: 0, batch, class: TabClass::Data, title: table.name.clone(), pinned: false, sql: String::new(), status: TabStatus::Running, notices: Vec::new(), body })
            }
        };
        if let Some(v) = self.tabs.iter_mut().find(|t| t.id == id).and_then(ResultTab::grid_mut).and_then(|g| g.view.as_mut()) {
            v.filter = filter;
            // The field shows the new filter on the next render.
            v.inputs = None;
        }
        self.browse(id, cx);
    }

    /// (Re)load a data view with its WHERE / ORDER BY.
    fn browse(&mut self, tab: u64, cx: &mut Context<Self>) {
        let d = self.dialect();
        let Some(view) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(ResultTab::grid_mut).and_then(|g| g.view.as_mut()) else { return };
        if let Some((f, o, _)) = &view.inputs {
            view.filter = f.read(cx).value().trim().to_owned();
            view.order = o.read(cx).value().trim().to_owned();
        }
        let sql = sql_lang::browse_query(&view.table, &view.filter, &view.order, BROWSE_LIMIT, d);
        let batch = self.queue.new_batch();
        self.queue.push(batch, sql, Vec::new(), Kind::Run(Purpose::Browse { tab }));
        self.pump(cx);
    }

    fn show_ddl(&mut self, session: Rc<Session>, table: TableRef, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let fut = session.ddl(table.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(ddl) => {
                        let editor = cx.new(|cx| {
                            let mut e = EditorState::new(window, cx).language("sql").default_value(ddl);
                            e.set_readonly(true, cx);
                            e
                        });
                        let batch = this.queue.new_batch();
                        let title = format!("{} DDL", table.name);
                        this.add_tab(ResultTab { id: 0, batch, class: TabClass::Ddl, title, pinned: false, sql: String::new(), status: TabStatus::Done { ms: 0, rows: 0, affected: None, capped: false }, notices: Vec::new(), body: Body::Text(editor) });
                    }
                    Err(e) => {
                        this.log(format!("DDL of {}", table.name), Err(format!("{e:#}")), None);
                        this.active = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Put text into the console on a line of its own at the caret.
    fn insert_snippet(&mut self, snippet: String, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.editor.update(cx, |e, cx| {
            let text = e.value();
            let at = e.cursor().min(text.len());
            let prefix = if at == 0 || text[..at].ends_with('\n') { "" } else { "\n" };
            e.insert(format!("{prefix}{snippet}\n"), window, cx);
            e.focus(window, cx);
        });
    }

    // ── Editing results ─────────────────────────────────────────────────────

    /// A context menu is open (it may spill over a browser pane's web view).
    pub(crate) fn has_menu(&self, cx: &App) -> bool {
        self.menu.is_some() || self.active_grid().is_some_and(|(_, g)| g.read(cx).has_menu())
    }

    fn active_grid(&self) -> Option<(u64, Entity<ResultGrid>)> {
        let t = self.tabs.iter().find(|t| Some(t.id) == self.active)?;
        Some((t.id, t.grid()?.grid.clone()?))
    }

    fn edit_statements(&self, tab: u64, cx: &App) -> Option<Vec<String>> {
        let t = self.tabs.iter().find(|t| t.id == tab)?;
        let g = t.grid()?;
        let source = g.editable.as_ref()?;
        let edits = g.grid.as_ref()?.read(cx).pending_edits();
        (!edits.is_empty()).then(|| sql_lang::build_edits(&source.table, &edits, self.dialect()))
    }

    fn preview_edits(&mut self, tab: u64, cx: &mut Context<Self>) {
        if let Some(stmts) = self.edit_statements(tab, cx) {
            let sql = stmts.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n");
            self.modal = Some(Modal::Preview { tab, sql });
            cx.notify();
        }
    }

    /// Run the staged edits: in a transaction of their own (Auto), or in the open one (Manual).
    fn submit_edits(&mut self, tab: u64, cx: &mut Context<Self>) {
        self.modal = None;
        let Some(stmts) = self.edit_statements(tab, cx) else { return };
        let batch = self.queue.new_batch();
        let own = self.tx.mode == TxMode::Auto && !self.tx.open;
        if own {
            self.queue.push(batch, run::begin_sql(self.dialect()), Vec::new(), Kind::Begin);
        }
        for s in stmts {
            self.queue.push(batch, s, Vec::new(), Kind::Run(Purpose::Edit { tab }));
        }
        if own {
            self.queue.push(batch, run::COMMIT, Vec::new(), Kind::Commit);
        }
        self.edit_batches.insert(batch, tab);
        if let Some(g) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(ResultTab::grid_mut) {
            g.error = None;
        }
        self.pump(cx);
    }

    // ── Transactions ────────────────────────────────────────────────────────

    fn set_tx_mode(&mut self, mode: TxMode, cx: &mut Context<Self>) {
        // Switching to Auto with work pending would leave it hanging: settle it first.
        if mode == TxMode::Auto && self.tx.open {
            return;
        }
        self.tx.mode = mode;
        cx.notify();
    }

    fn end_tx(&mut self, commit: bool, cx: &mut Context<Self>) {
        let batch = self.queue.new_batch();
        let (sql, kind) = if commit { (run::COMMIT, Kind::Commit) } else { (run::ROLLBACK, Kind::Rollback) };
        self.queue.push(batch, sql, Vec::new(), kind);
        self.pump(cx);
    }

    // ── Value panel ─────────────────────────────────────────────────────────

    /// The selected cell of the active result: (tab, row, col, value, editable).
    fn selected_cell(&self, cx: &App) -> Option<(u64, usize, usize, Value, bool)> {
        let (id, grid) = self.active_grid()?;
        let g = grid.read(cx);
        let (row, col) = g.selected()?;
        let value = g.cell(row, col)?.clone();
        let editable = self.tabs.iter().find(|t| t.id == id).and_then(ResultTab::grid).and_then(|t| t.editable.as_ref()).is_some_and(|s| s.column_map.get(col).is_some_and(Option::is_some));
        Some((id, row, col, value, editable))
    }

    /// Keep the value panel's editor on the selected cell.
    fn sync_value_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.value_panel {
            return;
        }
        let Some((tab, row, col, value, editable)) = self.selected_cell(cx) else {
            self.value_editor = None;
            return;
        };
        let key = (tab, row, col);
        if self.value_editor.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let (text, lang) = value_text(&value);
        let editor = cx.new(|cx| {
            let mut e = EditorState::new(window, cx).language(lang).soft_wrap(true).default_value(text);
            e.set_readonly(!editable, cx);
            e
        });
        self.value_editor = Some((key, editor));
    }

    /// Write the value panel's text back into the cell.
    fn apply_value(&mut self, cx: &mut Context<Self>) {
        let Some(((tab, row, col), editor)) = self.value_editor.clone() else { return };
        let Some(grid) = self.tabs.iter().find(|t| t.id == tab).and_then(ResultTab::grid).and_then(|g| g.grid.clone()) else { return };
        let text = editor.read(cx).value().to_string();
        let value = {
            let g = grid.read(cx);
            let type_name = g.columns().get(col).map_or("", |c| c.type_name.as_str());
            grid::parse_input(&text, type_name, g.cell(row, col))
        };
        grid.update(cx, |g, cx| g.set_cell(row, col, value, cx));
        cx.notify();
    }
}

/// A data view's edit source: the table's key, columns mapped by name.
fn data_view_source(catalog: &Catalog, table: &TableRef, names: &[String]) -> Option<EditSource> {
    let (_, t) = catalog.table(table)?;
    if t.primary_key.is_empty() || !t.primary_key.iter().all(|k| names.contains(k)) {
        return None;
    }
    let column_map = names.iter().map(|n| t.columns.iter().any(|c| &c.name == n).then(|| n.clone())).collect();
    Some(EditSource { table: table.clone(), key_columns: t.primary_key.clone(), column_map })
}

/// The result columns that are foreign keys, by the first column of each key.
fn references(catalog: &Catalog, source: &EditSource) -> Vec<(usize, ForeignKey)> {
    let Some((_, table)) = catalog.table(&source.table) else { return Vec::new() };
    table
        .foreign_keys
        .iter()
        .filter_map(|fk| {
            let first = fk.columns.first()?;
            let col = source.column_map.iter().position(|m| m.as_ref() == Some(first))?;
            Some((col, fk.clone()))
        })
        .collect()
}

/// A typed parameter: a number, NULL, a quoted or bare string.
fn param_value(text: &str) -> Value {
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        Value::Null
    } else if let Ok(i) = t.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = t.parse::<f64>().map_err(|_| ()).and_then(|f| if f.is_finite() { Ok(f) } else { Err(()) }) {
        Value::Float(f)
    } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        Value::Text(t[1..t.len() - 1].replace("''", "'"))
    } else {
        Value::Text(text.to_owned())
    }
}


/// A cell in full for the value panel, and the language to show it in.
fn value_text(v: &Value) -> (String, &'static str) {
    let pretty = |j: &Json| serde_json::to_string_pretty(j).unwrap_or_default();
    match v {
        Value::Null => (String::new(), "text"),
        Value::Json(j) => (pretty(j), "json"),
        Value::Text(s) | Value::Other(s) => match serde_json::from_str::<Json>(s) {
            Ok(j @ (Json::Object(_) | Json::Array(_))) => (pretty(&j), "json"),
            _ => (s.clone(), "text"),
        },
        other => (other.display(), "text"),
    }
}

/// The first line of a statement, cut to `max` characters.
fn first_line(sql: &str, max: usize) -> String {
    let line = sql.trim().lines().next().unwrap_or_default();
    let more = sql.trim().lines().nth(1).is_some();
    let mut out: String = line.chars().take(max).collect();
    if more || line.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's `test` attribute would shadow the standard one.
    use super::{data_view_source, first_line, param_value, references, value_text};
    use sql_core::{Catalog, ColumnInfo, ForeignKey, Schema, Table, TableRef, Value};

    #[test]
    fn parameters_are_typed_like_datagrip() {
        assert_eq!(param_value("42"), Value::Int(42));
        assert_eq!(param_value(" 4.5 "), Value::Float(4.5));
        assert_eq!(param_value("null"), Value::Null);
        assert_eq!(param_value("'it''s'"), Value::Text("it's".into()));
        assert_eq!(param_value("abc"), Value::Text("abc".into()));
        assert_eq!(param_value("NaN"), Value::Text("NaN".into()));
    }

    #[test]
    fn value_panel_pretty_prints_json() {
        assert_eq!(value_text(&Value::Text(r#"{"a":1}"#.into())), ("{\n  \"a\": 1\n}".into(), "json"));
        assert_eq!(value_text(&Value::Text("42".into())).1, "text", "a bare number is text");
        assert_eq!(value_text(&Value::Null).0, "");
        assert_eq!(first_line("select 1\nfrom t", 80), "select 1…");
        assert_eq!(first_line("  select 1  ", 80), "select 1");
    }

    #[test]
    fn data_views_and_references_follow_the_catalog() {
        let col = |n: &str| ColumnInfo { name: n.into(), type_name: "int".into(), ..Default::default() };
        let orders = Table {
            name: "orders".into(),
            columns: vec![col("id"), col("user_id"), col("total")],
            primary_key: vec!["id".into()],
            foreign_keys: vec![ForeignKey { name: "fk".into(), columns: vec!["user_id".into()], references: TableRef::new(None, "users"), ref_columns: vec!["id".into()] }],
            ..Default::default()
        };
        let cat = Catalog { database: "d".into(), search_path: vec!["main".into()], schemas: vec![Schema { name: "main".into(), tables: vec![orders], ..Default::default() }] };
        let names: Vec<String> = ["id", "user_id", "total", "computed"].map(String::from).to_vec();
        let src = data_view_source(&cat, &TableRef::new(None, "orders"), &names).unwrap();
        assert_eq!(src.column_map[3], None);
        assert_eq!(references(&cat, &src).iter().map(|(i, fk)| (*i, fk.name.as_str())).collect::<Vec<_>>(), [(1, "fk")]);
        assert!(data_view_source(&cat, &TableRef::new(None, "orders"), &["total".into()]).is_none(), "no key, no edits");
    }
}
