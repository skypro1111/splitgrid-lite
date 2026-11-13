//! Settings (SettingsModal in the Electron app): General, and the SSH
//! connection manager (SSHConnectionManager) — groups, test, edit, duplicate, delete.
//!
//! General holds every Electron setting this app acts on, stored in the shared
//! `app-settings.json` as patches (keys this app doesn't know are kept). Left
//! out because Lite has no such feature (yet): the background colour, the
//! terminal renderer and Windows shell, browser-extension control, Workspace
//! history, the Claude Code pane, and the Fast chat and Account tabs.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::AppView;
use crate::app_settings::{self as prefs, Hotkey};
use crate::model::connections::{AuthMethod, SavedConnection};
use crate::model::workspace::new_id;
use crate::theme::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Ssh,
}

/// What a group header's inline field is doing.
#[derive(Clone, PartialEq)]
enum GroupEdit {
    New,
    Rename(String),
}

#[derive(Default)]
pub struct ManagerState {
    /// Last Test result per connection: Ok, or the error.
    tested: HashMap<String, Result<(), String>>,
    testing: HashSet<String>,
    confirm_delete: Option<String>,
    confirm_ungroup: Option<String>,
    group_edit: Option<GroupEdit>,
    group_input: Option<Entity<InputState>>,
    _group_sub: Option<Subscription>,
    error: Option<String>,

    // General's controls with state of their own.
    volume: Option<Entity<SliderState>>,
    _volume_sub: Option<Subscription>,
    sound_menu: bool,
    /// The focus-mode shortcut recorder is waiting for a chord.
    pub(crate) recording: bool,
    hotkey_hint: Option<&'static str>,
    _recorder: Option<Subscription>,
}

// ── General ──────────────────────────────────────────────────────────────────

/// A settings card (settings-ui's Card): title, description, then its controls.
fn card(title: &'static str, desc: Option<&'static str>) -> Div {
    div()
        .mb(px(14.))
        .p(px(18.))
        .rounded(px(12.))
        .border_1()
        .border_color(BORDER)
        .bg(BG_SURFACE)
        .child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).text_color(TEXT_PRIMARY).child(title))
        .when_some(desc, |d, desc| d.child(div().mt(px(5.)).text_size(px(12.)).line_height(px(18.)).text_color(TEXT_MUTED).child(desc)))
}

/// A labelled switch (settings-ui's Toggle): 40×23 track, 18px knob.
fn toggle(
    key: &'static str,
    label: &'static str,
    desc: Option<&'static str>,
    checked: bool,
    disabled: bool,
    on_change: impl Fn(&mut AppView, bool, &mut Context<AppView>) + 'static,
    cx: &mut Context<AppView>,
) -> Div {
    let switch = div()
        .id(SharedString::from(format!("toggle-{key}")))
        .relative()
        .w(px(40.))
        .h(px(23.))
        .mt(px(1.))
        .flex_none()
        .rounded(px(12.))
        .bg(if checked { ACCENT } else { BORDER })
        .when(!disabled, |d| d.cursor_pointer())
        .child(
            div()
                .absolute()
                .top(px(2.5))
                .left(px(if checked { 19.5 } else { 2.5 }))
                .size(px(18.))
                .rounded_full()
                .bg(hex(0xffffff))
                .shadow(vec![BoxShadow { color: c(hexa(0, 0.35)), offset: point(px(0.), px(1.)), blur_radius: px(3.), spread_radius: px(0.), inset: false }]),
        )
        .when(!disabled, |d| d.on_click(cx.listener(move |this, _, _, cx| on_change(this, !checked, cx))));
    div()
        .flex()
        .items_start()
        .gap(px(12.))
        .when(disabled, |d| d.opacity(0.5))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_PRIMARY).child(label))
                .when_some(desc, |d, desc| d.child(div().mt(px(4.)).text_size(px(11.)).line_height(px(17.)).text_color(TEXT_MUTED).child(desc))),
        )
        .child(switch)
}

/// Save some switches of the shared app settings.
fn set(this: &mut AppView, patch: &[(&str, bool)], cx: &mut Context<AppView>) {
    let patch: Vec<(&str, serde_json::Value)> = patch.iter().map(|(k, v)| (*k, (*v).into())).collect();
    set_values(this, &patch, cx);
}

