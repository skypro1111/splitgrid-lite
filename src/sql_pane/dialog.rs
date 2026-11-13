//! New / Edit connection (dialogs/ConnectionDialog.tsx): the dialect as
//! segmented buttons, then the fields that dialect needs — a file for SQLite;
//! host, port, user, password, database and SSL for the servers — and Test,
//! Save, Connect.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;
use sql_core::Dialect;

use super::session::Session;
use super::store::{SavedSql, dialect_name};
use super::ui;
use crate::theme::*;

pub enum DialogEvent {
    Cancel,
    /// Saved; `connect` when the user asked to connect too.
    Saved { id: String, connect: bool },
}

#[derive(Clone, PartialEq)]
enum Test {
    Idle,
    Running,
    Ok(String),
    Failed(String),
}

pub struct ConnectionDialog {
    editing: Option<SavedSql>,
    dialect: Dialect,
    ssl: bool,
    label: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    database: Entity<InputState>,
    file: Entity<InputState>,
    test: Test,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for ConnectionDialog {}

impl ConnectionDialog {
    pub fn new(existing: Option<SavedSql>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let e = existing.as_ref();
        let dialect = e.and_then(SavedSql::dialect).unwrap_or(Dialect::Postgres);
        let mut text = |value: String, ph: &str| {
            let ph = SharedString::from(ph.to_owned());
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(value))
        };
        let ssl = e.is_some_and(|c| c.ssl);
        let label = text(e.map(|c| c.label.clone()).unwrap_or_default(), "Label");
        let host = text(e.map(|c| c.host.clone()).unwrap_or_else(|| "127.0.0.1".into()), "127.0.0.1");
        let port = text(e.map(|c| c.port).filter(|p| *p != 0).map(|p| p.to_string()).unwrap_or_default(), &dialect.default_port().to_string());
        let user = text(e.map(|c| c.user.clone()).unwrap_or_default(), "User");
        let database = text(e.map(|c| c.database.clone()).filter(|_| !dialect.is_file()).unwrap_or_default(), "Database");
        let file = text(e.and_then(|c| c.file()).map(str::to_owned).unwrap_or_default(), "Database file (e.g. /path/to/app.sqlite)");
        let ph = if e.is_some() { "Password (unchanged if blank)" } else { "Password" };
        let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(ph));
        // Enter in any field saves and connects.
        let subs = [&label, &host, &port, &user, &password, &database, &file]
            .into_iter()
            .map(|i| {
                cx.subscribe_in(i, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save(true, cx),
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                })
            })
            .collect();
        label.update(cx, |i, cx| i.focus(window, cx));
        Self { editing: existing, dialect, ssl, label, host, port, user, password, database, file, test: Test::Idle, error: None, _subs: subs }
    }

    fn value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    /// The form as a saved connection, and the password typed (None: blank).
    fn form(&self, cx: &App) -> Result<(SavedSql, Option<String>), String> {
        let d = self.dialect;
        let mut conn = self.editing.clone().unwrap_or_else(|| SavedSql { id: crate::model::workspace::new_id("sql"), ..Default::default() });
        // Editing keeps the other app's spelling (mariadb) unless the dialect changed.
        if conn.dialect() != Some(d) {
            conn.dialect = dialect_name(d).into();
        }
        let port_text = Self::value(&self.port, cx);
        if d.is_file() {
            let file = crate::grid::expand_tilde(&Self::value(&self.file, cx));
            if file.is_empty() {
                return Err("Choose the database file".into());
            }
            conn.file_path = Some(file.clone());
            // The Electron app reads SQLite's file from `database` when `filePath` is absent.
            conn.database = file;
            (conn.host, conn.port, conn.user, conn.ssl) = (String::new(), 0, String::new(), false);
        } else {
            conn.host = Self::value(&self.host, cx);
            if conn.host.is_empty() {
                return Err("Enter the host".into());
            }
            conn.port = if port_text.is_empty() { d.default_port() } else { port_text.parse().map_err(|_| format!("Port must be a number, not “{port_text}”"))? };
            conn.user = Self::value(&self.user, cx);
            conn.database = Self::value(&self.database, cx);
            conn.ssl = self.ssl;
            conn.file_path = None;
        }
        conn.label = Some(Self::value(&self.label, cx)).filter(|l| !l.is_empty()).unwrap_or_else(|| {
            if d.is_file() {
                std::path::Path::new(&conn.database).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            } else if conn.database.is_empty() {
                conn.host.clone()
            } else {
                format!("{}@{}", conn.database, conn.host)
            }
        });
        let password = self.password.read(cx).value().to_string();
        Ok((conn, Some(password).filter(|p| !p.is_empty())))
    }

    fn save(&mut self, connect: bool, cx: &mut Context<Self>) {
        let (conn, password) = match self.form(cx) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let id = conn.id.clone();
        // A new server connection without a password stores none; an edit keeps the old one.
        let password = match (&password, self.editing.is_some()) {
            (Some(p), _) => Some(p.as_str()),
            (None, true) => None,
            (None, false) => Some(""),
        };
        if let Err(e) = super::store(cx).borrow_mut().upsert(conn, password) {
            self.error = Some(format!("Could not save: {e:#}"));
            cx.notify();
            return;
        }
        cx.emit(DialogEvent::Saved { id, connect });
    }

    fn run_test(&mut self, cx: &mut Context<Self>) {
        let (conn, typed) = match self.form(cx) {
            Ok(f) => f,
            Err(e) => {
                self.test = Test::Failed(e);
                cx.notify();
                return;
            }
        };
        // Blank while editing: test with the stored password.
        let password = typed.or_else(|| match self.editing.as_ref().map(|c| super::store(cx).borrow().password(c)) {
            Some(super::store::Password::Known(p)) => Some(p),
            _ => None,
        });
        let cfg = conn.config(password.unwrap_or_default());
        self.test = Test::Running;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = Session::test(cfg).await;
            let _ = this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(v) if v.is_empty() => Test::Ok("Connection succeeded".into()),
                    Ok(v) => Test::Ok(format!("Connected — {v}")),
                    Err(e) => Test::Failed(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn pick_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.file.update(cx, |i, cx| i.set_value(path.to_string_lossy().into_owned(), window, cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn set_dialect(&mut self, d: Dialect, window: &mut Window, cx: &mut Context<Self>) {
        if d == self.dialect {
            return;
        }
        let old_default = self.dialect.default_port().to_string();
        self.dialect = d;
        let port = Self::value(&self.port, cx);
        // A port that was the old default follows the new dialect.
        if port.is_empty() || port == old_default {
            let ph = SharedString::from(d.default_port().to_string());
            self.port.update(cx, |i, cx| {
                i.set_value("", window, cx);
                i.set_placeholder(ph, window, cx);
            });
        }
        self.test = Test::Idle;
        cx.notify();
    }
}

impl Render for ConnectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.dialect;
        let dialects = div().flex().gap(px(6.)).children(Dialect::ALL.into_iter().map(|opt| {
            let name = dialect_name(opt);
            let (_, color) = ui::dialect_badge(name);
            let active = opt == d;
            div()
                .id(SharedString::from(format!("dialect-{name}")))
                .flex_1()
                .h(px(30.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(if active { color } else { BORDER })
                .bg(if active { BG_HOVER } else { BG_PRIMARY })
                .cursor_pointer()
                .hover(|d| d.bg(BG_HOVER))
                .child(ui::badge(name))
                .child(div().text_size(px(11.)).truncate().child(opt.label()))
                .on_click(cx.listener(move |this, _, window, cx| this.set_dialect(opt, window, cx)))
        }));

        let fields = if d.is_file() {
            div().flex().flex_col().gap(px(8.)).child(ui::field(&self.label, false, window, cx)).child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().flex_1().child(ui::field(&self.file, true, window, cx)))
                    .child(ui::button("browse", "Browse…").on_click(cx.listener(|this, _, window, cx| this.pick_file(window, cx)))),
            )
        } else {
            let ssl = self.ssl;
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(ui::field(&self.label, false, window, cx))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(div().flex_1().child(ui::field(&self.host, true, window, cx)))
                        .child(div().w(px(90.)).child(ui::field(&self.port, true, window, cx))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(div().flex_1().child(ui::field(&self.user, false, window, cx)))
                        .child(div().flex_1().child(ui::field(&self.password, false, window, cx))),
                )
                .child(ui::field(&self.database, false, window, cx))
                .child(Checkbox::new("sql-ssl").checked(ssl).label("SSL").on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.ssl = *checked;
                    cx.notify();
                })))
        };

        let (message, color) = match (&self.error, &self.test) {
            (Some(e), _) => (Some(e.clone()), ACCENT_RED),
            (None, Test::Ok(m)) => (Some(m.clone()), ACCENT_GREEN),
            (None, Test::Failed(m)) => (Some(m.clone()), ACCENT_RED),
            (None, Test::Running) => (Some("Testing…".into()), TEXT_MUTED),
            (None, Test::Idle) => (None, TEXT_MUTED),
        };
        let testing = self.test == Test::Running;

        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(ui::modal_title(if self.editing.is_some() { "Edit Connection" } else { "New Connection" }))
            .child(dialects)
            .child(fields)
            .when_some(message, |el, m| el.child(div().text_size(px(11.)).text_color(color).child(m)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(ui::disabled(ui::button("test", if testing { "Testing…" } else { "Test Connection" }), testing).on_click(cx.listener(move |this, _, _, cx| {
                        if !testing {
                            this.run_test(cx)
                        }
                    })))
                    .child(div().flex_1())
                    .child(ui::button("cancel", "Cancel").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Cancel))))
                    .child(ui::button("save", if self.editing.is_some() { "Save Changes" } else { "Save" }).on_click(cx.listener(|this, _, _, cx| this.save(false, cx))))
                    .child(ui::primary(ui::button("connect", "Connect")).on_click(cx.listener(|this, _, _, cx| this.save(true, cx)))),
            )
    }
}
