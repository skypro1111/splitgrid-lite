//! Drawing the SQL pane: the workbench — selector strip, explorer tree,
//! console, result tabs, value panel — and the modals over it.


use gpui_kit::component::input::{Editor, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;
use sql_core::{Dialect, TableRef};

use super::plan::Plan;
use super::run::TxMode;
use super::tree::Scope;
use super::{Body, DataView, Drag, Modal, Phase, ResultTab, SqlPane, TabStatus, store, ui};
use crate::theme::*;

pub const TREE_ROW_H: f32 = 22.;
const TAB_BAR_H: f32 = 30.;

#[derive(Clone)]
pub enum Menu {
    Connections,
    Databases,
    /// A table or view of the tree (its connection + database), or of the active one.
    Table(Option<Scope>, TableRef),
    /// A connection node.
    Conn(String),
    /// A database node.
    Database(Scope),
}

pub(super) fn splitter(id: &'static str, vertical: bool) -> Stateful<Div> {
    let line = div().absolute().bg(BORDER_SUBTLE);
    let line = if vertical { line.top_0().bottom_0().left(px(2.5)).w(px(1.)) } else { line.left_0().right_0().top(px(2.5)).h(px(1.)) };
    div()
        .id(id)
        .relative()
        .flex_none()
        .map(|d| if vertical { d.w(px(6.)).mx(px(-3.)).h_full().cursor_col_resize() } else { d.h(px(6.)).my(px(-3.)).w_full().cursor_row_resize() })
        .hover(|d| d.bg(hexa(0xffffff, 0.04)))
        .child(line)
}

pub(super) fn muted(text: impl Into<SharedString>) -> Div {
    div().text_size(px(11.)).text_color(TEXT_MUTED).child(text.into())
}

impl SqlPane {
    fn on_drag_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.persist(cx);
            return;
        }
        let b = self.body_bounds.get();
        match drag {
            Drag::Tree => self.tree_w = crate::ui::unpx(ev.position.x - b.left()).clamp(140., crate::ui::unpx(b.size.width) * 0.6),
            Drag::Editor => self.editor_h = crate::ui::unpx(ev.position.y - b.top()).clamp(60., (crate::ui::unpx(b.size.height) - 90.).max(60.)),
            Drag::Value => self.value_w = crate::ui::unpx(b.right() - ev.position.x).clamp(180., crate::ui::unpx(b.size.width) * 0.6),
        }
        cx.notify();
    }

    fn on_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.persist(cx);
        }
    }

    /// Data views' WHERE / ORDER BY inputs, made on first sight (they need the window).
    fn ensure_view_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active else { return };
        let Some(view) = self.tabs.iter_mut().find(|t| t.id == id).and_then(ResultTab::grid_mut).and_then(|g| g.view.as_mut()) else { return };
        if view.inputs.is_some() {
            return;
        }
        let (filter, order) = (view.filter.clone(), view.order.clone());
        let f = cx.new(|cx| InputState::new(window, cx).placeholder("WHERE …").default_value(filter));
        let o = cx.new(|cx| InputState::new(window, cx).placeholder("ORDER BY …").default_value(order));
        let rerun = move |this: &mut SqlPane, _: Entity<InputState>, ev: &InputEvent, cx: &mut Context<SqlPane>| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.browse(id, cx);
            }
        };
        let subs = [cx.subscribe(&f, rerun), cx.subscribe(&o, rerun)];
        if let Some(view) = self.tabs.iter_mut().find(|t| t.id == id).and_then(ResultTab::grid_mut).and_then(|g| g.view.as_mut()) {
            view.inputs = Some((f, o, subs));
        }
    }

    // ── Toolbar ─────────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let conn = self.conn.as_ref();
        let dialect = conn.map(|c| c.dialect.clone()).unwrap_or_default();
        let ready = self.session.is_some();
        let running = self.running.is_some();
        let db_label = self.database.clone().or_else(|| conn.map(|c| c.database.clone()).filter(|d| !d.is_empty())).unwrap_or_else(|| "database".into());
        let selector = |id: &'static str, content: AnyElement| {
            div()
                .id(id)
                .h(px(24.))
                .max_w(px(220.))
                .px(px(8.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .rounded(px(5.))
                .border_1()
                .border_color(border())
                .bg(bg_surface())
                .text_size(px(11.))
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(content)
                .child(ui::lucide("chevron-down", 11., TEXT_MUTED))
        };
        let status: AnyElement = match &self.phase {
            Phase::Connecting => muted("Connecting…").into_any_element(),
            Phase::Failed(e) => div()
                .flex()
                .items_center()
                .gap(px(6.))
                .min_w_0()
                .child(div().text_size(px(11.)).text_color(ACCENT_RED).truncate().child(e.clone()))
                .child(ui::button("sql-retry", "Retry").on_click(cx.listener(|this, _, window, cx| this.retry_active(window, cx))))
                .into_any_element(),
            Phase::Idle => muted("Choose a connection in the explorer").into_any_element(),
            _ if running => muted(format!("Running… {}", if self.queue.is_empty() { String::new() } else { format!("({} queued)", self.queue.len()) })).into_any_element(),
            _ => muted(self.session.as_ref().map(|s| s.server_version.clone()).unwrap_or_default()).truncate().into_any_element(),
        };
        let tx = self.tx;
        let seg = |id: &'static str, label: &'static str, mode: TxMode, cx: &mut Context<Self>| {
            let on = tx.mode == mode;
            div()
                .id(id)
                .px(px(8.))
                .h_full()
                .flex()
                .items_center()
                .text_size(px(10.5))
                .cursor_pointer()
                .when(on, |d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                .when(!on, |d| d.text_color(TEXT_MUTED))
                .on_click(cx.listener(move |this, _, _, cx| this.set_tx_mode(mode, cx)))
                .child(label)
        };
        div()
            .h(px(ui::BAR_H))
            .flex_none()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(ui::BG_BAR)
            .border_b_1()
            .border_color(border())
            .child(
                selector(
                    "sql-conn-select",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .min_w_0()
                        .when(conn.is_some(), |d| d.child(ui::badge(&dialect)))
                        .child(div().truncate().when(conn.is_none(), |d| d.text_color(TEXT_MUTED)).child(conn.map(|c| c.label.clone()).unwrap_or_else(|| "Choose a connection".into())))
                        .into_any_element(),
                )
                    .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                        this.menu = Some((ev.position(), Menu::Connections));
                        cx.notify();
                    })),
            )
            .when(conn.and_then(|c| c.dialect()).is_some_and(|d| d != Dialect::Sqlite), |d| {
                d.child(selector("sql-db-select", div().truncate().child(db_label).into_any_element()).on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                    this.menu = Some((ev.position(), Menu::Databases));
                    if let Some(c) = this.conn.as_ref().map(|c| c.id.clone()) {
                        this.load_databases(&c, cx);
                    }
                    cx.notify();
                })))
            })
            .child(div().w(px(1.)).h(px(16.)).bg(border()).mx(px(2.)))
            .child(ui::disabled(ui::tool_text("sql-run", "play", ACCENT_GREEN, "Run"), !ready).on_click(cx.listener(|this, _, window, cx| this.run_statement(&super::RunStatement, window, cx))))
            .child(ui::disabled(ui::tool_text("sql-run-all", "play", ACCENT_GREEN, "Script"), !ready).on_click(cx.listener(|this, _, window, cx| this.run_script(&super::RunScript, window, cx))))
            .child(ui::disabled(ui::tool_text("sql-cancel", "square", ACCENT_RED, "Cancel"), !running).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))))
            .child(ui::disabled(ui::tool_text("sql-explain", "eye", ACCENT_BLUE, "Explain"), !ready).on_click(cx.listener(|this, _, window, cx| this.explain(false, window, cx))))
            .child(ui::disabled(ui::tool_text("sql-analyze", "eye", ACCENT_YELLOW, "Analyze"), !ready).on_click(cx.listener(|this, _, window, cx| this.explain(true, window, cx))))
            .child(div().w(px(1.)).h(px(16.)).bg(border()).mx(px(2.)))
            .child(
                div()
                    .h(px(22.))
                    .flex()
                    .flex_none()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(border())
                    .overflow_hidden()
                    .child(seg("sql-tx-auto", "Auto", TxMode::Auto, cx))
                    .child(seg("sql-tx-manual", "Manual", TxMode::Manual, cx)),
            )
            .when(tx.open, |d| {
                d.child(div().px(px(5.)).py(px(1.)).rounded(px(3.)).bg(ACCENT_AMBER).text_color(ON_BADGE).text_size(px(10.)).font_weight(FontWeight::BOLD).child("Tx"))
                    .child(ui::tool_text("sql-commit", "check", ACCENT_GREEN, "Commit").on_click(cx.listener(|this, _, _, cx| this.end_tx(true, cx))))
                    .child(ui::tool_text("sql-rollback", "undo-2", ACCENT_RED, "Rollback").on_click(cx.listener(|this, _, _, cx| this.end_tx(false, cx))))
            })
            .child(div().flex_1().min_w_0().flex().justify_end().child(status))
            .child(ui::disabled(ui::tool("sql-refresh", "refresh-cw", TEXT_MUTED), !ready).on_click(cx.listener(|this, _, _, cx| {
                if let Some((id, db)) = this.active_pair() {
                    this.load_catalog(&id, &db, cx);
                }
            })))
    }

    // ── Schema tree ──

    // (tree_view.rs)

    // ── Menus ───────────────────────────────────────────────────────────────

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (at, menu) = self.menu.as_ref()?;
        let d = self.dialect();
        let items: Vec<AnyElement> = match menu {
            Menu::Connections => {
                let mut items: Vec<AnyElement> = store(cx)
                    .borrow()
                    .connections
                    .iter()
                    .map(|c| {
                        let conn = c.clone();
                        let current = self.conn.as_ref().is_some_and(|x| x.id == c.id);
                        ui::menu_item(SharedString::from(format!("sqlm-c-{}", c.id)), c.label.clone())
                            .when(current, |d| d.text_color(ACCENT_GREEN))
                            .on_click(cx.listener(move |this, _, window, cx| this.activate(&conn.id, None, window, cx)))
                            .into_any_element()
                    })
                    .collect();
                items.push(ui::menu_sep().into_any_element());
                items.push(ui::menu_item("sqlm-new", "New connection…").on_click(cx.listener(|this, _, window, cx| this.open_dialog(None, window, cx))).into_any_element());
                items.push(
                    ui::menu_item("sqlm-edit", "Edit connection…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let conn = this.conn.clone();
                            this.open_dialog(conn, window, cx)
                        }))
                        .into_any_element(),
                );
                if let Some(id) = self.conn.as_ref().map(|c| c.id.clone()) {
                    items.push(ui::menu_item("sqlm-disconnect", "Disconnect").on_click(cx.listener(move |this, _, _, cx| this.disconnect_conn(&id, cx))).into_any_element());
                }
                items
            }
            Menu::Databases => {
                let entry = self.conn.as_ref().and_then(|c| self.forest.conn(&c.id));
                let listed = entry.map(|e| e.listed()).unwrap_or_default();
                if listed.is_empty() {
                    vec![div().px(px(12.)).py(px(4.)).child(muted("Loading…")).into_any_element()]
                } else {
                    let id = entry.map(|e| e.id.clone()).unwrap_or_default();
                    let current = self.database.clone();
                    listed
                        .iter()
                        .map(|db| {
                            let (id, name) = (id.clone(), db.clone());
                            ui::menu_item(SharedString::from(format!("sqlm-db-{db}")), db.clone())
                                .when(current.as_deref() == Some(db.as_str()), |d| d.text_color(ACCENT_GREEN))
                                .on_click(cx.listener(move |this, _, window, cx| this.activate(&id, Some(name.clone()), window, cx)))
                                .into_any_element()
                        })
                        .collect()
                }
            }
            Menu::Conn(id) => self.conn_menu(id, cx),
            Menu::Database(scope) => self.db_menu(scope, cx),
            Menu::Table(scope, table) => {
                let (t1, t2, t3, t4) = (table.clone(), table.clone(), table.clone(), table.clone());
                let (s1, s2, s4) = (scope.clone(), scope.clone(), scope.clone());
                vec![
                    ui::menu_item("sqlm-open", "Open data")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            if s1.as_ref().is_none_or(|s| this.set_active(&s.conn, &s.db, cx)) {
                                this.open_table(t1.clone(), String::new(), cx)
                            }
                        }))
                        .into_any_element(),
                    ui::menu_item("sqlm-ddl", "Show DDL").on_click(cx.listener(move |this, _, window, cx| this.show_ddl_in(s2.as_ref(), t2.clone(), window, cx))).into_any_element(),
                    ui::menu_sep().into_any_element(),
                    ui::menu_item("sqlm-copy", "Copy name")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(sql_lang::qualified(&t3, d)));
                            this.menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                    ui::menu_item("sqlm-select", "SELECT * snippet")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.menu = None;
                            if s4.as_ref().is_none_or(|s| this.set_active(&s.conn, &s.db, cx)) {
                                let q = sql_lang::browse_query(&t4, "", "", 100, d);
                                this.insert_snippet(format!("{q};"), window, cx)
                            }
                        }))
                        .into_any_element(),
                ]
            }
        };
        let entity = cx.entity().downgrade();
        Some(ui::popup("sql-menu", *at, items, move |_, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            });
        }))
    }

    // ── Results ─────────────────────────────────────────────────────────────

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let tab = |id: SharedString, active: bool, label: SharedString| {
            div()
                .id(id)
                .h_full()
                .px(px(10.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .max_w(px(220.))
                .text_size(px(11.))
                .border_r_1()
                .border_color(border())
                .cursor_pointer()
                .when(active, |d| d.bg(bg_primary()).text_color(TEXT_PRIMARY).border_t_2().border_color(ACCENT))
                .when(!active, |d| d.text_color(TEXT_MUTED).hover(|d| d.bg(bg_hover())))
                .child(div().truncate().child(label))
        };
        let errors = self.log.last().is_some_and(|l| l.outcome.is_err());
        let output = tab("sql-tab-output".into(), self.active.is_none(), "Output".into())
            .when(errors, |d| d.child(div().size(px(6.)).rounded_full().bg(ACCENT_RED)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.active = None;
                cx.notify();
            }));
        let tabs = self.tabs.iter().map(|t| {
            let (id, pinned) = (t.id, t.pinned);
            let dot = match t.status {
                TabStatus::Running => Some(ACCENT_YELLOW),
                TabStatus::Failed(_) => Some(ACCENT_RED),
                _ => None,
            };
            let pending = t.grid().map(|g| g.pending).unwrap_or(0);
            let title = if pending > 0 { format!("{} *", t.title) } else { t.title.clone() };
            tab(SharedString::from(format!("sql-tab-{id}")), self.active == Some(id), title.into())
                .when_some(dot, |d, c| d.child(div().size(px(6.)).rounded_full().bg(c)))
                .child(
                    div()
                        .id(SharedString::from(format!("sql-pin-{id}")))
                        .flex_none()
                        // Pin icons aren't in the bundled icon set.
                        .text_size(px(9.))
                        .text_color(if pinned { ACCENT } else { TEXT_MUTED })
                        .child(if pinned { "●" } else { "○" })
                        .when(!pinned, |d| d.opacity(0.5).hover(|d| d.opacity(1.)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(t) = this.tabs.iter_mut().find(|t| t.id == id) {
                                t.pinned = !t.pinned;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("sql-close-{id}")))
                        .flex_none()
                        .px(px(2.))
                        .rounded(px(3.))
                        .text_color(TEXT_MUTED)
                        .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                        .child("×")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_tab(id, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = Some(id);
                    cx.notify();
                }))
        });
        div()
            .id("sql-tabs")
            .h(px(TAB_BAR_H))
            .flex_none()
            .flex()
            .bg(ui::BG_BAR)
            .border_b_1()
            .border_color(border())
            .overflow_x_scroll()
            .child(output)
            .children(tabs)
            .child(div().flex_1())
            .child(
                div()
                    .h_full()
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .child(ui::tool("sql-value-toggle", "panel-right", if self.value_panel { ACCENT } else { TEXT_MUTED }).on_click(cx.listener(|this, _, _, cx| {
                        this.value_panel = !this.value_panel;
                        this.value_editor = None;
                        cx.notify();
                    }))),
            )
    }

    fn render_output(&self) -> AnyElement {
        let entries = self.log.iter().rev().map(|e| {
            let (text, color) = match &e.outcome {
                Ok(s) if s.is_empty() => (String::new(), TEXT_MUTED),
                Ok(s) => (s.clone(), TEXT_MUTED),
                Err(s) => (s.clone(), ACCENT_RED),
            };
            div()
                .px(px(10.))
                .py(px(3.))
                .flex()
                .gap(px(10.))
                .font_family(ui::MONO)
                .text_size(px(11.5))
                .border_b_1()
                .border_color(BORDER_SUBTLE)
                .child(div().flex_none().text_color(TEXT_MUTED).child(e.at.clone()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_color(TEXT_PRIMARY).truncate().child(e.sql.clone()))
                        .when(!text.is_empty(), |d| d.child(div().text_color(color).child(text))),
                )
                .when_some(e.ms, |d, ms| d.child(div().flex_none().text_color(TEXT_MUTED).child(ui::elapsed(ms))))
        });
        div()
            .id("sql-output")
            .size_full()
            .overflow_y_scroll()
            .when(self.log.is_empty(), |d| d.child(div().p(px(12.)).child(muted("Run a statement with ⌘↩ (the statement at the caret) or ⇧⌘↩ (the whole script)."))))
            .children(entries)
            .into_any_element()
    }

    fn render_result(&self, tab: &ResultTab, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let id = tab.id;
        let status = match &tab.status {
            TabStatus::Running => "Running…".to_owned(),
            TabStatus::Done { ms, rows, affected, capped } => {
                let mut s = if matches!(tab.body, Body::Text(_)) {
                    String::new()
                } else if *capped {
                    format!("first {} rows", ui::count(*rows))
                } else if *rows == 0 && affected.is_some() {
                    format!("{} rows affected", affected.unwrap_or(0))
                } else {
                    format!("{} row{}", ui::count(*rows), if *rows == 1 { "" } else { "s" })
                };
                if *ms > 0 {
                    s.push_str(&format!(" · {}", ui::elapsed(*ms)));
                }
                s
            }
            TabStatus::Failed(e) => e.clone(),
        };
        let failed = matches!(tab.status, TabStatus::Failed(_));
        let body: AnyElement = match &tab.body {
            Body::Text(editor) => Editor::new(editor).appearance(false).bordered(false).font_family(crate::theme::fonts().editor).text_size(px(crate::theme::fonts().editor_size - 1.)).size_full().into_any_element(),
            Body::Plan(plan) => render_plan(plan),
            Body::Grid(g) => {
                let view_bar = g.view.as_ref().map(|v| self.render_view_bar(id, v, window, cx));
                let edit_bar = g.editable.as_ref().map(|_| {
                    let pending = g.pending;
                    div()
                        .h(px(30.))
                        .flex_none()
                        .px(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .border_b_1()
                        .border_color(border())
                        .child(ui::tool_text(SharedString::from(format!("sqlr-add-{id}")), "plus", ACCENT_GREEN, "Row").on_click(cx.listener(move |this, _, _, cx| {
                            if let Some((_, g)) = this.active_grid() {
                                g.update(cx, |g, cx| g.add_row(cx));
                            }
                        })))
                        .child(ui::tool_text(SharedString::from(format!("sqlr-del-{id}")), "minus", ACCENT_RED, "Delete").on_click(cx.listener(move |this, _, _, cx| {
                            if let Some((_, g)) = this.active_grid() {
                                g.update(cx, |g, cx| g.delete_selected_rows(cx));
                            }
                        })))
                        .child(ui::disabled(ui::tool_text(SharedString::from(format!("sqlr-revert-{id}")), "undo-2", TEXT_MUTED, "Revert"), pending == 0).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some((_, g)) = this.active_grid() {
                                g.update(cx, |g, cx| g.revert(cx));
                            }
                        })))
                        .child(ui::disabled(ui::tool_text(SharedString::from(format!("sqlr-preview-{id}")), "eye", TEXT_MUTED, "Preview SQL"), pending == 0).on_click(cx.listener(move |this, _, _, cx| this.preview_edits(id, cx))))
                        .child(
                            ui::disabled(ui::tool_text(SharedString::from(format!("sqlr-submit-{id}")), "check", ACCENT_GREEN, if pending > 0 { format!("Submit ({pending})") } else { "Submit".into() }), pending == 0)
                                .on_click(cx.listener(move |this, _, _, cx| this.submit_edits(id, cx))),
                        )
                        .when_some(g.error.clone(), |d, e| d.child(div().ml(px(8.)).flex_1().min_w_0().truncate().text_size(px(11.)).text_color(ACCENT_RED).child(e)))
                });
                let grid: AnyElement = match &g.grid {
                    Some(grid) => grid.clone().into_any_element(),
                    None => div().p(px(12.)).child(muted(if failed { "" } else { "Loading…" })).into_any_element(),
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .children(view_bar)
                    .children(edit_bar)
                    .child(div().flex_1().min_h_0().child(grid))
                    .into_any_element()
            }
        };
        let notices = (!tab.notices.is_empty()).then(|| {
            div()
                .id(SharedString::from(format!("sqlr-notices-{id}")))
                .max_h(px(80.))
                .flex_none()
                .overflow_y_scroll()
                .px(px(10.))
                .py(px(4.))
                .border_t_1()
                .border_color(border())
                .font_family(ui::MONO)
                .text_size(px(11.))
                .text_color(ACCENT_YELLOW)
                .children(tab.notices.iter().map(|n| div().child(n.clone())))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(body))
            .children(notices)
            .child(
                div()
                    .h(px(22.))
                    .flex_none()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_t_1()
                    .border_color(border())
                    .bg(ui::BG_BAR)
                    .text_size(px(10.5))
                    .text_color(if failed { ACCENT_RED } else { TEXT_MUTED })
                    .child(div().flex_1().min_w_0().truncate().child(status))
                    .when_some(tab.grid().and_then(|g| g.grid.clone()).filter(|g| g.read(cx).row_count() > 0), |d, grid| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("sqlr-copy-{id}")))
                                .px(px(4.))
                                .rounded(px(3.))
                                .cursor_pointer()
                                .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                                .child("Copy")
                                .on_click(move |_, _, cx| grid.update(cx, |g, cx| g.copy_selection(cx))),
                        )
                    })
                    .when(!tab.sql.is_empty(), |d| d.child(div().max_w(px(320.)).truncate().font_family(ui::MONO).text_color(TEXT_MUTED).child(super::first_line(&tab.sql, 80)))),
            )
            .into_any_element()
    }

    fn render_view_bar(&self, id: u64, view: &DataView, window: &Window, cx: &mut Context<Self>) -> Div {
        let Some((f, o, _)) = &view.inputs else { return div() };
        div()
            .h(px(32.))
            .flex_none()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(border())
            .child(ui::label("Where"))
            .child(div().flex_1().child(ui::field(f, true, window, cx)))
            .child(ui::label("Order by"))
            .child(div().w(relative(0.3)).child(ui::field(o, true, window, cx)))
            .child(ui::tool(SharedString::from(format!("sqlr-reload-{id}")), "refresh-cw", TEXT_MUTED).on_click(cx.listener(move |this, _, _, cx| this.browse(id, cx))))
    }

    fn render_value_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let cell = self.selected_cell(cx);
        let editable = cell.as_ref().is_some_and(|c| c.4);
        let header = div()
            .h(px(TAB_BAR_H))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(border())
            .bg(ui::BG_BAR)
            .child(div().flex_1().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).child("Value"))
            .when_some(cell.as_ref().filter(|c| c.3.is_null()), |d, _| d.child(muted("NULL")))
            .when(editable, |d| d.child(ui::button("sql-value-apply", "Apply").on_click(cx.listener(|this, _, _, cx| this.apply_value(cx)))));
        let body: AnyElement = match &self.value_editor {
            Some((_, editor)) => Editor::new(editor).appearance(false).bordered(false).font_family(crate::theme::fonts().editor).text_size(px(crate::theme::fonts().editor_size - 1.)).size_full().into_any_element(),
            None => div().p(px(12.)).child(muted("Select a cell")).into_any_element(),
        };
        div()
            .w(px(self.value_w))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(editor_bg())
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_results(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.tabs.iter().find(|t| Some(t.id) == self.active) {
            Some(tab) => self.render_result(tab, window, cx),
            None => self.render_output(),
        };
        div().size_full().flex().flex_col().child(self.render_tab_bar(cx)).child(div().flex_1().min_h_0().bg(bg_primary()).child(body)).into_any_element()
    }

    // ── Modals ──────────────────────────────────────────────────────────────

    fn close_modal(cx: &mut Context<Self>) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        cx.listener(|this, _: &ClickEvent, _, cx| {
            this.modal = None;
            cx.notify();
        })
    }

    fn render_modal(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let actions = |children: Vec<AnyElement>| div().flex().justify_end().gap(px(8.)).children(children);
        let el = match self.modal.as_ref()? {
            Modal::Dialog(d) => ui::modal("sql-dialog", 520., d.clone()),
            Modal::Password { id, db, input } => {
                let label = store(cx).borrow().get(id).map(|c| c.label.clone()).unwrap_or_default();
                let (id1, db1, input1) = (id.clone(), db.clone(), input.clone());
                ui::modal(
                    "sql-password",
                    380.,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(ui::modal_title(format!("Password for {label}")))
                        .child(muted("The saved password is sealed in the Keychain by the other SplitGrid app. Enter it once; it is saved for both apps."))
                        .child(ui::field(input, false, window, cx))
                        .child(actions(vec![
                            ui::button("sql-pw-cancel", "Cancel").on_click(Self::close_modal(cx)).into_any_element(),
                            ui::primary(ui::button("sql-pw-ok", "Connect"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let typed = input1.read(cx).value().to_string();
                                    this.submit_password(id1.clone(), db1.clone(), typed, window, cx)
                                }))
                                .into_any_element(),
                        ])),
                )
            }
            Modal::Danger { lines, .. } => ui::modal(
                "sql-danger",
                480.,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(ui::modal_title("Run a dangerous statement?"))
                    .children(lines.iter().map(|l| div().text_size(px(11.5)).text_color(ACCENT_YELLOW).font_family(ui::MONO).child(l.clone())))
                    .child(actions(vec![
                        ui::button("sql-danger-cancel", "Cancel").on_click(Self::close_modal(cx)).into_any_element(),
                        ui::button("sql-danger-run", "Run anyway")
                            .bg(ACCENT_RED)
                            .border_color(ACCENT_RED)
                            .text_color(hex(0xffffff))
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_danger(window, cx)))
                            .into_any_element(),
                    ])),
            ),
            Modal::Params { inputs, .. } => ui::modal(
                "sql-params",
                420.,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(ui::modal_title("Parameters"))
                    .children(inputs.iter().map(|(name, input)| {
                        div().flex().items_center().gap(px(10.)).child(div().w(px(100.)).flex_none().truncate().font_family(ui::MONO).text_size(px(12.)).child(name.clone())).child(ui::field(input, true, window, cx))
                    }))
                    .child(actions(vec![
                        ui::button("sql-params-cancel", "Cancel").on_click(Self::close_modal(cx)).into_any_element(),
                        ui::primary(ui::button("sql-params-run", "Run")).on_click(cx.listener(|this, _, _, cx| this.submit_params(cx))).into_any_element(),
                    ])),
            ),
            Modal::Preview { tab, sql } => {
                let tab = *tab;
                ui::modal(
                    "sql-preview",
                    600.,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(ui::modal_title("Pending changes"))
                        .child(
                            div()
                                .id("sql-preview-text")
                                .max_h(px(360.))
                                .overflow_y_scroll()
                                .p(px(10.))
                                .rounded(px(6.))
                                .bg(editor_bg())
                                .border_1()
                                .border_color(border())
                                .font_family(ui::MONO)
                                .text_size(px(11.5))
                                .children(sql.lines().map(|l| div().child(l.to_owned()))),
                        )
                        .child(actions(vec![
                            ui::button("sql-preview-copy", "Copy")
                                .on_click({
                                    let sql = sql.clone();
                                    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()))
                                })
                                .into_any_element(),
                            ui::button("sql-preview-close", "Close").on_click(Self::close_modal(cx)).into_any_element(),
                            ui::primary(ui::button("sql-preview-submit", "Submit")).on_click(cx.listener(move |this, _, _, cx| this.submit_edits(tab, cx))).into_any_element(),
                        ])),
                )
            }
            Modal::OpenTx(after) => {
                let (a1, a2, a3) = (after.clone(), after.clone(), after.clone());
                ui::modal(
                    "sql-open-tx",
                    400.,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(ui::modal_title("Uncommitted transaction"))
                        .child(muted("The transaction has changes that are not committed yet."))
                        .child(actions(vec![
                            ui::button("sql-tx-cancel", "Cancel").on_click(cx.listener(move |this, _, window, cx| this.settle_tx(None, a1.clone(), window, cx))).into_any_element(),
                            ui::button("sql-tx-rollback", "Rollback").on_click(cx.listener(move |this, _, window, cx| this.settle_tx(Some(false), a2.clone(), window, cx))).into_any_element(),
                            ui::primary(ui::button("sql-tx-commit", "Commit")).on_click(cx.listener(move |this, _, window, cx| this.settle_tx(Some(true), a3.clone(), window, cx))).into_any_element(),
                        ])),
                )
            }
            Modal::Delete { id, label } => {
                let id = id.clone();
                ui::modal(
                    "sql-delete",
                    380.,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(ui::modal_title(format!("Delete “{label}”?")))
                        .child(muted("The saved connection is removed for both SplitGrid apps."))
                        .child(actions(vec![
                            ui::button("sql-del-cancel", "Cancel").on_click(Self::close_modal(cx)).into_any_element(),
                            ui::button("sql-del-ok", "Delete")
                                .bg(ACCENT_RED)
                                .border_color(ACCENT_RED)
                                .text_color(hex(0xffffff))
                                .on_click(cx.listener(move |this, _, _, cx| this.delete_connection(&id, cx)))
                                .into_any_element(),
                        ])),
                )
            }
        };
        Some(el.into_any_element())
    }
}

