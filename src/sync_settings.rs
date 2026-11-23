//! Workspace SFTP sync settings — the Electron app's Workspace Settings modal
//! (App.tsx: the "SFTP auto sync" and "Sync logs" cards with the Save footer)
//! and the status bar's sync segment (StatusBar.tsx `deriveSync`), opened the
//! same two ways: the workspace menu's "Settings…" (which switches to that
//! workspace first) and a click on the status-bar segment.
//!
//! Like Electron, the modal edits a draft of the config fields (enabled,
//! useGitIgnore, each target's name / enabled / connection / remote path) and
//! only Save writes it. Save re-reads the live `sync` and carries its runtime
//! data — logs, fileStates, each target's lastSync* — and every field this app
//! doesn't know (at both levels), so the JSON in `workspace.sync` stays the
//! shape the Electron app writes. Electron's sync has no exclude list or
//! direction of its own: `.gitignore` is the only exclusion knob.
//!
//! Left out: the modal's Working Directory card (Lite has the menu's "Set
//! working folder…") and the per-workspace Notifications card.

use std::collections::HashMap;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Map, Value};
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::AppView;
use crate::model::workspace::{Workspace, now_ms};
use crate::settings::{card, toggle};
use crate::sftp_sync::{self, SyncConfig, SyncTarget};
use crate::theme::*;

// ─── Draft (pure) ───────────────────────────────────────────────────────────

/// The editable part of `workspace.sync` (App.tsx's WorkspaceSettingsDraft).
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub enabled: bool,
    pub use_git_ignore: bool,
    pub targets: Vec<SyncTarget>,
}

impl Draft {
    /// Seeded from the stored config; no config is sync off with `.gitignore` on (Electron's default).
    pub fn from_value(raw: Option<&Value>) -> Draft {
        let cfg: Option<SyncConfig> = raw.and_then(|v| serde_json::from_value(v.clone()).ok());
        Draft {
            enabled: cfg.as_ref().is_some_and(|c| c.enabled),
            use_git_ignore: raw.and_then(|v| v.get("useGitIgnore")).and_then(Value::as_bool).unwrap_or(true),
            targets: cfg.map(|c| c.targets).unwrap_or_default(),
        }
    }

    pub fn of(ws: &Workspace) -> Draft {
        Self::from_value(ws.extra.get("sync"))
    }

    /// Differs from what is stored (runtime status fields on targets don't count).
    pub fn dirty_against(&self, raw: Option<&Value>) -> bool {
        let live = Self::from_value(raw);
        if self.enabled != live.enabled || self.use_git_ignore != live.use_git_ignore || self.targets.len() != live.targets.len() {
            return true;
        }
        let keys: HashMap<&str, String> = live.targets.iter().map(|t| (t.id.as_str(), config_key(t))).collect();
        self.targets.iter().any(|t| keys.get(t.id.as_str()) != Some(&config_key(t)))
    }

    /// The config Save stores: the draft over the live config, keeping its
    /// runtime data and unknown fields; `fileStates` / `logs` written even when
    /// empty, as Electron's save does.
    pub fn merged_into(&self, live: Option<SyncConfig>) -> SyncConfig {
        let mut out = live.unwrap_or_default();
        let by_id: HashMap<String, SyncTarget> = out.targets.drain(..).map(|t| (t.id.clone(), t)).collect();
        out.enabled = self.enabled;
        out.use_git_ignore = self.use_git_ignore;
        out.targets = self
            .targets
            .iter()
            .map(|t| {
                let live = by_id.get(&t.id);
                let mut extra = t.extra.clone();
                if let Some(l) = live {
                    for (k, v) in &l.extra {
                        extra.insert(k.clone(), v.clone());
                    }
                }
                SyncTarget {
                    last_sync_at: live.and_then(|l| l.last_sync_at),
                    last_sync_status: live.and_then(|l| l.last_sync_status.clone()),
                    last_sync_error: live.and_then(|l| l.last_sync_error.clone()),
                    extra,
                    ..t.clone()
                }
            })
            .collect();
        out.extra.entry("fileStates").or_insert_with(|| Value::Object(Map::new()));
        out.logs.get_or_insert_with(Vec::new);
        out
    }
}

/// What makes a target's config (App.tsx's targetConfigKey).
fn config_key(t: &SyncTarget) -> String {
    format!("{}|{}|{}|{}|{}", t.id, t.name, u8::from(t.enabled), t.connection_id.as_deref().unwrap_or(""), t.remote_path)
}