/// Save some keys of the shared app settings, then act on them at once (as
/// the Electron app's app-settings:save does).
fn set_values(this: &mut AppView, patch: &[(&str, serde_json::Value)], cx: &mut Context<AppView>) {
    if let Err(e) = this.store.patch_app_settings(patch) {
        log::error!("saving settings: {e:#}");
    }
    let changed = |k: &str| patch.iter().any(|(p, _)| *p == k);
    // Agent integrations install or remove the hooks and skills.
    if crate::agent::install::SWITCH_KEYS.iter().any(|k| changed(k)) {
        let installer = crate::agent::install::Installer::new(this.store.dir());
        std::thread::spawn(move || installer.apply());
    }
    if let Some((_, v)) = patch.iter().find(|(k, _)| *k == "terminalMouseScroll") {
        let home = std::env::home_dir().unwrap_or_default();
        prefs::apply_terminal_mouse_config(&home, v.as_bool().unwrap_or(false));
    }
    if changed("focusModeHotkey") {
        prefs::apply_focus_hotkey(&this.store.app_settings(), cx);
    }
    cx.notify();
}

/// A form row (settings-ui's Row): a 130px label, then the controls.
fn row(label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .mb(px(12.))
        .child(div().w(px(130.)).flex_none().text_size(px(12.)).text_color(TEXT_PRIMARY).child(label))
}

/// settings-ui's Hint.
fn hint(text: &'static str) -> Div {
    div().mt(px(-4.)).mb(px(10.)).text_size(px(11.)).line_height(px(16.5)).text_color(TEXT_MUTED).child(text)
}

/// A bordered control box (settings-ui's controlStyle).
fn control(id: &'static str, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(11.))
        .py(px(8.))
        .rounded(px(9.))
        .bg(BG_PRIMARY)
        .border_1()
        .border_color(if active { ACCENT } else { BORDER })
        .when(active, |d| d.shadow(vec![BoxShadow { color: c(hexa(0x228df2, 0.16)), offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(3.), inset: false }]))
        .text_size(px(12.))
        .cursor_pointer()
}

fn ghost_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(12.))
        .py(px(8.))
        .rounded(px(9.))
        .border_1()
        .border_color(BORDER)
        .bg(BG_PRIMARY)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(TEXT_PRIMARY)
        .cursor_pointer()
        .hover(|d| d.bg(BG_HOVER))
        .child(label)
}

impl AppView {
    /// The General tab: the settings this app acts on, stored with the Electron app's.
    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let all = self.store.app_settings();
        let on = |k: &str| all.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let mute = on("muteAll");
        let system = all.get("systemNotifications").and_then(serde_json::Value::as_bool) != Some(false);
        let waiting = all.get("notifyOnWaiting").and_then(serde_json::Value::as_bool) != Some(false);
        let (integrations, terminal, long_running) = (on("agentIntegrations"), on("agentTerminalControl"), on("agentLongRunningInPanes"));
        let (sql, sql_write, sftp, sftp_write) = (on("agentSqlControl"), on("agentSqlWrite"), on("agentSftpControl"), on("agentSftpWrite"));
        let nested = |d: Div| div().ml(px(22.)).mt(px(14.)).child(d);
        let sound = prefs::default_sound(&all);
        let volume = prefs::default_volume(&all);
        let mouse_scroll = on("terminalMouseScroll");
        let hotkey = Hotkey::focus_mode(&all);
        let recording = self.manager.recording;

        let notifications = card(
            "Notifications",
            Some("Play a sound when an agent finishes (Done) or stops to wait for you in a background workspace or while the window is unfocused."),
        )
        .child(div().mt(px(14.)).child(toggle("mute", "Mute all notifications", None, mute, false, |t, v, cx| set(t, &[("muteAll", v)], cx), cx)))
        .child(
            div()
                .mt(px(14.))
                .flex()
                .flex_col()
                .gap(px(14.))
                .child(toggle(
                    "system",
                    "System notifications",
                    Some("While SplitGrid is in the background, also show an OS notification. Clicking it opens that workspace and terminal. The OS may ask for permission the first time, and can turn them off in its notification settings."),
                    system,
                    mute,
                    |t, v, cx| set(t, &[("systemNotifications", v)], cx),
                    cx,
                ))
                .child(toggle(
                    "waiting",
                    "Notify when an agent is waiting",
                    Some("Also notify when an agent pauses for a permission decision or a question, not only when it is Done."),
                    waiting,
                    mute,
                    |t, v, cx| set(t, &[("notifyOnWaiting", v)], cx),
                    cx,
                )),
        )
        .child(
            div()
                .mt(px(14.))
                .when(mute, |d| d.opacity(0.5))
                .child(
                    row("Default sound")
                        .child(self.sound_picker(&sound, cx))
                        .child(ghost_button("sound-test", "▶ Test").on_click(move |_, _, _| prefs::play(&sound, volume))),
                )
                .child(
                    row("Default volume")
                        .child(div().flex_1().max_w(px(220.)).when_some(self.manager.volume.as_ref(), |d, v| d.child(Slider::new(v))))
                        .child(div().w(px(38.)).text_right().text_size(px(12.)).text_color(TEXT_MUTED).child(format!("{}%", (volume * 100.).round()))),
                ),
        );