/// The Explain tab for a Postgres plan: one row per node, with cost and time bars.
fn render_plan(plan: &Plan) -> AnyElement {
    let max_cost = plan.max_cost().max(1.);
    let total = plan.total_ms();
    let bar = |frac: f64, color: Rgba| {
        div().w(px(80.)).h(px(6.)).flex_none().rounded(px(3.)).bg(bg_hover()).child(div().h_full().rounded(px(3.)).bg(color).w(px((frac.clamp(0., 1.) * 80.) as f32)))
    };
    let num = |v: f64| if v >= 100. || v.fract() == 0. { format!("{v:.0}") } else { format!("{v:.2}") };
    let rows = plan.rows().into_iter().enumerate().map(|(i, (depth, n))| {
        let slow = plan.is_slow(n);
        let off = Plan::misestimated(n);
        let rows = match n.actual_rows {
            Some(a) => format!("{} / {}", num(n.est_rows), num(a)),
            None => num(n.est_rows),
        };
        div()
            .id(("sql-plan-row", i))
            .px(px(10.))
            .py(px(4.))
            .flex()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(BORDER_SUBTLE)
            .when(slow, |d| d.bg(hexa(0xf14c4c, 0.10)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(depth as f32 * 16.))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .items_center()
                            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(if slow { ACCENT_RED } else { TEXT_PRIMARY }).child(n.node_type.clone()))
                            .child(div().text_size(px(11.)).text_color(ACCENT_BLUE).truncate().child(n.detail.clone())),
                    )
                    .when_some(n.condition.clone(), |d, c| d.child(div().text_size(px(10.5)).font_family(ui::MONO).text_color(TEXT_MUTED).truncate().child(c))),
            )
            .child(div().w(px(110.)).flex_none().text_size(px(11.)).text_color(if off { ACCENT_AMBER } else { TEXT_MUTED }).child(rows))
            .child(div().w(px(70.)).flex_none().text_size(px(11.)).text_color(TEXT_MUTED).child(num(n.total_cost)))
            .child(bar(n.total_cost / max_cost, ACCENT_BLUE))
            .child(div().w(px(70.)).flex_none().text_size(px(11.)).text_color(TEXT_MUTED).child(n.self_ms.map(|t| format!("{} ms", num(t))).unwrap_or_default()))
            .child(bar(match (n.self_ms, total) {
                (Some(own), Some(all)) if all > 0. => own / all,
                _ => 0.,
            }, if slow { ACCENT_RED } else { ACCENT_GREEN }))
    });
    let head = |t: &'static str, w: f32| div().w(px(w)).flex_none().child(t);
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(24.))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .border_b_1()
                .border_color(border())
                .text_size(px(10.))
                .font_weight(FontWeight::BOLD)
                .text_color(TEXT_MUTED)
                .child(div().flex_1().child("NODE"))
                .child(head("ROWS EST / ACTUAL", 110.))
                .child(head("COST", 70.))
                .child(head("", 80.))
                .child(head("SELF TIME", 70.))
                .child(head("", 80.)),
        )
        .child(div().id("sql-plan").flex_1().min_h_0().overflow_y_scroll().children(rows))
        .child(
            div()
                .h(px(22.))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(12.))
                .text_size(px(10.5))
                .text_color(TEXT_MUTED)
                .border_t_1()
                .border_color(border())
                .when_some(plan.planning_ms, |d, t| d.child(format!("Planning {} ms", num(t))))
                .when_some(plan.execution_ms, |d, t| d.child(format!("Execution {} ms", num(t))))
                .when(plan.execution_ms.is_none(), |d| d.child("Estimates only — Analyze runs the statement for real times")),
        )
        .into_any_element()
}

