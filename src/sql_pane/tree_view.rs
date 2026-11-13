//! Drawing the database explorer (ConnectionTree.tsx): a header with New
//! connection and Refresh, the filter field, and the virtual list of rows —
//! plus the connection and database context menus.

use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;
use sql_core::TableKind;

use super::tree::{self, ConnStatus, Node, Scope, TreeRow};
use super::view::{Menu, TREE_ROW_H, muted};
use super::{Modal, SqlPane, store, ui};
use crate::theme::*;

const TABLE: Rgba = hex(0x4ec9b0);
const VIEW: Rgba = hex(0x569cd6);
const SCHEMA: Rgba = hex(0xd7ba7d);
const COLUMN: Rgba = hex(0x9cdcfe);
const INDEX: Rgba = hex(0xdcdcaa);
const ROUTINE: Rgba = hex(0xc586c0);
const DATABASE: Rgba = hex(0x6a9bd8);

fn icon(name: &str, color: Rgba) -> AnyElement {
    svg().path(crate::assets::icon(name)).size(px(13.)).flex_none().text_color(color).into_any_element()
}

fn spinner(id: SharedString) -> AnyElement {
    ui::lucide("loader-circle", 12., TEXT_MUTED)
        .with_animation(id, Animation::new(Duration::from_millis(900)).repeat(), |svg, t| svg.with_transformation(Transformation::rotate(percentage(t))))
        .into_any_element()
}