/// "+ Add target": `target-<ms>-<6 base36>`, "Target N", enabled, nothing chosen yet.
pub fn new_target(index: usize, now: i64) -> SyncTarget {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let suffix: String = (0..6).map(|_| DIGITS[rand::random_range(0..DIGITS.len())] as char).collect();
    SyncTarget {
        id: format!("target-{now}-{suffix}"),
        name: format!("Target {index}"),
        enabled: true,
        connection_id: None,
        remote_path: String::new(),
        ..Default::default()
    }
}

// ─── Status (pure) ──────────────────────────────────────────────────────────

/// The status bar's sync segment (StatusBar.tsx `deriveSync`, plus a running Sync now).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Off,
    Ready,
    Synced(i64),
    Error,
    Running,
}

pub fn badge(sync: Option<&SyncConfig>, running: bool) -> Badge {
    let Some(sync) = sync.filter(|s| s.enabled) else {
        return Badge::Off;
    };
    let enabled: Vec<&SyncTarget> = sync.targets.iter().filter(|t| t.enabled).collect();
    if enabled.is_empty() {
        return Badge::Off;
    }
    if running {
        return Badge::Running;
    }
    if enabled.iter().any(|t| t.last_sync_status.as_deref() == Some("error")) {
        return Badge::Error;
    }
    match enabled.iter().filter_map(|t| t.last_sync_at).filter(|&t| t > 0).max() {
        Some(at) => Badge::Synced(at),
        None => Badge::Ready,
    }
}

/// Local wall-clock time of a ms timestamp: "14:03" or "14:03:22".
fn clock(ms: i64, seconds: bool) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: an all-zero tm is a valid value for localtime_r to fill.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes the struct we own.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    if seconds { format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec) } else { format!("{:02}:{:02}", tm.tm_hour, tm.tm_min) }
}

/// The settings card's line under Sync now, from the run it started (Electron's syncNowStatus).
fn run_status(running: bool, started: Option<i64>, sync: Option<&SyncConfig>) -> Option<String> {
    if running {
        return Some("Running sync...".into());
    }
    let started = started?;
    let log = sync?.logs.as_ref()?.iter().rev().find(|l| l["action"] == "sync-now" && l["at"].as_i64().is_some_and(|at| at >= started))?;
    let msg = log["message"].as_str().unwrap_or_default();
    Some(match msg.strip_suffix(" (cancelled)") {
        Some(m) => format!("Cancelled: {m}"),
        None => format!("Done: {msg}"),
    })
}

// ─── Modal state ────────────────────────────────────────────────────────────

struct TargetRow {
    /// The target as seeded / added; name and remote path live in the inputs.
    base: SyncTarget,
    name: Entity<InputState>,
    path: Entity<InputState>,
}

/// The open Workspace Settings modal.
pub struct SyncSettingsUi {
    ws_id: String,
    enabled: bool,
    use_git_ignore: bool,
    targets: Vec<TargetRow>,
    /// The target whose connection menu is open.
    conn_menu: Option<String>,
    saved: bool,
    /// "Save changes first" and the like; cleared when a run starts.
    notice: Option<String>,
    /// When this modal started a Sync now (for its Done / Cancelled line).
    run_started: Option<i64>,
    _subs: Vec<Subscription>,
}

impl SyncSettingsUi {
    fn draft(&self, cx: &App) -> Draft {
        Draft {
            enabled: self.enabled,
            use_git_ignore: self.use_git_ignore,
            targets: self
                .targets
                .iter()
                .map(|r| SyncTarget { name: r.name.read(cx).value().to_string(), remote_path: r.path.read(cx).value().to_string(), ..r.base.clone() })
                .collect(),
        }
    }
}

fn make_row(t: SyncTarget, subs: &mut Vec<Subscription>, window: &mut Window, cx: &mut Context<AppView>) -> TargetRow {
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Target name").default_value(t.name.clone()));
    let path = cx.new(|cx| InputState::new(window, cx).placeholder("/var/www/project").default_value(t.remote_path.clone()));
    for input in [&name, &path] {
        subs.push(cx.subscribe_in(input, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                if let Some(ui) = this.sync_settings.as_mut() {
                    ui.saved = false;
                }
                cx.notify();
            }
        }));
    }
    TargetRow { base: t, name, path }
}

fn field(state: &Entity<InputState>, window: &Window, cx: &App) -> Div {
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    div()
        .px(px(11.))
        .py(px(4.))
        .rounded(px(9.))
        .bg(bg_primary())
        .border_1()
        .border_color(if focused { ACCENT } else { border() })
        .text_size(px(12.))
        .child(Input::new(state).appearance(false).small())
}

