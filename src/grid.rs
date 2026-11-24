//! The workspace grid: splits, splitters and pane chrome, drawn like the
//! Electron app's WorkspaceGrid / XtermTerminal (see theme.rs for colours).

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::{AppView, Drag};
use crate::assets::icon;
use crate::agent::zones::{ZonePlace, zone_places};
use crate::model::layout::{Direction, LayoutNode, NodePath, ZoneTag};
use crate::model::workspace::ContentType;
use crate::terminal::RemoteStatus;
use crate::theme::*;

/// The splitter's hit area; its line is drawn centred in it.
const HANDLE: f32 = 6.;
const PANE_PADDING: f32 = 8.;
const HEADER_H: f32 = 32.;

/// `/Users/me/x` → `~/x`.
pub fn tilde(path: &str) -> String {
    match std::env::home_dir().map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if path == home => "~".into(),
        Some(home) if path.starts_with(&(home.clone() + "/")) => format!("~{}", &path[home.len()..]),
        _ => path.to_owned(),
    }
}

/// `~/x` → `/Users/me/x`.
pub fn expand_tilde(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ if path == "~" => std::env::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default(),
        _ => path.to_owned(),
    }
}

/// A 20×20 icon button of the pane header and sidebar.
pub fn icon_button(id: impl Into<ElementId>, name: &str, size: f32) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(20.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(TEXT_MUTED)
        .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
        .child(svg().path(icon(name)).size(px(size)).text_color(TEXT_MUTED))
}

/// Focus mode's open/close reveal (Electron's FOCUS_ANIM_MS).
pub const FOCUS_ANIM: std::time::Duration = std::time::Duration::from_millis(220);