        let terminal_card = card("Terminal", None).child(div().mt(px(14.)).child(toggle(
            "mouse-scroll",
            "Mouse wheel scroll in tmux / screen",
            Some("Enables mouse mode in your local ~/.tmux.conf and ~/.screenrc so the wheel scrolls their scrollback. Trade-off: text selection then goes through tmux/screen — hold Shift to select natively. Only affects this machine (not remote tmux over SSH); reopen tmux or run “tmux source-file ~/.tmux.conf” to apply to running sessions."),
            mouse_scroll,
            false,
            |t, v, cx| set(t, &[("terminalMouseScroll", v)], cx),
            cx,
        )));

        let shortcuts = card("Shortcuts", None)
            .child(
                div().mt(px(14.)).child(
                    row("Focus mode").child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                control("hotkey-focus", recording)
                                    .min_w(px(140.))
                                    .font_family("Menlo")
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(if recording { TEXT_MUTED } else { TEXT_PRIMARY })
                                    .child(if recording { "Press keys… (Esc)".to_owned() } else { hotkey.label() })
                                    .on_click(cx.listener(|this, _, _, cx| if this.manager.recording { this.stop_recording(cx) } else { this.start_recording(cx) })),
                            )
                            .when(!recording && hotkey != Hotkey::default_focus_mode(), |d| {
                                d.child(
                                    ghost_button("hotkey-reset", "Reset")
                                        .text_color(TEXT_MUTED)
                                        .on_click(cx.listener(|this, _, _, cx| set_values(this, &[("focusModeHotkey", serde_json::Value::Null)], cx))),
                                )
                            })
                            .when_some(self.manager.hotkey_hint, |d, h| d.child(div().text_size(px(11.)).text_color(ACCENT_RED).child(h))),
                    ),
                ),
            )
            .child(hint("Toggle a single container to full-screen focus (and back). Works over terminals, editors and browser panes."));

        let agents = card("Agent integrations", None)
            .child(div().mt(px(14.)).child(toggle(
                "integrations",
                "Enable agent integrations",
                Some("When on, splitgrid installs the sg-browser skill and Claude/Codex lifecycle hooks into your global ~/.claude & ~/.codex (and inside WSL distros), and injects SPLITGRID_* env into new terminals so agents can report activity and drive the browser pane. Off by default — nothing is installed or injected until you turn this on. Turning it off uninstalls the host hooks & skill; env changes apply to newly opened terminals."),
                integrations,
                false,
                |t, v, cx| set(t, &[("agentIntegrations", v)], cx),
                cx,
            )))
            .child(nested(
                div().child(toggle(
                    "terminal",
                    "Terminal control",
                    Some("Also install the sg-terminal skill and inject SPLITGRID_TERMINAL_* env, letting an agent list, read and drive the other terminals in its workspace (run commands, send keys, Ctrl-C). Scoped to the agent's own workspace. Writing into a sibling shell runs arbitrary commands, so it's a separate opt-in — off by default."),
                    integrations && terminal,
                    !integrations,
                    |t, v, cx| set(t, &[("agentTerminalControl", v)], cx),
                    cx,
                ))
                .child(nested(toggle(
                    "long-running",
                    "Long-running processes in panes",
                    Some("Off (default): simple background jobs such as a dev server or a watcher stay background jobs of the agent, and a pane opens only when you ask to see one. On: agents start every dev server, watcher and log tail in a small tray pane at the bottom of their zone, where you can watch and stop it. SSH always opens in its own pane. Applies to agents started after the change."),
                    long_running,
                    !integrations || !terminal,
                    |t, v, cx| set(t, &[("agentLongRunningInPanes", v)], cx),
                    cx,
                ))),
            ))
            .child(nested(
                div().child(toggle(
                    "sql",
                    "Allow agents to query SQL (read-only)",
                    Some("Inject SPLITGRID_SQL_* env, letting an agent run read-only queries, inspect schema and export results against the SQL component in its workspace. Off by default."),
                    integrations && sql,
                    !integrations,
                    |t, v, cx| if v { set(t, &[("agentSqlControl", true)], cx) } else { set(t, &[("agentSqlControl", false), ("agentSqlWrite", false)], cx) },
                    cx,
                ))
                .child(nested(toggle(
                    "sql-write",
                    "Allow agents to modify data / run DDL (write)",
                    Some("Also permit writes, data modification and schema changes (INSERT/UPDATE/DELETE, DDL). Off by default — leave off to keep agents read-only."),
                    integrations && sql && sql_write,
                    !integrations || !sql,
                    |t, v, cx| set(t, &[("agentSqlWrite", v)], cx),
                    cx,
                ))),
            ))
            .child(nested(
                div().child(toggle(
                    "sftp",
                    "Allow agents to transfer files over SFTP (read-only)",
                    Some("Install the sg-sftp skill and inject SPLITGRID_SFTP_* env, letting an agent list and download files from its workspace's remote hosts — its sync targets and the hosts of its SSH panes. Local paths stay inside the workspace directory. Off by default."),
                    integrations && sftp,
                    !integrations,
                    |t, v, cx| if v { set(t, &[("agentSftpControl", true)], cx) } else { set(t, &[("agentSftpControl", false), ("agentSftpWrite", false)], cx) },
                    cx,
                ))
                .child(nested(toggle(
                    "sftp-write",
                    "Allow agents to upload / change remote files (write)",
                    Some("Also permit uploads, workspace sync and remote mkdir/rename/delete. Without this an agent can only look and download. Off by default."),
                    integrations && sftp && sftp_write,
                    !integrations || !sftp,
                    |t, v, cx| set(t, &[("agentSftpWrite", v)], cx),
                    cx,
                ))),
            ));

        div()
            .id("settings-general")
            .size_full()
            .overflow_y_scroll()
            .bg(BG_PRIMARY)
            .p(px(20.))
            .child(notifications)
            .child(terminal_card)
            .child(shortcuts)
            .child(agents)
            .into_any_element()
    }

    /// The default-sound select (settings-ui's PSelect): the current sound, and its menu below.
    fn sound_picker(&self, current: &str, cx: &mut Context<Self>) -> Div {
        let open = self.manager.sound_menu;
        let options: Vec<(&'static str, &'static str)> = prefs::SOUNDS.iter().copied().chain([(prefs::SILENT_SOUND_ID, "Silent")]).collect();
        let menu = div()
            .id("sound-menu")
            .mt(px(4.))
            .min_w(px(170.))
            .py(px(4.))
            .flex()
            .flex_col()
            .bg(BG_SURFACE)
            .border_1()
            .border_color(BORDER)
            .rounded(px(9.))
            .shadow(vec![BoxShadow { color: c(hexa(0, 0.45)), offset: point(px(0.), px(8.)), blur_radius: px(24.), spread_radius: px(0.), inset: false }])
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.manager.sound_menu = false;
                cx.notify();
            }))
            .children(options.into_iter().map(|(id, label)| {
                let selected = id == current;
                div()
                    .id(SharedString::from(format!("sound-{id}")))
                    .mx(px(4.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .map(|d| if selected { d.bg(hexa(0x228df2, 0.14)).text_color(ACCENT) } else { d.text_color(TEXT_PRIMARY).hover(|d| d.bg(BG_HOVER)) })
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.manager.sound_menu = false;
                        set_values(this, &[("defaultSoundId", id.into())], cx);
                    }))
            }));
        div()
            .flex()
            .flex_col()
            .child(
                control("sound-select", open)
                    .min_w(px(170.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .text_color(TEXT_PRIMARY)
                    .child(prefs::sound_label(current))
                    .child(div().text_color(TEXT_MUTED).text_size(px(10.)).child("▾"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.manager.sound_menu = !this.manager.sound_menu;
                        cx.notify();
                    })),
            )
            .when(open, |d| d.child(deferred(anchored().child(menu)).with_priority(2)))
    }

    /// General's stateful controls, made when the Settings window opens.
    fn ensure_settings_controls(&mut self, cx: &mut Context<Self>) {
        if self.manager.volume.is_some() {
            return;
        }
        let value = prefs::default_volume(&self.store.app_settings());
        let slider = cx.new(|_| SliderState::new().min(0.).max(1.).step(0.05).default_value(value));
        self.manager._volume_sub = Some(cx.subscribe(&slider, |this, _, ev: &SliderEvent, cx| {
            let SliderEvent::Change(v) = ev else { return };
            let v = ((v.start() * 100.).round() / 100.) as f64;
            if (v - prefs::default_volume(&this.store.app_settings()) as f64).abs() > 0.001 {
                set_values(this, &[("defaultVolume", v.into())], cx);
            }
        }));
        self.manager.volume = Some(slider);
    }

    /// Bring the controls with state of their own in line with settings saved elsewhere.
    pub(crate) fn sync_settings_controls(&mut self, all: &prefs::Settings, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slider) = &self.manager.volume {
            let v = prefs::default_volume(all);
            slider.update(cx, |s, cx| {
                if (s.value().start() - v).abs() > 0.001 {
                    s.set_value(v, window, cx);
                }
            });
        }
    }

    /// Record the next chord as the focus-mode shortcut (settings-ui's HotkeyRecorder).
    fn start_recording(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        // Ahead of every key binding, so the chord being recorded doesn't fire.
        let sub = cx.intercept_keystrokes(move |ev, _, cx| {
            let k = ev.keystroke.clone();
            if this.update(cx, |this, cx| this.record_hotkey(&k, cx)).is_ok() {
                cx.stop_propagation();
            }
        });
        self.manager.recording = true;
        self.manager.hotkey_hint = None;
        self.manager._recorder = Some(sub);
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.manager.recording = false;
        // Not dropped from inside its own callback.
        let sub = self.manager._recorder.take();
        cx.defer(move |_| drop(sub));
        cx.notify();
    }

    fn record_hotkey(&mut self, k: &Keystroke, cx: &mut Context<Self>) {
        if !self.manager.recording {
            return;
        }
        let bare = !(k.modifiers.platform || k.modifiers.control || k.modifiers.alt || k.modifiers.shift);
        if k.key == "escape" && bare {
            self.stop_recording(cx);
            return;
        }
        if matches!(k.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
            return;
        }
        match Hotkey::from_keystroke(k) {
            Some(h) => {
                self.stop_recording(cx);
                set_values(self, &[("focusModeHotkey", h.to_value())], cx);
            }
            None => {
                self.manager.hotkey_hint = Some("Add a modifier (⌘ / ⌃ / ⌥).");
                cx.notify();
            }
        }
    }
}

