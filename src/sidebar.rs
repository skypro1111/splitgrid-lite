//! The sidebar, drawn like the Electron app's WorkspaceSidebar: logo header,
//! workspace rows with their terminals underneath, the "+ workspace" button,
//! and the bottom bar (environment todos, commands, prompts, settings).

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::{AppView, Drag, PopoverKind, Renaming, SIDEBAR_COLLAPSED};
use crate::assets::icon;
use crate::model::workspace::{TodoStatus, Workspace};
use crate::agent::attention::Kind;
use crate::theme::*;
use std::time::{Duration, Instant};

/// The small round count on the todo buttons: amber while a task is in progress.
fn todo_badge(open: usize, busy: bool) -> Div {
    div()
        .absolute()
        .min_w(px(13.))
        .h(px(13.))
        .px(px(3.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .bg(if busy { ACCENT_AMBER } else { ACCENT_GREEN })
        .text_color(ON_BADGE)
        .text_size(px(9.))
        .line_height(px(13.))
        .font_weight(FontWeight::BOLD)
        .child(open.to_string())
}

// Agent activity (WorkspaceSidebar's ACTIVITY_LABEL colours).
const WORKING: Rgba = hex(0x8a8a8a);
const WAITING: Rgba = hex(0xe5b95c);
const DONE_UNVIEWED: Rgba = hex(0x15ac91);
const DONE_VIEWED: Rgba = hex(0x5e7a73);
const DONE_UNVIEWED_BG: Rgba = hexa(0x15ac91, 0.16);

/// "Working ··", "Waiting", "Done!" after a terminal's name.
fn activity_label(id: &str, kind: Kind, unviewed: bool) -> AnyElement {
    let label = div().flex_none().font_weight(if kind == Kind::Done && unviewed { FontWeight::BOLD } else { FontWeight::MEDIUM });
    match kind {
        // The dots step · ·· ··· every 1.4s / 3, like the CSS animation.
        Kind::Working => label
            .text_color(WORKING)
            .with_animation(SharedString::from(format!("working-{id}")), Animation::new(Duration::from_millis(1400)).repeat(), |d, t| {
                d.child(["Working ·  ", "Working ·· ", "Working ···"][((t * 3.) as usize).min(2)])
            })
            .into_any_element(),
        Kind::Waiting => label.text_color(WAITING).child("Waiting").into_any_element(),
        Kind::Done => label.text_color(if unviewed { DONE_UNVIEWED } else { DONE_VIEWED }).child("Done!").into_any_element(),
    }
}

const SIDEBAR_ANIM: Duration = Duration::from_millis(150);

impl AppView {
    /// The sidebar's resting width for its current state.
    fn sidebar_target_width(&self) -> f32 {
        if self.settings.sidebar_collapsed { SIDEBAR_COLLAPSED } else { self.settings.sidebar_width }
    }

    /// The width drawn now: mid-tween while collapsing or expanding.
    fn sidebar_current_width(&self) -> f32 {
        let to = self.sidebar_target_width();
        match self.sidebar_anim {
            Some((from, start)) => {
                let t = (start.elapsed().as_secs_f32() / SIDEBAR_ANIM.as_secs_f32()).min(1.);
                from + (to - from) * crate::anim::ease(t)
            }
            None => to,
        }
    }

    /// Collapse or expand, tweening the width like the Electron app's
    /// `transition: width 0.15s ease` (the content switches at once).
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_current_width();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_anim = Some((from, Instant::now()));
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.settings.sidebar_collapsed;
        let width = self.sidebar_current_width();
        if let Some((_, start)) = self.sidebar_anim {
            if start.elapsed() >= SIDEBAR_ANIM {
                self.sidebar_anim = None;
            } else {
                window.request_animation_frame();
            }
        }
        let resizing = matches!(self.drag, Some(Drag::Sidebar));

        div()
            .relative()
            .flex_none()
            .w(px(width))
            .overflow_hidden()
            .h_full()
            .flex()
            .flex_col()
            .bg(bg_primary())
            .border_r_1()
            .border_color(border())
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| this.focus_window_chrome(window, cx)))
            .child(self.render_sidebar_header(collapsed, cx))
            .child(self.render_workspace_list(collapsed, cx))
            .child(self.render_bottom_bar(collapsed, cx))
            .when(!collapsed, |d| {
                d.child(
                    div()
                        .id("sidebar-resize")
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(-1.))
                        .w(px(5.))
                        .cursor_col_resize()
                        .when(resizing, |d| d.bg(TEXT_MUTED))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.drag = Some(Drag::Sidebar);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_sidebar_header(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let toggle = div()
            .id("sidebar-toggle")
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|d| d.bg(bg_hover()))
            .group("sb-toggle")
            .child(
                svg()
                    .path(icon("sidebar"))
                    .size(px(16.))
                    .text_color(TEXT_MUTED)
                    .group_hover("sb-toggle", |s| s.text_color(TEXT_PRIMARY)),
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));

        div()
            .flex_none()
            .flex()
            .items_center()
            .map(|d| if collapsed { d.justify_center().pt(px(10.)).pb(px(8.)) } else { d.justify_between().pt(px(10.)).px(px(12.)).pb(px(8.)) })
            .when(!collapsed, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .min_w_0()
                        .child(svg().path(icon("logo")).size(px(28.)).flex_none().text_color(hexa(0xffffff, 0.9)))
                        .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).text_color(TEXT_PRIMARY).child("SplitGrid"))
                        .child(
                            div()
                                .flex_none()
                                .ml(px(-4.))
                                .px(px(5.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(hexa(0xffffff, 0.14))
                                .text_size(px(10.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(TEXT_MUTED)
                                .child("Lite"),
                        ),
                )
            })
            .child(toggle)
            .into_any_element()
    }

    fn render_workspace_list(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let active_id = self.state.active_workspace_id.clone();
        let rows: Vec<AnyElement> = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .map(|(ix, ws)| {
                let active = Some(&ws.id) == active_id.as_ref();
                if collapsed { self.render_collapsed_row(ws, active, cx) } else { self.render_workspace_row(ix, ws, active, cx) }
            })
            .collect();

        let add = div()
            .id("ws-new")
            .h(px(if collapsed { 36. } else { 28. }))
            .map(|d| if collapsed { d.w(px(36.)).mx_auto() } else { d.w_full() })
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .rounded(px(6.))
            .border_1()
            .border_dashed()
            .border_color(border())
            .text_size(px(11.))
            .text_color(TEXT_MUTED)
            .cursor_pointer()
            .hover(|d| d.bg(bg_hover()).border_color(TEXT_MUTED).text_color(TEXT_PRIMARY))
            .group("ws-add")
            .child(svg().path(icon("plus-square")).size(px(14.)).text_color(TEXT_MUTED).group_hover("ws-add", |s| s.text_color(TEXT_PRIMARY)))
            .when(!collapsed, |d| d.child("workspace"))
            // Opens the menu just below the button: New, Open (a folder), Open Recent.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let at = point(ev.position.x.min(px(this.settings.sidebar_width - 130.)).max(px(8.)), ev.position.y + px(18.));
                    this.toggle_popover(PopoverKind::AddWorkspace, at, window, cx);
                }),
            );

        div()
            .id("ws-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(|d| if collapsed { d.px(px(4.)).pb(px(8.)) } else { d.px(px(8.)).pb(px(8.)) })
            .children(rows)
            .child(div().mt(px(8.)).pt(px(8.)).border_t_1().border_color(border()))
            .child(add)
            .into_any_element()
    }

    fn render_collapsed_row(&self, ws: &Workspace, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let row = self.render_collapsed_icon(ws, active, cx);
        self.workspace_drag_and_drop(row, ws, cx).into_any_element()
    }

    fn render_collapsed_icon(&self, ws: &Workspace, active: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = ws.id.clone();
        let initial = ws.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        div()
            .id(SharedString::from(format!("ws-c-{}", ws.id)))
            .size(px(36.))
            .mx_auto()
            .mb(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.))
            .text_size(px(14.))
            .font_weight(FontWeight::BOLD)
            .cursor_pointer()
            .border_1()
            .map(|d| {
                if active {
                    d.bg(bg_hover()).border_color(border()).text_color(TEXT_PRIMARY)
                } else if self.attention.unviewed_workspaces.contains(&ws.id) {
                    // A finished agent nobody has looked at yet.
                    d.bg(DONE_UNVIEWED).border_color(hexa(0, 0.)).text_color(hex(0xffffff))
                } else {
                    d.border_color(hexa(0, 0.)).text_color(TEXT_MUTED).hover(|d| d.bg(bg_hover()))
                }
            })
            .child(initial)
            .on_click(cx.listener(move |this, _, window, cx| this.switch_workspace(id.clone(), window, cx)))
    }

    fn render_workspace_row(&self, ix: usize, ws: &Workspace, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let renaming = matches!(&self.renaming, Some(Renaming::Workspace(id)) if *id == ws.id);
        let (id, rename_id) = (ws.id.clone(), ws.id.clone());
        let name = ws.name.clone();

        let row = div()
            .id(SharedString::from(format!("ws-{}", ws.id)))
            .relative()
            .mb(px(3.))
            .py(px(8.))
            // 10px padding plus the always-transparent 3px left border.
            .pl(px(13.))
            .pr(px(10.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |d| d.bg(bg_hover()))
            .when(!active && self.attention.unviewed_workspaces.contains(&ws.id), |d| d.bg(DONE_UNVIEWED_BG))
            .when(!active, |d| d.hover(|d| d.bg(bg_hover())))
            .when(ix > 0, |d| {
                // The faint separator above every row but the first.
                d.child(
                    div()
                        .absolute()
                        .left(px(12.))
                        .right(px(12.))
                        .top(px(-2.))
                        .h(px(1.))
                        .flex()
                        .child(div().flex_1().h_full().bg(linear_gradient(
                            90.,
                            linear_color_stop(c(hexa(0xffffff, 0.)), 0.),
                            linear_color_stop(c(hexa(0xffffff, 0.055)), 1.),
                        )))
                        .child(div().flex_1().h_full().bg(linear_gradient(
                            90.,
                            linear_color_stop(c(hexa(0xffffff, 0.055)), 0.),
                            linear_color_stop(c(hexa(0xffffff, 0.)), 1.),
                        ))),
                )
            });

        if renaming {
            return row.child(self.rename_field(cx)).into_any_element();
        }

        let title_line = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .min_w_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                    .text_color(TEXT_PRIMARY)
                    .child(name.clone()),
            )
            .child(self.render_notes_todo_buttons(ws, cx));

        let leaf_ids = ws.layout_tree.as_ref().map(|t| t.leaf_ids()).unwrap_or_default();
        let subrows: Vec<AnyElement> = leaf_ids
            .iter()
            .filter_map(|cid| ws.container(cid))
            .map(|c| {
                let pane = self.panes.get(&c.id);
                let label = c
                    .content
                    .custom_name
                    .clone()
                    .or_else(|| pane.and_then(|p| p.process.as_ref()).map(|p| p.name.clone()))
                    .unwrap_or_else(|| c.content.kind.label().to_lowercase());
                let dot = if pane.is_some() { ACCENT_GREEN } else { TEXT_MUTED };
                let activity = self.attention.kinds.get(&c.id).map(|k| (*k, self.attention.unviewed.contains(&c.id)));
                div()
                    .mt(px(3.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(10.))
                    .text_color(if active { TEXT_PRIMARY } else { TEXT_MUTED })
                    .child(div().size(px(5.)).flex_none().rounded_full().bg(dot))
                    .child(div().min_w_0().truncate().child(label.clone()))
                    .children(self.render_ports_button(&c.id, &label, cx))
                    .when_some(activity, |d, (kind, unviewed)| d.child(activity_label(&c.id, kind, unviewed)))
                    .into_any_element()
            })
            .collect();

        let menu_id = ws.id.clone();
        let ports_row = self.render_workspace_ports(&ws.id, &leaf_ids, cx);
        row.child(title_line)
            .children(subrows)
            .children(ports_row)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.toggle_popover(PopoverKind::WorkspaceMenu(menu_id.clone()), ev.position, window, cx);
                }),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.start_rename(Renaming::Workspace(rename_id.clone()), name.clone(), window, cx);
                } else {
                    this.switch_workspace(id.clone(), window, cx);
                }
            }))
            .map(|d| self.workspace_drag_and_drop(d, ws, cx))
            .into_any_element()
    }

    /// A workspace row (full or collapsed) as a drag source and drop target:
    /// dragged onto another row it moves there; a pane header dropped on it moves
    /// the pane into that workspace (AppView::on_drag_end). Records the row's
    /// bounds for the latter.
    fn workspace_drag_and_drop(&self, row: Stateful<Div>, ws: &Workspace, cx: &mut Context<Self>) -> Stateful<Div> {
        let dragged = DraggedWorkspace { id: ws.id.clone(), name: ws.name.clone().into() };
        let (onto, rec, key) = (ws.id.clone(), self.ws_row_bounds.clone(), ws.id.clone());
        let pane_over = matches!(&self.drag, Some(Drag::Pane { id, pos, active: true, .. })
            if self.workspace_drop_target(id, *pos).as_deref() == Some(ws.id.as_str()));
        row.relative()
            .child(canvas(move |b, _, _| { rec.borrow_mut().insert(key, b); }, |_, _, _, _| {}).absolute().size_full())
            .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            .drag_over::<DraggedWorkspace>(|s, _, _, _| s.bg(hexa(0x4c9df3, 0.18)))
            .on_drop(cx.listener(move |this, d: &DraggedWorkspace, _, cx| {
                if this.state.move_workspace(&d.id, &onto) {
                    this.changed(cx);
                }
            }))
            .when(pane_over, |d| d.child(crate::grid::drop_target_frame()))
    }

    /// The workspace under `pos` that pane `pane` can be dropped into (not its own).
    pub(crate) fn workspace_drop_target(&self, pane: &str, pos: Point<Pixels>) -> Option<String> {
        let bounds = self.ws_row_bounds.borrow();
        self.state
            .workspaces
            .iter()
            .filter(|w| !w.leaves().iter().any(|l| l == pane))
            .find(|w| bounds.get(&w.id).is_some_and(|b| b.contains(&pos)))
            .map(|w| w.id.clone())
    }

    /// The notes and todo buttons on a workspace row (WorkspaceNotesTodo).
    fn render_notes_todo_buttons(&self, ws: &Workspace, cx: &mut Context<Self>) -> AnyElement {
        let has_notes = ws.notes.as_ref().is_some_and(|n| !n.trim().is_empty());
        let open = ws.open_todos();
        let busy = ws.todos.iter().any(|t| t.status() == TodoStatus::InProgress);
        let notes_kind = PopoverKind::Notes(ws.id.clone());
        let todos_kind = PopoverKind::Todos(ws.id.clone());
        let notes_open = self.popover.as_ref().is_some_and(|p| p.kind == notes_kind);
        let todos_open = self.popover.as_ref().is_some_and(|p| p.kind == todos_kind);
        let sidebar_w = px(self.settings.sidebar_width);

        let button = |id: String, group: String, name: &'static str, lit: bool, open: bool| {
            div()
                .id(SharedString::from(id))
                .relative()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .when(open, |d| d.bg(bg_hover()))
                .group(SharedString::from(group.clone()))
                .child(
                    svg()
                        .path(icon(name))
                        .size(px(13.))
                        .text_color(if lit || open { TEXT_PRIMARY } else { TEXT_MUTED })
                        .group_hover(SharedString::from(group), |s| s.text_color(TEXT_PRIMARY)),
                )
        };

        div()
            .flex()
            .flex_none()
            .gap(px(2.))
            .child(
                button(format!("notes-{}", ws.id), format!("nb-{}", ws.id), "notes", has_notes, notes_open)
                    .when(has_notes, |d| d.child(div().absolute().top(px(2.)).right(px(2.)).size(px(5.)).rounded_full().bg(ACCENT_GREEN)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        let at = point(sidebar_w + px(8.), ev.position().y - px(14.));
                        this.toggle_popover(notes_kind.clone(), at, window, cx);
                    })),
            )
            .child(
                button(format!("todos-{}", ws.id), format!("tb-{}", ws.id), "checklist", false, todos_open)
                    .when(open > 0, |d| d.child(todo_badge(open, busy).top(px(-3.)).right(px(-3.))))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        let at = point(sidebar_w + px(8.), ev.position().y - px(14.));
                        this.toggle_popover(todos_kind.clone(), at, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_bottom_bar(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let env_open = self.state.env_todos.iter().filter(|t| t.status() != TodoStatus::Done).count();
        let env_busy = self.state.env_todos.iter().any(|t| t.status() == TodoStatus::InProgress);
        let env_popover = self.popover.as_ref().is_some_and(|p| p.kind == PopoverKind::EnvTodos);
        let cmds_popover = self.popover.as_ref().is_some_and(|p| p.kind == PopoverKind::Commands);
        let prompts_popover = self.popover.as_ref().is_some_and(|p| p.kind == PopoverKind::Prompts);
        let (fav_count, prompt_count) = (self.command_favorite_count(), self.env_prompts().len());
        let sidebar_w = px(if collapsed { SIDEBAR_COLLAPSED } else { self.settings.sidebar_width });

        let button = |id: &'static str, icon_name: &'static str, label: &'static str, open: bool| {
            let group = SharedString::from(format!("bb-{id}"));
            div()
                .id(id)
                .relative()
                .h(px(28.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(6.))
                .text_size(px(11.))
                .cursor_pointer()
                .map(|d| if collapsed { d.w(px(36.)).mx_auto().justify_center() } else { d.w_full().px(px(10.)) })
                .map(|d| if open { d.bg(bg_hover()).text_color(TEXT_PRIMARY) } else { d.text_color(TEXT_MUTED) })
                .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                .group(group.clone())
                .child(
                    svg()
                        .path(icon(icon_name))
                        .size(px(14.))
                        .text_color(if open { TEXT_PRIMARY } else { TEXT_MUTED })
                        .group_hover(group, |s| s.text_color(TEXT_PRIMARY)),
                )
                .when(!collapsed, |d| d.child(label))
        };

        div()
            .flex_none()
            .p(px(if collapsed { 4. } else { 8. }))
            .border_t_1()
            .border_color(border())
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                button("env-todos", "checklist", "Todos", env_popover)
                    .when(env_open > 0, |d| d.child(todo_badge(env_open, env_busy).top(px(4.)).right(px(8.))))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        let at = point(sidebar_w + px(8.), ev.position().y - px(200.));
                        this.toggle_popover(PopoverKind::EnvTodos, at, window, cx);
                    })),
            )
            .child(
                button("commands", "commands", "Commands", cmds_popover)
                    .when(fav_count > 0, |d| d.child(crate::commands::count_badge(fav_count, collapsed)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        let at = point(sidebar_w + px(8.), ev.position().y - px(200.));
                        this.toggle_popover(PopoverKind::Commands, at, window, cx);
                    })),
            )
            .child(
                button("prompts", "prompts", "Prompts", prompts_popover)
                    .when(prompt_count > 0, |d| d.child(crate::commands::count_badge(prompt_count, collapsed)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        let at = point(sidebar_w + px(8.), ev.position().y - px(200.));
                        this.toggle_popover(PopoverKind::Prompts, at, window, cx);
                    })),
            )
            .child(
                button("settings", "gear", "Settings", self.settings_tab.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.open_settings(crate::settings::SettingsTab::Ssh, cx))),
            )
            .into_any_element()
    }
}

/// A workspace row being dragged to a new place in the list; renders as its ghost.
#[derive(Clone)]
pub(crate) struct DraggedWorkspace {
    id: String,
    name: SharedString,
}

impl Render for DraggedWorkspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(bg_hover())
            .border_1()
            .border_color(border())
            .opacity(0.92)
            .text_size(px(13.))
            .text_color(TEXT_PRIMARY)
            .font_family(crate::theme::ui_font())
            .child(self.name.clone())
    }
}
