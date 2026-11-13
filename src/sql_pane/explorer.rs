//! The database explorer's loading: every saved connection in the tree, each
//! opened lazily when expanded (or made active), one live session per
//! connection + database. The console runs on the *active* one; the others
//! stay open until Disconnect or the pane closes (dropping a `Session` closes
//! its connection).

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use serde_json::Value as Json;
use sql_core::{Catalog, Dialect, TableRef};

use super::dialog::{ConnectionDialog, DialogEvent};
use super::run::Kind;
use super::session::Session;
use super::state;
use super::store::{Password, SavedSql};
use super::tree::{self, Cmd, ConnEntry, ConnStatus, DbState, Load, Node};
use super::{AfterTx, Modal, Phase, SqlPane, store};

/// The database a saved connection's own session opens: the file for SQLite.
fn default_db(conn: &SavedSql) -> String {
    match conn.dialect() {
        Some(d) if d.is_file() => conn.file().unwrap_or_default().to_owned(),
        _ => conn.database.clone(),
    }
}

fn entry_of(conn: &SavedSql) -> ConnEntry {
    let flat = conn.dialect().is_some_and(Dialect::is_file);
    ConnEntry::new(&conn.id, &conn.label, &conn.dialect, &conn.address(), flat, &default_db(conn))
}

impl SqlPane {
    fn saved(&self, id: &str, cx: &mut Context<Self>) -> Option<SavedSql> {
        store(cx).borrow().get(id).cloned()
    }

    /// The connection and database the console runs on.
    pub(super) fn active_pair(&self) -> Option<(String, String)> {
        Some((self.conn.as_ref()?.id.clone(), self.database.clone()?))
    }

    /// Re-read the saved connections (the dialog or the other app changed them).
    pub(super) fn sync_connections(&mut self, cx: &mut Context<Self>) {
        let store = store(cx);
        store.borrow_mut().sync_from_disk();
        let saved = store.borrow().connections.clone();
        let mut old: Vec<ConnEntry> = std::mem::take(&mut self.forest.conns);
        for c in &saved {
            let entry = match old.iter().position(|e| e.id == c.id) {
                Some(i) => {
                    let mut e = old.swap_remove(i);
                    e.label = c.label.clone();
                    e.dialect = c.dialect.clone();
                    e.address = c.address();
                    e
                }
                None => entry_of(c),
            };
            self.forest.conns.push(entry);
        }
        // Deleted (here or in the other app): close what was open.
        for gone in old {
            self.sessions.retain(|(c, _), _| *c != gone.id);
            self.tree.forget(&gone.id);
            if self.conn.as_ref().is_some_and(|c| c.id == gone.id) {
                self.clear_active(cx);
            }
        }
        self.after_change(cx);
    }