impl AppView {
    pub(crate) fn render_grid_area(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(ws) = self.state.active() else { return div().flex_1().into_any_element() };
        let focus = self.focus_mode.clone().filter(|id| ws.leaves().contains(id));
        // While a close plays, the tree as it was (layout_anim.rs).
        let body = match self.layout_anim.display_tree().cloned().or_else(|| ws.layout_tree.clone()) {
            Some(tree) => self.render_node(&tree, Vec::new(), focus.as_deref(), window, cx),
            None => self.render_empty_grid(cx),
        };

        let grid_rec = self.grid_bounds.clone();
        // Focus mode's reveal (WorkspaceGrid's FOCUS_ANIM): the lifted pane is laid
        // out full-size at once and only its visible window grows from the tile to
        // the whole grid (clip-path in Electron), while the backdrop fades in.
        let grid = *self.grid_bounds.borrow();
        let progress = match self.focus_anim {
            Some((start, closing)) => {
                let t = start.elapsed().as_secs_f32() / FOCUS_ANIM.as_secs_f32();
                let e = crate::anim::cubic_bezier(0.22, 1., 0.36, 1., t);
                if closing { 1. - e } else { e }
            }
            None => 1.,
        };

        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(canvas(move |b, _, _| *grid_rec.borrow_mut() = Some(b), |_, _, _, _| {}).absolute().size_full())
            .child(body)
            .when_some(focus, |d, id| {
                let size = grid.map(|g| g.size);
                let from = self.focus_from.filter(|_| progress < 1.);
                // The visible window: the tile, eased out to the full grid.
                let clip = match (from, size) {
                    (Some(f), Some(full)) => {
                        let lerp = |a: Pixels, b: Pixels| a + (b - a) * progress;
                        Some(Bounds::new(
                            point(lerp(f.origin.x, px(0.)), lerp(f.origin.y, px(0.))),
                            gpui_kit::size(lerp(f.size.width, full.width), lerp(f.size.height, full.height)),
                        ))
                    }
                    _ => None,
                };
                let lifted = div()
                    .absolute()
                    .rounded(px(8.))
                    .overflow_hidden()
                    .shadow(vec![BoxShadow {
                        color: c(hexa(0x000000, 0.55)),
                        offset: point(px(0.), px(12.)),
                        blur_radius: px(48.),
                        spread_radius: px(0.),
                        inset: false,
                    }])
                    .child(self.render_pane(&id, true, window, cx))
                    .child(
                        // The purple frame breathes (opacity 0.55 ↔ 1 over 3.4s), as in Electron,
                        // with a glow inward (the lifted box clips anything outside it).
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(8.))
                            .border(px(2.))
                            .border_color(hexa(0xb98aec, 1.))
                            .shadow(vec![BoxShadow {
                                color: c(hexa(0xb082e2, 0.55)),
                                offset: point(px(0.), px(0.)),
                                blur_radius: px(18.),
                                spread_radius: px(1.),
                                inset: true,
                            }])
                            .with_animation(
                                "focus-glow",
                                Animation::new(std::time::Duration::from_millis(3400)).repeat(),
                                |d, t| d.opacity(0.55 + 0.45 * (1. - (t * std::f32::consts::TAU).cos()) / 2.),
                            ),
                    );
                let lifted = match (clip, size) {
                    // Content pinned at full size; the clipping box moves over it.
                    (Some(clip), Some(full)) => div()
                        .absolute()
                        .left(clip.origin.x)
                        .top(clip.origin.y)
                        .w(clip.size.width)
                        .h(clip.size.height)
                        .rounded(px(8.))
                        .overflow_hidden()
                        .child(lifted.left(-clip.origin.x).top(-clip.origin.y).w(full.width).h(full.height)),
                    _ => div().absolute().inset_0().child(lifted.inset_0()),
                };
                // The backdrop occludes the grid underneath: without it a click or a
                // drag over the lifted pane also lands on the tile below it, which then
                // takes the keyboard focus and starts its own selection.
                let backdrop = div().id("focus-backdrop").absolute().inset_0().occlude().child(div().size_full().bg(hexa(0x000000, 0.6)).opacity(progress));
                d.child(backdrop).child(lifted)
            })
            .into_any_element()
    }

    fn render_empty_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .items_center()
            .justify_center()
            .child(div().text_size(px(32.)).font_weight(FontWeight::BOLD).text_color(bg_surface()).child("SplitGrid"))
            .child(
                div()
                    .id("add-container")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .bg(bg_surface())
                    .border_1()
                    .border_color(border())
                    .cursor_pointer()
                    .hover(|d| d.border_color(ACCENT))
                    .child(svg().path(icon("plus-square")).size(px(16.)).text_color(TEXT_PRIMARY))
                    .child("Add Container")
                    .on_click(cx.listener(|this, _, _, cx| this.split(Direction::Horizontal, None, cx))),
            )
            .into_any_element()
    }

    fn render_node(&self, node: &LayoutNode, path: NodePath, lifted: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let inner = self.render_node_inner(node, path, lifted, window, cx);
        match node.zone() {
            Some(zone) => self.render_zone_frame(zone, node.leaf_ids(), inner, cx),
            None => inner,
        }
    }

    /// An agent's zone (agent::zones): a thin labelled frame around its panes,
    /// with × to close them all (the Electron app's ZoneFrame).
    fn render_zone_frame(&self, zone: &ZoneTag, panes: Vec<String>, inner: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let count = panes.len();
        let key = format!("zone-{}-{}", zone.terminal, zone.name);
        div()
            .size_full()
            .flex()
            .flex_col()
            .border_1()
            .border_color(border())
            .rounded(px(6.))
            .overflow_hidden()
            .child(
                div()
                    .h(px(18.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(6.))
                    .text_size(px(10.))
                    .text_color(TEXT_MUTED)
                    .bg(bg_primary())
                    .border_b_1()
                    .border_color(border())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("Agent zone · {} · {count} {}", zone.label, if count == 1 { "pane" } else { "panes" })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(key))
                            .px(px(2.))
                            .text_size(px(11.))
                            .cursor_pointer()
                            .hover(|d| d.text_color(TEXT_PRIMARY))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                for id in &panes {
                                    this.close_pane(id, cx);
                                }
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().child(inner))
            .into_any_element()
    }

    fn render_node_inner(&self, node: &LayoutNode, path: NodePath, lifted: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match node {
            LayoutNode::Leaf(l) => {
                // A closed pane shrinking away: its slot, blank.
                if self.layout_anim.is_closing(&l.container_id) {
                    return div().size_full().bg(bg_primary()).into_any_element();
                }
                // Shown in the focus-mode overlay instead; keep its slot.
                let pane = if lifted == Some(l.container_id.as_str()) {
                    div().size_full().bg(bg_primary()).into_any_element()
                } else {
                    self.render_pane(&l.container_id, false, window, cx)
                };
                // A pane being dragged fades (.drag-source); the one it would swap with
                // gets a pulsing dashed frame (.drag-target).
                let (source, target) = match &self.drag {
                    Some(Drag::Pane { id, pos, active: true, .. }) => {
                        let is_source = *id == l.container_id;
                        (is_source, !is_source && self.pane_drop_target(id, *pos).as_deref() == Some(l.container_id.as_str()))
                    }
                    _ => (false, false),
                };
                // Its tile, for focus mode's reveal to start from.
                let (rec, key) = (self.pane_bounds.clone(), l.container_id.clone());
                div()
                    .relative()
                    .size_full()
                    .child(canvas(move |b, _, _| { rec.borrow_mut().insert(key, b); }, |_, _, _, _| {}).absolute().size_full())
                    .child(div().size_full().when(source, |d| d.opacity(0.3)).child(pane))
                    .when(target, |d| d.child(drop_target_frame()))
                    .into_any_element()
            }
            LayoutNode::Split(s) => {
                let horizontal = s.direction == Direction::Horizontal;
                // Tweened while a split grows or a close shrinks (layout_anim.rs).
                let ratio = self.layout_anim.ratio(&path, std::time::Instant::now()).unwrap_or_else(|| crate::layout_anim::shown_ratio(s.ratio));
                // A close's ghost tree has other paths than the real one: its
                // dividers neither record bounds nor start drags.
                let ghost = self.layout_anim.is_ghost();
                let bounds = self.split_bounds.clone();
                let key = path.clone();
                let recorder = canvas(
                    move |b, _, _| {
                        if !ghost {
                            bounds.borrow_mut().insert(key, b);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full();

                let mut first_path = path.clone();
                first_path.push(false);
                let mut second_path = path.clone();
                second_path.push(true);
                let drag_path = path.clone();
                let active = matches!(&self.drag, Some(Drag::Split { path: p, .. }) if *p == path);

                // Each side gets `ratio − 3px`, the splitter the 6px between:
                // the first side spans the full ratio and the splitter overlaps it.
                let half = px(HANDLE / 2.);
                let first = div()
                    .relative()
                    .flex_none()
                    .overflow_hidden()
                    .map(|d| if horizontal { d.h_full().w(relative(ratio)).mr(-half) } else { d.w_full().h(relative(ratio)).mb(-half) })
                    .child(self.render_node(&s.first, first_path, lifted, window, cx));
                let line = div()
                    .bg(if active { c(TEXT_MUTED) } else { c(BORDER_SUBTLE) })
                    .map(|d| {
                        let w = px(if active { 2. } else { 1. });
                        if horizontal { d.w(w).h_full() } else { d.h(w).w_full() }
                    });
                let handle = div()
                    .id(SharedString::from(format!("split-{path:?}")))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|d| {
                        if horizontal {
                            d.w(px(HANDLE)).h_full().cursor_col_resize()
                        } else {
                            d.h(px(HANDLE)).w_full().cursor_row_resize()
                        }
                    })
                    .child(line)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            if ghost {
                                return;
                            }
                            this.drag = Some(Drag::Split { path: drag_path.clone(), horizontal });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                let second = div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .map(|d| if horizontal { d.ml(-half) } else { d.mt(-half) })
                    .child(self.render_node(&s.second, second_path, lifted, window, cx));

                div()
                    .relative()
                    .size_full()
                    .flex()
                    .map(|d| if horizontal { d.flex_row() } else { d.flex_col() })
                    .child(recorder)
                    .child(first)
                    .child(handle)
                    .child(second)
                    .into_any_element()
            }
        }
    }

    pub(crate) fn render_pane(&self, id: &str, lifted: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(ws) = self.state.active() else { return div().into_any_element() };
        let Some(container) = ws.container(id) else { return div().into_any_element() };
        let focused = lifted || ws.focused_container_id.as_deref() == Some(id);
        let pane = self.panes.get(id);
        let process = pane.and_then(|p| p.process.clone());
        let running = pane.is_some();
        let content = &container.content;
        let ssh = content.is_ssh_terminal();
        let remote = pane.and_then(|p| p.view.read(cx).remote_status().cloned());

        // Header: (name, name is a muted caption, subtitle, status dot).
        let (name, caption, subtitle, dot): (SharedString, bool, String, Option<Rgba>) = match content.kind {
            ContentType::Empty => ("Empty Container".into(), true, String::new(), None),
            ContentType::SshConnect => ("Connect over SSH".into(), true, String::new(), None),
            ContentType::Ide => (content.label.clone().unwrap_or_else(|| "IDE".into()).into(), true, String::new(), None),
            ContentType::Sql => {
                let title = self.sql_panes.get(id).map(|(p, _)| p.read(cx).title()).unwrap_or_else(|| "SQL".into());
                (title.into(), true, String::new(), None)
            }
            ContentType::Browser => {
                let title = self.browser_panes.get(id).map(|e| e.pane.read(cx).title()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Browser".into());
                (title.into(), true, String::new(), None)
            }
            ContentType::Sftp => {
                let title = match self.sftp_panes.get(id) {
                    Some((pane, _)) => pane.read(cx).title(),
                    None => content.label.clone().filter(|_| content.extra_str("sftpConnectionId").is_some()).unwrap_or_else(|| "SFTP".into()),
                };
                (title.into(), true, String::new(), None)
            }
            _ if ssh => {
                let conn = content.connection_id.as_deref().and_then(|cid| self.connections.lock().unwrap().get(cid).cloned());
                let name = content.custom_name.clone().or_else(|| content.label.clone()).unwrap_or_else(|| "SSH".into());
                let sub = conn.map(|c| format!("{}@{}", c.username, c.host)).unwrap_or_default();
                let dot = match remote {
                    Some(RemoteStatus::Connected) => DOT_CONNECTED,
                    Some(RemoteStatus::Connecting) => DOT_CONNECTING,
                    _ if self.ssh_errors.contains_key(id) => DOT_ERROR,
                    None => DOT_CONNECTING,
                    _ => DOT_ERROR,
                };
                (name.into(), false, sub, Some(dot))
            }
            _ => {
                // Custom name, then the foreground program, then the pane type.
                let name = content.custom_name.clone().or_else(|| process.as_ref().map(|p| p.name.clone())).unwrap_or_else(|| content.kind.label().to_owned());
                let sub = process.as_ref().and_then(|p| p.cwd.clone()).or_else(|| content.cwd.clone()).map(|p| tilde(&p)).unwrap_or_default();
                (name.into(), false, sub, Some(if running { DOT_CONNECTED } else { DOT_CONNECTING }))
            }
        };

        let pid = id.to_owned();
        let (p_close, p_focus, p_right, p_down, p_tray) = (pid.clone(), pid.clone(), pid.clone(), pid.clone(), pid.clone());
        // In an agent's zone: the button to collapse into (or expand out of) its tray.
        let tray_place = zone_places(ws.layout_tree.as_ref()).get(id).copied();
        // The header is the pane's drag handle (Electron's .container-drag-handle):
        // drop it on another pane to swap the two. Not in focus mode, where the
        // lifted pane covers every other one.
        let draggable = !lifted && self.focus_mode.is_none();
        let p_drag = pid.clone();
        let header = div()
            .h(px(HEADER_H))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .when(draggable, |d| {
                d.cursor_grab().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                        if this.drag.is_none() {
                            this.drag = Some(Drag::Pane { id: p_drag.clone(), start: ev.position, pos: ev.position, active: false });
                        }
                    }),
                )
            })
            // macOS-style lights: red closes, green lifts into (or out of) focus mode;
            // their glyphs show while the pointer is over either.
            .child(
                div()
                    .group("pane-lights")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        traffic_light(SharedString::from(format!("close-{pid}")), LIGHT_RED, Some("light-close"))
                            .on_click(cx.listener(move |this, _, _, cx| this.close_pane(&p_close, cx))),
                    )
                    .child({
                        // Yellow collapses into the neighbour (crate::collapsed); grey when
                        // it is the only pane or lifted.
                        let p_collapse = pid.clone();
                        let can = !lifted && ws.can_collapse(id);
                        traffic_light(SharedString::from(format!("collapse-{pid}")), if can { LIGHT_YELLOW } else { LIGHT_OFF }, can.then_some("light-collapse"))
                            .when(can, |d| d.on_click(cx.listener(move |this, _, _, cx| this.collapse_pane(&p_collapse, cx))))
                            .when(!can, |d| d.cursor_default())
                    })
                    .child(
                        traffic_light(SharedString::from(format!("focus-{pid}")), LIGHT_GREEN, Some(if lifted { "light-unzoom" } else { "light-zoom" }))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_focus_mode(Some(p_focus.clone()), cx))),
                    ),
            )
            .when_some(tray_place, |d, place| {
                let into = place == ZonePlace::Main;
                d.child(
                    div()
                        .id(SharedString::from(format!("tray-{pid}")))
                        .size(px(16.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|d| d.bg(bg_hover()))
                        .child(svg().path(icon(if into { "tray-in" } else { "tray-out" })).size(px(13.)).text_color(TEXT_MUTED))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_zone_tray(&p_tray, cx))),
                )
            })
            .child(
                div()
                    .px(px(if caption { 0. } else { 4. }))
                    .py(px(1.))
                    .rounded(px(3.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    // The status (running, connecting, failed) tints the name: a dot
                    // beside the lights read as a third one.
                    .text_color(if caption { TEXT_MUTED } else { dot.unwrap_or(TEXT_PRIMARY) })
                    .flex_none()
                    .child(name),
            )
            .child(div().flex_1().min_w_0().truncate().text_size(px(10.)).text_color(TEXT_MUTED).child(subtitle))
            .children(self.render_collapsed_chips(ws, id, cx))
            .when(ssh, |d| {
                let rid = pid.clone();
                d.child(
                    icon_button(SharedString::from(format!("reconnect-{pid}")), "reconnect", 14.)
                        .on_click(cx.listener(move |this, _, _, cx| this.reconnect(&rid, cx))),
                )
            })
            .children(self.render_processes_button(&pid, ssh, cx))
            .child(
                icon_button(SharedString::from(format!("split-r-{pid}")), "split-right", 14.)
                    .on_click(cx.listener(move |this, _, _, cx| this.split(Direction::Horizontal, Some(p_right.clone()), cx))),
            )
            .child(
                icon_button(SharedString::from(format!("split-d-{pid}")), "split-down", 14.)
                    .on_click(cx.listener(move |this, _, _, cx| this.split(Direction::Vertical, Some(p_down.clone()), cx))),
            );

        let ssh_state = if ssh { self.render_ssh_state(id, remote.as_ref(), cx) } else { None };
        let body: AnyElement = match (content.kind, pane) {
            (ContentType::Empty, _) => self.render_type_picker(id, cx),
            (ContentType::SshConnect, _) => self.render_ssh_picker(id, false, window, cx),
            (ContentType::Ide, _) => match self.ide_panes.get(id) {
                Some((pane, _)) => pane.clone().into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(TEXT_MUTED)
                    .child(format!("Folder not found: {}", content.extra_str("rootPath").unwrap_or_default()))
                    .into_any_element(),
            },
            (ContentType::Sql, _) => match self.sql_panes.get(id) {
                Some((pane, _)) => pane.clone().into_any_element(),
                None => div().into_any_element(),
            },
            (ContentType::Browser, _) => match self.browser_panes.get(id) {
                Some(e) => e.pane.clone().into_any_element(),
                None => div().into_any_element(),
            },
            (ContentType::Sftp, _) => match self.sftp_panes.get(id) {
                Some((pane, _)) => pane.clone().into_any_element(),
                None => self.render_ssh_picker(id, true, window, cx),
            },
            _ if ssh_state.is_some() => ssh_state.unwrap(),
            (_, Some(p)) => p.view.clone().into_any_element(),
            (_, None) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .text_color(TEXT_MUTED)
                .child(if container.content.kind == ContentType::Terminal {
                    format!("{} terminal — arrives with SSH support", container.content.terminal_type.clone().unwrap_or_default())
                } else {
                    format!("{} pane — arrives in a later phase", container.content.kind.label())
                })
                .into_any_element(),
        };

        let focus_id = pid.clone();
        div()
            .id(SharedString::from(format!("pane-{pid}")))
            .relative()
            .size_full()
            .p(px(PANE_PADDING))
            .bg(bg_primary())
            .flex()
            .flex_col()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _: &MouseDownEvent, _, cx| this.set_focused_pane(&focus_id, cx)))
            .child(header)
            .child(div().flex_1().min_h_0().bg(bg_primary()).child(body))
            // Over everything, without catching the mouse: the focus ring, or the dimming.
            .map(|d| {
                if focused && !lifted {
                    // gpui fills a shadow's whole rectangle, so the CSS ring
                    // (1px line + 1px halo) is drawn as two borders instead.
                    // Kept 1px inside the pane: at the window's edge the outermost
                    // pixel sits under macOS's own window border and disappears.
                    // Two pixels in for the line and one for its halo: at the window's
                    // edge macOS draws its own border over the outermost pixel.
                    d.child(div().absolute().inset(px(2.)).rounded(px(6.)).border_1().border_color(FOCUS_RING))
                    .child(div().absolute().inset(px(1.)).rounded(px(7.)).border_1().border_color(FOCUS_GLOW))
                } else if !focused {
                    d.child(div().absolute().inset_0().rounded(px(6.)).bg(UNFOCUSED_DIM))
                } else {
                    d
                }
            })
            .into_any_element()
    }
}