fn small_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(7.))
        .py(px(2.))
        .rounded(px(4.))
        .bg(BG_HOVER)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|d| d.opacity(0.85))
        .child(label.into())
}

fn header_button(id: &'static str, label: &'static str, primary: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(14.))
        .py(px(6.))
        .rounded(px(5.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|d| d.opacity(0.85))
        .map(|d| if primary { d.bg(ACCENT).text_color(BG_PRIMARY) } else { d.bg(BG_HOVER).text_color(TEXT_PRIMARY) })
        .child(label)
}

impl AppView {
    pub(crate) fn open_settings(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings_tab = Some(tab);
        self.ensure_settings_controls(cx);
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_tab = None;
        self.manager = ManagerState::default();
        self.focus_pending_now();
        cx.notify();
    }

    // ── Manager actions ─────────────────────────────────────────────────────

    fn test_connection(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(conn) = self.connections.lock().unwrap().get(&id).cloned() else { return };
        let params = self.ssh_params(&conn);
        let check = self.host_key_checker_for(&conn);
        self.manager.testing.insert(id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::ssh::test(params, check).await;
            let _ = this.update(cx, |this, cx| {
                this.manager.testing.remove(&id);
                this.manager.tested.insert(id, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn duplicate_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut store = self.connections.lock().unwrap();
        let Some(ix) = store.connections.iter().position(|c| c.id == id) else { return };
        let mut copy = store.connections[ix].clone();
        copy.id = new_id("conn");
        copy.label = format!("{} (copy)", copy.label);
        let copy_id = copy.id.clone();
        store.connections.insert(ix + 1, copy);
        let saved = store.save();
        drop(store);
        match saved {
            Ok(()) => self.open_editor(Some(copy_id), None, window, cx),
            Err(_) => self.manager.error = Some("Failed to duplicate".into()),
        }
        cx.notify();
    }

    fn start_group_edit(&mut self, edit: GroupEdit, window: &mut Window, cx: &mut Context<Self>) {
        let (value, placeholder) = match &edit {
            GroupEdit::New => (String::new(), "New group name"),
            GroupEdit::Rename(name) => (name.clone(), "Group name"),
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value));
        input.update(cx, |i, cx| i.focus(window, cx));
        let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let name = input.read(cx).value().trim().to_string();
                this.commit_group_edit(name, cx);
            }
        });
        self.manager.group_edit = Some(edit);
        self.manager.group_input = Some(input);
        self.manager._group_sub = Some(sub);
        cx.notify();
    }

