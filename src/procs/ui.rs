//! The process views: the sidebar's per-terminal ports button and workspace
//! "ports" row (TerminalPorts / WorkspaceSidebar), the pane header's processes
//! button (TerminalProcessesButton), and the Resources task manager behind the
//! status bar's CPU segment (WorkspaceTaskManager).
//!
//! One poll per window feeds them all: every 2s (1.5s while the task manager
//! is open) it reads the shells' pids on the UI thread, scans on a background
//! thread, and folds the result in here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use super::table::{SortDir, SortKey, TermRow, next_sort, sort_rows, totals};
use super::{CPU_HOT, CpuTracker, Level, ListenPort, Scan, Signal, format_bytes, format_percent, merged_ports, meter, summarize, tree_entries};
use crate::app::AppView;
use crate::assets::icon;
use crate::model::layout::Direction;
use crate::model::workspace::ContentType;
use crate::terminal::procinfo::TreeEntry;
use crate::theme::*;

const POLL: Duration = Duration::from_millis(2000);
const POLL_TASK_MANAGER: Duration = Duration::from_millis(1500);
const PORTS_W: f32 = 240.;
const PROCESSES_W: f32 = 460.;

/// A terminal's tree, as of the last poll.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TermMetrics {
    pub pid: i32,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    /// The shell included.
    pub process_count: usize,
    pub ports: Vec<ListenPort>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PopKind {
    /// The sidebar's ports list of a terminal (container id).
    Ports(String),
    /// The pane header's process list of a terminal (container id).
    Processes(String),
}

pub(crate) struct Pop {
    kind: PopKind,
    at: Point<Pixels>,
    /// The terminal's name, for the header.
    label: String,
    tree: Option<Vec<TreeEntry>>,
    killing: HashSet<i32>,
    error: Option<String>,
}

pub(crate) struct TaskManager {
    all: bool,
    selected_ws: Option<String>,
    sort: (SortKey, SortDir),
    expanded: Option<String>,
    tree: Option<Vec<TreeEntry>>,
    killing: Option<i32>,
}

/// The window's process state (one field on AppView).
#[derive(Default)]
pub(crate) struct Procs {
    /// By container id; local terminals only.
    pub metrics: HashMap<String, TermMetrics>,
    cpu: CpuTracker,
    supported: bool,
    collected_at: Option<SystemTime>,
    pop: Option<Pop>,
    task_manager: Option<TaskManager>,
    poll: Option<Task<()>>,
    focus: Option<FocusHandle>,
    /// The popover a press outside just closed: when that press was on its own
    /// button, the click that follows mustn't open it again.
    dismissed: Option<(PopKind, std::time::Instant)>,
}

impl Procs {
    /// A popover or the task manager is drawn over the window.
    pub fn overlay_open(&self) -> bool {
        self.pop.is_some() || self.task_manager.is_some()
    }

    /// The task manager, drawn over the whole window.
    pub fn modal_open(&self) -> bool {
        self.task_manager.is_some()
    }

    /// The ports / processes popover, for the browser panes it may cover.
    pub(crate) fn pop_cover(&self) -> Option<crate::overlay::Cover> {
        self.pop.as_ref().map(|p| crate::overlay::Cover::menu(p.at))
    }
}

fn level_color(level: Level) -> Rgba {
    match level {
        Level::Ok => ACCENT_GREEN,
        Level::Warn => ACCENT_YELLOW,
        Level::Hot => ACCENT_RED,
    }
}

/// A horizontal usage bar: green, amber, then red as it fills (the Meter).
fn meter_bar(value: f64, max: f64, height: f32) -> Div {
    let (pct, level) = meter(value, max);
    div()
        .h(px(height))
        .mt(px(4.))
        .w_full()
        .rounded(px(height))
        .bg(bg_surface())
        .overflow_hidden()
        .child(div().h_full().w(relative(pct / 100.)).when(pct > 0., |d| d.min_w(px(2.))).rounded(px(height)).bg(level_color(level)))
}

fn popover_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow { color: c(hexa(0x000000, 0.45)), offset: point(px(0.), px(8.)), blur_radius: px(24.), spread_radius: px(0.), inset: false }]
}

/// "14:03:07" in local time.
fn clock(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as libc::time_t).unwrap_or(0);
    // SAFETY: localtime_r fills the struct we pass; no shared state.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

impl AppView {
    // ── Polling ─────────────────────────────────────────────────────────────