impl SqlPane {
    pub(super) fn render_tree(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let active = self.active_pair();
        let rows = Rc::new(self.tree.rows(&self.forest, active.as_ref().map(|(c, d)| (c.as_str(), d.as_str()))));
        self.tree_rows = rows.clone();
        let focused = self.tree_focus.is_focused(window);
        let body: AnyElement = if self.forest.conns.is_empty() {
            div()
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(muted("No saved database connections yet."))
                .child(div().flex().child(ui::primary(ui::button("sql-new-conn", "+ New connection")).on_click(cx.listener(|this, _, window, cx| this.open_dialog(None, window, cx)))))
                .into_any_element()
        } else if rows.is_empty() {
            div().p(px(10.)).child(muted("Nothing matches the filter (only loaded databases are searched).")).into_any_element()
        } else {
            let count = rows.len();
            uniform_list(
                "sql-tree",
                count,
                cx.processor(move |this, range: Range<usize>, _, cx| range.map(|i| this.render_tree_row(&rows[i], focused, cx)).collect::<Vec<_>>()),
            )
            .track_scroll(&self.tree.scroll)
            .size_full()
            .into_any_element()
        };
        let header = div()
            .h(px(28.))
            .flex_none()
            .pl(px(10.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(2.))
            .child(div().flex_1().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(TEXT_MUTED).child("DATABASE EXPLORER"))
            .child(ui::tool("sql-tree-new", "plus", TEXT_MUTED).on_click(cx.listener(|this, _, window, cx| this.open_dialog(None, window, cx))))
            .child(ui::tool("sql-tree-refresh", "refresh-cw", TEXT_MUTED).on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx))));
        let filter = self.tree_filter.as_ref().map(|input| {
            div()
                .flex_none()
                .px(px(6.))
                .pb(px(6.))
                .flex()
                .items_center()
                .child(div().relative().w_full().child(ui::field(input, false, window, cx)))
        });
        div()
            .id("sql-tree-panel")
            .key_context("SqlTree")
            .w(px(self.tree_w))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(BG_PRIMARY)
            .child(header)
            .children(filter)
            .child(
                div()
                    .id("sql-tree-body")
                    .track_focus(&self.tree_focus)
                    .flex_1()
                    .min_h_0()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if this.modal.is_none() && this.menu.is_none() && this.tree_key(ev, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.tree_focus.focus(window, cx)))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| this.tree_focus.focus(window, cx)))
                    .child(body),
            )
            .into_any_element()
    }

    fn render_tree_row(&mut self, row: &TreeRow, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.tree.selected.as_deref() == Some(row.key.as_str());
        let key = row.key.clone();
        let id = SharedString::from(format!("sqlt-{}", key.replace('\u{1f}', "/")));
        let mut bold = false;
        let mut name_color = TEXT_PRIMARY;
        let mut trailing: Option<AnyElement> = None;
        let (icon, name, detail): (AnyElement, String, Option<String>) = match &row.node {
            Node::Conn { dialect, label, address, status, active, .. } => {
                bold = *active;
                if *active {
                    name_color = ACCENT_GREEN;
                }
                let detail = match status {
                    ConnStatus::Failed(_) => {
                        name_color = ACCENT_RED;
                        "connection failed".to_owned()
                    }
                    _ => address.clone(),
                };
                if *status == ConnStatus::Connecting {
                    trailing = Some(spinner(SharedString::from(format!("{id}-spin"))));
                } else if *status == ConnStatus::Open {
                    trailing = Some(div().size(px(6.)).flex_none().rounded_full().bg(if *active { ACCENT_GREEN } else { TEXT_MUTED }).into_any_element());
                }
                (ui::badge(dialect).into_any_element(), label.clone(), Some(detail))
            }
            Node::Database { scope, default, active, loading } => {
                bold = *active;
                if *active {
                    name_color = ACCENT_GREEN;
                }
                if *loading {
                    trailing = Some(spinner(SharedString::from(format!("{id}-spin"))));
                } else if *active {
                    trailing = Some(div().size(px(6.)).flex_none().rounded_full().bg(ACCENT_GREEN).into_any_element());
                }
                (icon("sql-database", DATABASE), scope.db.clone(), default.then(|| "default".to_owned()))
            }
            Node::Schema { name, default, .. } => {
                bold = *default;
                (icon("sql-schema", SCHEMA), name.clone(), default.then(|| "default".to_owned()))
            }
            Node::Group { group, count } => (ui::lucide("folder", 13., TEXT_MUTED).into_any_element(), group.label().to_owned(), Some(count.to_string())),
            Node::Table { kind, table, rows, .. } => {
                let ic = match kind {
                    TableKind::View | TableKind::MaterializedView => icon("sql-view", VIEW),
                    _ => icon("sql-table", TABLE),
                };
                (ic, table.name.clone(), rows.map(tree::rows_estimate))
            }
            Node::Column { column, pk, fk } => {
                let ic = if *pk {
                    icon("sql-key", ACCENT_YELLOW)
                } else if *fk {
                    icon("sql-link", ACCENT_BLUE)
                } else {
                    icon("sql-column", COLUMN)
                };
                let mut t = column.type_name.clone();
                if !column.nullable {
                    t.push_str(" not null");
                }
                (ic, column.name.clone(), Some(t))
            }
            Node::Folder { folder, count } => (ui::lucide("folder", 13., TEXT_MUTED).into_any_element(), folder.label().to_owned(), Some(count.to_string())),
            Node::Index { name, columns, unique } => (icon("sql-index", INDEX), name.clone(), Some(format!("{}({})", if *unique { "unique " } else { "" }, columns.join(", ")))),
            Node::ForeignKey { name, columns, target } => (icon("sql-link", ACCENT_BLUE), name.clone(), Some(format!("({}) → {target}", columns.join(", ")))),
            Node::Routine(r) => (icon("sql-function", ROUTINE), r.name.clone(), Some(r.signature.clone())),
            Node::Sequence(s) => (icon("sql-sequence", INDEX), s.clone(), None),
            Node::Message { text, error, busy, retry } => {
                name_color = if *error { ACCENT_RED } else { TEXT_MUTED };
                let ic = if *busy {
                    spinner(SharedString::from(format!("{id}-spin")))
                } else if *error {
                    ui::lucide("circle-alert", 12., ACCENT_RED).into_any_element()
                } else {
                    div().w(px(12.)).flex_none().into_any_element()
                };
                if let Some((c, d)) = retry.clone() {
                    let label = if *error { "Retry" } else { "Connect" };
                    trailing = Some(
                        div()
                            .id(SharedString::from(format!("{id}-retry")))
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(3.))
                            .text_size(px(10.5))
                            .text_color(ACCENT)
                            .cursor_pointer()
                            .hover(|d| d.bg(BG_HOVER))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.retry(&c, d.clone(), window, cx);
                            }))
                            .into_any_element(),
                    );
                }
                (ic, text.clone(), None)
            }
        };
        let menu = match &row.node {
            Node::Conn { id, .. } => Some(Menu::Conn(id.clone())),
            Node::Database { scope, .. } => Some(Menu::Database(scope.clone())),
            Node::Table { scope, table, .. } => Some(Menu::Table(Some(scope.clone()), table.clone())),
            _ => None,
        };
        let (k_click, k_chevron, k_menu) = (key.clone(), key.clone(), key.clone());
        let (expandable, expanded) = (row.expandable, row.expanded);
        let is_message = matches!(row.node, Node::Message { .. });
        div()
            .id(id.clone())
            .h(px(TREE_ROW_H))
            .w_full()
            .pl(px(4. + row.depth as f32 * 14.))
            .pr(px(6.))
            .flex()
            .items_center()
            .gap(px(5.))
            .text_size(px(12.))
            .text_color(TEXT_PRIMARY)
            .cursor_pointer()
            .when(selected && focused, |d| d.bg(hexa(0x228df2, 0.30)))
            .when(selected && !focused, |d| d.bg(hexa(0xffffff, 0.08)))
            .when(!selected, |d| d.hover(|d| d.bg(BG_HOVER)))
            .child(
                div()
                    .id(SharedString::from(format!("{id}-chev")))
                    .size(px(14.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(expandable, |d| d.child(ui::lucide(if expanded { "chevron-down" } else { "chevron-right" }, 12., TEXT_MUTED)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.tree_focus.focus(window, cx);
                        this.tree.selected = Some(k_chevron.clone());
                        this.set_expanded(&k_chevron, !expanded, window, cx);
                    })),
            )
            .child(icon)
            .child(
                div()
                    .flex_none()
                    .max_w(relative(if is_message { 0.9 } else { 0.75 }))
                    .truncate()
                    .text_color(name_color)
                    .when(bold, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .when(is_message, |d| d.text_size(px(11.)))
                    .child(name),
            )
            .child(div().flex_1().min_w_0().truncate().text_size(px(10.5)).text_color(TEXT_MUTED).children(detail))
            .children(trailing)
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.tree_focus.focus(window, cx);
                this.tree.selected = Some(k_click.clone());
                if ev.click_count() >= 2 {
                    this.activate_node(&k_click, window, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.tree.selected = Some(k_menu.clone());
                    if let Some(m) = menu.clone() {
                        this.menu = Some((ev.position, m));
                    }
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// Right-click on a connection.
    pub(super) fn conn_menu(&self, id: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(entry) = self.forest.conn(id) else { return Vec::new() };
        let active = self.conn.as_ref().is_some_and(|c| c.id == id);
        let saved = store(cx).borrow().get(id).cloned();
        let label = entry.label.clone();
        let item = |name: &'static str, text: &'static str| ui::menu_item(SharedString::from(format!("sqlm-{name}")), text);
        let mut items: Vec<AnyElement> = Vec::new();
        let i = id.to_owned();
        match entry.status {
            ConnStatus::Open | ConnStatus::Connecting => {
                if !active && entry.status == ConnStatus::Open {
                    items.push(item("use", "Use in console").on_click(cx.listener({
                        let i = i.clone();
                        move |this, _, window, cx| this.activate(&i, None, window, cx)
                    })).into_any_element());
                }
                items.push(item("disconnect", "Disconnect").on_click(cx.listener({
                    let i = i.clone();
                    move |this, _, _, cx| this.disconnect_conn(&i, cx)
                })).into_any_element());
                if entry.status == ConnStatus::Open {
                    items.push(item("refresh", "Refresh").on_click(cx.listener({
                        let i = i.clone();
                        move |this, _, _, cx| {
                            this.menu = None;
                            this.refresh_conn(&i, cx)
                        }
                    })).into_any_element());
                }
            }
            ConnStatus::Idle | ConnStatus::Failed(_) => {
                items.push(item("connect", "Connect").on_click(cx.listener({
                    let i = i.clone();
                    move |this, _, window, cx| this.activate(&i, None, window, cx)
                })).into_any_element());
            }
        }
        items.push(ui::menu_sep().into_any_element());
        items.push(item("edit", "Edit…").on_click(cx.listener(move |this, _, window, cx| this.open_dialog(saved.clone(), window, cx))).into_any_element());
        items.push(item("duplicate", "Duplicate").on_click(cx.listener({
            let i = i.clone();
            move |this, _, _, cx| this.duplicate_connection(&i, cx)
        })).into_any_element());
        let copy = label.clone();
        items.push(
            item("copy-conn", "Copy name")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    this.menu = None;
                    cx.notify();
                }))
                .into_any_element(),
        );
        items.push(ui::menu_sep().into_any_element());
        items.push(
            item("delete", "Delete…")
                .text_color(ACCENT_RED)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    this.modal = Some(Modal::Delete { id: i.clone(), label: label.clone() });
                    cx.notify();
                }))
                .into_any_element(),
        );
        items
    }

    /// Right-click on a database.
    pub(super) fn db_menu(&self, scope: &Scope, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let active = self.active_pair().is_some_and(|(c, d)| c == scope.conn && d == scope.db);
        let open = self.sessions.contains_key(&(scope.conn.clone(), scope.db.clone()));
        let mut items = Vec::new();
        if !active {
            let s = scope.clone();
            items.push(ui::menu_item("sqlm-db-use", "Use in console").on_click(cx.listener(move |this, _, window, cx| this.activate(&s.conn, Some(s.db.clone()), window, cx))).into_any_element());
        }
        let s = scope.clone();
        items.push(
            ui::menu_item("sqlm-db-refresh", if open { "Refresh" } else { "Connect" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.menu = None;
                    this.tree.expand(tree::db_key(&s.conn, &s.db));
                    if this.sessions.contains_key(&(s.conn.clone(), s.db.clone())) {
                        this.load_catalog(&s.conn, &s.db, cx);
                    } else {
                        this.retry(&s.conn, Some(s.db.clone()), window, cx);
                    }
                }))
                .into_any_element(),
        );
        let name = scope.db.clone();
        items.push(
            ui::menu_item("sqlm-db-copy", "Copy name")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                    this.menu = None;
                    cx.notify();
                }))
                .into_any_element(),
        );
        items
    }
}