    fn commit_group_edit(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(edit) = self.manager.group_edit.take() else { return };
        self.manager.group_input = None;
        if !name.is_empty() {
            let mut store = self.connections.lock().unwrap();
            match edit {
                GroupEdit::New => {
                    if !store.groups.groups.contains(&name) {
                        store.groups.groups.push(name);
                    }
                }
                GroupEdit::Rename(old) if old != name => {
                    // Renaming onto an existing group merges the two.
                    for c in store.connections.iter_mut().filter(|c| c.group.as_deref() == Some(old.as_str())) {
                        c.group = Some(name.clone());
                    }
                    let groups = &mut store.groups.groups;
                    match (groups.iter().position(|g| *g == old), groups.contains(&name)) {
                        (Some(i), false) => groups[i] = name.clone(),
                        (Some(i), true) => {
                            groups.remove(i);
                        }
                        _ => {}
                    }
                    store.groups.collapsed.retain(|g| *g != old);
                }
                GroupEdit::Rename(_) => {}
            }
            if store.save().is_err() {
                self.manager.error = Some("Failed to update groups".into());
            }
        }
        cx.notify();
    }

    fn ungroup_all(&mut self, group: &str, cx: &mut Context<Self>) {
        let mut store = self.connections.lock().unwrap();
        for c in store.connections.iter_mut().filter(|c| c.group.as_deref() == Some(group)) {
            c.group = None;
        }
        store.groups.groups.retain(|g| g != group);
        store.groups.collapsed.retain(|g| g != group);
        if store.save().is_err() {
            self.manager.error = Some("Failed to update groups".into());
        }
        drop(store);
        self.manager.confirm_ungroup = None;
        cx.notify();
    }