    /// Start this window's process poll (once).
    pub(crate) fn ensure_proc_poll(&mut self, cx: &mut Context<Self>) {
        if self.procs.poll.is_some() {
            return;
        }
        self.procs.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok((any, interval)) = this.update(cx, |this, cx| {
                    let interval = if this.procs.task_manager.is_some() { POLL_TASK_MANAGER } else { POLL };
                    (!this.proc_roots(cx).is_empty(), interval)
                }) else {
                    break;
                };
                if any {
                    let scan = cx.background_executor().spawn(async move { super::scan(false) }).await;
                    if this.update(cx, |this, cx| this.apply_scan(&scan, cx)).is_err() {
                        break;
                    }
                } else if this.update(cx, |this, cx| this.clear_metrics(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(interval).await;
            }
        }));
    }

    /// Each local terminal's container id and shell pid.
    fn proc_roots(&self, cx: &App) -> Vec<(String, i32)> {
        self.panes.iter().filter_map(|(id, p)| Some((id.clone(), p.view.read(cx).shell_pid()?))).collect()
    }

    fn clear_metrics(&mut self, cx: &mut Context<Self>) {
        if !self.procs.metrics.is_empty() {
            self.procs.metrics.clear();
            self.procs.cpu.retain(&HashSet::new());
            cx.notify();
        }
    }

    /// One scan now, outside the poll's rhythm: a popover opened, a row
    /// expanded, Refresh, or a kill (`fresh`: don't reuse a cached scan).
    fn refresh_procs_now(&mut self, fresh: bool, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let scan = cx.background_executor().spawn(async move { super::scan(fresh) }).await;
            let _ = this.update(cx, |this, cx| this.apply_scan(&scan, cx));
        })
        .detach();
    }

    fn apply_scan(&mut self, scan: &Arc<Scan>, cx: &mut Context<Self>) {
        let roots = self.proc_roots(cx);
        let procs = &mut self.procs;
        let mut metrics = HashMap::new();
        let mut live = HashSet::new();
        for (id, pid) in &roots {
            let s = summarize(scan, *pid);
            if s.process_count == 0 {
                continue;
            }
            live.insert(*pid);
            let cpu_percent = procs.cpu.sample(*pid, s.cpu_seconds, scan.at);
            metrics.insert(id.clone(), TermMetrics { pid: *pid, cpu_percent, rss_bytes: s.rss_bytes, process_count: s.process_count, ports: s.ports });
        }
        procs.cpu.retain(&live);
        let root_of = |id: &str| roots.iter().find(|(c, _)| c == id).map(|(_, p)| *p);
        let mut close_pop = false;
        if let Some(pop) = &mut procs.pop {
            match &pop.kind {
                PopKind::Processes(id) => pop.tree = Some(root_of(id).map(|p| tree_entries(&scan.rows, p)).unwrap_or_default()),
                // The ports button goes away with the last port, and its list with it.
                PopKind::Ports(id) => close_pop = metrics.get(id).is_none_or(|m| m.ports.is_empty()),
            }
        }
        if close_pop {
            procs.pop = None;
            if procs.task_manager.is_none() {
                self.focus_pending_now();
            }
        }
        let procs = &mut self.procs;
        if let Some(tm) = &mut procs.task_manager
            && let Some(id) = &tm.expanded
        {
            tm.tree = Some(root_of(id).map(|p| tree_entries(&scan.rows, p)).unwrap_or_default());
        }
        procs.supported = scan.supported;
        procs.collected_at = Some(SystemTime::now());
        if metrics != procs.metrics || procs.overlay_open() || close_pop {
            procs.metrics = metrics;
            cx.notify();
        }
    }

    // ── Overlay state ───────────────────────────────────────────────────────

    fn procs_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.procs.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        handle.focus(window, cx);
    }

    fn toggle_proc_pop(&mut self, kind: PopKind, label: String, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.procs.pop.as_ref().is_some_and(|p| p.kind == kind) {
            self.close_proc_pop(cx);
            return;
        }
        if let Some((k, at)) = self.procs.dismissed.take()
            && k == kind
            && at.elapsed() < Duration::from_millis(500)
        {
            return;
        }
        let processes = matches!(kind, PopKind::Processes(_));
        self.procs.pop = Some(Pop { kind, at, label, tree: None, killing: HashSet::new(), error: None });
        self.procs_focus(window, cx);
        if processes {
            self.refresh_procs_now(false, cx);
        }
        cx.notify();
    }

    /// A press outside the popover (capture phase, before any button's click).
    fn dismiss_proc_pop(&mut self, cx: &mut Context<Self>) {
        if let Some(pop) = &self.procs.pop {
            self.procs.dismissed = Some((pop.kind.clone(), std::time::Instant::now()));
        }
        self.close_proc_pop(cx);
    }

    fn close_proc_pop(&mut self, cx: &mut Context<Self>) {
        if self.procs.pop.take().is_some() {
            if self.procs.task_manager.is_none() {
                self.focus_pending_now();
            }
            cx.notify();
        }
    }

    pub(crate) fn toggle_task_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.procs.task_manager.take().is_some() {
            self.focus_pending_now();
            cx.notify();
            return;
        }
        self.procs.pop = None;
        let selected_ws = self.state.active_workspace_id.clone();
        self.procs.task_manager = Some(TaskManager { all: false, selected_ws, sort: (SortKey::Cpu, SortDir::Desc), expanded: None, tree: None, killing: None });
        self.procs_focus(window, cx);
        self.refresh_procs_now(false, cx);
        cx.notify();
    }

    fn close_task_manager(&mut self, cx: &mut Context<Self>) {
        if self.procs.task_manager.take().is_some() {
            self.focus_pending_now();
            cx.notify();
        }
    }

    // ── Actions ─────────────────────────────────────────────────────────────

    /// Signal processes of a terminal's tree off the UI thread, then rescan.
    /// The result's first error lands in the ports popover.
    fn kill_procs(&mut self, container_id: &str, pids: Vec<i32>, signal: Signal, cx: &mut Context<Self>) {
        let Some(root) = self.procs.metrics.get(container_id).map(|m| m.pid) else {
            return;
        };
        match signal {
            Signal::Kill => {
                if let Some(pop) = &mut self.procs.pop {
                    pop.error = None;
                    pop.killing.extend(&pids);
                }
            }
            Signal::Term => {
                if let Some(tm) = &mut self.procs.task_manager {
                    tm.killing = pids.first().copied();
                }
            }
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let done = pids.clone();
            let error =
                cx.background_executor().spawn(async move { pids.into_iter().map(|pid| super::kill_in_tree(root, pid, signal)).find_map(Result::err) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(pop) = &mut this.procs.pop {
                    for pid in &done {
                        pop.killing.remove(pid);
                    }
                    if signal == Signal::Kill {
                        pop.error = error;
                    }
                }
                if let Some(tm) = &mut this.procs.task_manager {
                    tm.killing = None;
                }
                this.refresh_procs_now(true, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Show `http://localhost:<port>` in workspace `ws_id`'s browser pane, or in
    /// a new one split off the terminal `near` (the Electron app opens the
    /// system browser; here the browser pane is the browser).
    pub(crate) fn open_port(&mut self, ws_id: &str, near: Option<String>, port: u16, window: &mut Window, cx: &mut Context<Self>) {
        let url = format!("http://localhost:{port}");
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) else {
            return;
        };
        // A browser pane of the user's; never one an agent is driving.
        let existing = ws
            .containers
            .iter()
            .find(|c| c.content.kind == ContentType::Browser && c.content.extra_str("browserOwnerTerminal").is_none())
            .map(|c| c.id.clone());
        let id = match existing {
            Some(id) => {
                if let Some(c) = ws.container_mut(&id) {
                    c.content.set_extra_str("browserUrl", Some(url.clone()));
                }
                id
            }
            None => {
                let id = ws.split(near.as_deref(), Direction::Horizontal);
                if let Some(c) = ws.container_mut(&id) {
                    let mut content = crate::browser::panes::new_content();
                    content.set_extra_str("browserUrl", Some(url.clone()));
                    c.content = content;
                }
                id
            }
        };
        ws.focused_container_id = Some(id.clone());
        if let Some(pane) = self.browser_pane(&id) {
            pane.update(cx, |p, cx| p.navigate(&url, cx));
        }
        self.procs.pop = None;
        self.procs.task_manager = None;
        self.focus_mode = None;
        self.focus_pending_now();
        self.switch_workspace(ws_id.to_owned(), window, cx);
        self.changed(cx);
    }

    fn workspace_of(&self, container_id: &str) -> Option<String> {
        self.state.workspaces.iter().find(|w| w.container(container_id).is_some()).map(|w| w.id.clone())
    }

    // ── Sidebar: ports button and the workspace's ports row ─────────────────

    /// The ports icon with its count after a terminal's name in the sidebar;
    /// None while the terminal listens on nothing.
    pub(crate) fn render_ports_button(&self, container_id: &str, label: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ports = &self.procs.metrics.get(container_id)?.ports;
        if ports.is_empty() {
            return None;
        }
        let kind = PopKind::Ports(container_id.to_owned());
        let open = self.procs.pop.as_ref().is_some_and(|p| p.kind == kind);
        let n = ports.len();
        let label = label.to_owned();
        let sidebar_w = px(self.settings.sidebar_width);
        Some(
            div()
                .id(SharedString::from(format!("ports-{container_id}")))
                .flex_none()
                .h(px(16.))
                .px(px(3.))
                .flex()
                .items_center()
                .gap(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(if open { TEXT_PRIMARY } else { ACCENT_BLUE })
                .when(open, |d| d.bg(bg_hover()))
                .hover(|d| d.bg(bg_hover()))
                .child(svg().path(icon("ports")).size(px(11.)).text_color(if open { TEXT_PRIMARY } else { ACCENT_BLUE }))
                .child(div().text_size(px(9.)).font_weight(FontWeight::BOLD).child(n.to_string()))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let at = point(sidebar_w + px(8.), ev.position().y - px(10.));
                    this.toggle_proc_pop(kind.clone(), label.clone(), at, window, cx);
                }))
                .into_any_element(),
        )
    }

    /// Every port the workspace's terminals host, as chips under its terminal
    /// rows (renderWorkspacePorts); a click opens it in the browser pane.
    pub(crate) fn render_workspace_ports(&self, ws_id: &str, container_ids: &[String], cx: &mut Context<Self>) -> Option<AnyElement> {
        let lists: Vec<&[ListenPort]> = container_ids.iter().filter_map(|id| self.procs.metrics.get(id)).map(|m| m.ports.as_slice()).collect();
        let ports = merged_ports(lists);
        if ports.is_empty() {
            return None;
        }
        let chips = ports.into_iter().map(|port| {
            // The pane to split a new browser off: the terminal hosting the port.
            let near = container_ids.iter().find(|id| self.procs.metrics.get(*id).is_some_and(|m| m.ports.iter().any(|p| p.port == port))).cloned();
            let ws_id = ws_id.to_owned();
            div()
                .id(SharedString::from(format!("wsport-{ws_id}-{port}")))
                .flex_none()
                .px(px(4.))
                .py(px(1.))
                .rounded(px(4.))
                .text_size(px(9.))
                .line_height(px(10.))
                .text_color(ACCENT_BLUE)
                .bg(hexa(0x4c9df3, 0.14))
                .border_1()
                .border_color(hexa(0x4c9df3, 0.35))
                .cursor_pointer()
                .hover(|d| d.bg(hexa(0x4c9df3, 0.24)))
                .child(format!(":{port}"))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_port(&ws_id, near.clone(), port, window, cx);
                }))
        });
        Some(
            div()
                .mt(px(5.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(4.))
                .child(div().flex_none().text_size(px(9.)).text_color(TEXT_MUTED).child("ports"))
                .children(chips)
                .into_any_element(),
        )
    }

    // ── Pane header: processes button ───────────────────────────────────────

    /// Lit while the terminal's shell has children; hidden otherwise (kept
    /// while its list is open, so it can't vanish from under the pointer).
    pub(crate) fn render_processes_button(&self, container_id: &str, ssh: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let kind = PopKind::Processes(container_id.to_owned());
        let open = self.procs.pop.as_ref().is_some_and(|p| p.kind == kind);
        let children = self.procs.metrics.get(container_id).map_or(0, |m| m.process_count.saturating_sub(1));
        let lit = !ssh && children > 0;
        if !lit && !open {
            return None;
        }
        let color = if open {
            TEXT_PRIMARY
        } else if lit {
            ACCENT_GREEN
        } else {
            TEXT_MUTED
        };
        let group = SharedString::from(format!("procs-{container_id}"));
        Some(
            div()
                .id(SharedString::from(format!("procs-btn-{container_id}")))
                .group(group.clone())
                .size(px(20.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .when(open, |d| d.bg(bg_hover()))
                .hover(|d| d.bg(bg_hover()))
                .child(svg().path(icon("processes")).size(px(14.)).text_color(color).group_hover(group, |s| s.text_color(TEXT_PRIMARY)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let p = ev.position();
                    let at = point(p.x + px(10.) - px(PROCESSES_W), p.y + px(14.));
                    this.toggle_proc_pop(kind.clone(), String::new(), at, window, cx);
                }))
                .into_any_element(),
        )
    }

    // ── Status bar: the Resources segment ───────────────────────────────────

    /// "CPU 3.2% · 412 MB · ⎓ 5 (2 bg)": opens the task manager.
    pub(crate) fn render_resources_segment(&self, cx: &mut Context<Self>) -> AnyElement {
        let cpu: f32 = self.procs.metrics.values().map(|m| m.cpu_percent).sum();
        let rss: u64 = self.procs.metrics.values().map(|m| m.rss_bytes).sum();
        let visible: HashSet<String> = self.state.active().and_then(|w| w.layout_tree.as_ref()).map(|t| t.leaf_ids().into_iter().collect()).unwrap_or_default();
        let terminals = self.panes.len();
        let hidden = self.panes.keys().filter(|id| !visible.contains(*id)).count();
        let open = self.procs.task_manager.is_some();
        div()
            .id("resources-segment")
            .h_full()
            .flex_none()
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(5.))
            .cursor_pointer()
            .when(open, |d| d.bg(bg_hover()))
            .hover(|d| d.bg(bg_hover()))
            .child(svg().path(icon("activity")).size(px(12.)).text_color(TEXT_MUTED))
            .child(format!("CPU {}", format_percent(cpu)))
            .child(div().opacity(0.5).child("·"))
            .child(format_bytes(rss))
            .child(div().opacity(0.5).child("·"))
            .child(if hidden > 0 { format!("⎓ {terminals} ({hidden} bg)") } else { format!("⎓ {terminals}") })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_task_manager(window, cx)))
            .into_any_element()
    }

    // ── Overlays ────────────────────────────────────────────────────────────

    /// The open popover and/or the task manager, drawn over everything.
    pub(crate) fn render_procs_overlay(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.procs.task_manager.is_some() {
            out.push(self.render_task_manager(cx));
        }
        if let Some(pop) = &self.procs.pop {
            let body = match &pop.kind {
                PopKind::Ports(id) => self.render_ports_pop(id, pop, cx),
                PopKind::Processes(id) => self.render_processes_pop(id, pop, window, cx),
            };
            let at = pop.at;
            let focus = self.procs.focus.clone();
            out.push(
                deferred(
                    anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                        div()
                            .id("procs-popover")
                            .child(crate::overlay::recorder(at, 0))
                            .when_some(focus, |d, f| d.track_focus(&f))
                            .flex()
                            .flex_col()
                            .bg(bg_surface())
                            .border_1()
                            .border_color(border())
                            .rounded(px(8.))
                            .shadow(popover_shadow())
                            .overflow_hidden()
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_proc_pop(cx)))
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                                if ev.keystroke.key == "escape" {
                                    this.close_proc_pop(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .child(body),
                    ),
                )
                .with_priority(2)
                .into_any_element(),
            );
        }
        out
    }

    fn render_ports_pop(&self, container_id: &str, pop: &Pop, cx: &mut Context<Self>) -> AnyElement {
        let ports = self.procs.metrics.get(container_id).map(|m| m.ports.clone()).unwrap_or_default();
        let ws_id = self.workspace_of(container_id).unwrap_or_default();
        let rows = ports.into_iter().map(|p| {
            let busy = p.pids.iter().any(|pid| pop.killing.contains(pid));
            let port = p.port;
            let (ws, near, cid) = (ws_id.clone(), container_id.to_owned(), container_id.to_owned());
            let pids = p.pids.clone();
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(6.))
                .child(
                    div()
                        .id(SharedString::from(format!("port-open-{port}")))
                        .flex_none()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ACCENT_BLUE)
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .child(format!(":{port}"))
                        .on_click(cx.listener(move |this, _, window, cx| this.open_port(&ws, Some(near.clone()), port, window, cx))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.))
                        .text_color(TEXT_MUTED)
                        .child(format!("pid {}", p.pids.iter().map(i32::to_string).collect::<Vec<_>>().join(", "))),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("port-kill-{port}")))
                        .flex_none()
                        .px(px(6.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(hexa(0xf14c4c, 0.4))
                        .text_size(px(10.))
                        .line_height(px(16.))
                        .map(|d| {
                            if busy {
                                d.text_color(TEXT_MUTED).opacity(0.6)
                            } else {
                                d.text_color(ACCENT_RED).cursor_pointer().hover(|d| d.bg(hexa(0xf14c4c, 0.12)))
                            }
                        })
                        .child(if busy { "…" } else { "Kill" })
                        .when(!busy, |d| d.on_click(cx.listener(move |this, _, _, cx| this.kill_procs(&cid, pids.clone(), Signal::Kill, cx)))),
                )
        });
        div()
            .w(px(PORTS_W))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .flex()
                    .justify_between()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(border())
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(TEXT_PRIMARY)
                    .child("Ports")
                    .when(!pop.label.is_empty(), |d| {
                        d.child(div().max_w(px(130.)).truncate().font_weight(FontWeight::NORMAL).text_color(TEXT_MUTED).child(pop.label.clone()))
                    }),
            )
            .child(div().id("ports-list").max_h(px(240.)).overflow_y_scroll().children(rows))
            .when_some(pop.error.clone(), |d, e| {
                d.child(div().px(px(12.)).py(px(6.)).border_t_1().border_color(border()).text_size(px(10.)).text_color(ACCENT_RED).child(e))
            })
            .into_any_element()
    }

    fn render_processes_pop(&self, container_id: &str, pop: &Pop, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let ssh = self.state.workspaces.iter().find_map(|w| w.container(container_id)).is_some_and(|c| c.content.is_ssh_terminal());
        let spawned = pop.tree.as_ref().map_or(0, |t| t.iter().filter(|p| p.depth > 0).count());
        let message = |text: &'static str| div().p(px(14.)).text_size(px(12.)).text_color(TEXT_MUTED).child(text).into_any_element();
        let mono = fonts().terminal;
        let body: Vec<AnyElement> = if ssh {
            vec![message("Process list is only available for local terminals (SSH runs on the remote host).")]
        } else {
            match &pop.tree {
                None => vec![message("Loading…")],
                Some(t) if t.is_empty() => vec![message("No processes.")],
                Some(t) => t
                    .iter()
                    .map(|p| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(6.))
                            .border_b_1()
                            .border_color(BORDER_SUBTLE)
                            .text_size(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .pl(px(p.depth.min(6) as f32 * 12.))
                                    .font_family(mono.clone())
                                    .text_color(if p.depth == 0 { TEXT_MUTED } else { TEXT_PRIMARY })
                                    .child(if p.depth > 0 { format!("└ {}", p.command) } else { p.command.clone() }),
                            )
                            .child(div().w(px(56.)).flex_none().text_right().text_size(px(11.)).text_color(TEXT_MUTED).child(p.pid.to_string()))
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_none()
                                    .text_right()
                                    .text_color(if p.cpu_percent > CPU_HOT { ACCENT_RED } else { TEXT_PRIMARY })
                                    .child(format_percent(p.cpu_percent)),
                            )
                            .child(div().w(px(64.)).flex_none().text_right().text_color(TEXT_PRIMARY).child(format_bytes(p.rss_bytes)))
                            .into_any_element()
                    })
                    .collect(),
            }
        };
        div()
            .w(px(PROCESSES_W))
            .max_h(window.viewport_size().height * 0.6)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .py(px(8.))
                    .border_b_1()
                    .border_color(border())
                    .text_size(px(11.))
                    .text_color(TEXT_MUTED)
                    .child(format!("PROCESSES — {spawned} SPAWNED"))
                    .child(
                        div()
                            .id("procs-refresh")
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .group("procs-refresh")
                            .child(
                                svg().path(icon("refresh")).size(px(12.)).text_color(TEXT_MUTED).group_hover("procs-refresh", |s| s.text_color(TEXT_PRIMARY)),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_procs_now(true, cx))),
                    ),
            )
            .child(div().id("procs-list").flex_1().min_h_0().overflow_y_scroll().children(body))
            .into_any_element()
    }

    // ── Task manager ────────────────────────────────────────────────────────

    /// One row per terminal pane of these workspaces (local and SSH).
    fn task_rows(&self, ws_ids: Option<&str>, cx: &App) -> Vec<TermRow> {
        let mut rows = Vec::new();
        for ws in self.state.workspaces.iter().filter(|w| ws_ids.is_none_or(|id| w.id == id)) {
            let order = ws.layout_tree.as_ref().map(|t| t.leaf_ids()).unwrap_or_default();
            for c in order.iter().filter_map(|id| ws.container(id)) {
                let Some(pane) = self.panes.get(&c.id) else {
                    continue;
                };
                let ssh = c.content.is_ssh_terminal();
                let m = self.procs.metrics.get(&c.id);
                let process = pane.process.as_ref();
                let label = c
                    .content
                    .custom_name
                    .clone()
                    .or_else(|| process.map(|p| p.name.clone()))
                    .or_else(|| c.content.label.clone())
                    .unwrap_or_else(|| "Terminal".into());
                let sub = if ssh {
                    let conn = c.content.connection_id.as_deref().and_then(|cid| self.connections.lock().unwrap().get(cid).cloned());
                    conn.map(|c| format!("{}@{}", c.username, c.host)).unwrap_or_else(|| "ssh".into())
                } else {
                    process.and_then(|p| p.cwd.clone()).or_else(|| c.content.cwd.clone()).map(|p| crate::grid::tilde(&p)).unwrap_or_else(|| "local".into())
                };
                rows.push(TermRow {
                    container_id: c.id.clone(),
                    workspace_id: ws.id.clone(),
                    workspace_name: ws.name.clone(),
                    label,
                    sub,
                    pid: m.map(|m| m.pid).or_else(|| pane.view.read(cx).shell_pid()),
                    ssh,
                    cpu_percent: m.map_or(0., |m| m.cpu_percent),
                    rss_bytes: m.map_or(0, |m| m.rss_bytes),
                    process_count: m.map_or(0, |m| m.process_count),
                    port_count: m.map_or(0, |m| m.ports.len()),
                });
            }
        }
        rows
    }

    fn render_task_manager(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(tm) = &self.procs.task_manager else {
            return div().into_any_element();
        };
        let all_rows = self.task_rows(None, cx);
        let sum = totals(&all_rows);
        let selected = tm
            .selected_ws
            .clone()
            .filter(|id| self.state.workspaces.iter().any(|w| &w.id == id))
            .or_else(|| self.state.workspaces.first().map(|w| w.id.clone()));
        let mut rows: Vec<TermRow> =
            if tm.all { all_rows.clone() } else { all_rows.iter().filter(|r| Some(&r.workspace_id) == selected.as_ref()).cloned().collect() };
        sort_rows(&mut rows, tm.sort.0, tm.sort.1);

        let stat = |label: &'static str, value: String| {
            div()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(10.)).text_color(TEXT_MUTED).child(label.to_uppercase()))
                .child(div().mt(px(4.)).text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).truncate().child(value))
        };
        let summary = div()
            .flex_none()
            .flex()
            .gap(px(10.))
            .px(px(18.))
            .py(px(14.))
            .border_b_1()
            .border_color(border())
            .child(stat("Terminals", sum.terminals.to_string()))
            .child(stat("Shell CPU", format_percent(sum.cpu_percent)))
            .child(stat("Shell RSS", format_bytes(sum.rss_bytes)))
            .child(stat("Processes", sum.processes.to_string()))
            .child(stat("Ports", sum.ports.to_string()));

        let mode_button = |id: &'static str, label: &'static str, on: bool, all: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(5.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .map(|d| if on { d.bg(bg_hover()).text_color(TEXT_PRIMARY) } else { d.text_color(TEXT_MUTED).hover(|d| d.text_color(TEXT_PRIMARY)) })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(tm) = &mut this.procs.task_manager {
                        tm.all = all;
                        cx.notify();
                    }
                }))
        };
        let modes = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(14.))
            .py(px(8.))
            .border_b_1()
            .border_color(border())
            .child(mode_button("tm-grouped", "By workspace", !tm.all, false, cx))
            .child(mode_button("tm-all", "All terminals", tm.all, true, cx))
            .child(div().ml_auto().text_size(px(10.)).text_color(TEXT_MUTED).child("Click a terminal to see its process tree"));

        let table = self.render_term_table(tm, &rows, if tm.all { "No terminal sessions." } else { "No terminal sessions in this workspace." }, cx);
        let main = if tm.all {
            div().flex_1().min_h_0().flex().flex_col().child(table)
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(div().w(relative(0.4)).min_w(px(300.)).flex_none().flex().flex_col().border_r_1().border_color(border()).child(self.render_ws_list(
                    &all_rows,
                    selected.as_deref(),
                    cx,
                )))
                .child(div().flex_1().min_w_0().flex().flex_col().child(table))
        };

        let footer = div()
            .flex_none()
            .flex()
            .justify_between()
            .px(px(14.))
            .py(px(7.))
            .border_t_1()
            .border_color(border())
            .text_size(px(10.))
            .text_color(TEXT_MUTED)
            .child(if self.procs.supported { "Process metrics: enabled" } else { "Process metrics: unavailable" })
            .child(self.procs.collected_at.map(clock).unwrap_or_else(|| "Loading".into()));

        let panel = div()
            .id("task-manager")
            .w(relative(0.92))
            .max_w(px(1100.))
            .h(relative(0.82))
            .flex()
            .flex_col()
            .bg(bg_surface())
            .rounded(px(12.))
            .overflow_hidden()
            .shadow(vec![BoxShadow { color: c(hexa(0, 0.5)), offset: point(px(0.), px(20.)), blur_radius: px(60.), spread_radius: px(0.), inset: false }])
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(18.))
                    .py(px(14.))
                    .border_b_1()
                    .border_color(border())
                    .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).child("Resources"))
                    .child(
                        div()
                            .id("tm-close")
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .text_size(px(16.))
                            .text_color(TEXT_MUTED)
                            .cursor_pointer()
                            .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                            .child("✕")
                            .on_click(cx.listener(|this, _, _, cx| this.close_task_manager(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().bg(bg_primary()).child(summary).child(modes).child(main).child(footer));

        let focus = self.procs.focus.clone();
        div()
            .id("task-manager-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hexa(0, 0.6))
            .occlude()
            .when_some(focus, |d, f| d.track_focus(&f))
            .on_click(cx.listener(|this, _, _, cx| this.close_task_manager(cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.procs.pop.is_none() {
                    this.close_task_manager(cx);
                    cx.stop_propagation();
                }
            }))
            .child(panel)
            .into_any_element()
    }

    /// The "By workspace" list: each workspace's terminals summed; a click selects it.
    fn render_ws_list(&self, all_rows: &[TermRow], selected: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .flex_none()
            .flex()
            .gap(px(8.))
            .px(px(14.))
            .py(px(9.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .text_size(px(10.))
            .text_color(TEXT_MUTED)
            .child(div().flex_1().child("WORKSPACE"))
            .child(div().w(px(48.)).child("TERMS"))
            .child(div().w(px(80.)).child("CPU"))
            .child(div().w(px(80.)).child("RSS"));
        let max_rss = self.state.workspaces.iter().map(|w| totals(all_rows.iter().filter(|r| r.workspace_id == w.id)).rss_bytes).max().unwrap_or(0).max(1);
        let items = self.state.workspaces.iter().map(|w| {
            let t = totals(all_rows.iter().filter(|r| r.workspace_id == w.id));
            let on = selected == Some(w.id.as_str());
            let id = w.id.clone();
            div()
                .id(SharedString::from(format!("tm-ws-{}", w.id)))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(10.))
                .border_b_1()
                .border_color(BORDER_SUBTLE)
                .cursor_pointer()
                .map(|d| if on { d.bg(bg_hover()) } else { d.hover(|d| d.bg(bg_hover())) })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div().truncate().text_size(px(12.)).font_weight(if on { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM }).child(w.name.clone()),
                        )
                        .child(div().mt(px(3.)).truncate().text_size(px(10.)).text_color(TEXT_MUTED).child(format!(
                            "{} processes | {} ports | {}",
                            t.processes,
                            t.ports,
                            format_bytes(t.rss_bytes)
                        ))),
                )
                .child(div().w(px(48.)).text_size(px(12.)).child(t.terminals.to_string()))
                .child(
                    div()
                        .w(px(80.))
                        .text_size(px(12.))
                        .child(div().text_color(if t.cpu_percent > CPU_HOT { ACCENT_RED } else { TEXT_PRIMARY }).child(format_percent(t.cpu_percent)))
                        .child(meter_bar(t.cpu_percent as f64, 100., 4.)),
                )
                .child(div().w(px(80.)).text_size(px(12.)).child(format_bytes(t.rss_bytes)).child(meter_bar(t.rss_bytes as f64, max_rss as f64, 4.)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(tm) = &mut this.procs.task_manager {
                        tm.selected_ws = Some(id.clone());
                        cx.notify();
                    }
                }))
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(div().id("tm-ws-list").flex_1().min_h_0().overflow_y_scroll().children(items))
            .into_any_element()
    }

    fn render_term_table(&self, tm: &TaskManager, rows: &[TermRow], empty: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let sort_header = |label: &'static str, key: SortKey, w: Option<f32>, cx: &mut Context<Self>| {
            let active = tm.sort.0 == key;
            let arrow = if !active {
                ""
            } else if tm.sort.1 == SortDir::Asc {
                "▲"
            } else {
                "▼"
            };
            div()
                .id(SharedString::from(format!("tm-sort-{label}")))
                .map(|d| match w {
                    Some(w) => d.w(px(w)).flex_none(),
                    None => d.flex_1().min_w_0(),
                })
                .flex()
                .items_center()
                .gap(px(3.))
                .cursor_pointer()
                .text_color(if active { TEXT_PRIMARY } else { TEXT_MUTED })
                .hover(|d| d.text_color(TEXT_PRIMARY))
                .child(label)
                .child(div().w(px(7.)).child(arrow))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(tm) = &mut this.procs.task_manager {
                        tm.sort = next_sort(tm.sort, key);
                        cx.notify();
                    }
                }))
        };
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(9.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .text_size(px(10.))
            .child(div().w(px(18.)).flex_none())
            .child(sort_header("TERMINAL", SortKey::Name, None, cx))
            .child(div().w(px(58.)).flex_none().text_color(TEXT_MUTED).child("PID"))
            .child(sort_header("PROCS", SortKey::Procs, Some(58.), cx))
            .child(sort_header("PORTS", SortKey::Ports, Some(58.), cx))
            .child(sort_header("CPU", SortKey::Cpu, Some(82.), cx))
            .child(sort_header("RSS", SortKey::Rss, Some(90.), cx));

        let max_rss = rows.iter().map(|r| r.rss_bytes).max().unwrap_or(0).max(1) as f64;
        let body: Vec<AnyElement> = if rows.is_empty() {
            vec![div().p(px(18.)).text_size(px(12.)).text_color(TEXT_MUTED).child(empty).into_any_element()]
        } else {
            rows.iter()
                .map(|r| {
                    let expanded = tm.expanded.as_deref() == Some(r.container_id.as_str());
                    let toggle_id = r.container_id.clone();
                    let sub = if tm.all { format!("{} · {}", r.workspace_name, r.sub) } else { r.sub.clone() };
                    let alive = r.ssh || self.procs.metrics.contains_key(&r.container_id);
                    let line = div()
                        .id(SharedString::from(format!("tm-row-{}", r.container_id)))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(14.))
                        .py(px(10.))
                        .text_size(px(12.))
                        .cursor_pointer()
                        .map(|d| if expanded { d.bg(bg_hover()) } else { d.hover(|d| d.bg(bg_hover())) })
                        .child(div().w(px(18.)).flex_none().text_size(px(10.)).text_color(TEXT_MUTED).child(if expanded { "▼" } else { "▶" }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(div().size(px(6.)).flex_none().rounded_full().bg(if alive { ACCENT_GREEN } else { ACCENT_YELLOW }))
                                        .child(div().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(r.label.clone())),
                                )
                                .child(div().mt(px(3.)).truncate().text_size(px(10.)).text_color(TEXT_MUTED).child(sub)),
                        )
                        .child(div().w(px(58.)).flex_none().text_color(TEXT_MUTED).child(r.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())))
                        .child(div().w(px(58.)).flex_none().child(if r.ssh { "-".into() } else { r.process_count.to_string() }))
                        .child(
                            div().w(px(58.)).flex_none().text_color(if r.port_count > 0 { ACCENT_BLUE } else { TEXT_PRIMARY }).child(r.port_count.to_string()),
                        )
                        .child(
                            div()
                                .w(px(82.))
                                .flex_none()
                                .child(div().text_color(if r.cpu_percent > CPU_HOT { ACCENT_RED } else { TEXT_PRIMARY }).child(if r.ssh {
                                    "-".into()
                                } else {
                                    format_percent(r.cpu_percent)
                                }))
                                .child(meter_bar(r.cpu_percent as f64, 100., 4.)),
                        )
                        .child(div().w(px(90.)).flex_none().child(format_bytes(r.rss_bytes)).child(meter_bar(r.rss_bytes as f64, max_rss, 4.)))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_task_row(&toggle_id, cx)));
                    div().border_b_1().border_color(BORDER_SUBTLE).child(line).when(expanded, |d| d.child(self.render_tm_tree(tm, r, cx))).into_any_element()
                })
                .collect()
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(div().id("tm-rows").flex_1().min_h_0().overflow_y_scroll().children(body))
            .into_any_element()
    }

    fn toggle_task_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(tm) = &mut self.procs.task_manager else {
            return;
        };
        if tm.expanded.as_deref() == Some(id) {
            tm.expanded = None;
        } else {
            tm.expanded = Some(id.to_owned());
            self.refresh_procs_now(false, cx);
        }
        if let Some(tm) = &mut self.procs.task_manager {
            tm.tree = None;
        }
        cx.notify();
    }

    /// The expanded terminal's tree: each process with CPU, RSS and Kill (SIGTERM).
    fn render_tm_tree(&self, tm: &TaskManager, r: &TermRow, cx: &mut Context<Self>) -> AnyElement {
        let note =
            |text: &'static str| div().pl(px(40.)).pr(px(14.)).pt(px(8.)).pb(px(12.)).text_size(px(11.)).text_color(TEXT_MUTED).child(text).into_any_element();
        if r.ssh {
            return note("Process tree is unavailable for SSH sessions (the processes run on the remote host).");
        }
        let Some(tree) = &tm.tree else {
            return note("Loading process tree…");
        };
        if tree.is_empty() {
            return note("No child processes.");
        }
        let max_rss = tree.iter().map(|p| p.rss_bytes).max().unwrap_or(0).max(1) as f64;
        let rows = tree.iter().map(|p| {
            let killing = tm.killing == Some(p.pid);
            let (cid, pid) = (r.container_id.clone(), p.pid);
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(6.))
                .py(px(5.))
                .text_size(px(11.))
                .border_b_1()
                .border_color(BORDER_SUBTLE)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pl(px(p.depth as f32 * 14.))
                        .flex()
                        .gap(px(4.))
                        .child(if p.depth > 0 { div().flex_none().text_color(TEXT_MUTED).child("└") } else { div() })
                        .child(div().min_w_0().truncate().child(p.command.clone())),
                )
                .child(div().w(px(60.)).flex_none().text_color(TEXT_MUTED).child(p.pid.to_string()))
                .child(
                    div()
                        .w(px(84.))
                        .flex_none()
                        .text_color(if p.cpu_percent > CPU_HOT { ACCENT_RED } else { TEXT_PRIMARY })
                        .child(format_percent(p.cpu_percent)),
                )
                .child(div().w(px(90.)).flex_none().child(format_bytes(p.rss_bytes)).child(meter_bar(p.rss_bytes as f64, max_rss, 3.)))
                .child(
                    div().w(px(60.)).flex_none().flex().justify_end().child(
                        div()
                            .id(SharedString::from(format!("tm-kill-{pid}")))
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(border())
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .map(|d| {
                                if killing {
                                    d.text_color(TEXT_MUTED).opacity(0.5)
                                } else {
                                    d.text_color(ACCENT_RED).cursor_pointer().hover(|d| d.bg(hexa(0xf14c4c, 0.12)))
                                }
                            })
                            .child(if killing { "…" } else { "Kill" })
                            .when(!killing, |d| {
                                d.on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.kill_procs(&cid, vec![pid], Signal::Term, cx);
                                }))
                            }),
                    ),
                )
        });
        div().px(px(14.)).pt(px(4.)).pb(px(10.)).bg(bg_primary()).children(rows).into_any_element()
    }
}