/// The frame on the pane a dragged one would swap with (Electron's
/// `.drag-target::after`: 2px dashed #4c9df3 inset 2px, pulsing over 1.2s).
fn drop_target_frame() -> impl IntoElement {
    div()
        .absolute()
        .inset(px(2.))
        .rounded(px(6.))
        .border_2()
        .border_dashed()
        .with_animation("drag-target-pulse", Animation::new(std::time::Duration::from_millis(1200)).repeat(), |d, t| {
            // 0 → 1 → 0, eased like CSS ease-in-out.
            let k = (1. - (t * std::f32::consts::TAU).cos()) / 2.;
            d.border_color(hexa(0x4c9df3, 0.3 + 0.3 * k)).bg(hexa(0x4c9df3, 0.03 + 0.04 * k))
        })
}

impl AppView {
    /// The ghost of a dragged pane under the cursor, over a full-window layer
    /// that keeps the panes beneath from seeing the drag (Electron's `.drag-ghost`:
    /// the pane scaled to fit 280×200, centred on the cursor, at 92% opacity).
    pub(crate) fn render_pane_drag(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Drag::Pane { id, pos, active: true, .. }) = &self.drag else { return None };
        let ws = self.state.active()?;
        let container = ws.container(id)?;
        let tile = self.pane_bounds.borrow().get(id).copied();
        let (w, h) = tile.map_or((280., 200.), |b| (crate::ui::unpx(b.size.width), crate::ui::unpx(b.size.height)));
        let scale = (280. / w).min(200. / h).min(1.);
        let (gw, gh) = (w * scale, h * scale);
        let content = &container.content;
        let process = self.panes.get(id).and_then(|p| p.process.as_ref()).map(|p| p.name.clone());
        let name = content.custom_name.clone().or(process).or_else(|| content.label.clone()).unwrap_or_else(|| content.kind.label().to_owned());