    fn toggle_manager_group(&mut self, group: &str, cx: &mut Context<Self>) {
        let mut store = self.connections.lock().unwrap();
        let collapsed = &mut store.groups.collapsed;
        match collapsed.iter().position(|g| g == group) {
            Some(i) => {
                collapsed.remove(i);
            }
            None => collapsed.push(group.to_owned()),
        }
        let _ = store.save();
        drop(store);
        cx.notify();
    }

    // ── Rendering ───────────────────────────────────────────────────────────

    pub(crate) fn render_settings(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.settings_tab?;
        let tab_button = |key: SettingsTab, id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            let active = tab == key;
            div()
                .id(id)
                .px(px(14.))
                .py(px(7.))
                .rounded(px(9.))
                .border_1()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .map(|d| {
                    if active {
                        d.border_color(hex(0x2f5a82)).bg(hexa(0x228df2, 0.14)).text_color(ACCENT)
                    } else {
                        d.border_color(hexa(0, 0.)).text_color(TEXT_MUTED).hover(|d| d.text_color(TEXT_PRIMARY))
                    }
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_tab = Some(key);
                    this.ensure_settings_controls(cx);
                    cx.notify();
                }))
        };

        let body = match tab {
            SettingsTab::General => self.render_general(cx),
            SettingsTab::Ssh => self.render_manager(window, cx),
        };

        let modal = div()
            .id("settings")
            .w(px(1152.))
            .max_w(relative(1.))
            .h(relative(0.82))
            .max_h(px(1056.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(BG_SURFACE)
            .border_1()
            .border_color(BORDER)
            .rounded(px(16.))
            .shadow(vec![BoxShadow {
                color: c(hexa(0, 0.55)),
                offset: point(px(0.), px(24.)),
                blur_radius: px(70.),
                spread_radius: px(0.),
                inset: false,
            }])
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(20.))
                    .py(px(16.))
                    .border_b_1()
                    .border_color(BORDER)
                    .child(
                        div()
                            .size(px(34.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.))
                            .bg(hexa(0x228df2, 0.16))
                            .child(svg().path(crate::assets::icon("gear")).size(px(17.)).text_color(ACCENT)),
                    )
                    .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("Settings"))
                    .child(
                        div()
                            .ml(px(10.))
                            .flex()
                            .gap(px(4.))
                            .child(tab_button(SettingsTab::General, "tab-general", "General", cx))
                            .child(tab_button(SettingsTab::Ssh, "tab-ssh", "SSH Connections", cx)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("settings-close")
                            .size(px(26.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .text_size(px(16.))
                            .text_color(TEXT_MUTED)
                            .cursor_pointer()
                            .hover(|d| d.bg(BG_HOVER).text_color(TEXT_PRIMARY))
                            .child("✕")
                            .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().bg(if tab == SettingsTab::Ssh { BG_SURFACE } else { BG_PRIMARY }).child(body));

        Some(
            div()
                .id("settings-overlay")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.))
                .py(px(40.))
                .bg(hexa(0, 0.55))
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" && this.editor.is_none() && !this.manager.recording {
                        this.close_settings(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(modal)
                .into_any_element(),
        )
    }

    fn render_manager(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.connections.lock().unwrap();
        let connections = store.connections.clone();
        let groups = store.groups.clone();
        drop(store);

        let mut names = groups.groups.clone();
        for c in &connections {
            if let Some(g) = &c.group
                && !names.contains(g)
            {
                names.push(g.clone());
            }
        }

        let mut list: Vec<AnyElement> = Vec::new();
        if self.manager.group_edit == Some(GroupEdit::New)
            && let Some(input) = &self.manager.group_input
        {
            list.push(div().mb(px(6.)).child(group_field(input)).into_any_element());
        }
        if connections.is_empty() && names.is_empty() {
            list.push(
                div()
                    .py(px(30.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(div().text_size(px(13.)).text_color(TEXT_MUTED).child("No saved connections"))
                    .child(header_button("mgr-first", "Add your first connection", false).on_click(cx.listener(|this, _, window, cx| this.open_editor(None, None, window, cx))))
                    .into_any_element(),
            );
        } else if names.is_empty() {
            list.extend(connections.iter().map(|c| self.manager_row(c, cx)));
        } else {
            for g in &names {
                let members: Vec<&SavedConnection> = connections.iter().filter(|c| c.group.as_ref() == Some(g)).collect();
                let collapsed = groups.collapsed.contains(g);
                list.push(self.group_header(g, members.len(), collapsed, true, cx));
                if !collapsed {
                    let rows: Vec<AnyElement> = members.iter().map(|c| self.manager_row(c, cx)).collect();
                    list.push(
                        div()
                            .pl(px(14.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .when(rows.is_empty(), |d| {
                                d.child(div().pl(px(20.)).py(px(6.)).text_size(px(11.)).text_color(TEXT_MUTED).child("Empty — add connections with “+ Add”"))
                            })
                            .children(rows)
                            .into_any_element(),
                    );
                }
            }
            let ungrouped: Vec<&SavedConnection> = connections.iter().filter(|c| c.group.is_none()).collect();
            let collapsed = groups.collapsed.iter().any(|g| g == "\u{0}ungrouped");
            list.push(self.group_header("Ungrouped", ungrouped.len(), collapsed, false, cx));
            if !collapsed {
                list.push(div().pl(px(14.)).flex().flex_col().gap(px(2.)).children(ungrouped.iter().map(|c| self.manager_row(c, cx))).into_any_element());
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(20.))
                    .pt(px(16.))
                    .pb(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(BORDER)
                    .child(div().flex_1().text_size(px(15.)).font_weight(FontWeight::BOLD).child("SSH Connections"))
                    .child(header_button("mgr-group", "+ Group", false).on_click(cx.listener(|this, _, window, cx| this.start_group_edit(GroupEdit::New, window, cx))))
                    .child(header_button("mgr-add", "+ Add", true).on_click(cx.listener(|this, _, window, cx| this.open_editor(None, None, window, cx)))),
            )
            .when_some(self.manager.error.clone(), |d, e| {
                d.child(div().px(px(20.)).py(px(8.)).bg(hexa(0xf14c4c, 0.08)).text_size(px(12.)).text_color(ACCENT_RED).child(e))
            })
            .child(div().id("mgr-list").flex_1().min_h_0().overflow_y_scroll().px(px(12.)).py(px(8.)).flex().flex_col().gap(px(2.)).children(list))
            .into_any_element()
    }

    fn group_header(&self, name: &str, count: usize, collapsed: bool, editable: bool, cx: &mut Context<Self>) -> AnyElement {
        let key = if editable { name.to_owned() } else { "\u{0}ungrouped".to_owned() };
        if self.manager.group_edit == Some(GroupEdit::Rename(name.to_owned()))
            && let Some(input) = &self.manager.group_input
        {
            return div().py(px(2.)).child(group_field(input)).into_any_element();
        }
        let hover = SharedString::from(format!("grp-{key}"));
        let (k_toggle, k_add, k_ren, k_del, k_yes) = (key.clone(), name.to_owned(), name.to_owned(), name.to_owned(), name.to_owned());
        let confirming = self.manager.confirm_ungroup.as_deref() == Some(name);
        div()
            .id(SharedString::from(format!("grp-{key}")))
            .group(hover.clone())
            .px(px(8.))
            .py(px(6.))
            .rounded(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(TEXT_MUTED)
            .cursor_pointer()
            .hover(|d| d.bg(BG_HOVER))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_manager_group(&k_toggle, cx)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if !*hovered && this.manager.confirm_ungroup.is_some() {
                    this.manager.confirm_ungroup = None;
                    cx.notify();
                }
            }))
            .child(div().w(px(12.)).child(if collapsed { "▸" } else { "▾" }))
            .child(div().text_color(TEXT_PRIMARY).child(name.to_uppercase()))
            .child(div().flex_1())
            .child(div().group_hover(hover.clone(), |s| s.invisible()).child(count.to_string()))
            .when(editable, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .invisible()
                        .group_hover(hover, |s| s.visible())
                        .child(small_button(SharedString::from(format!("grp-add-{key}")), "+ Add").on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_editor(None, Some(k_add.clone()), window, cx);
                        })))
                        .child(small_button(SharedString::from(format!("grp-ren-{key}")), "Rename").on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.start_group_edit(GroupEdit::Rename(k_ren.clone()), window, cx);
                        })))
                        .map(|d| {
                            if confirming {
                                d.child(
                                    small_button(SharedString::from(format!("grp-yes-{key}")), "Ungroup all")
                                        .bg(ACCENT_RED)
                                        .text_color(BG_PRIMARY)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.ungroup_all(&k_yes, cx);
                                        })),
                                )
                            } else {
                                d.child(small_button(SharedString::from(format!("grp-del-{key}")), "Delete").text_color(ACCENT_RED).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.manager.confirm_ungroup = Some(k_del.clone());
                                        cx.notify();
                                    },
                                )))
                            }
                        }),
                )
            })
            .into_any_element()
    }

    fn manager_row(&self, c: &SavedConnection, cx: &mut Context<Self>) -> AnyElement {
        let id = c.id.clone();
        let hover = SharedString::from(format!("mrow-{id}"));
        let tested = self.manager.tested.get(&id).cloned();
        let testing = self.manager.testing.contains(&id);
        let confirming = self.manager.confirm_delete.as_deref() == Some(id.as_str());
        let dot = match &tested {
            Some(Ok(())) => ACCENT_GREEN,
            Some(Err(_)) => ACCENT_RED,
            None => BORDER,
        };
        let auth = if c.auth_method == AuthMethod::PrivateKey { "key" } else { "password" };
        let (t_id, e_id, d_id, x_id, y_id) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());

        div()
            .id(SharedString::from(format!("mrow-{id}")))
            .group(hover.clone())
            .px(px(12.))
            .py(px(10.))
            .rounded(px(6.))
            .flex()
            .items_center()
            .gap(px(10.))
            .hover(|d| d.bg(BG_HOVER))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if !*hovered && this.manager.confirm_delete.as_deref() == Some(x_id.as_str()) {
                    this.manager.confirm_delete = None;
                    cx.notify();
                }
            }))
            .child(div().size(px(8.)).flex_none().rounded_full().bg(dot))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(c.label.clone()))
                    .child(
                        div()
                            .flex()
                            .text_size(px(11.))
                            .text_color(TEXT_MUTED)
                            .child(format!("{}@{}:{}", c.username, c.host, c.port))
                            .child(div().ml(px(8.)).opacity(0.6).child(auth)),
                    )
                    .when_some(tested.and_then(|t| t.err()), |d, e| d.child(div().text_size(px(10.)).text_color(ACCENT_RED).child(e))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .invisible()
                    .group_hover(hover, |s| s.visible())
                    .when(testing || confirming, |d| d.visible())
                    .child(
                        small_button(SharedString::from(format!("m-test-{id}")), if testing { "Testing…" } else { "Test" })
                            .on_click(cx.listener(move |this, _, _, cx| this.test_connection(t_id.clone(), cx))),
                    )
                    .child(small_button(SharedString::from(format!("m-edit-{id}")), "Edit").on_click(cx.listener(move |this, _, window, cx| this.open_editor(Some(e_id.clone()), None, window, cx))))
                    .child(
                        small_button(SharedString::from(format!("m-dup-{id}")), "Duplicate")
                            .tooltip(|window, cx| Tooltip::new("Copy this connection (password and key included)").build(window, cx))
                            .on_click(cx.listener(move |this, _, window, cx| this.duplicate_connection(&d_id, window, cx))),
                    )
                    .map(|d| {
                        if confirming {
                            d.child(
                                small_button(SharedString::from(format!("m-yes-{id}")), "Confirm")
                                    .bg(ACCENT_RED)
                                    .text_color(BG_PRIMARY)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.manager.confirm_delete = None;
                                        this.delete_connection(&y_id, cx);
                                    })),
                            )
                        } else {
                            d.child(small_button(SharedString::from(format!("m-del-{id}")), "Delete").text_color(ACCENT_RED).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.manager.confirm_delete = Some(id.clone());
                                    cx.notify();
                                },
                            )))
                        }
                    }),
            )
            .into_any_element()
    }
}

fn group_field(input: &Entity<InputState>) -> Div {
    div()
        .px(px(10.))
        .py(px(2.))
        .rounded(px(6.))
        .bg(BG_PRIMARY)
        .border_1()
        .border_color(ACCENT)
        .text_size(px(12.))
        .child(Input::new(input).appearance(false).xsmall())
}