    /// The password to open `conn` with; None when the user has to type it first
    /// (the prompt then opens `db` of the connection, or the connection itself).
    fn password_for(&mut self, conn: &SavedSql, db: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        if let Some(p) = self.passwords.get(&conn.id) {
            return Some(p.clone());
        }
        match store(cx).borrow().password(conn) {
            Password::None => return Some(String::new()),
            Password::Known(p) => return Some(p),
            Password::Ask => {}
        }
        let input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Password"));
        let (id, db1) = (conn.id.clone(), db.clone());
        let sub = cx.subscribe_in(&input, window, move |this, input, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let typed = input.read(cx).value().to_string();
                this.submit_password(id.clone(), db1.clone(), typed, window, cx);
            }
        });
        self._subs.push(sub);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.modal = Some(Modal::Password { id: conn.id.clone(), db, input });
        cx.notify();
        None
    }

    pub(super) fn submit_password(&mut self, id: String, db: Option<String>, typed: String, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        self.passwords.insert(id.clone(), typed);
        match db {
            Some(db) => self.open_database(&id, &db, window, cx),
            None => self.open_connection(&id, window, cx),
        }
    }

    /// Open a connection's own session (its saved database), then its databases and catalog.
    pub(super) fn open_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.saved(id, cx) else { return };
        if self.forest.conn(id).is_some_and(|e| matches!(e.status, ConnStatus::Connecting | ConnStatus::Open)) {
            return;
        }
        let Some(password) = self.password_for(&conn, None, window, cx) else { return };
        let Some(entry) = self.forest.conn_mut(id) else { return };
        entry.reset();
        entry.status = ConnStatus::Connecting;
        entry.default_db = default_db(&conn);
        let db = entry.default_db.clone();
        self.tree.expand(tree::conn_key(id));
        self.after_change(cx);
        let cfg = conn.config(password);
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let result = Session::connect(cfg).await;
            let _ = this.update_in(cx, |this, window, cx| this.on_connected(&id, &db, result, window, cx));
        })
        .detach();
    }

    fn on_connected(&mut self, id: &str, db: &str, result: anyhow::Result<Session>, window: &mut Window, cx: &mut Context<Self>) {
        // Deleted or disconnected meanwhile: the session closes as it drops.
        let Some(entry) = self.forest.conn_mut(id) else { return };
        if entry.status != ConnStatus::Connecting {
            return;
        }
        match result {
            Ok(session) => {
                entry.status = ConnStatus::Open;
                entry.dbs.insert(db.to_owned(), DbState::Introspecting);
                let label = entry.label.clone();
                self.log(format!("Connected to {label} — {}", session.server_version), Ok(String::new()), None);
                self.keep_typed_password(id, cx);
                self.sessions.insert((id.to_owned(), db.to_owned()), Rc::new(session));
                self.tree.expand(tree::db_key(id, db));
                self.load_catalog(id, db, cx);
                self.load_databases(id, cx);
                // Databases waiting on the connection: the active one, and those left expanded.
                let mut more: Vec<String> = self
                    .tree
                    .expanded
                    .iter()
                    .filter_map(|k| match tree::parts(k).as_slice() {
                        ["d", c, d] if *c == id && *d != db => Some((*d).to_owned()),
                        _ => None,
                    })
                    .collect();
                if let Some((c, d)) = self.active_pair()
                    && c == id
                    && d != db
                    && !more.contains(&d)
                {
                    more.push(d);
                }
                for d in more {
                    self.open_database(id, &d, window, cx);
                }
            }
            Err(e) => {
                entry.status = ConnStatus::Failed(format!("{e:#}"));
                self.forget_typed_password(id, cx);
            }
        }
        self.after_change(cx);
    }

    /// A password typed for a Keychain-sealed connection worked: save it for both apps.
    fn keep_typed_password(&mut self, id: &str, cx: &mut Context<Self>) {
        let store = store(cx);
        let ask = store.borrow().get(id).is_some_and(|c| store.borrow().password(c) == Password::Ask);
        if ask
            && let Some(p) = self.passwords.get(id)
            && let Err(e) = store.borrow_mut().set_password(id, p)
        {
            log::warn!("saving the SQL password: {e:#}");
        }
    }

    /// Connecting failed: a typed password may be the reason, ask again next time.
    fn forget_typed_password(&mut self, id: &str, cx: &mut Context<Self>) {
        let ask = self.saved(id, cx).is_some_and(|c| store(cx).borrow().password(&c) == Password::Ask);
        if ask {
            self.passwords.remove(id);
        }
    }

    /// Open another database of an open connection (its own session).
    pub(super) fn open_database(&mut self, id: &str, db: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.forest.conn(id) else { return };
        if entry.status != ConnStatus::Open {
            self.open_connection(id, window, cx);
            return;
        }
        match entry.dbs.get(db) {
            Some(DbState::Connecting | DbState::Introspecting | DbState::Ready { .. }) => return,
            _ if self.sessions.contains_key(&(id.to_owned(), db.to_owned())) => {
                self.load_catalog(id, db, cx);
                return;
            }
            _ => {}
        }
        let Some(conn) = self.saved(id, cx) else { return };
        let Some(password) = self.password_for(&conn, Some(db.to_owned()), window, cx) else { return };
        let mut cfg = conn.config(password);
        cfg.database = db.to_owned();
        if let Some(entry) = self.forest.conn_mut(id) {
            entry.dbs.insert(db.to_owned(), DbState::Connecting);
        }
        self.after_change(cx);
        let (id, db) = (id.to_owned(), db.to_owned());
        cx.spawn(async move |this, cx| {
            let result = Session::connect(cfg).await;
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.forest.conn_mut(&id) else { return };
                if entry.dbs.get(&db) != Some(&DbState::Connecting) {
                    return;
                }
                match result {
                    Ok(session) => {
                        entry.dbs.insert(db.clone(), DbState::Introspecting);
                        this.sessions.insert((id.clone(), db.clone()), Rc::new(session));
                        this.load_catalog(&id, &db, cx);
                    }
                    Err(e) => {
                        entry.dbs.insert(db.clone(), DbState::Failed(format!("{e:#}")));
                    }
                }
                this.after_change(cx);
            });
        })
        .detach();
    }

    /// (Re-)introspect one database; the tree keeps the old catalog meanwhile.
    pub(super) fn load_catalog(&mut self, id: &str, db: &str, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(&(id.to_owned(), db.to_owned())).cloned() else { return };
        let Some(entry) = self.forest.conn_mut(id) else { return };
        let next = match entry.dbs.remove(db) {
            Some(DbState::Ready { catalog, .. }) => DbState::Ready { catalog, refreshing: true },
            _ => DbState::Introspecting,
        };
        entry.dbs.insert(db.to_owned(), next);
        cx.notify();
        let fut = session.introspect();
        let (id, db) = (id.to_owned(), db.to_owned());
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |this, cx| this.on_catalog(&id, &db, result, cx));
        })
        .detach();
    }

    fn on_catalog(&mut self, id: &str, db: &str, result: anyhow::Result<Catalog>, cx: &mut Context<Self>) {
        let key = (id.to_owned(), db.to_owned());
        if !self.sessions.contains_key(&key) {
            return;
        }
        let Some(entry) = self.forest.conn_mut(id) else { return };
        match result {
            Ok(cat) => {
                let mut db = db.to_owned();
                // The saved database was blank (the server's default): name it as the server does.
                if !entry.flat && db == entry.default_db && !cat.database.is_empty() && cat.database != db && !entry.dbs.contains_key(&cat.database) {
                    let real = cat.database.clone();
                    entry.dbs.remove(&db);
                    entry.default_db = real.clone();
                    if let Some(s) = self.sessions.remove(&key) {
                        self.sessions.insert((id.to_owned(), real.clone()), s);
                    }
                    if self.tree.expanded.remove(&tree::db_key(id, &db)) {
                        self.tree.expand(tree::db_key(id, &real));
                    }
                    if self.active_pair() == Some(key.clone()) {
                        self.database = Some(real.clone());
                        self.state.insert("database".into(), Json::from(real.clone()));
                    }
                    db = real;
                }
                let Some(entry) = self.forest.conn_mut(id) else { return };
                let cat = Arc::new(cat);
                self.tree.on_catalog(id, &db, &cat);
                entry.dbs.insert(db, DbState::Ready { catalog: cat, refreshing: false });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                match entry.dbs.remove(db) {
                    // A failed refresh keeps what was there.
                    Some(DbState::Ready { catalog, .. }) => {
                        entry.dbs.insert(db.to_owned(), DbState::Ready { catalog, refreshing: false });
                        self.log(format!("Refreshing {db}"), Err(msg), None);
                    }
                    _ => {
                        entry.dbs.insert(db.to_owned(), DbState::Failed(msg));
                    }
                }
            }
        }
        self.after_change(cx);
    }

    pub(super) fn load_databases(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.forest.conn(id) else { return };
        if entry.flat {
            return;
        }
        let Some(session) = self.sessions.get(&(id.to_owned(), entry.default_db.clone())).cloned() else { return };
        let fut = session.databases();
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(entry) = this.forest.conn_mut(&id)
                    && entry.status == ConnStatus::Open
                {
                    entry.databases = match result {
                        Ok(list) => Load::Ready(list),
                        Err(e) => Load::Failed(format!("{e:#}")),
                    };
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Refresh: the saved connections, then every open database's catalog and lists.
    pub(super) fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.sync_connections(cx);
        let ids: Vec<String> = self.forest.conns.iter().filter(|c| c.status == ConnStatus::Open).map(|c| c.id.clone()).collect();
        for id in ids {
            self.refresh_conn(&id, cx);
        }
    }

    pub(super) fn refresh_conn(&mut self, id: &str, cx: &mut Context<Self>) {
        self.load_databases(id, cx);
        let dbs: Vec<String> = self.sessions.keys().filter(|(c, _)| c == id).map(|(_, d)| d.clone()).collect();
        for db in dbs {
            self.load_catalog(id, &db, cx);
        }
    }

    /// Close every session of a connection (after settling the active one's transaction).
    pub(super) fn disconnect_conn(&mut self, id: &str, cx: &mut Context<Self>) {
        self.menu = None;
        let active = self.conn.as_ref().is_some_and(|c| c.id == id);
        if active && self.tx.open && self.session.is_some() {
            self.modal = Some(Modal::OpenTx(AfterTx::Disconnect(id.to_owned())));
            cx.notify();
            return;
        }
        if active {
            self.stop_work();
            self.session = None;
        }
        self.sessions.retain(|(c, _), _| c != id);
        if let Some(e) = self.forest.conn_mut(id) {
            e.reset();
        }
        self.tree.expanded.remove(&tree::conn_key(id));
        self.after_change(cx);
    }

    /// Drop a connection's sessions and open it again (it was edited).
    fn reopen(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.conn.as_ref().is_some_and(|c| c.id == id);
        if active && self.tx.open {
            return;
        }
        let was_open = self.forest.conn(id).is_some_and(|e| e.status != ConnStatus::Idle);
        if !was_open {
            return;
        }
        if active {
            self.stop_work();
            self.session = None;
            self.conn = self.saved(id, cx);
            // The database may have been edited too: follow the saved one.
            self.database = self.conn.as_ref().map(default_db);
        }
        self.sessions.retain(|(c, _), _| c != id);
        if let Some(e) = self.forest.conn_mut(id) {
            e.reset();
        }
        self.open_connection(id, window, cx);
    }

    /// Cancel what runs on the active session and forget the queue (switching away from it).
    pub(super) fn stop_work(&mut self) {
        if let Some(r) = self.running.take() {
            // A COMMIT / ROLLBACK settling the transaction is let through.
            let control = matches!(r.job.kind, Kind::Begin | Kind::Commit | Kind::Rollback);
            if !control && let Some(s) = &self.session {
                s.cancel();
            }
        }
        self.queue.clear();
        self.tx.open = false;
        self.agent_settle();
    }

    /// No active connection: the console waits for one.
    pub(super) fn clear_active(&mut self, cx: &mut Context<Self>) {
        self.stop_work();
        self.session = None;
        self.conn = None;
        self.database = None;
        self.tabs.clear();
        self.active = None;
        state::set_connection(&mut self.state, None);
        self.persist(cx);
    }

    /// Make (`id`, `db`) the console's connection. False when an open
    /// transaction has to be settled first (the dialog asks, then switches).
    pub(super) fn set_active(&mut self, id: &str, db: &str, cx: &mut Context<Self>) -> bool {
        if self.active_pair().is_some_and(|(c, d)| c == id && d == db) {
            return true;
        }
        if self.tx.open && self.session.is_some() {
            self.modal = Some(Modal::OpenTx(AfterTx::Activate(id.to_owned(), db.to_owned())));
            cx.notify();
            return false;
        }
        let Some(conn) = self.saved(id, cx) else { return false };
        self.stop_work();
        if self.conn.as_ref().map(|c| &c.id) != Some(&conn.id) {
            self.tabs.clear();
            self.active = None;
        }
        state::set_connection(&mut self.state, Some(&conn));
        self.state.insert("database".into(), Json::from(db));
        self.completion.dialect.set(conn.dialect().unwrap_or(Dialect::Postgres));
        self.conn = Some(conn);
        self.database = Some(db.to_owned());
        self.session = None;
        self.sync_active(cx);
        self.persist(cx);
        true
    }

    /// Use `id` (on `db`, else its default database) for the console, opening it as needed.
    pub(super) fn activate(&mut self, id: &str, db: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(entry) = self.forest.conn(id) else { return };
        let db = db.filter(|d| !d.is_empty() && !entry.flat).unwrap_or_else(|| entry.default_db.clone());
        let open = entry.status == ConnStatus::Open;
        if !self.set_active(id, &db, cx) {
            return;
        }
        self.tree.expand(tree::conn_key(id));
        if !open {
            // The database opens once the connection is (`on_connected`).
            self.open_connection(id, window, cx);
        } else if !self.sessions.contains_key(&(id.to_owned(), db.clone())) {
            self.tree.expand(tree::db_key(id, &db));
            self.open_database(id, &db, window, cx);
        }
        self.after_change(cx);
    }

    /// Mirror the active connection's session, catalog and status into the pane.
    pub(super) fn sync_active(&mut self, cx: &mut Context<Self>) {
        let Some((id, db)) = self.active_pair() else {
            self.phase = Phase::Idle;
            self.session = None;
            self.catalog = None;
            return;
        };
        let session = self.sessions.get(&(id.clone(), db.clone())).cloned();
        let same = match (&session, &self.session) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            // A new session (reconnected): nothing of the old one carries over.
            self.queue.clear();
            self.running = None;
            self.tx.open = false;
            self.agent_settle();
            self.session = session.clone();
            if let Some(s) = &session {
                self.completion.dialect.set(s.dialect);
            }
        }
        let catalog = self.forest.catalog(&id, &db).cloned();
        let changed = match (&catalog, &self.catalog) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        if changed {
            *self.completion.catalog.borrow_mut() = catalog.clone().unwrap_or_default();
            self.catalog = catalog;
            if self.catalog.is_some() && !self.debug_done {
                self.debug_done = true;
                self.debug_run(cx);
            }
        }
        let entry = self.forest.conn(&id);
        self.phase = match (session.is_some(), entry.map(|e| (&e.status, e.dbs.get(&db)))) {
            (true, _) => Phase::Ready,
            (false, Some((ConnStatus::Failed(e), _))) | (false, Some((_, Some(DbState::Failed(e))))) => Phase::Failed(e.clone()),
            (false, Some((ConnStatus::Idle, _))) | (false, None) => Phase::Failed("Not connected".into()),
            (false, _) => Phase::Connecting,
        };
    }

    /// After the forest changed: follow the active connection, save the expansion, redraw.
    pub(super) fn after_change(&mut self, cx: &mut Context<Self>) {
        self.sync_active(cx);
        self.persist(cx);
        cx.notify();
    }

    /// Retry what failed: the connection, or one of its databases.
    pub(super) fn retry(&mut self, id: &str, db: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.forest.conn(id) else { return };
        if let ConnStatus::Failed(_) = entry.status
            && let Some(e) = self.forest.conn_mut(id)
        {
            e.status = ConnStatus::Idle;
        }
        match db {
            Some(db) if self.forest.conn(id).is_some_and(|e| e.status == ConnStatus::Open) => {
                if let Some(e) = self.forest.conn_mut(id)
                    && matches!(e.dbs.get(&db), Some(DbState::Failed(_)))
                {
                    e.dbs.remove(&db);
                }
                self.open_database(id, &db, window, cx);
            }
            _ => self.open_connection(id, window, cx),
        }
    }

    /// Retry the active connection (the toolbar).
    pub(super) fn retry_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, db)) = self.active_pair() {
            self.retry(&id, Some(db), window, cx);
        }
    }

    // ── Tree interaction ────────────────────────────────────────────────────

    /// Expand or collapse a node; expanding opens what it needs.
    pub(super) fn set_expanded(&mut self, key: &str, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !open {
            self.tree.expanded.remove(key);
            self.after_change(cx);
            return;
        }
        self.tree.expand(key.to_owned());
        match tree::parts(key).as_slice() {
            ["c", id] => {
                let id = id.to_string();
                if self.forest.conn(&id).is_some_and(|e| e.status == ConnStatus::Idle) {
                    self.open_connection(&id, window, cx);
                }
            }
            ["d", id, db] => {
                let (id, db) = (id.to_string(), db.to_string());
                if self.forest.conn(&id).is_some_and(|e| e.status == ConnStatus::Open && !e.dbs.contains_key(&db)) {
                    self.open_database(&id, &db, window, cx);
                }
            }
            _ => {}
        }
        self.after_change(cx);
    }

    pub(super) fn select_node(&mut self, key: String) {
        if let Some(i) = self.tree_rows.iter().position(|r| r.key == key) {
            self.tree.scroll.scroll_to_item(i, ScrollStrategy::Center);
        }
        self.tree.selected = Some(key);
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        match cmd {
            Cmd::Select(k) => {
                self.select_node(k);
                cx.notify();
            }
            Cmd::Expand(k) => self.set_expanded(&k, true, window, cx),
            Cmd::Collapse(k) => self.set_expanded(&k, false, window, cx),
            Cmd::Activate(k) => self.activate_node(&k, window, cx),
        }
    }

    /// Enter / double-click: open a table's data, use a connection or database, toggle a folder.
    pub(super) fn activate_node(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.tree_rows.iter().find(|r| r.key == key).cloned() else { return };
        self.tree.selected = Some(row.key.clone());
        match row.node {
            Node::Conn { id, .. } => self.activate(&id, None, window, cx),
            Node::Database { scope, .. } => self.activate(&scope.conn, Some(scope.db), window, cx),
            Node::Table { scope, table, .. } => {
                if self.set_active(&scope.conn, &scope.db, cx) {
                    self.open_table(table, String::new(), cx);
                }
            }
            Node::Message { retry: Some((id, db)), .. } => self.retry(&id, db, window, cx),
            _ if row.expandable => self.set_expanded(&row.key, !row.expanded, window, cx),
            _ => {}
        }
        cx.notify();
    }

    /// A tree key press: arrows, Enter, Space, type-to-search. True when handled.
    pub(super) fn tree_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        if m.platform || m.control || m.alt {
            return false;
        }
        let rows = self.tree_rows.clone();
        if let Some(nk) = tree::NavKey::parse(&k.key) {
            // A blank space is a toggle, not a search, unless a search is being typed.
            if nk == tree::NavKey::Space && self.type_ahead_active() {
                return self.type_char(" ", window, cx);
            }
            let page = (crate::ui::unpx(self.body_bounds.get().size.height) / super::view::TREE_ROW_H) as usize;
            if let Some(cmd) = tree::nav(&rows, self.tree.selected.as_deref(), nk, page.saturating_sub(3)) {
                self.run_cmd(cmd, window, cx);
            }
            return true;
        }
        if k.key == "escape" && !self.tree.filter.is_empty() {
            self.set_filter(String::new(), window, cx);
            return true;
        }
        match k.key_char.as_deref().filter(|ch| !ch.chars().any(char::is_control)) {
            Some(ch) => self.type_char(ch, window, cx),
            None => false,
        }
    }

    fn type_ahead_active(&self) -> bool {
        self.tree.type_ahead.active(std::time::Instant::now())
    }

    fn type_char(&mut self, ch: &str, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let rows = self.tree_rows.clone();
        if let Some(k) = self.tree.type_ahead.push(ch, std::time::Instant::now(), &rows, self.tree.selected.as_deref()) {
            self.select_node(k);
            cx.notify();
        }
        true
    }

    pub(super) fn set_filter(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.tree.filter = text.clone();
        if text.is_empty()
            && let Some(input) = &self.tree_filter
        {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        cx.notify();
    }

    // ── Connection actions (tree menu, toolbar, dialog) ─────────────────────

    pub(super) fn open_dialog(&mut self, existing: Option<SavedSql>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let dialog = cx.new(|cx| ConnectionDialog::new(existing, window, cx));
        let sub = cx.subscribe_in(&dialog, window, |this, _, ev: &DialogEvent, window, cx| {
            this.modal = None;
            if let DialogEvent::Saved { id, connect } = ev {
                this.sync_connections(cx);
                this.reopen(id, window, cx);
                this.tree.selected = Some(tree::conn_key(id));
                if *connect {
                    this.activate(id, None, window, cx);
                }
            }
            cx.notify();
        });
        self._subs.push(sub);
        self.modal = Some(Modal::Dialog(dialog));
        cx.notify();
    }

    pub(super) fn duplicate_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.menu = None;
        let new_id = crate::model::workspace::new_id("sql");
        if let Err(e) = store(cx).borrow_mut().duplicate(id, &new_id) {
            log::error!("duplicating a SQL connection: {e:#}");
            return;
        }
        self.sync_connections(cx);
        self.select_node(tree::conn_key(&new_id));
    }

    pub(super) fn delete_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.modal = None;
        if self.conn.as_ref().is_some_and(|c| c.id == id) {
            self.clear_active(cx);
        }
        if let Err(e) = store(cx).borrow_mut().delete(id) {
            log::error!("deleting a SQL connection: {e:#}");
        }
        self.sync_connections(cx);
    }

    /// Show a table's CREATE statement, read on the session of its own database.
    pub(super) fn show_ddl_in(&mut self, scope: Option<&tree::Scope>, table: TableRef, window: &mut Window, cx: &mut Context<Self>) {
        let session = match scope {
            Some(s) => self.sessions.get(&(s.conn.clone(), s.db.clone())).cloned(),
            None => self.session.clone(),
        };
        if let Some(session) = session {
            self.show_ddl(session, table, window, cx);
        }
    }

    /// Development aid: SPLITGRID_DEBUG_SQL_TREE=down,right,enter,type:or,filter:user,wait…
    /// presses tree keys (one step every 700 ms) and logs the rows after each.
    fn debug_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(spec) = std::env::var("SPLITGRID_DEBUG_SQL_TREE") else { return };
        let steps: Vec<String> = spec.split(',').map(str::to_owned).collect();
        cx.spawn_in(window, async move |this, cx| {
            for step in steps.into_iter().chain(["wait".to_owned()]) {
                cx.background_executor().timer(std::time::Duration::from_millis(700)).await;
                let ok = this.update_in(cx, |this, window, cx| {
                    match step.split_once(':') {
                        Some(("type", text)) => {
                            for ch in text.chars() {
                                this.type_char(&ch.to_string(), window, cx);
                            }
                        }
                        Some(("filter", text)) => this.set_filter(text.to_owned(), window, cx),
                        _ => {
                            if let Some(nk) = tree::NavKey::parse(&step)
                                && let Some(cmd) = tree::nav(&this.tree_rows.clone(), this.tree.selected.as_deref(), nk, 10)
                            {
                                this.run_cmd(cmd, window, cx);
                            }
                        }
                    }
                    let active = this.active_pair();
                    let rows = this.tree.rows(&this.forest, active.as_ref().map(|(c, d)| (c.as_str(), d.as_str())));
                    this.tree_rows = Rc::new(rows.clone());
                    let sel = this.tree.selected.clone().unwrap_or_default().replace('\u{1f}', "/");
                    log::info!("sql tree [{step}] selected={sel} active={active:?} phase={:?} rows={} tabs={:?}", this.phase, rows.len(), this.tabs.iter().map(|t| t.title.clone()).collect::<Vec<_>>());
                    let at = rows.iter().position(|r| Some(&r.key) == this.tree.selected.as_ref()).unwrap_or(0);
                    for r in rows.iter().take(8).chain(rows.iter().skip(at.saturating_sub(4).max(8)).take(9)) {
                        let mark = if r.expandable { if r.expanded { "v" } else { ">" } } else { " " };
                        let extra = match &r.node {
                            Node::Conn { status, active, .. } => format!(" {status:?}{}", if *active { " ACTIVE" } else { "" }),
                            Node::Database { active, default, .. } => format!("{}{}", if *default { " default" } else { "" }, if *active { " ACTIVE" } else { "" }),
                            Node::Message { text, .. } => format!(" «{text}»"),
                            _ => String::new(),
                        };
                        log::info!("sql tree   {}{mark} {}{extra}", "  ".repeat(r.depth), r.node.name());
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Restore a saved pane: the active connection, and the expanded ones that open without a prompt.
    pub(super) fn restore(&mut self, saved: Option<String>, database: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_connections(cx);
        if let Some(id) = saved
            && self.forest.conn(&id).is_some()
        {
            self.activate(&id, database, window, cx);
        }
        let expanded: Vec<String> = self.forest.conns.iter().filter(|c| self.tree.expanded.contains(&tree::conn_key(&c.id))).map(|c| c.id.clone()).collect();
        for id in expanded {
            let quiet = self.saved(&id, cx).is_some_and(|c| store(cx).borrow().password(&c) != Password::Ask);
            if quiet {
                self.open_connection(&id, window, cx);
            }
        }
        self.debug_tree(window, cx);
    }
}