        let ghost = div()
            .absolute()
            .left(pos.x - px(gw / 2.))
            .top(pos.y - px(gh / 2.))
            .w(px(gw))
            .h(px(gh))
            .rounded(px(8.))
            .overflow_hidden()
            .opacity(0.92)
            .bg(bg_primary())
            .border_1()
            .border_color(hexa(0xffffff, 0.08))
            .shadow(vec![BoxShadow {
                color: c(hexa(0x000000, 0.5)),
                offset: point(px(0.), px(12.)),
                blur_radius: px(40.),
                spread_radius: px(0.),
                inset: false,
            }])
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px((HEADER_H * scale).max(18.)))
                    .flex_none()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(border())
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(ACCENT_RED).opacity(0.7))
                    .child(div().min_w_0().truncate().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_PRIMARY).child(name)),
            )
            .child(div().flex_1().bg(bg_surface()));

        Some(
            div()
                .id("pane-drag-layer")
                .absolute()
                .inset_0()
                .occlude()
                .cursor_grabbing()
                .on_mouse_move(cx.listener(Self::on_drag_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
                .child(ghost)
                .into_any_element(),
        )
    }
}

/// macOS's close red and zoom green.
const LIGHT_RED: Rgba = hex(0xff5f57);
const LIGHT_GREEN: Rgba = hex(0x28c840);
const LIGHT_YELLOW: Rgba = hex(0xfebc2e);
/// A light that does nothing here (macOS's disabled grey).
const LIGHT_OFF: Rgba = hex(0x4a4a4c);

/// A 12px pane-header light, its glyph shown while the header's lights are hovered.
fn traffic_light(id: SharedString, color: Rgba, glyph: Option<&str>) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(12.))
        .flex_none()
        .rounded_full()
        .bg(color)
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .when_some(glyph, |d, glyph| {
            d.child(svg().path(icon(glyph)).size(px(12.)).text_color(hexa(0x000000, 0.)).group_hover("pane-lights", |s| s.text_color(hexa(0x000000, 0.55))))
        })
}