fn ghost(id: impl Into<ElementId>, label: &'static str, color: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(12.))
        .py(px(7.))
        .rounded(px(9.))
        .border_1()
        .border_color(border())
        .bg(bg_primary())
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .cursor_pointer()
        .hover(|d| d.bg(bg_hover()))
        .child(label)
}

fn checkbox(checked: bool) -> Div {
    div()
        .size(px(13.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .border_color(if checked { ACCENT } else { border() })
        .when(checked, |d| d.bg(ACCENT).text_color(hex(0xffffff)).text_size(px(9.)).child("✓"))
}

impl AppView {
    // ── Opening / closing ───────────────────────────────────────────────────

    /// Workspace menu → Settings…: switch to that workspace, then open its settings.
    pub(crate) fn open_workspace_settings(&mut self, ws_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_workspace(ws_id.clone(), window, cx);
        let Some(ws) = self.state.workspaces.iter().find(|w| w.id == ws_id) else {
            return;
        };
        let draft = Draft::of(ws);
        let mut subs = Vec::new();
        let targets = draft.targets.into_iter().map(|t| make_row(t, &mut subs, window, cx)).collect();
        self.sync_settings = Some(SyncSettingsUi {
            ws_id,
            enabled: draft.enabled,
            use_git_ignore: draft.use_git_ignore,
            targets,
            conn_menu: None,
            saved: false,
            notice: None,
            run_started: None,
            _subs: subs,
        });
        cx.notify();
    }

    fn close_workspace_settings(&mut self, cx: &mut Context<Self>) {
        if self.sync_settings.take().is_some() {
            self.focus_pending_now();
            cx.notify();
        }
    }

    // ── Actions ─────────────────────────────────────────────────────────────

    fn sync_settings_dirty(&self, cx: &App) -> bool {
        let Some(ui) = &self.sync_settings else {
            return false;
        };
        let live = self.state.workspaces.iter().find(|w| w.id == ui.ws_id).and_then(|w| w.extra.get("sync"));
        ui.draft(cx).dirty_against(live)
    }

    fn save_sync_settings(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &self.sync_settings else {
            return;
        };
        let draft = ui.draft(cx);
        let ws_id = ui.ws_id.clone();
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) else {
            return;
        };
        draft.merged_into(SyncConfig::of(ws)).store(ws);
        ws.updated_at = now_ms();
        if let Some(ui) = self.sync_settings.as_mut() {
            ui.saved = true;
        }
        self.changed(cx);
        // Start, restart (changed targets / root) or stop this workspace's watcher now.
        self.reconcile_sync_watchers(cx);
    }

    fn sync_now_from_settings(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &self.sync_settings else {
            return;
        };
        let ws_id = ui.ws_id.clone();
        if sftp_sync::is_running(&ws_id) {
            sftp_sync::cancel(&ws_id);
            return;
        }
        let notice = if self.sync_settings_dirty(cx) {
            Some("Save changes first")
        } else {
            let ws = self.state.workspaces.iter().find(|w| w.id == ws_id);
            if ws.and_then(|w| w.working_directory.as_deref()).is_none_or(str::is_empty) {
                Some("Set Working Directory first")
            } else if !ws.and_then(SyncConfig::of).is_some_and(|s| s.configured()) {
                Some("Enable sync and at least one target first")
            } else {
                None
            }
        };
        let Some(ui) = self.sync_settings.as_mut() else {
            return;
        };
        ui.notice = notice.map(Into::into);
        if notice.is_none() {
            ui.run_started = Some(now_ms());
            self.sync_workspace_now(&ws_id, cx);
        }
        cx.notify();
    }

    fn add_sync_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self.sync_settings.as_ref().map(|ui| ui.targets.len() + 1) else {
            return;
        };
        let mut subs = Vec::new();
        let row = make_row(new_target(n, now_ms()), &mut subs, window, cx);
        if let Some(ui) = self.sync_settings.as_mut() {
            ui.targets.push(row);
            ui._subs.extend(subs);
            ui.saved = false;
        }
        cx.notify();
    }

    fn edit_sync_target(&mut self, id: &str, cx: &mut Context<Self>, f: impl FnOnce(&mut SyncTarget)) {
        if let Some(ui) = self.sync_settings.as_mut()
            && let Some(row) = ui.targets.iter_mut().find(|r| r.base.id == id)
        {
            f(&mut row.base);
            ui.saved = false;
            cx.notify();
        }
    }

    fn clear_sync_logs(&mut self, cx: &mut Context<Self>) {
        let Some(ws_id) = self.sync_settings.as_ref().map(|ui| ui.ws_id.clone()) else {
            return;
        };
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) else {
            return;
        };
        // Electron's updateWorkspaceSync starts from its default config when there is none.
        let mut sync = SyncConfig::of(ws).unwrap_or_else(|| Draft::from_value(None).merged_into(None));
        sync.logs = Some(Vec::new());
        sync.store(ws);
        ws.updated_at = now_ms();
        self.changed(cx);
    }

    // ── Status bar ──────────────────────────────────────────────────────────

    /// The status bar's sync segment (hidden while sync is off); a click opens the settings.
    pub(crate) fn render_sync_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ws = self.state.active()?;
        let sync = SyncConfig::of(ws);
        let (dot, label) = match badge(sync.as_ref(), sftp_sync::is_running(&ws.id)) {
            Badge::Off => return None,
            Badge::Ready => (hex(0x888888), "Sync ready".to_owned()),
            Badge::Synced(at) => (ACCENT_GREEN, format!("Sync {}", clock(at, false))),
            Badge::Error => (ACCENT_RED, "Sync error".to_owned()),
            Badge::Running => (ACCENT_YELLOW, "Syncing…".to_owned()),
        };
        let id = ws.id.clone();
        Some(
            div()
                .id("status-sync")
                .flex_none()
                .h_full()
                .px(px(6.))
                .flex()
                .items_center()
                .gap(px(5.))
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("SFTP sync — open settings").build(window, cx))
                .child(div().size(px(6.)).flex_none().rounded_full().bg(dot))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.open_workspace_settings(id.clone(), window, cx)))
                .into_any_element(),
        )
    }

    // ── Modal ───────────────────────────────────────────────────────────────

    pub(crate) fn render_sync_settings(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ui = self.sync_settings.as_ref()?;
        let ws = self.state.workspaces.iter().find(|w| w.id == ui.ws_id)?;
        let live = SyncConfig::of(ws);
        let running = sftp_sync::is_running(&ws.id);
        let dirty = self.sync_settings_dirty(cx);
        let status = ui.notice.clone().or_else(|| run_status(running, ui.run_started, live.as_ref()));
        let conns: Vec<(String, String)> = self
            .connections
            .lock()
            .unwrap()
            .connections
            .iter()
            .map(|c| (c.id.clone(), format!("{} ({}@{}:{})", c.label, c.username, c.host, c.port)))
            .collect();
        let live_targets: HashMap<&str, &SyncTarget> = live.as_ref().map(|s| s.targets.iter().map(|t| (t.id.as_str(), t)).collect()).unwrap_or_default();

        let header_buttons = div()
            .flex()
            .gap(px(8.))
            .child(if running {
                ghost("sync-cancel", "Cancel", ACCENT_RED).on_click(cx.listener(|this, _, _, cx| this.sync_now_from_settings(cx)))
            } else {
                ghost("sync-now", "Sync now", TEXT_PRIMARY).on_click(cx.listener(|this, _, _, cx| this.sync_now_from_settings(cx)))
            })
            .child(ghost("sync-add", "+ Add target", TEXT_PRIMARY).on_click(cx.listener(|this, _, window, cx| this.add_sync_target(window, cx))));

        let targets: Vec<AnyElement> = if ui.targets.is_empty() {
            vec![div().text_size(px(11.)).text_color(TEXT_MUTED).child("No targets configured.").into_any_element()]
        } else {
            ui.targets
                .iter()
                .enumerate()
                .map(|(ix, row)| {
                    let t = &row.base;
                    let (en_id, rm_id, menu_id) = (t.id.clone(), t.id.clone(), t.id.clone());
                    let enabled = t.enabled;
                    let menu_open = ui.conn_menu.as_deref() == Some(t.id.as_str());
                    let chosen = t.connection_id.as_deref().filter(|c| !c.is_empty());
                    let chosen_label = match chosen {
                        None => "Select saved connection".to_owned(),
                        Some(c) => conns.iter().find(|(id, _)| id == c).map(|(_, l)| l.clone()).unwrap_or_else(|| format!("Unknown connection ({c})")),
                    };
                    let menu = menu_open.then(|| {
                        let options = std::iter::once((String::new(), "Select saved connection".to_owned())).chain(conns.iter().cloned());
                        div()
                            .id(SharedString::from(format!("sync-conn-menu-{ix}")))
                            .mt(px(4.))
                            .min_w(px(320.))
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .py(px(4.))
                            .flex()
                            .flex_col()
                            .bg(bg_surface())
                            .border_1()
                            .border_color(border())
                            .rounded(px(9.))
                            .shadow(vec![BoxShadow {
                                color: c(hexa(0, 0.45)),
                                offset: point(px(0.), px(8.)),
                                blur_radius: px(24.),
                                spread_radius: px(0.),
                                inset: false,
                            }])
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                if let Some(ui) = this.sync_settings.as_mut() {
                                    ui.conn_menu = None;
                                }
                                cx.notify();
                            }))
                            .children(options.enumerate().map(|(oi, (id, label))| {
                                let selected = chosen.unwrap_or("") == id;
                                let tid = t.id.clone();
                                div()
                                    .id(SharedString::from(format!("sync-conn-{ix}-{oi}")))
                                    .mx(px(4.))
                                    .px(px(8.))
                                    .py(px(6.))
                                    .rounded(px(6.))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .map(|d| {
                                        if selected {
                                            d.bg(hexa(0x228df2, 0.14)).text_color(ACCENT)
                                        } else {
                                            d.text_color(TEXT_PRIMARY).hover(|d| d.bg(bg_hover()))
                                        }
                                    })
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(ui) = this.sync_settings.as_mut() {
                                            ui.conn_menu = None;
                                        }
                                        let id = id.clone();
                                        this.edit_sync_target(&tid, cx, |t| t.connection_id = (!id.is_empty()).then_some(id));
                                    }))
                            }))
                    });
                    let last = live_targets.get(t.id.as_str()).copied().unwrap_or(t);
                    let failed = last.last_sync_status.as_deref() == Some("error");
                    let last_line = match last.last_sync_at {
                        Some(at) if failed => format!("Last sync: {} (error: {})", clock(at, true), last.last_sync_error.as_deref().unwrap_or("unknown")),
                        Some(at) => format!("Last sync: {} (ok)", clock(at, true)),
                        None => "No sync events yet".to_owned(),
                    };
                    div()
                        .p(px(12.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(border())
                        .bg(bg_primary())
                        .child(
                            div()
                                .mb(px(10.))
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.))
                                .child(div().w(px(200.)).font_weight(FontWeight::SEMIBOLD).child(field(&row.name, window, cx)))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(10.))
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("sync-en-{ix}")))
                                                .flex()
                                                .items_center()
                                                .gap(px(6.))
                                                .text_size(px(11.))
                                                .text_color(TEXT_MUTED)
                                                .cursor_pointer()
                                                .child(checkbox(enabled))
                                                .child("Enabled")
                                                .on_click(cx.listener(move |this, _, _, cx| this.edit_sync_target(&en_id, cx, |t| t.enabled = !enabled))),
                                        )
                                        .child(
                                            ghost(SharedString::from(format!("sync-rm-{ix}")), "Remove", ACCENT_RED)
                                                .px(px(9.))
                                                .py(px(5.))
                                                .text_size(px(11.))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if let Some(ui) = this.sync_settings.as_mut() {
                                                        ui.targets.retain(|r| r.base.id != rm_id);
                                                        ui.saved = false;
                                                    }
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .mb(px(8.))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .id(SharedString::from(format!("sync-conn-select-{ix}")))
                                        .px(px(11.))
                                        .py(px(7.))
                                        .rounded(px(9.))
                                        .bg(bg_primary())
                                        .border_1()
                                        .border_color(if menu_open { ACCENT } else { border() })
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(8.))
                                        .text_size(px(12.))
                                        .text_color(if chosen.is_some() { TEXT_PRIMARY } else { TEXT_MUTED })
                                        .cursor_pointer()
                                        .child(div().min_w_0().truncate().child(chosen_label))
                                        .child(div().text_color(TEXT_MUTED).text_size(px(10.)).child("▾"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(ui) = this.sync_settings.as_mut() {
                                                ui.conn_menu = if ui.conn_menu.as_deref() == Some(menu_id.as_str()) { None } else { Some(menu_id.clone()) };
                                            }
                                            cx.notify();
                                        })),
                                )
                                .when_some(menu, |d, menu| d.child(deferred(anchored().child(menu)).with_priority(3))),
                        )
                        .child(field(&row.path, window, cx))
                        .child(div().mt(px(8.)).text_size(px(11.)).text_color(if failed { ACCENT_RED } else { TEXT_MUTED }).child(last_line))
                        .into_any_element()
                })
                .collect()
        };

        let sync_card = card("SFTP auto sync", Some("Upload files on save to one or more remote SFTP targets."))
            .child(div().mt(px(12.)).flex().justify_end().child(header_buttons))
            .child(div().mt(px(14.)).child(toggle(
                "ws-sync-enabled",
                "Enable SFTP sync on save",
                None,
                ui.enabled,
                false,
                |this, v, cx| {
                    if let Some(ui) = this.sync_settings.as_mut() {
                        ui.enabled = v;
                        ui.saved = false;
                    }
                    cx.notify();
                },
                cx,
            )))
            .child(div().mt(px(14.)).child(toggle(
                "ws-sync-gitignore",
                "Use .gitignore to exclude files",
                Some("Skip anything matched by the project's .gitignore when syncing."),
                ui.use_git_ignore,
                false,
                |this, v, cx| {
                    if let Some(ui) = this.sync_settings.as_mut() {
                        ui.use_git_ignore = v;
                        ui.saved = false;
                    }
                    cx.notify();
                },
                cx,
            )))
            .when_some(status, |d, s| d.child(div().mt(px(14.)).mb(px(6.)).text_size(px(11.)).text_color(TEXT_MUTED).child(s)))
            .child(div().mt(px(14.)).flex().flex_col().gap(px(10.)).children(targets));

        let logs: Vec<Value> = live.as_ref().and_then(|s| s.logs.clone()).unwrap_or_default();
        let log_rows: Vec<AnyElement> = if logs.is_empty() {
            vec![div().p(px(12.)).text_size(px(11.)).text_color(TEXT_MUTED).child("No sync logs yet.").into_any_element()]
        } else {
            let skip = logs.len().saturating_sub(120);
            logs.iter()
                .skip(skip)
                .rev()
                .map(|log| {
                    let ok = log["ok"].as_bool().unwrap_or(false);
                    let s = |k: &str| log[k].as_str().unwrap_or_default().to_owned();
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .border_b_1()
                        .border_color(border())
                        .text_size(px(11.))
                        .text_color(if ok { TEXT_PRIMARY } else { ACCENT_RED })
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .gap(px(8.))
                                .child(s("action"))
                                .child(div().text_color(TEXT_MUTED).child(log["at"].as_i64().map(|at| clock(at, true)).unwrap_or_default())),
                        )
                        .child(div().text_color(TEXT_MUTED).truncate().child(s("filePath")))
                        .child(div().child(s("message")))
                        .into_any_element()
                })
                .collect()
        };
        let logs_card = card("Sync logs", None)
            .relative()
            .child(
                div().absolute().top(px(12.)).right(px(14.)).child(
                    ghost("sync-clear-logs", "Clear logs", TEXT_MUTED)
                        .px(px(10.))
                        .py(px(6.))
                        .text_size(px(11.))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_sync_logs(cx))),
                ),
            )
            .child(
                div()
                    .id("sync-logs")
                    .mt(px(14.))
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .rounded(px(9.))
                    .border_1()
                    .border_color(border())
                    .bg(bg_primary())
                    .children(log_rows),
            );

        let initial = ws.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(20.))
            .py(px(16.))
            .border_b_1()
            .border_color(border())
            .child(
                div()
                    .size(px(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(10.))
                    .bg(hexa(0x228df2, 0.16))
                    .text_color(ACCENT)
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .child(initial),
            )
            .child(
                div()
                    .min_w_0()
                    .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("Workspace Settings"))
                    .child(div().mt(px(1.)).text_size(px(12.)).text_color(TEXT_MUTED).truncate().child(ws.name.clone())),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("ws-settings-close")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .text_size(px(16.))
                    .text_color(TEXT_MUTED)
                    .cursor_pointer()
                    .hover(|d| d.bg(bg_hover()).text_color(TEXT_PRIMARY))
                    .child("✕")
                    .on_click(cx.listener(|this, _, _, cx| this.close_workspace_settings(cx))),
            );

        let footer = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(20.))
            .py(px(14.))
            .border_t_1()
            .border_color(border())
            .child(
                div()
                    .id("ws-settings-save")
                    .px(px(16.))
                    .py(px(8.))
                    .rounded(px(9.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .map(|d| {
                        if dirty {
                            d.bg(ACCENT)
                                .text_color(hex(0xffffff))
                                .cursor_pointer()
                                .hover(|d| d.opacity(0.85))
                                .on_click(cx.listener(|this, _, _, cx| this.save_sync_settings(cx)))
                        } else {
                            d.bg(bg_hover()).text_color(TEXT_MUTED)
                        }
                    })
                    .child("Save"),
            )
            .map(|d| {
                if dirty {
                    d.child(div().text_size(px(12.)).text_color(TEXT_MUTED).child("Unsaved changes"))
                } else if ui.saved {
                    d.child(div().text_size(px(12.)).text_color(ACCENT_GREEN).child("✓ Saved"))
                } else {
                    d
                }
            });

        let modal = div()
            .id("ws-settings")
            .w(px(760.))
            .max_w(relative(1.))
            .h(relative(0.82))
            .max_h(px(680.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(bg_surface())
            .border_1()
            .border_color(border())
            .rounded(px(16.))
            .shadow(vec![BoxShadow { color: c(hexa(0, 0.55)), offset: point(px(0.), px(24.)), blur_radius: px(70.), spread_radius: px(0.), inset: false }])
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(div().id("ws-settings-body").flex_1().min_h_0().overflow_y_scroll().bg(bg_primary()).p(px(20.)).child(sync_card).child(logs_card))
            .child(footer);

        Some(
            div()
                .id("ws-settings-overlay")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.))
                .py(px(40.))
                .bg(hexa(0, 0.55))
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| this.close_workspace_settings(cx)))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.close_workspace_settings(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(modal)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's `test` attribute in over the built-in one.
    use super::{Badge, Draft, SyncConfig, SyncTarget, badge, new_target, run_status};
    use serde_json::json;

    fn target(id: &str, name: &str, conn: Option<&str>, path: &str) -> SyncTarget {
        SyncTarget { id: id.into(), name: name.into(), enabled: true, connection_id: conn.map(Into::into), remote_path: path.into(), ..Default::default() }
    }

    #[test]
    fn a_fresh_save_writes_electrons_shape() {
        let mut draft = Draft::from_value(None);
        assert_eq!((draft.enabled, draft.use_git_ignore, draft.targets.len()), (false, true, 0));
        draft.enabled = true;
        draft.targets.push(SyncTarget { id: "target-1-abcdef".into(), ..new_target(1, 1) });
        let out = serde_json::to_value(draft.merged_into(None)).unwrap();
        assert_eq!(
            out,
            json!({
                "enabled": true,
                "useGitIgnore": true,
                "targets": [{ "id": "target-1-abcdef", "name": "Target 1", "enabled": true, "connectionId": null, "remotePath": "" }],
                "fileStates": {},
                "logs": []
            })
        );
    }

    #[test]
    fn saving_keeps_runtime_data_and_unknown_fields() {
        let raw = json!({
            "enabled": true, "useGitIgnore": false, "futureTop": [1, 2],
            "targets": [
                { "id": "t1", "name": "prod", "enabled": true, "connectionId": "c1", "remotePath": "/srv/app",
                  "lastSyncAt": 1700000000000i64, "lastSyncStatus": "error", "lastSyncError": "boom", "futureT": "x" },
                { "id": "t2", "name": "stage", "enabled": false, "connectionId": null, "remotePath": "/srv/stage" }
            ],
            "fileStates": { "a.txt": { "status": "synced", "updatedAt": 5 } },
            "logs": [{ "id": "1-abc", "at": 1, "action": "sync-now", "filePath": "/ws", "ok": true, "message": "m" }]
        });
        let mut draft = Draft::from_value(Some(&raw));
        assert!(!draft.use_git_ignore && draft.enabled);
        assert!(!draft.dirty_against(Some(&raw)));
        // Rename t1, drop t2, add t3.
        draft.targets[0].name = "production".into();
        draft.targets.remove(1);
        draft.targets.push(target("t3", "Target 2", Some("c2"), "/x"));
        assert!(draft.dirty_against(Some(&raw)));
        let live: SyncConfig = serde_json::from_value(raw.clone()).unwrap();
        let out = serde_json::to_value(draft.merged_into(Some(live))).unwrap();
        // Everything but the edited target list is kept as it was.
        let mut t1 = raw["targets"][0].clone();
        t1["name"] = json!("production");
        let t3 = json!({ "id": "t3", "name": "Target 2", "enabled": true, "connectionId": "c2", "remotePath": "/x" });
        let mut expected = raw.clone();
        expected["targets"] = json!([t1, t3]);
        assert_eq!(out, expected);
        // What Lite stored reads back as the same draft (and Electron's TS shape).
        let back = Draft::from_value(Some(&out));
        assert_eq!(back.targets.len(), 2);
        assert!(!back.dirty_against(Some(&out)));
    }

    #[test]
    fn runtime_status_is_not_a_change() {
        let raw = json!({ "enabled": true, "useGitIgnore": true,
            "targets": [{ "id": "t1", "name": "a", "enabled": true, "connectionId": "c", "remotePath": "/r" }] });
        let draft = Draft::from_value(Some(&raw));
        let mut later = raw.clone();
        later["targets"][0]["lastSyncAt"] = json!(99);
        later["targets"][0]["lastSyncStatus"] = json!("success");
        later["logs"] = json!([{ "id": "x" }]);
        assert!(!draft.dirty_against(Some(&later)));
        let mut d = draft.clone();
        d.targets[0].connection_id = None;
        assert!(d.dirty_against(Some(&raw)));
        let mut d = draft.clone();
        d.use_git_ignore = false;
        assert!(d.dirty_against(Some(&raw)));
        // No config yet: the default draft is clean, enabling it is a change.
        assert!(!Draft::from_value(None).dirty_against(None));
    }

    #[test]
    fn a_save_picks_up_status_recorded_while_editing() {
        let raw = json!({ "enabled": true, "useGitIgnore": true,
            "targets": [{ "id": "t1", "name": "a", "enabled": true, "connectionId": "c", "remotePath": "/r" }] });
        let draft = Draft::from_value(Some(&raw));
        let mut live: SyncConfig = serde_json::from_value(raw).unwrap();
        live.record_sync_now("/ws", &json!({ "targetResults": [{ "targetId": "t1", "ok": true }] }), 77);
        let out = draft.merged_into(Some(live));
        assert_eq!(out.targets[0].last_sync_at, Some(77));
        assert_eq!(out.logs.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn new_targets_look_like_electrons() {
        let t = new_target(3, 1700000000123);
        assert_eq!(t.name, "Target 3");
        let rest = t.id.strip_prefix("target-1700000000123-").unwrap();
        assert!(rest.len() == 6 && rest.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert!(t.enabled && t.connection_id.is_none() && t.remote_path.is_empty());
    }

    #[test]
    fn status_badge_follows_derive_sync() {
        let cfg = |enabled: bool, targets: Vec<SyncTarget>| SyncConfig { enabled, targets, ..Default::default() };
        assert_eq!(badge(None, false), Badge::Off);
        assert_eq!(badge(Some(&cfg(false, vec![target("a", "a", Some("c"), "/")])), false), Badge::Off);
        let mut off = target("a", "a", Some("c"), "/");
        off.enabled = false;
        assert_eq!(badge(Some(&cfg(true, vec![off.clone()])), false), Badge::Off);
        assert_eq!(badge(Some(&cfg(true, vec![target("a", "a", None, "")])), false), Badge::Ready);
        let mut a = target("a", "a", Some("c"), "/");
        a.last_sync_at = Some(10);
        let mut b = target("b", "b", Some("c"), "/");
        b.last_sync_at = Some(20);
        assert_eq!(badge(Some(&cfg(true, vec![a.clone(), b.clone(), off])), false), Badge::Synced(20));
        b.last_sync_status = Some("error".into());
        assert_eq!(badge(Some(&cfg(true, vec![a.clone(), b])), false), Badge::Error);
        assert_eq!(badge(Some(&cfg(true, vec![a])), true), Badge::Running);
    }

    #[test]
    fn run_status_reports_the_run_this_modal_started() {
        let mut cfg = SyncConfig::default();
        assert_eq!(run_status(true, None, None).as_deref(), Some("Running sync..."));
        assert_eq!(run_status(false, None, Some(&cfg)), None);
        cfg.logs = Some(vec![json!({ "action": "sync-now", "at": 5, "message": "old" })]);
        assert_eq!(run_status(false, Some(10), Some(&cfg)), None);
        cfg.logs.as_mut().unwrap().push(json!({ "action": "sync-now", "at": 12, "message": "scanned 1, uploaded 1, skipped 0" }));
        assert_eq!(run_status(false, Some(10), Some(&cfg)).as_deref(), Some("Done: scanned 1, uploaded 1, skipped 0"));
        cfg.logs.as_mut().unwrap().push(json!({ "action": "sync-now", "at": 13, "message": "scanned 1, uploaded 0, skipped 0 (cancelled)" }));
        assert_eq!(run_status(false, Some(10), Some(&cfg)).as_deref(), Some("Cancelled: scanned 1, uploaded 0, skipped 0"));
    }
}