impl Render for SqlPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_value_editor(window, cx);
        self.ensure_view_inputs(window, cx);
        let content: AnyElement = {
            let bounds = self.body_bounds.clone();
            let recorder = canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full();
            let editor = div()
                .h(px(self.editor_h))
                .flex_none()
                .key_context("SqlEditor")
                .bg(editor_bg())
                .pt(px(4.))
                .child(Editor::new(&self.editor).appearance(false).bordered(false).font_family(crate::theme::fonts().editor).text_size(px(crate::theme::fonts().editor_size)).line_height(px((crate::theme::fonts().editor_size * 1.46).round())).size_full());
            let right = div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(editor)
                .child(splitter("sql-split-editor", false).on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.drag = Some(Drag::Editor);
                    cx.stop_propagation();
                })))
                .child(div().flex_1().min_h_0().child(self.render_results(window, cx)));
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_toolbar(cx))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .child(recorder)
                        .child(self.render_tree(window, cx))
                        .child(splitter("sql-split-tree", true).on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            this.drag = Some(Drag::Tree);
                            cx.stop_propagation();
                        })))
                        .child(right)
                        .when(self.value_panel, |d| {
                            d.child(splitter("sql-split-value", true).on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                                this.drag = Some(Drag::Value);
                                cx.stop_propagation();
                            })))
                            .child(self.render_value_panel(cx))
                        }),
                )
                .into_any_element()
        };
        div()
            .id("sql-pane")
            .key_context("SqlPane")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .bg(bg_primary())
            .text_color(TEXT_PRIMARY)
            .on_action(cx.listener(Self::run_statement))
            .on_action(cx.listener(Self::run_script))
            .on_action(cx.listener(Self::cancel_run))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && (this.modal.is_some() || this.menu.is_some()) {
                    this.modal = None;
                    this.menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(content)
            .children(self.render_modal(window, cx))
            .children(self.render_menu(cx))
    }
}
