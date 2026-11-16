//! Window chrome: the title strip (WorkspaceBar), the status bar (StatusBar),
//! the environment picker, and the sidebar's notes / todo popovers.

use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::{AppView, Popover, PopoverKind, Renaming};
use crate::assets::icon;
use crate::grid::tilde;
use crate::model::store::DEFAULT_ENV;
use crate::model::workspace::TodoStatus;
use crate::theme::*;

pub const TITLE_H: f32 = 36.;
const STATUS_H: f32 = 22.;
const POPOVER_W: f32 = 300.;

fn popover_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: c(hexa(0x000000, 0.45)),
        offset: point(px(0.), px(8.)),
        blur_radius: px(24.),
        spread_radius: px(0.),
        inset: false,
    }]
}

impl AppView {
    /// 36px strip under the traffic lights: the active workspace's name, centred; drags the window.
    pub(crate) fn render_title_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let name = self.state.active().map(|w| w.name.clone()).unwrap_or_else(|| "Workspace".into());
        div()
            .id("title-strip")
            .relative()
            .h(px(TITLE_H))
            .flex_none()
            .px(px(10.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| this.focus_window_chrome(window, cx)))
            .on_mouse_down(MouseButton::Left, |ev, window, _| {
                if ev.click_count == 2 {
                    window.titlebar_double_click();
                } else {
                    window.start_window_move();
                }
            })
            .child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    div()
                        .max_w(relative(0.55))
                        .truncate()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(TEXT_PRIMARY)
                        .child(name),
                ),
            )
            .into_any_element()
    }

    /// 22px bar along the bottom: workspace name and what the focused pane runs.
    pub(crate) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let ws = self.state.active();
        let name = ws.map(|w| w.name.clone()).unwrap_or_default();
        let focused = ws.and_then(|w| w.focused_container_id.clone());
        let pane = focused.as_ref().and_then(|id| self.panes.get(id));
        let ide = focused.as_ref().and_then(|id| self.ide_panes.get(id)).map(|(p, _)| p.read(cx).status(cx));
        let context = ide.or_else(|| {
            pane.and_then(|p| p.process.as_ref()).map(|p| match &p.cwd {
                Some(cwd) => format!("{} · {}", p.name, tilde(cwd)),
                None => p.name.clone(),
            })
        });
        let dot = if pane.is_some() || context.is_some() { ACCENT_GREEN } else { TEXT_MUTED };

        div()
            .h(px(STATUS_H))
            .flex_none()
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(12.))
            .bg(bg_primary())
            .border_t_1()
            .border_color(border())
            .text_size(px(11.))
            .text_color(TEXT_MUTED)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex_none().text_color(TEXT_PRIMARY).child(name))
                    .when_some(context, |d, text| {
                        d.child(
                            div()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(5.))
                                .child(div().size(px(6.)).flex_none().rounded_full().bg(dot))
                                .child(div().min_w_0().truncate().child(text)),
                        )
                    }),
            )
            .into_any_element()
    }

    // ── Environment picker (File ▸ Open Env…, ⌘O) ───────────────────────────

    pub(crate) fn render_env_picker(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Environments shown in other windows: clicking one goes to that window.
        let elsewhere = crate::windows::open_elsewhere(window.window_handle(), cx);
        let for_window = self.env_picker_new_window;
        let rows: Vec<AnyElement> = self
            .envs
            .iter()
            .map(|env| {
                let (id, del_id, ren_id, win_id) = (env.id.clone(), env.id.clone(), env.id.clone(), env.id.clone());
                let name = env.name.clone();
                let active = env.id == self.env_id;
                let other_window = elsewhere.contains(&env.id);
                let renaming = matches!(&self.renaming, Some(Renaming::Environment(r)) if *r == env.id);
                let row = div()
                    .id(SharedString::from(format!("env-{}", env.id)))
                    .group("env-row")
                    .h(px(36.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(6.))
                    .when(active, |d| d.bg(bg_hover()))
                    .hover(|d| d.bg(bg_hover()));
                if renaming {
                    return row.child(self.rename_field(cx)).into_any_element();
                }
                if for_window {
                    // Choosing what a new window shows: this window's environment can't be
                    // opened twice; one open elsewhere brings its window forward.
                    return row
                        .when(!active, |d| d.cursor_pointer())
                        .child(div().flex_1().truncate().text_size(px(13.)).when(active, |d| d.text_color(TEXT_MUTED)).child(name))
                        .when(active, |d| d.child(div().text_size(px(11.)).text_color(TEXT_MUTED).child("this window")))
                        .when(other_window, |d| d.child(div().text_size(px(11.)).text_color(TEXT_MUTED).child("open — show window")))
                        .when(!active, |d| d.on_click(cx.listener(move |this, _, _, cx| this.open_in_new_window(Some(id.clone()), cx))))
                        .into_any_element();
                }
                row.cursor_pointer()
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(13.))
                            .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(name.clone()),
                    )
                    .when(active, |d| d.child(div().text_size(px(11.)).text_color(TEXT_MUTED).child("current")))
                    .when(other_window, |d| d.child(div().text_size(px(11.)).text_color(TEXT_MUTED).child("open in another window")))
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .invisible()
                            .group_hover("env-row", |s| s.visible())
                            .when(!active, |d| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("env-win-{win_id}")))
                                        .px(px(6.))
                                        .rounded(px(4.))
                                        .text_size(px(11.))
                                        .text_color(TEXT_MUTED)
                                        .hover(|d| d.bg(bg_surface()).text_color(TEXT_PRIMARY))
                                        .child(if other_window { "Show window" } else { "New window" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.open_in_new_window(Some(win_id.clone()), cx);
                                        })),
                                )
                            })
                            .child(
                            div()
                                .id(SharedString::from(format!("env-ren-{ren_id}")))
                                .px(px(6.))
                                .rounded(px(4.))
                                .text_size(px(11.))
                                .text_color(TEXT_MUTED)
                                .hover(|d| d.bg(bg_surface()).text_color(TEXT_PRIMARY))
                                .child("Rename")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.start_rename(Renaming::Environment(ren_id.clone()), name.clone(), window, cx);
                                })),
                        )
                        .when(env.id != DEFAULT_ENV && !other_window, |d| {
                            d.child(
                                div()
                                    .id(SharedString::from(format!("env-del-{del_id}")))
                                    .px(px(6.))
                                    .rounded(px(4.))
                                    .text_size(px(11.))
                                    .text_color(TEXT_MUTED)
                                    .hover(|d| d.bg(bg_surface()).text_color(ACCENT_RED))
                                    .child("Delete")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.delete_env(&del_id, window, cx);
                                    })),
                            )
                        }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.switch_env(id.clone(), window, cx)))
                    .into_any_element()
            })
            .collect();

        div()
            .id("env-picker-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(20.))
            .bg(hexa(0x000000, 0.55))
            .occlude()
            .on_click(cx.listener(|this, _, _, cx| {
                this.env_picker_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("env-picker")
                    .w(px(520.))
                    .max_h(relative(1.))
                    .flex()
                    .flex_col()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(border())
                    .bg(bg_primary())
                    .shadow(vec![BoxShadow {
                        color: c(hexa(0x000000, 0.45)),
                        offset: point(px(0.), px(18.)),
                        blur_radius: px(48.),
                        spread_radius: px(0.),
                        inset: false,
                    }])
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px(px(16.))
                            .py(px(12.))
                            .border_b_1()
                            .border_color(border())
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if for_window { "Open in a new window" } else { "Environments" }),
                    )
                    .child(div().id("env-list").p(px(8.)).flex().flex_col().gap(px(2.)).overflow_y_scroll().children(rows))
                    .child(div().p(px(8.)).flex().gap(px(8.)).border_t_1().border_color(border()).map(|d| {
                        let button = |id: &'static str, label: &'static str| {
                            div()
                                .id(id)
                                .flex_1()
                                .h(px(28.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(border())
                                .text_size(px(11.))
                                .cursor_pointer()
                                .child(label)
                        };
                        if for_window {
                            d.child(
                                button("env-new-window", "New environment in a new window")
                                    .bg(bg_surface())
                                    .text_color(TEXT_PRIMARY)
                                    .hover(|d| d.bg(bg_hover()))
                                    .on_click(cx.listener(|this, _, _, cx| this.open_in_new_window(None, cx))),
                            )
                        } else {
                            d.child(
                                button("env-new", "New environment")
                                    .border_dashed()
                                    .text_color(TEXT_MUTED)
                                    .hover(|d| d.bg(bg_hover()).border_color(TEXT_MUTED).text_color(TEXT_PRIMARY))
                                    .on_click(cx.listener(|this, _, window, cx| this.new_env(window, cx))),
                            )
                            .child(
                                // Pick what the new window shows (⌘⇧N).
                                button("env-pick-window", "New window  ⌘⇧N")
                                    .bg(bg_surface())
                                    .text_color(TEXT_PRIMARY)
                                    .hover(|d| d.bg(bg_hover()))
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_env_picker(true, cx))),
                            )
                        }
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn rename_field(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.cancel_rename(cx);
                    cx.stop_propagation();
                }
            }))
            .child(Input::new(&self.rename_input).xsmall())
            .into_any_element()
    }

    // ── Notes / todo popovers ───────────────────────────────────────────────

    pub(crate) fn render_popover(&self, popover: Popover, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Popover { kind, at } = popover;
        if let PopoverKind::WorkspaceMenu(ws_id) = &kind {
            return self.render_workspace_menu(ws_id.clone(), at, cx);
        }
        if kind == PopoverKind::AddWorkspace {
            return self.render_add_workspace_menu(at, cx);
        }
        let ws_name = match &kind {
            PopoverKind::Notes(id) | PopoverKind::Todos(id) => {
                self.state.workspaces.iter().find(|w| &w.id == id).map(|w| w.name.clone()).unwrap_or_default()
            }
            PopoverKind::EnvTodos | PopoverKind::WorkspaceMenu(_) | PopoverKind::AddWorkspace => String::new(),
        };

        let (title, in_progress): (String, usize) = match &kind {
            PopoverKind::Notes(_) => ("Notes".into(), 0),
            _ => {
                let list = self.todo_list(&kind).cloned().unwrap_or_default();
                let open = list.iter().filter(|t| t.status() != TodoStatus::Done).count();
                let busy = list.iter().filter(|t| t.status() == TodoStatus::InProgress).count();
                let label = if kind == PopoverKind::EnvTodos { "Environment todos" } else { "Todos" };
                (if open > 0 { format!("{label} · {open}") } else { label.to_owned() }, busy)
            }
        };

        let header = div()
            .px(px(12.))
            .py(px(8.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .border_b_1()
            .border_color(border())
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(TEXT_PRIMARY)
            .child(
                div()
                    .flex()
                    .child(title)
                    .when(in_progress > 0, |d| {
                        d.child(
                            div()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(ACCENT_AMBER)
                                .child(format!(" · {in_progress} in progress")),
                        )
                    }),
            )
            .child(
                div()
                    .max_w(px(150.))
                    .truncate()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(TEXT_MUTED)
                    .child(ws_name),
            );

        let body = match &kind {
            PopoverKind::Notes(_) => div()
                .h(px(220.))
                .px(px(4.))
                .py(px(2.))
                .text_size(px(12.))
                .child(Textarea::new(&self.notes).appearance(false).h_full())
                .into_any_element(),
            _ => self.render_todo_panel(&kind, cx),
        };

        // Keep it on screen: the window height bounds how low it may start.
        let max_top = window.viewport_size().height - px(440.);
        let top = if window.is_fullscreen() { px(0.) } else { px(TITLE_H) };
        let at = point(at.x, at.y.min(max_top).max(top));

        deferred(
            anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("popover")
                    .w(px(POPOVER_W))
                    .flex()
                    .flex_col()
                    .bg(bg_surface())
                    .border_1()
                    .border_color(border())
                    .rounded(px(8.))
                    .shadow(popover_shadow())
                    .overflow_hidden()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popover(cx)))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                        if ev.keystroke.key == "escape" {
                            this.close_popover(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(header)
                    .child(body),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_workspace_menu(&self, ws_id: String, at: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let Some(ws) = self.state.workspaces.iter().find(|w| w.id == ws_id) else { return div().into_any_element() };
        let name = ws.name.clone();
        let folder = ws.working_directory.clone().map(|f| tilde(&f));
        let item = |id: &'static str, label: String, danger: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if danger { ACCENT_RED } else { TEXT_PRIMARY })
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(label)
        };
        let (r_id, f_id, d_id, s_id) = (ws_id.clone(), ws_id.clone(), ws_id.clone(), ws_id.clone());
        let menu = || {
            div()
                .id("ws-menu")
                .min_w(px(200.))
                .p(px(4.))
                .flex()
                .flex_col()
                .bg(bg_surface())
                .border_1()
                .border_color(border())
                .rounded(px(8.))
                .shadow(popover_shadow())
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popover(cx)))
        };
        // SplitGrid asks before closing: "Close <name>?" — Cancel / Close workspace.
        if self.ws_menu_confirm {
            let confirm = menu()
                .child(
                    div()
                        .px(px(10.))
                        .pt(px(6.))
                        .pb(px(4.))
                        .max_w(px(220.))
                        .flex()
                        .flex_wrap()
                        .gap_x(px(4.))
                        .text_size(px(12.))
                        .text_color(TEXT_MUTED)
                        .child("Close")
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(TEXT_PRIMARY).child(ws.name.clone()))
                        .child("?"),
                )
                .child(div().my(px(4.)).h(px(1.)).bg(border()))
                .child(item("ws-menu-cancel", "Cancel".into(), false).on_click(cx.listener(|this, _, _, cx| this.close_popover(cx))))
                .child(item("ws-menu-close", "Close workspace".into(), true).on_click(cx.listener(move |this, _, window, cx| {
                    this.close_popover(cx);
                    this.close_workspace(&d_id, window, cx);
                })));
            return deferred(anchored().position(at).snap_to_window_with_margin(px(8.)).child(confirm)).with_priority(1).into_any_element();
        }
        // Workspace SFTP sync (configured in the Electron app's workspace settings).
        let sync = self.sync_menu_entry(ws);
        deferred(
            anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                menu()
                    .child(item("ws-menu-rename", "Rename".into(), false).on_click(cx.listener(move |this, _, window, cx| {
                        this.popover = None;
                        this.start_rename(Renaming::Workspace(r_id.clone()), name.clone(), window, cx);
                    })))
                    .child(
                        item("ws-menu-folder", "Set working folder…".into(), false)
                            .when_some(folder, |d, f| d.child(div().flex_1()).child(div().ml(px(12.)).max_w(px(160.)).truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(f)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_popover(cx);
                                this.choose_folder(f_id.clone(), cx);
                            })),
                    )
                    .when_some(sync, |d, (label, detail)| {
                        d.child(
                            item("ws-menu-sync", label, false)
                                .child(div().flex_1())
                                .child(div().ml(px(12.)).text_size(px(11.)).text_color(TEXT_MUTED).child(detail))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.close_popover(cx);
                                    this.sync_workspace_now(&s_id, cx);
                                })),
                        )
                    })
                    .child(div().my(px(4.)).h(px(1.)).bg(border()))
                    .child(item("ws-menu-close", "Close workspace…".into(), true).on_click(cx.listener(|this, _, _, cx| {
                        this.ws_menu_confirm = true;
                        cx.notify();
                    }))),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// "+ workspace": New, Open (a folder), Open Recent › (WorkspaceSidebar's add menu).
    fn render_add_workspace_menu(&self, at: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, icon_name: &'static str, label: &'static str| {
            div()
                .id(id)
                .w_full()
                .px(px(14.))
                .py(px(7.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(TEXT_PRIMARY)
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(svg().path(icon(icon_name)).size(px(14.)).flex_none().text_color(TEXT_PRIMARY))
                .child(label)
        };
        let flyout = self.recent_flyout.then(|| {
            let rows: Vec<AnyElement> = if self.recent_workspaces.is_empty() {
                vec![div().px(px(14.)).py(px(8.)).text_size(px(12.)).text_color(TEXT_MUTED).child("No recent workspaces").into_any_element()]
            } else {
                self.recent_workspaces
                    .iter()
                    .map(|ws| {
                        let pick = ws.clone();
                        div()
                            .id(SharedString::from(format!("recent-{}", ws.id)))
                            .px(px(14.))
                            .py(px(7.))
                            .cursor_pointer()
                            .hover(|d| d.bg(bg_hover()))
                            .child(div().text_size(px(12.)).text_color(TEXT_PRIMARY).truncate().child(ws.name.clone()))
                            .when_some(ws.working_directory.clone(), |d, dir| {
                                d.child(div().text_size(px(10.)).text_color(TEXT_MUTED).truncate().child(dir))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_popover(cx);
                                this.open_recent_workspace(pick.clone(), window, cx);
                            }))
                            .into_any_element()
                    })
                    .collect()
            };
            div()
                .id("recent-flyout")
                .absolute()
                .left(relative(1.))
                .top(px(-5.))
                .min_w(px(210.))
                .max_w(px(340.))
                .max_h(px(360.))
                .overflow_y_scroll()
                .bg(bg_surface())
                .border_1()
                .border_color(border())
                .rounded(px(6.))
                .shadow(popover_shadow())
                .occlude()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.set_recent_hover(1, *hovered, cx)))
                .children(rows)
        });

        deferred(
            anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("ws-add-menu")
                    .min_w(px(120.))
                    .flex()
                    .flex_col()
                    .bg(bg_surface())
                    .border_1()
                    .border_color(border())
                    .rounded(px(6.))
                    .shadow(popover_shadow())
                    .occlude()
                    // The flyout lies outside the menu's bounds: a press on it isn't "outside".
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if !this.recent_hover[1] {
                            this.close_popover(cx);
                        }
                    }))
                    .child(item("ws-add-new", "menu-new", "New").rounded_t(px(5.)).on_click(cx.listener(|this, _, window, cx| {
                        this.close_popover(cx);
                        this.new_workspace(window, cx);
                    })))
                    .child(item("ws-add-open", "menu-open", "Open").on_click(cx.listener(|this, _, window, cx| {
                        this.close_popover(cx);
                        this.open_folder_workspace(window, cx);
                    })))
                    .child(
                        div()
                            .id("ws-add-recent")
                            .relative()
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.set_recent_hover(0, *hovered, cx)))
                            .child(
                                div()
                                    .px(px(14.))
                                    .py(px(7.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(8.))
                                    .rounded_b(px(5.))
                                    .text_size(px(12.))
                                    .text_color(TEXT_PRIMARY)
                                    .when(self.recent_flyout, |d| d.bg(bg_hover()))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.))
                                            .child(svg().path(icon("menu-recent")).size(px(14.)).flex_none().text_color(TEXT_PRIMARY))
                                            .child("Open Recent"),
                                    )
                                    .child(div().text_size(px(13.)).text_color(TEXT_MUTED).child("›")),
                            )
                            .children(flyout),
                    ),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_todo_panel(&self, kind: &PopoverKind, cx: &mut Context<Self>) -> AnyElement {
        let list = self.todo_list(kind).cloned().unwrap_or_default();
        let runnable = matches!(kind, PopoverKind::Todos(_));
        let any_done = list.iter().any(|t| t.status() == TodoStatus::Done);

        let items: Vec<AnyElement> = list
            .iter()
            .map(|t| {
                let status = t.status();
                let (k1, k2) = (kind.clone(), kind.clone());
                let (cycle_id, del_id) = (t.id.clone(), t.id.clone());
                let text = t.text.clone();
                let (frame, fill, glyph, glyph_color) = match status {
                    TodoStatus::Todo => (c(border()), None, "", c(TEXT_MUTED)),
                    TodoStatus::InProgress => (c(ACCENT_AMBER), None, "●", c(ACCENT_AMBER)),
                    TodoStatus::Done => (c(ACCENT_GREEN), Some(c(ACCENT_GREEN)), "✓", c(ON_BADGE)),
                };
                div()
                    .id(SharedString::from(format!("todo-{}", t.id)))
                    .group("todo-row")
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(6.))
                    .child(
                        div()
                            .id(SharedString::from(format!("todo-st-{}", t.id)))
                            .mt(px(1.))
                            .size(px(16.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .border(px(1.5))
                            .border_color(frame)
                            .when_some(fill, |d, f| d.bg(f))
                            .cursor_pointer()
                            .text_size(px(10.))
                            .line_height(px(10.))
                            .text_color(glyph_color)
                            .child(glyph)
                            .on_click(cx.listener(move |this, _, _, cx| this.cycle_todo(&k1, &cycle_id, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .line_height(relative(1.4))
                            .map(|d| {
                                if status == TodoStatus::Done {
                                    d.text_color(TEXT_MUTED).line_through()
                                } else {
                                    d.text_color(TEXT_PRIMARY)
                                }
                            })
                            .child(t.text.clone())
                            .when(status == TodoStatus::InProgress, |d| {
                                d.child(
                                    div()
                                        .ml(px(6.))
                                        .text_size(px(9.))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(ACCENT_AMBER)
                                        .child("IN PROGRESS"),
                                )
                            }),
                    )
                    .when(runnable && status != TodoStatus::Done, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("todo-run-{}", t.id)))
                                .h(px(18.))
                                .px(px(7.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(3.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(border())
                                .text_size(px(10.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(TEXT_MUTED)
                                .cursor_pointer()
                                .hover(|d| d.text_color(ACCENT_GREEN).border_color(ACCENT_GREEN))
                                .child(div().text_size(px(8.)).child("▶"))
                                .child("Run")
                                .on_click(cx.listener(move |this, _, _, cx| this.run_text(text.clone(), cx))),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("todo-del-{}", t.id)))
                            .size(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(14.))
                            .text_color(TEXT_MUTED)
                            .cursor_pointer()
                            .hover(|d| d.text_color(ACCENT_RED))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| this.delete_todo(&k2, &del_id, cx))),
                    )
                    .into_any_element()
            })
            .collect();

        let empty = items.is_empty();
        let clear_kind = kind.clone();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("todo-items")
                    .min_h(px(180.))
                    .max_h(px(390.))
                    .overflow_y_scroll()
                    .when(empty, |d| {
                        d.child(
                            div()
                                .px(px(12.))
                                .py(px(14.))
                                .text_size(px(11.))
                                .text_color(TEXT_MUTED)
                                .child("No tasks yet. Add one below."),
                        )
                    })
                    .children(items),
            )
            .child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .flex()
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(border())
                    .child(div().flex_1().child(Input::new(&self.todo_input).small()))
                    .child(
                        div()
                            .id("todo-add")
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(border())
                            .bg(bg_hover())
                            .text_size(px(12.))
                            .cursor_pointer()
                            .child("Add")
                            .on_click(cx.listener(|this, _, window, cx| this.submit_todo(window, cx))),
                    ),
            )
            .when(any_done, |d| {
                d.child(
                    div()
                        .id("todo-clear")
                        .px(px(12.))
                        .py(px(6.))
                        .border_t_1()
                        .border_color(border())
                        .text_size(px(11.))
                        .text_color(TEXT_MUTED)
                        .cursor_pointer()
                        .hover(|d| d.text_color(TEXT_PRIMARY))
                        .child("Clear completed")
                        .on_click(cx.listener(move |this, _, _, cx| this.clear_done(&clear_kind, cx))),
                )
            })
            .into_any_element()
    }
}
