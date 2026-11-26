//! The window's root: the environment's state and everything drawn around it.
//!
//! State is one `WorkspaceState` per environment, edited in place and saved a
//! moment after the last change. Terminals are keyed by container id and live
//! as long as their pane: switching workspace or environment only hides them,
//! the shells keep running — as in the Electron app.
//!
//! Drawing is split by surface: `chrome` (title strip, status bar, environment
//! picker), `sidebar`, `grid` (panes and splitters).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::model::connections::{AuthMethod, ConnectionStore, SavedConnection};
use crate::model::layout::{Direction, NodePath};
use crate::model::store::{DEFAULT_ENV, EnvSummary, NativeSettings, Store};
use crate::model::workspace::{ContainerContent, ContentType, Todo, TodoStatus, Workspace, WorkspaceState, now_ms};
use crate::ssh::{HostKeyChecker, SshAuth, SshParams};
use crate::terminal::RemoteStatus;
use crate::terminal::procinfo::Process;
use crate::terminal::view::{DEFAULT_FONT_SIZE, TerminalEvent, TerminalView};
use crate::theme::*;

actions!(
    splitgrid,
    [
        SplitRight,
        SplitDown,
        ClosePane,
        NewWorkspace,
        ToggleSidebar,
        NextWorkspace,
        PrevWorkspace,
        ToggleFocusMode,
        OpenEnvironments,
        NewWindow,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        UiZoomIn,
        UiZoomOut,
        UiZoomReset,
        OpenSettings,
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-d", SplitRight, None),
        KeyBinding::new("cmd-shift-d", SplitDown, None),
        // The UI kit's Root binds Tab to "focus the next control in the window".
        // In a terminal (shell completion) and the SQL grid (next cell) Tab is
        // the view's own key: NoAction drops Root's binding there, so the key
        // reaches their key-down handlers.
        KeyBinding::new("tab", NoAction, Some("Terminal")),
        KeyBinding::new("shift-tab", NoAction, Some("Terminal")),
        KeyBinding::new("tab", NoAction, Some("ResultGrid")),
        KeyBinding::new("shift-tab", NoAction, Some("ResultGrid")),
        // Only from a terminal: in the notes or a text field ⌘W must not close a pane.
        KeyBinding::new("cmd-w", ClosePane, Some("Terminal")),
        KeyBinding::new("cmd-n", NewWorkspace, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-]", NextWorkspace, None),
        KeyBinding::new("cmd-shift-[", PrevWorkspace, None),
        KeyBinding::new("cmd-shift-f", ToggleFocusMode, None),
        KeyBinding::new("cmd-o", OpenEnvironments, None),
        KeyBinding::new("cmd-shift-n", NewWindow, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        // Terminal font size, per pane (SplitGrid: ⌘+ / ⌘− / ⌘0, 8–28pt).
        KeyBinding::new("cmd-=", ZoomIn, Some("Terminal")),
        KeyBinding::new("cmd-+", ZoomIn, Some("Terminal")),
        KeyBinding::new("cmd-shift-=", ZoomIn, Some("Terminal")),
        KeyBinding::new("cmd--", ZoomOut, Some("Terminal")),
        KeyBinding::new("cmd-0", ZoomReset, Some("Terminal")),
        KeyBinding::new("cmd-=", ZoomIn, Some("Ide")),
        KeyBinding::new("cmd-+", ZoomIn, Some("Ide")),
        KeyBinding::new("cmd-shift-=", ZoomIn, Some("Ide")),
        KeyBinding::new("cmd--", ZoomOut, Some("Ide")),
        KeyBinding::new("cmd-0", ZoomReset, Some("Ide")),
        // Everywhere else (sidebar, title strip, SQL / SFTP panes, text fields):
        // the whole window's UI, like the Electron app's page zoom. The pane
        // bindings above are deeper in the context stack, so they win there.
        KeyBinding::new("cmd-=", UiZoomIn, None),
        KeyBinding::new("cmd-+", UiZoomIn, None),
        KeyBinding::new("cmd-shift-=", UiZoomIn, None),
        KeyBinding::new("cmd--", UiZoomOut, None),
        KeyBinding::new("cmd-0", UiZoomReset, None),
    ]);
    // App-level, so they work from any window and with nothing focused.
    cx.on_action(|_: &UiZoomIn, cx| ui_zoom(crate::ui::Step::In, cx));
    cx.on_action(|_: &UiZoomOut, cx| ui_zoom(crate::ui::Step::Out, cx));
    cx.on_action(|_: &UiZoomReset, cx| ui_zoom(crate::ui::Step::Reset, cx));
}

/// One whole-window zoom step: every window follows, and the level is saved.
pub fn ui_zoom(step: crate::ui::Step, cx: &mut App) {
    let level = crate::ui::step(crate::ui::level(), step);
    if level == crate::ui::level() {
        return;
    }
    crate::ui::apply(level, cx);
    if let Some(shared) = cx.try_global::<crate::windows::Shared>()
        && let Err(e) = shared.store.save_ui_zoom_level(level)
    {
        log::error!("saving the UI zoom: {e:#}");
    }
}

const SAVE_DELAY: Duration = Duration::from_millis(400);
/// After an automatic reconnect, further drops of the same pane wait for the user (SplitGrid: 10s).
const AUTO_RECONNECT_COOLDOWN: Duration = Duration::from_secs(10);
/// How often pane labels (foreground process, cwd) are refreshed.
const PROCESS_POLL: Duration = Duration::from_millis(1000);
pub const SIDEBAR_MIN: f32 = 140.;
pub const SIDEBAR_MAX: f32 = 400.;
pub const SIDEBAR_COLLAPSED: f32 = 48.;

pub(crate) enum Drag {
    Split { path: NodePath, horizontal: bool },
    Sidebar,
    /// A pane picked up by its header, to swap with the pane it's dropped on
    /// (WorkspaceGrid's handleDragStart). `active` once the mouse has moved
    /// past PANE_DRAG_THRESHOLD, so header clicks stay clicks.
    Pane { id: String, start: Point<Pixels>, pos: Point<Pixels>, active: bool },
}

/// How far the mouse travels with a pane header held before it's a drag.
const PANE_DRAG_THRESHOLD: f32 = 4.;

/// What the rename field is renaming.
#[derive(Clone, PartialEq)]
pub enum Renaming {
    Workspace(String),
    Environment(String),
    /// A terminal pane's header name (its container id): Electron's EditableTerminalName.
    Pane(String),
}

/// The sidebar popovers (WorkspaceNotesTodo / EnvTodoButton in the Electron app).
#[derive(Clone, PartialEq)]
pub enum PopoverKind {
    Notes(String),
    Todos(String),
    EnvTodos,
    /// Right-click menu of a workspace row.
    WorkspaceMenu(String),
    /// The "+ workspace" menu: New, Open (a folder), Open Recent.
    AddWorkspace,
    /// The bottom bar's Commands and Prompts (crate::commands, crate::prompts).
    Commands,
    Prompts,
}

#[derive(Clone)]
pub struct Popover {
    pub kind: PopoverKind,
    /// Window position of the popover's top-left corner.
    pub at: Point<Pixels>,
    /// What it may cover before it has painted (crate::overlay).
    pub guess: Bounds<Pixels>,
}

pub(crate) struct Pane {
    pub view: Entity<TerminalView>,
    /// The foreground job, refreshed every second.
    pub process: Option<Process>,
    _subscription: Subscription,
}

pub struct AppView {
    pub(crate) store: Store,
    pub(crate) settings: NativeSettings,
    pub(crate) envs: Vec<EnvSummary>,
    pub(crate) env_id: String,
    pub(crate) state: WorkspaceState,
    pub(crate) panes: HashMap<String, Pane>,
    /// Each split's on-screen bounds from the last frame, for dragging its divider.
    pub(crate) split_bounds: Rc<RefCell<HashMap<NodePath, Bounds<Pixels>>>>,
    pub(crate) drag: Option<Drag>,
    save_task: Option<Task<()>>,
    focus_pending: bool,
    /// Keyboard focus "on the window itself" (sidebar, title strip): no pane's
    /// key context, so ⌘+/− zoom the window rather than a pane's font.
    pub(crate) root_focus: FocusHandle,
    /// The pane shown alone over the grid (focus mode), if any.
    pub(crate) focus_mode: Option<String>,
    /// The pane each workspace left lifted in focus mode, restored on coming back.
    focus_by_ws: HashMap<String, String>,
    /// Collapsed panes' chips and preview (crate::collapsed).
    pub(crate) collapsed_ui: crate::collapsed::CollapsedUi,
    /// Focus mode's open/close reveal: (start, closing).
    pub(crate) focus_anim: Option<(Instant, bool)>,
    /// The lifted pane's tile, relative to the grid: where the reveal starts and ends.
    pub(crate) focus_from: Option<Bounds<Pixels>>,
    /// Each pane's on-screen bounds and the grid area's, from the last frame.
    pub(crate) pane_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Each sidebar workspace row's bounds, by workspace id, for dropping a pane on it.
    pub(crate) ws_row_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    pub(crate) grid_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// The sidebar's width tween while it collapses or expands: (from, start).
    pub(crate) sidebar_anim: Option<(f32, std::time::Instant)>,
    /// Splits growing and closes shrinking (layout_anim.rs), render-only.
    pub(crate) layout_anim: crate::layout_anim::LayoutAnim,

    pub(crate) popover: Option<Popover>,
    /// The notes / todo popover's bounds as last painted (None until it has
    /// painted): browser panes it doesn't cover stay on screen.
    /// The Commands / Prompts popovers' fields and editor state.
    pub(crate) snippets: crate::commands::SnippetsUi,
    /// The "+ workspace → Open Recent" list, read when that menu opens; and whether its flyout shows.
    pub(crate) recent_workspaces: Vec<Workspace>,
    pub(crate) recent_flyout: bool,
    /// The workspace menu is asking "Close <name>?".
    pub(crate) ws_menu_confirm: bool,
    /// Whether the pointer is over the "Open Recent" row / its flyout; the flyout
    /// closes a beat after it leaves both, so the pointer can cross the seam.
    pub(crate) recent_hover: [bool; 2],
    recent_close: Option<Task<()>>,
    pub(crate) env_picker_open: bool,
    /// The picker is choosing what a new window shows (⌘⇧N), not switching this one.
    pub(crate) env_picker_new_window: bool,
    pub(crate) notes: Entity<TextareaState>,
    pub(crate) todo_input: Entity<InputState>,
    pub(crate) rename_input: Entity<InputState>,
    pub(crate) renaming: Option<Renaming>,
    /// An image ⌘-clicked in a terminal, shown over the window (crate::image_view).
    pub(crate) image_preview: Option<crate::image_view::ImagePreview>,
    /// The notes text last pushed into the textarea, to tell its own edits from ours.
    notes_shown: String,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,

    // SSH.
    pub(crate) connections: Arc<Mutex<ConnectionStore>>,
    pub(crate) ssh_search: Entity<InputState>,
    /// "Connect over SSH" panes connecting now: container id → saved connection id.
    pub(crate) ssh_pending: HashMap<String, String>,
    /// The last failure shown in an SSH picker or a failed SSH pane, by container id.
    pub(crate) ssh_errors: HashMap<String, String>,
    last_auto_reconnect: HashMap<String, Instant>,
    /// The connection a picker last tried, to retry it after accepting a new host key.
    ssh_last_attempt: HashMap<String, String>,
    pub(crate) editor: Option<crate::ssh_ui::ConnectionEditor>,
    /// The open Workspace Settings modal (SFTP sync, sync_settings.rs).
    pub(crate) sync_settings: Option<crate::sync_settings::SyncSettingsUi>,
    pub(crate) sftp_panes: HashMap<String, (Entity<crate::sftp_pane::SftpPane>, Subscription)>,
    pub(crate) ide_panes: HashMap<String, (Entity<crate::ide::IdePane>, Subscription)>,
    /// What each IDE pane was last told about its workspace's sync (push_ide_sync).
    pub(crate) ide_sync_seen: HashMap<String, (bool, Option<serde_json::Value>)>,
    /// A running IDE push / pull per IDE pane: its cancel flag.
    pub(crate) ide_sftp_runs: HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) sql_panes: HashMap<String, (Entity<crate::sql_pane::SqlPane>, Subscription)>,
    /// Browser panes of every workspace in this window (browser/panes.rs).
    pub(crate) browser_panes: HashMap<String, crate::browser::panes::Entry>,
    pub(crate) settings_tab: Option<crate::settings::SettingsTab>,
    pub(crate) manager: crate::settings::ManagerState,
    /// The agent bridge (receiver port, token, switches); None when it couldn't start.
    pub(crate) agent: Option<Rc<crate::agent::AgentConfig>>,
    /// Agent terminals' Working / Waiting / Done, and what the user hasn't seen yet.
    pub(crate) attention: crate::agent::attention::Attention,
    /// `app-settings.json`'s mtime when last looked at (the Electron app edits it too).
    app_settings_seen: Option<std::time::SystemTime>,
    /// What SPLITGRID_DEBUG_LAYOUT printed last.
    debug_layout_seen: Option<String>,
    /// Terminals' process trees and ports, and their popovers / task manager (procs/ui.rs).
    pub(crate) procs: crate::procs::ui::Procs,
}

impl AppView {
    /// A window on `env` (None: the last active environment).
    pub fn new(env: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shared = cx.global::<crate::windows::Shared>().clone();
        let store = shared.store.clone();
        let settings = store.settings();
        let envs = store.list_envs();
        let wanted = env.unwrap_or_else(|| settings.active_environment.clone());
        let env_id = if envs.iter().any(|e| e.id == wanted) { wanted } else { DEFAULT_ENV.to_owned() };
        crate::windows::set_env(window.window_handle(), &env_id, cx);
        // Closing the window: save its environment, and it no longer holds it.
        let handle = window.window_handle();
        let sync_owner = cx.entity_id().to_string();
        cx.on_release(move |this, cx| {
            this.flush();
            crate::sftp_sync::stop_watchers(&sync_owner);
            crate::windows::forget(handle, cx);
        })
        .detach();
        let state = load_state(&store, &env_id);

        let notes = cx.new(|cx| TextareaState::new(window, cx).placeholder("Next steps, reminders, anything…"));
        let todo_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add a task…  (Enter)"));
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search connections…"));

        let subscriptions = vec![
            cx.subscribe_in(&notes, window, |this, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.set_notes(text, cx);
                }
            }),
            cx.subscribe_in(&todo_input, window, |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit_todo(window, cx);
                }
            }),
            cx.subscribe_in(&ssh_search, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&rename_input, window, |this, input, ev: &InputEvent, _window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    let text = input.read(cx).value().trim().to_string();
                    this.commit_rename(text, cx);
                }
                _ => {}
            }),
        ];

        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(PROCESS_POLL).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    this.refresh_processes(cx);
                    this.refresh_attention(window, cx);
                    // The Electron app shares the connection files: follow its edits.
                    if this.connections.lock().unwrap().sync_from_disk() {
                        cx.notify();
                    }
                    // …and app-settings.json: Settings shows its edits, the shortcut follows them.
                    this.sync_app_settings(window, cx);
                    // Workspace SFTP auto sync: a watcher per workspace whose sync is on.
                    this.reconcile_sync_watchers(cx);
                    // Persistent terminals: session names, Settings status.
                    this.ptyd_poll(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        // What SFTP sync uploaded: the IDE tree highlights synced files.
        let mut file_events = crate::sftp_sync::file_events();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(ev) = file_events.next().await {
                if this.update(cx, |this, cx| this.apply_sync_file_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Development aid: SPLITGRID_DEBUG_QUIT_AFTER=<secs> closes the window the
        // way a confirmed ⌘Q does (the last window closing quits the app).
        if let Some(secs) = std::env::var("SPLITGRID_DEBUG_QUIT_AFTER").ok().and_then(|v| v.parse::<u64>().ok()) {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(secs)).await;
                let _ = this.update_in(cx, |_, window, _| window.remove_window());
            })
            .detach();
        }

        cx.on_app_quit(|this, _cx| {
            this.flush();
            async {}
        })
        .detach();

        Self {
            store,
            settings,
            envs,
            env_id,
            state,
            panes: HashMap::new(),
            split_bounds: Default::default(),
            drag: None,
            save_task: None,
            focus_pending: true,
            root_focus: cx.focus_handle(),
            focus_mode: None,
            focus_by_ws: HashMap::new(),
            collapsed_ui: Default::default(),
            focus_anim: None,
            focus_from: None,
            pane_bounds: Default::default(),
            ws_row_bounds: Default::default(),
            grid_bounds: Default::default(),
            sidebar_anim: None,
            layout_anim: Default::default(),
            popover: None,
            snippets: crate::commands::SnippetsUi::new(window, cx),
            recent_workspaces: Vec::new(),
            recent_flyout: false,
            ws_menu_confirm: false,
            recent_hover: [false; 2],
            recent_close: None,
            env_picker_open: false,
            env_picker_new_window: false,
            notes,
            todo_input,
            rename_input,
            renaming: None,
            image_preview: None,
            notes_shown: String::new(),
            _subscriptions: subscriptions,
            _poll: poll,
            connections: shared.connections.clone(),
            ssh_search,
            ssh_pending: HashMap::new(),
            ssh_errors: HashMap::new(),
            last_auto_reconnect: HashMap::new(),
            ssh_last_attempt: HashMap::new(),
            editor: None,
            sync_settings: None,
            // Development aid for screenshots: SPLITGRID_DEBUG_OPEN=settings, general or skills.
            sftp_panes: HashMap::new(),
            ide_panes: HashMap::new(),
            ide_sync_seen: HashMap::new(),
            ide_sftp_runs: HashMap::new(),
            sql_panes: HashMap::new(),
            browser_panes: HashMap::new(),
            settings_tab: match std::env::var("SPLITGRID_DEBUG_OPEN").as_deref() {
                Ok("settings") => Some(crate::settings::SettingsTab::Ssh),
                Ok("skills") => Some(crate::settings::SettingsTab::Skills),
                Ok("general") => Some(crate::settings::SettingsTab::General),
                _ => None,
            },
            manager: Default::default(),
            agent: shared.agent.clone(),
            attention: Default::default(),
            app_settings_seen: None,
            debug_layout_seen: None,
            procs: Default::default(),
        }
    }

    /// Follow edits of the shared `app-settings.json` made elsewhere.
    fn sync_app_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seen = crate::app_settings::mtime(&self.store.app_settings_path());
        if seen == self.app_settings_seen {
            return;
        }
        self.app_settings_seen = seen;
        let all = self.store.app_settings();
        crate::app_settings::apply_focus_hotkey(&all, cx);
        self.follow_appearance(&all, cx);
        self.sync_settings_controls(&all, window, cx);
        self.sync_appearance_controls(&all, window, cx);
        cx.notify();
    }

    /// Whether pane `id` is in this window's environment (agent requests are routed by it).
    pub(crate) fn owns_terminal(&self, id: &str) -> bool {
        !id.is_empty() && self.state.workspaces.iter().any(|w| w.container(id).is_some())
    }

    /// Show or hide the environment picker; `new_window` makes it pick what a new window opens.
    pub(crate) fn toggle_env_picker(&mut self, new_window: bool, cx: &mut Context<Self>) {
        let same = self.env_picker_open && self.env_picker_new_window == new_window;
        self.env_picker_open = !same;
        self.env_picker_new_window = new_window;
        cx.notify();
    }

    /// Open `env` (None: a new environment) in a window of its own.
    pub(crate) fn open_in_new_window(&mut self, env: Option<String>, cx: &mut Context<Self>) {
        self.env_picker_open = false;
        cx.notify();
        cx.defer(move |cx| crate::windows::open_env(env, cx));
    }

    // ── Persistence ─────────────────────────────────────────────────────────

    /// Something in the state changed: save shortly, once edits settle.
    pub(crate) fn changed(&mut self, cx: &mut Context<Self>) {
        if let Some(ws) = self.state.active_mut() {
            ws.updated_at = now_ms();
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, _| this.flush());
        }));
        cx.notify();
    }

    /// ⌘Q / menu Quit closes only the active window, after asking: it ends that
    /// window's terminals and SSH sessions. Other windows close on their own; the
    /// last one closing quits the app (main.rs). One question at a time.
    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        static ASKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        use std::sync::atomic::Ordering;
        if ASKING.swap(true, Ordering::SeqCst) {
            return;
        }
        let last = cx.windows().len() <= 1;
        // Count what really survives: only panes attached to splitgrid-ptyd. SSH
        // panes and terminals opened before persistence was turned on end.
        let total = self.panes.len();
        let kept = self.panes.values().filter(|p| p.view.read(cx).daemon_session().is_some()).count();
        let ended = total - kept;
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let detail = match (kept, ended) {
            (0, _) if last => "This ends every terminal and SSH session.".to_owned(),
            (0, _) => "This ends the terminals and SSH sessions of this window.".to_owned(),
            (k, 0) => format!("Its {k} persistent terminal{} keep running and reattach next time.", plural(k)),
            (k, e) => format!(
                "{k} persistent terminal{} keep running and reattach next time. The other {e} (SSH, or opened before persistence was on) end.",
                plural(k)
            ),
        };
        let detail = if last { detail } else { format!("{detail} Other windows stay open.") };
        let (message, button) = if last { ("Quit SplitGrid Lite?", "Quit") } else { ("Close this window?", "Close Window") };
        let answer = window.prompt(PromptLevel::Info, message, Some(&detail), &[PromptButton::ok(button), PromptButton::cancel("Cancel")], cx);
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = answer.await.ok() == Some(0);
            ASKING.store(false, Ordering::SeqCst);
            if confirmed {
                // Removing the window releases this view, which saves the
                // environment; main.rs quits once no window is left.
                let _ = this.update_in(cx, |_, window, _| window.remove_window());
            }
        })
        .detach();
    }

    fn flush(&mut self) {
        self.save_task = None;
        if let Err(e) = self.store.save(&self.env_id, &self.state) {
            log::error!("saving environment {}: {e:#}", self.env_id);
        }
        crate::history::workspaces_saved(&self.env_id, &self.state);
    }

    pub(crate) fn save_settings(&mut self) {
        self.settings.active_environment = self.env_id.clone();
        // App-wide, and maybe changed from another window since this one loaded.
        self.settings.ui_zoom_level = crate::ui::level();
        if let Err(e) = self.store.save_settings(&self.settings) {
            log::error!("saving settings: {e:#}");
        }
    }

    // ── Environments ────────────────────────────────────────────────────────

    pub(crate) fn switch_env(&mut self, env_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.env_picker_open = false;
        if env_id == self.env_id {
            cx.notify();
            return;
        }
        // Open in another window already: go there (two windows on one environment would clobber its file).
        if crate::windows::open_elsewhere(window.window_handle(), cx).contains(&env_id) {
            cx.notify();
            cx.defer(move |cx| crate::windows::open_env(Some(env_id), cx));
            return;
        }
        self.flush();
        self.stash_focus_mode();
        self.env_id = env_id;
        crate::windows::set_env(window.window_handle(), &self.env_id, cx);
        self.state = load_state(&self.store, &self.env_id);
        self.save_settings();
        self.after_switch(window, cx);
    }

    pub(crate) fn new_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Environment {}", self.envs.len() + 1);
        match self.store.create_env(&name) {
            Ok(id) => {
                self.envs = self.store.list_envs();
                self.switch_env(id.clone(), window, cx);
                self.env_picker_open = true;
                self.start_rename(Renaming::Environment(id), name, window, cx);
            }
            Err(e) => log::error!("creating an environment: {e:#}"),
        }
    }

    pub(crate) fn delete_env(&mut self, env_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if env_id == DEFAULT_ENV {
            return;
        }
        // Its terminals go with it.
        if let Ok(state) = self.store.load(env_id) {
            for ws in &state.workspaces {
                for c in &ws.containers {
                    self.panes.remove(&c.id);
                    crate::ptyd::end_session(&c.id);
                }
            }
        }
        if let Err(e) = self.store.delete_env(env_id) {
            log::error!("deleting environment {env_id}: {e:#}");
        }
        self.envs = self.store.list_envs();
        if self.env_id == env_id {
            self.env_id = DEFAULT_ENV.into();
            self.state = load_state(&self.store, DEFAULT_ENV);
            self.save_settings();
            self.after_switch(window, cx);
        }
        cx.notify();
    }

    // ── Workspaces ──────────────────────────────────────────────────────────

    pub(crate) fn switch_workspace(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.active_workspace_id.as_deref() == Some(id.as_str()) {
            return;
        }
        self.stash_focus_mode();
        self.state.active_workspace_id = Some(id);
        self.changed(cx);
        self.after_switch(window, cx);
    }

    /// A clicked notification: show workspace `ws` and focus its terminal `id`
    /// (just the workspace when the terminal has gone since).
    pub(crate) fn reveal_terminal(&mut self, ws: &str, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.workspaces.iter().any(|w| w.id == ws) {
            return;
        }
        self.switch_workspace(ws.to_owned(), window, cx);
        if let Some(w) = self.state.workspaces.iter_mut().find(|w| w.id == ws)
            && w.container(id).is_some()
        {
            // A collapsed terminal: its parent, whose header holds it.
            let id = w.collapsed_parent(id).unwrap_or_else(|| id.to_owned());
            let id = id.as_str();
            w.focused_container_id = Some(id.to_owned());
            if self.focus_mode.as_deref().is_some_and(|f| f != id) {
                self.focus_mode = None;
            }
        }
        self.focus_pending = true;
        self.changed(cx);
    }

    /// Remember the active workspace's lifted pane before leaving it.
    fn stash_focus_mode(&mut self) {
        let Some(ws) = self.state.active_workspace_id.clone() else { return };
        match self.focus_mode.take().filter(|_| !matches!(self.focus_anim, Some((_, true)))) {
            Some(id) => self.focus_by_ws.insert(ws, id),
            None => self.focus_by_ws.remove(&ws),
        };
    }

    fn after_switch(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.close_preview(cx);
        self.renaming = None;
        self.popover = None;
        // Back in a workspace left in focus mode: its pane is lifted again, without the grow animation.
        let active = self.state.active();
        self.focus_mode = active.and_then(|w| self.focus_by_ws.get(&w.id).filter(|id| w.leaves().contains(id)).cloned());
        self.focus_anim = None;
        self.focus_from = None;
        self.focus_pending = true;
        cx.notify();
    }

    pub(crate) fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Workspace {}", self.state.workspaces.len() + 1);
        let ws = Workspace::new(name.clone(), None);
        let id = ws.id.clone();
        self.state.workspaces.push(ws);
        self.switch_workspace(id.clone(), window, cx);
        self.start_rename(Renaming::Workspace(id), name, window, cx);
    }

    /// A new workspace on a folder, named after it ("+ workspace → Open").
    pub(crate) fn open_folder_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("Workspace {}", this.state.workspaces.len() + 1));
                let ws = Workspace::new(name, Some(dir.to_string_lossy().into_owned()));
                let id = ws.id.clone();
                this.state.workspaces.push(ws);
                this.switch_workspace(id, window, cx);
                this.changed(cx);
            });
        })
        .detach();
    }

    /// Bring a closed workspace back ("+ workspace → Open Recent").
    pub(crate) fn open_recent_workspace(&mut self, mut ws: Workspace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.workspaces.iter().any(|w| w.id == ws.id) {
            ws.extra.insert("frozen".into(), false.into());
            ws.updated_at = now_ms();
            // A snapshot can carry leaves whose containers are gone.
            ws.normalize();
            self.state.workspaces.push(ws.clone());
        }
        self.switch_workspace(ws.id, window, cx);
        self.changed(cx);
    }

    pub(crate) fn close_workspace(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.state.workspaces.iter().position(|w| w.id == id) else {
            return;
        };
        let ws = self.state.workspaces.remove(ix);
        // Reopenable from "+ workspace → Open Recent" (the Electron app keeps the same list).
        let mut snapshot = ws.clone();
        snapshot.extra.insert("frozen".into(), false.into());
        for c in &mut snapshot.containers {
            c.content.extra.remove("terminalOutput");
        }
        if let Err(e) = self.store.add_recent_workspace(&self.env_id, &snapshot) {
            log::error!("remembering a closed workspace: {e:#}");
        }
        for c in &ws.containers {
            self.panes.remove(&c.id);
            // Closing a workspace ends its persistent terminals too.
            crate::ptyd::end_session(&c.id);
        }
        if self.state.workspaces.is_empty() {
            self.state.workspaces.push(Workspace::new("Workspace 1", None));
        }
        if self.state.active().is_none() {
            let next = self.state.workspaces[ix.min(self.state.workspaces.len() - 1)].id.clone();
            self.state.active_workspace_id = Some(next);
            self.after_switch(window, cx);
        }
        self.changed(cx);
    }

    fn cycle_workspace(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.state.workspaces.len() as isize;
        if n < 2 {
            return;
        }
        let cur = self.state.workspaces.iter().position(|w| Some(&w.id) == self.state.active_workspace_id.as_ref()).unwrap_or(0) as isize;
        let next = self.state.workspaces[((cur + step).rem_euclid(n)) as usize].id.clone();
        self.switch_workspace(next, window, cx);
    }

    /// Ask for a folder and make it the workspace's working directory (new panes start there).
    pub(crate) fn choose_folder(&mut self, ws_id: String, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use as working folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(ws) = this.state.workspaces.iter_mut().find(|w| w.id == ws_id) {
                    let default_name = ws.name.starts_with("Workspace ");
                    ws.working_directory = Some(dir.to_string_lossy().into_owned());
                    if default_name && let Some(base) = dir.file_name() {
                        ws.name = base.to_string_lossy().into_owned();
                    }
                }
                this.changed(cx);
            });
        })
        .detach();
    }

    // ── Rename ──────────────────────────────────────────────────────────────

    pub(crate) fn start_rename(&mut self, what: Renaming, current: String, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(what);
        self.rename_input.update(cx, |i, cx| {
            i.set_value(current, window, cx);
            i.focus(window, cx);
        });
        cx.notify();
    }

    /// Give the focused pane its keyboard back on the next frame.
    pub(crate) fn focus_pending_now(&mut self) {
        self.focus_pending = true;
    }

    pub(crate) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        self.focus_pending = true;
        cx.notify();
    }

    fn commit_rename(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(what) = self.renaming.take() else {
            return;
        };
        self.focus_pending = true;
        // A pane's name may be cleared: it goes back to the automatic one (the
        // foreground program, or the SSH label), as in the Electron app.
        if let Renaming::Pane(id) = &what {
            if let Some(content) = self.state.workspaces.iter_mut().find_map(|w| w.container_mut(id)).map(|c| &mut c.content) {
                content.custom_name = (!text.is_empty()).then_some(text);
            }
            self.changed(cx);
            return;
        }
        if text.is_empty() {
            cx.notify();
            return;
        }
        match what {
            Renaming::Pane(_) => unreachable!("handled above"),
            Renaming::Workspace(id) => {
                if let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == id) {
                    ws.name = text;
                }
                self.changed(cx);
            }
            Renaming::Environment(id) => {
                if let Err(e) = self.store.rename_env(&id, &text) {
                    log::error!("renaming environment: {e:#}");
                }
                self.envs = self.store.list_envs();
                cx.notify();
            }
        }
    }

    // ── Popovers: notes & todos ─────────────────────────────────────────────

    pub(crate) fn toggle_popover(&mut self, kind: PopoverKind, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.popover.as_ref().is_some_and(|p| p.kind == kind) {
            self.close_popover(cx);
            return;
        }
        if let PopoverKind::Notes(ws_id) = &kind {
            let text = self.state.workspaces.iter().find(|w| &w.id == ws_id).and_then(|w| w.notes.clone()).unwrap_or_default();
            self.notes_shown = text.clone();
            self.notes.update(cx, |n, cx| {
                n.set_value(text, window, cx);
                n.focus(window, cx);
            });
        } else if matches!(kind, PopoverKind::Todos(_) | PopoverKind::EnvTodos) {
            self.todo_input.update(cx, |i, cx| {
                i.set_value("", window, cx);
                i.focus(window, cx);
            });
        } else if kind == PopoverKind::AddWorkspace {
            // The last 10 closed here, minus any that are open again (matched by id).
            let open: Vec<&str> = self.state.workspaces.iter().map(|w| w.id.as_str()).collect();
            let mut recent = self.store.recent_workspaces(&self.env_id);
            recent.retain(|w| !open.contains(&w.id.as_str()));
            recent.truncate(10);
            self.recent_workspaces = recent;
            self.recent_flyout = false;
            self.recent_hover = [false; 2];
        } else if matches!(kind, PopoverKind::Commands | PopoverKind::Prompts) {
            self.open_snippets(&kind, window, cx);
        }
        self.ws_menu_confirm = false;
        // Until it paints: the notes / todo popover's most, or just the clicked point.
        let guess = if matches!(kind, PopoverKind::Notes(_) | PopoverKind::Todos(_) | PopoverKind::EnvTodos) {
            crate::chrome::popover_estimate(at, window)
        } else {
            crate::overlay::Cover::menu(at).guess
        };
        self.popover = Some(Popover { kind, at, guess });
        cx.notify();
    }

    /// The pointer entered or left the "Open Recent" row (0) or its flyout (1).
    pub(crate) fn set_recent_hover(&mut self, part: usize, hovered: bool, cx: &mut Context<Self>) {
        self.recent_hover[part] = hovered;
        if hovered {
            self.recent_close = None;
            if !self.recent_flyout {
                self.recent_flyout = true;
                cx.notify();
            }
            return;
        }
        self.recent_close = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let _ = this.update(cx, |this, cx| {
                if this.recent_hover == [false; 2] && this.recent_flyout {
                    this.recent_flyout = false;
                    cx.notify();
                }
            });
        }));
    }

    pub(crate) fn close_popover(&mut self, cx: &mut Context<Self>) {
        if self.popover.take().is_some() {
            self.focus_pending = true;
            cx.notify();
        }
    }

    fn set_notes(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(Popover { kind: PopoverKind::Notes(ws_id), .. }) = &self.popover else {
            return;
        };
        if text == self.notes_shown {
            return;
        }
        self.notes_shown = text.clone();
        let ws_id = ws_id.clone();
        if let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) {
            ws.notes = (!text.is_empty()).then_some(text);
            self.changed(cx);
        }
    }

    /// The todo list a popover edits: a workspace's, or the environment's.
    pub(crate) fn todo_list(&self, kind: &PopoverKind) -> Option<&Vec<Todo>> {
        match kind {
            PopoverKind::Todos(ws_id) => self.state.workspaces.iter().find(|w| &w.id == ws_id).map(|w| &w.todos),
            PopoverKind::EnvTodos => Some(&self.state.env_todos),
            PopoverKind::Notes(_) | PopoverKind::WorkspaceMenu(_) | PopoverKind::AddWorkspace | PopoverKind::Commands | PopoverKind::Prompts => None,
        }
    }

    fn todo_list_mut(&mut self, kind: &PopoverKind) -> Option<&mut Vec<Todo>> {
        match kind {
            PopoverKind::Todos(ws_id) => self.state.workspaces.iter_mut().find(|w| &w.id == ws_id).map(|w| &mut w.todos),
            PopoverKind::EnvTodos => Some(&mut self.state.env_todos),
            PopoverKind::Notes(_) | PopoverKind::WorkspaceMenu(_) | PopoverKind::AddWorkspace | PopoverKind::Commands | PopoverKind::Prompts => None,
        }
    }

    pub(crate) fn submit_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.popover.as_ref().map(|p| p.kind.clone()) else {
            return;
        };
        let text = self.todo_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(list) = self.todo_list_mut(&kind) {
            list.push(Todo::new(text));
            self.todo_input.update(cx, |i, cx| i.set_value("", window, cx));
            self.changed(cx);
        }
    }

    pub(crate) fn cycle_todo(&mut self, kind: &PopoverKind, id: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.todo_list_mut(kind).and_then(|l| l.iter_mut().find(|t| t.id == id)) {
            let next = t.status().next();
            t.set_status(next);
            self.changed(cx);
        }
    }

    pub(crate) fn delete_todo(&mut self, kind: &PopoverKind, id: &str, cx: &mut Context<Self>) {
        if let Some(list) = self.todo_list_mut(kind) {
            list.retain(|t| t.id != id);
            self.changed(cx);
        }
    }

    pub(crate) fn clear_done(&mut self, kind: &PopoverKind, cx: &mut Context<Self>) {
        if let Some(list) = self.todo_list_mut(kind) {
            list.retain(|t| t.status() != TodoStatus::Done);
            self.changed(cx);
        }
    }

    /// "Run": type the task into the active workspace's focused terminal (not executed).
    pub(crate) fn run_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let target = ws
            .focused_container_id
            .clone()
            .filter(|id| self.panes.contains_key(id))
            .or_else(|| ws.layout_tree.as_ref()?.leaf_ids().into_iter().find(|id| self.panes.contains_key(id)));
        if let Some(pane) = target.and_then(|id| self.panes.get(&id)) {
            pane.view.read(cx).paste_text(&text);
            self.popover = None;
            self.focus_pending = true;
            cx.notify();
        }
    }

    // ── Panes ───────────────────────────────────────────────────────────────

    /// Start the active workspace's terminals that aren't running yet: local
    /// shells, and SSH sessions (reconnected on restore, as in SplitGrid).
    fn ensure_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let fallback = ws.working_directory.clone();
        // Collapsed terminals run too (model::collapse).
        let leaves = ws.live_ids();
        let mut local = Vec::new();
        let mut remote = Vec::new();
        for id in leaves {
            if self.panes.contains_key(&id) || self.ssh_errors.contains_key(&id) {
                continue;
            }
            let Some(c) = ws.container(&id) else { continue };
            if c.content.is_local_terminal() {
                local.push((id, c.content.cwd.clone().or_else(|| fallback.clone()), ws.zoom(&c.id)));
            } else if c.content.is_ssh_terminal() {
                remote.push((id, c.content.connection_id.clone(), c.content.label.clone(), ws.zoom(&c.id)));
            }
        }

        for (id, cwd, zoom) in local {
            match TerminalView::spawn_local(&id, cwd.map(PathBuf::from), self.agent_env(&id), self.ptyd_meta(&id)) {
                Ok(spawned) => self.add_pane(id, spawned, zoom, window, cx),
                Err(e) => log::error!("starting a shell for pane {id}: {e:#}"),
            }
        }
        self.ensure_sftp_panes(window, cx);
        self.ensure_ide_panes(window, cx);
        self.push_ide_sync(cx);
        self.ensure_sql_panes(window, cx);
        for (id, connection_id, label, zoom) in remote {
            // By id, else by label (older state without a connection id).
            let conn = {
                let store = self.connections.lock().unwrap();
                connection_id
                    .as_deref()
                    .and_then(|cid| store.get(cid).cloned())
                    .or_else(|| label.as_deref().and_then(|l| store.connections.iter().find(|c| c.label == l).cloned()))
            };
            match conn {
                Some(conn) => {
                    let spawned = TerminalView::connect_ssh(self.ssh_params(&conn), self.host_key_checker_for(&conn));
                    self.add_pane(id.clone(), spawned, zoom, window, cx);
                    self.set_password_offer(&id, &conn, cx);
                }
                None => {
                    self.ssh_errors.insert(id, "Saved connection not found. Re-create this terminal.".into());
                }
            }
        }
    }

    /// SFTP panes that have a connection but no live view yet (restore, or just chosen).
    fn ensure_sftp_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let fallback_local = ws.working_directory.clone().filter(|p| !p.trim().is_empty());
        let wanted: Vec<(String, String, Option<String>, Option<String>)> = ws
            .layout_tree
            .as_ref()
            .map(|t| t.leaf_ids())
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !self.sftp_panes.contains_key(id))
            .filter_map(|id| {
                let c = ws.container(&id)?;
                (c.content.kind == ContentType::Sftp).then_some(())?;
                let conn = c.content.extra_str("sftpConnectionId")?;
                Some((id, conn, c.content.extra_str("sftpRemotePath"), c.content.extra_str("sftpLocalPath")))
            })
            .collect();
        for (id, conn_id, remote, local) in wanted {
            let Some(conn) = self.connections.lock().unwrap().get(&conn_id).cloned() else {
                continue;
            };
            let local = local.or_else(|| fallback_local.clone()).unwrap_or_else(|| crate::sftp::local_home().to_string_lossy().into_owned());
            let (params, check) = (self.ssh_params(&conn), self.host_key_checker_for(&conn));
            let label = conn.label.clone();
            let pane = cx.new(|cx| crate::sftp_pane::SftpPane::new(label, params, check, remote, local, window, cx));
            let pid = id.clone();
            let sub = cx.subscribe(&pane, move |this, _, ev: &crate::sftp_pane::SftpEvent, cx| this.on_sftp_event(&pid, ev, cx));
            self.sftp_panes.insert(id, (pane, sub));
        }
    }

    /// IDE panes of the active workspace that have no live view yet.
    fn ensure_ide_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let wanted: Vec<(String, PathBuf, crate::ide::IdeState, f32)> = ws
            .layout_tree
            .as_ref()
            .map(|t| t.leaf_ids())
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !self.ide_panes.contains_key(id))
            .filter_map(|id| {
                let c = ws.container(&id)?;
                (c.content.kind == ContentType::Ide).then_some(())?;
                let root = PathBuf::from(c.content.extra_str("rootPath")?);
                let state = c.content.extra.get("ideState").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                let zoom = ws.zoom(&id).unwrap_or_else(|| crate::theme::fonts().editor_size);
                Some((id, root, state, zoom))
            })
            .collect();
        for (id, root, state, zoom) in wanted {
            if !root.is_dir() {
                continue;
            }
            let pane = cx.new(|cx| crate::ide::IdePane::new(root, state, zoom, window, cx));
            let pid = id.clone();
            let sub = cx.subscribe(&pane, move |this, _, ev: &crate::ide::IdeEvent, cx| match ev {
                crate::ide::IdeEvent::State(state) => {
                    if let Some(c) = this.state.workspaces.iter_mut().find_map(|w| w.container_mut(&pid))
                        && let Ok(v) = serde_json::to_value(state)
                        && c.content.extra.get("ideState") != Some(&v)
                    {
                        c.content.extra.insert("ideState".into(), v);
                        this.changed(cx);
                    }
                }
                crate::ide::IdeEvent::Cursor => cx.notify(),
                crate::ide::IdeEvent::Sftp { push, paths, root } => this.ide_sftp(&pid, *push, paths.clone(), root.clone(), cx),
                crate::ide::IdeEvent::SftpCancel => this.ide_sftp_cancel(&pid),
            });
            self.ide_panes.insert(id, (pane, sub));
        }
    }

    /// SQL panes of the active workspace that have no live view yet.
    fn ensure_sql_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let wanted: Vec<(String, Option<serde_json::Value>)> = ws
            .layout_tree
            .as_ref()
            .map(|t| t.leaf_ids())
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !self.sql_panes.contains_key(id))
            .filter_map(|id| {
                let c = ws.container(&id)?;
                (c.content.kind == ContentType::Sql).then(|| (id, c.content.extra.get("sqlState").cloned()))
            })
            .collect();
        for (id, state) in wanted {
            self.create_sql_pane(id, state, window, cx);
        }
    }

    /// A SQL pane's live view, made now if it has none (an agent drives a hidden workspace's pane).
    pub(crate) fn sql_pane_view(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<crate::sql_pane::SqlPane>> {
        if let Some((pane, _)) = self.sql_panes.get(id) {
            return Some(pane.clone());
        }
        let c = self.state.workspaces.iter().find_map(|w| w.container(id)).filter(|c| c.content.kind == ContentType::Sql)?;
        let state = c.content.extra.get("sqlState").cloned();
        Some(self.create_sql_pane(id.to_owned(), state, window, cx))
    }

    fn create_sql_pane(&mut self, id: String, state: Option<serde_json::Value>, window: &mut Window, cx: &mut Context<Self>) -> Entity<crate::sql_pane::SqlPane> {
        let pane = cx.new(|cx| crate::sql_pane::SqlPane::new(state.as_ref(), window, cx));
        let pid = id.clone();
        let sub = cx.subscribe(&pane, move |this, _, ev: &crate::sql_pane::SqlEvent, cx| match ev {
            crate::sql_pane::SqlEvent::State(state) => {
                if let Some(c) = this.state.workspaces.iter_mut().find_map(|w| w.container_mut(&pid))
                    && c.content.extra.get("sqlState") != Some(state)
                {
                    c.content.extra.insert("sqlState".into(), state.clone());
                    this.changed(cx);
                }
            }
            crate::sql_pane::SqlEvent::Close => this.close_pane(&pid, cx),
        });
        self.sql_panes.insert(id, (pane.clone(), sub));
        pane
    }

    /// "IDE" in an empty container: the workspace folder, or one picked now
    /// (which becomes the workspace's folder).
    pub(crate) fn fill_ide(&mut self, id: String, cx: &mut Context<Self>) {
        let folder = self.state.active().and_then(|w| w.working_directory.clone()).filter(|d| std::path::Path::new(d).is_dir());
        if let Some(root) = folder {
            self.set_ide_root(&id, root, cx);
            return;
        }
        let answer = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open folder".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let root = dir.to_string_lossy().into_owned();
                if let Some(ws) = this.state.active_mut() {
                    ws.working_directory = Some(root.clone());
                }
                this.set_ide_root(&id, root, cx);
            });
        })
        .detach();
    }

    fn set_ide_root(&mut self, id: &str, root: String, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active_mut() else {
            return;
        };
        let Some(c) = ws.container_mut(id) else {
            return;
        };
        let label = std::path::Path::new(&root).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "IDE".into());
        c.content = ContainerContent { kind: ContentType::Ide, label: Some(label), ..ContainerContent::empty() };
        c.content.set_extra_str("rootPath", Some(root));
        ws.focused_container_id = Some(id.to_owned());
        self.changed(cx);
    }

    fn on_sftp_event(&mut self, id: &str, ev: &crate::sftp_pane::SftpEvent, cx: &mut Context<Self>) {
        use crate::sftp_pane::SftpEvent;
        let Some(c) = self.state.workspaces.iter_mut().find_map(|w| w.container_mut(id)) else {
            return;
        };
        match ev {
            SftpEvent::Paths { remote, local } => {
                c.content.set_extra_str("sftpRemotePath", Some(remote.clone()));
                c.content.set_extra_str("sftpLocalPath", Some(local.clone()));
            }
            SftpEvent::ChangeConnection => {
                c.content.set_extra_str("sftpConnectionId", None);
                c.content.set_extra_str("sftpRemotePath", None);
                c.content.label = Some("SFTP".into());
                self.sftp_panes.remove(id);
            }
        }
        self.changed(cx);
    }

    /// The SFTP pane's picker chose a connection: it starts at that server's home.
    pub(crate) fn choose_sftp(&mut self, container_id: &str, connection_id: &str, cx: &mut Context<Self>) {
        let label = self.connections.lock().unwrap().get(connection_id).map(|c| c.label.clone());
        let Some(ws) = self.state.active_mut() else {
            return;
        };
        ws.record_connection_use(connection_id);
        if let Some(c) = ws.container_mut(container_id) {
            c.content.set_extra_str("sftpConnectionId", Some(connection_id.to_owned()));
            c.content.set_extra_str("sftpRemotePath", None);
            c.content.label = label;
        }
        self.changed(cx);
    }

    pub(crate) fn add_pane(&mut self, id: String, spawned: crate::terminal::view::Spawned, zoom: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| {
            let mut v = TerminalView::new(spawned, window, cx);
            if let Some(z) = zoom {
                v.set_font_size(z, cx);
            }
            v
        });
        let pane_id = id.clone();
        let subscription = cx.subscribe(&view, move |this, _, ev: &TerminalEvent, cx| match ev {
            TerminalEvent::Exited => this.close_pane(&pane_id, cx),
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::Remote(status) => this.on_remote_status(&pane_id, status.clone(), cx),
            TerminalEvent::Command(command) => this.record_recent_command(command, cx),
            TerminalEvent::OpenLink(target) => this.open_link(target.clone(), cx),
        });
        self.panes.insert(id, Pane { view, process: None, _subscription: subscription });
    }

    // ── SSH ─────────────────────────────────────────────────────────────────

    pub(crate) fn ssh_params(&self, c: &SavedConnection) -> SshParams {
        let store = self.connections.lock().unwrap();
        let auth = match c.auth_method {
            AuthMethod::Password => SshAuth::Password(store.password(c).unwrap_or_default()),
            AuthMethod::PrivateKey => SshAuth::Key {
                path: PathBuf::from(crate::grid::expand_tilde(c.private_key_path.as_deref().unwrap_or("~/.ssh/id_rsa"))),
                passphrase: store.passphrase(c),
            },
        };
        // The sudo password if set, else the login one — only when the connection opts in.
        let offer_password = if c.offer_saved_password == Some(true) { store.sudo_password(c) } else { None };
        SshParams { host: c.host.clone(), port: c.port, username: c.username.clone(), auth, offer_password }
    }

    pub(crate) fn host_key_checker_for(&self, c: &SavedConnection) -> HostKeyChecker {
        let store = self.connections.clone();
        let (host, port) = (c.host.clone(), c.port);
        Arc::new(move |fp: &str| store.lock().unwrap().check_host_key(&host, port, fp))
    }

    /// Tell an SSH pane how to word its "paste saved password" hint (if the connection offers one).
    fn set_password_offer(&self, id: &str, conn: &SavedConnection, cx: &mut Context<Self>) {
        if conn.offer_saved_password != Some(true) {
            return;
        }
        let sudo = conn.encrypted_sudo_password.is_some();
        if let Some(pane) = self.panes.get(id) {
            let label = conn.label.clone();
            pane.view.update(cx, |v, _| v.set_password_offer(label.into(), sudo));
        }
    }

    /// "Connect over SSH": open the session in the picker's container; it becomes
    /// an SSH terminal once the shell is ready.
    pub(crate) fn connect_saved(&mut self, container_id: String, connection_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.connections.lock().unwrap().get(&connection_id).cloned() else {
            return;
        };
        self.ssh_errors.remove(&container_id);
        let spawned = TerminalView::connect_ssh(self.ssh_params(&conn), self.host_key_checker_for(&conn));
        let zoom = self.state.active().and_then(|w| w.zoom(&container_id));
        self.add_pane(container_id.clone(), spawned, zoom, window, cx);
        self.set_password_offer(&container_id, &conn, cx);
        self.ssh_last_attempt.insert(container_id.clone(), connection_id.clone());
        self.ssh_pending.insert(container_id, connection_id);
        cx.notify();
    }

    fn on_remote_status(&mut self, id: &str, status: RemoteStatus, cx: &mut Context<Self>) {
        match status {
            RemoteStatus::Connected => {
                if let Some(conn_id) = self.ssh_pending.remove(id) {
                    let label = self.connections.lock().unwrap().get(&conn_id).map(|c| c.label.clone()).unwrap_or_default();
                    for ws in &mut self.state.workspaces {
                        if let Some(c) = ws.container_mut(id) {
                            c.content = ContainerContent::ssh_terminal(&conn_id, &label);
                            ws.record_connection_use(&conn_id);
                        }
                    }
                    self.focus_pending = true;
                    self.changed(cx);
                }
            }
            RemoteStatus::Failed(msg) => {
                // From the picker: back to the list, with the error above it.
                if self.ssh_pending.remove(id).is_some() {
                    self.panes.remove(id);
                }
                self.ssh_errors.insert(id.to_owned(), msg);
            }
            RemoteStatus::Closed { clean: false } => {
                // The link dropped: one automatic reconnect, then the user decides.
                let recent = self.last_auto_reconnect.get(id).is_some_and(|t| t.elapsed() < AUTO_RECONNECT_COOLDOWN);
                if !recent {
                    self.last_auto_reconnect.insert(id.to_owned(), Instant::now());
                    self.reconnect(id, cx);
                }
            }
            RemoteStatus::Connecting | RemoteStatus::Closed { clean: true } => {}
        }
        cx.notify();
    }

    /// Drop the SSH session (if any) and connect again; the pane restarts on the next frame.
    pub(crate) fn reconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.panes.remove(id);
        self.ssh_errors.remove(id);
        self.focus_pending = true;
        cx.notify();
    }

    /// Accept a server's new host key (the "Host key changed" card) and try again:
    /// from the picker with the same connection, or by reconnecting the pane.
    pub(crate) fn accept_new_host_key(&mut self, id: &str, host: &str, port: u16, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.connections.lock().unwrap().forget_host_key(host, port) {
            self.ssh_errors.insert(id.to_owned(), format!("Could not update the stored host key. {e:#}"));
            cx.notify();
            return;
        }
        match self.ssh_last_attempt.get(id).cloned() {
            Some(conn_id) if self.state.active().and_then(|w| w.container(id)).is_some_and(|c| c.content.kind == ContentType::SshConnect) => {
                self.connect_saved(id.to_owned(), conn_id, window, cx)
            }
            _ => self.reconnect(id, cx),
        }
    }

    /// Fill an empty container: a local shell, or the SSH picker.
    pub(crate) fn fill_container(&mut self, id: &str, kind: ContentType, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active_mut() else {
            return;
        };
        let cwd = ws.working_directory.clone();
        let Some(c) = ws.container_mut(id) else {
            return;
        };
        c.content = match kind {
            ContentType::Terminal => ContainerContent::local_terminal(cwd),
            ContentType::SshConnect => ContainerContent { kind: ContentType::SshConnect, label: Some("SSH".into()), ..ContainerContent::empty() },
            ContentType::Sftp => ContainerContent { kind: ContentType::Sftp, label: Some("SFTP".into()), ..ContainerContent::empty() },
            ContentType::Sql => ContainerContent { kind: ContentType::Sql, label: Some("SQL".into()), ..ContainerContent::empty() },
            ContentType::Browser => crate::browser::panes::new_content(),
            _ => return,
        };
        ws.focused_container_id = Some(id.to_owned());
        self.ssh_errors.remove(id);
        self.focus_pending = true;
        self.changed(cx);
    }

    /// The SSH picker's Cancel: back to an empty container.
    pub(crate) fn empty_container(&mut self, id: &str, cx: &mut Context<Self>) {
        self.panes.remove(id);
        self.ssh_pending.remove(id);
        self.ssh_errors.remove(id);
        if let Some(c) = self.state.active_mut().and_then(|w| w.container_mut(id)) {
            c.content = ContainerContent::empty();
        }
        self.changed(cx);
    }

    /// Refresh every pane's foreground process; remember a terminal's cwd for the next launch.
    fn refresh_processes(&mut self, cx: &mut Context<Self>) {
        let mut changed_any = false;
        let mut cwds = Vec::new();
        for (id, pane) in &mut self.panes {
            let now = pane.view.read(cx).foreground();
            if now != pane.process {
                if let Some(cwd) = now.as_ref().and_then(|p| p.cwd.clone()) {
                    cwds.push((id.clone(), cwd));
                }
                pane.process = now;
                changed_any = true;
            }
        }
        let mut persist = false;
        for (id, cwd) in cwds {
            for ws in &mut self.state.workspaces {
                if let Some(c) = ws.containers.iter_mut().find(|c| c.id == id)
                    && c.content.cwd.as_deref() != Some(cwd.as_str())
                {
                    c.content.cwd = Some(cwd.clone());
                    persist = true;
                }
            }
        }
        if persist {
            self.changed(cx);
        } else if changed_any {
            cx.notify();
        }
    }

    /// A press on the window's own chrome (sidebar, title strip) takes keyboard
    /// focus off the panes. Capture phase, so a text field or a workspace switch
    /// under the pointer still focuses what it wants afterwards.
    pub(crate) fn focus_window_chrome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.root_focus.focus(window, cx);
    }

    fn focus_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.state.active().and_then(|w| w.focused_container_id.clone()) else {
            return;
        };
        if let Some(pane) = self.panes.get(&id) {
            let handle = pane.view.read(cx).focus_handle().clone();
            window.defer(cx, move |window, cx| handle.focus(window, cx));
        } else if let Some(entry) = self.browser_panes.get(&id) {
            let pane = entry.pane.clone();
            window.defer(cx, move |window, cx| pane.update(cx, |p, cx| p.focus_page(window, cx)));
        }
    }

    pub(crate) fn split(&mut self, direction: Direction, target: Option<String>, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active_mut() else {
            return;
        };
        ws.split(target.as_deref(), direction);
        self.focus_mode = None;
        self.focus_pending = true;
        self.changed(cx);
    }

    /// Collapse a pane into the neighbour that takes its space (model::collapse):
    /// it keeps running, shown as a chip in that pane's header.
    pub(crate) fn collapse_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.leaves().iter().any(|l| l == id)) else { return };
        let zone = crate::agent::zones::zone_of_pane(ws.layout_tree.as_ref(), id);
        if ws.collapse(id, zone.as_ref()).is_none() {
            return;
        }
        self.close_preview(cx);
        if self.focus_mode.as_deref() == Some(id) {
            self.focus_mode = None;
        }
        self.focus_pending = true;
        self.changed(cx);
    }

    /// Bring a collapsed pane back by splitting its parent across its longer side.
    pub(crate) fn expand_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(parent) = self.state.workspaces.iter().find_map(|w| w.collapsed_parent(id)) else { return };
        let direction = match self.pane_bounds.borrow().get(&parent) {
            Some(b) if b.size.height > b.size.width => Direction::Vertical,
            _ => Direction::Horizontal,
        };
        if let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.is_collapsed(id)) {
            crate::agent::zones::expand_collapsed(ws, id, direction);
        }
        self.close_preview(cx);
        self.focus_mode = None;
        self.focus_pending = true;
        self.changed(cx);
    }

    /// Close a pane wherever it is (its shell exits with it).
    pub(crate) fn close_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        // A SQL pane with an open transaction asks first, and comes back with SqlEvent::Close.
        if let Some((sql, _)) = self.sql_panes.get(id)
            && !sql.update(cx, |p, cx| p.request_close(cx))
        {
            return;
        }
        self.sql_panes.remove(id);
        if self.collapsed_ui.preview.as_deref() == Some(id) {
            self.close_preview(cx);
        }
        self.drop_browser_pane(id, cx);
        self.panes.remove(id);
        // A persistent terminal's session ends with its pane (dropping the view only detaches).
        crate::ptyd::end_session(id);
        self.attention.forget(id);
        crate::agent::notify::withdraw([id]);
        self.sftp_panes.remove(id);
        self.ide_panes.remove(id);
        if self.focus_mode.as_deref() == Some(id) {
            self.focus_mode = None;
        }
        if let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.container(id).is_some()) {
            ws.close(id);
        }
        self.focus_pending = true;
        self.changed(cx);
    }

    /// Zoom the focused pane's terminal: `Some(step)` in points, `None` to reset.
    /// ⌘+ / ⌘− / ⌘0 from the View menu: the focused pane's font when the keyboard
    /// is in a terminal, IDE or browser pane, else the window zoom (propagated).
    fn pane_or_ui_zoom(&mut self, step: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let in_pane = window.context_stack().iter().any(|c| c.contains("Terminal") || c.contains("Ide") || c.contains("BrowserPane"));
        if in_pane {
            self.zoom_focused(step, cx);
        } else {
            cx.propagate();
        }
    }

    fn zoom_focused(&mut self, step: Option<f32>, cx: &mut Context<Self>) {
        let Some(id) = self.state.active().and_then(|w| w.focused_container_id.clone()) else {
            return;
        };
        let applied = if let Some(pane) = self.panes.get(&id) {
            let current = pane.view.read(cx).font_size();
            let next = step.map_or(crate::theme::fonts().terminal_size, |s| current + s);
            pane.view.update(cx, |v, cx| v.set_font_size(next, cx));
            pane.view.read(cx).font_size()
        } else if let Some((ide, _)) = self.ide_panes.get(&id) {
            let current = ide.read(cx).font_size();
            let next = step.map_or(crate::theme::fonts().editor_size, |s| (current + s).clamp(8., 28.));
            ide.update(cx, |v, cx| v.set_font_size(next, cx));
            next
        } else if let Some(entry) = self.browser_panes.get(&id) {
            let current = entry.pane.read(cx).font_size();
            let next = step.map_or(DEFAULT_FONT_SIZE, |s| (current + s).clamp(8., 28.));
            entry.pane.update(cx, |p, _| p.set_font_size(next));
            next
        } else {
            return;
        };
        if let Some(ws) = self.state.active_mut() {
            // ⌘0 hands the pane back to the default size, which then follows Settings.
            if step.is_some() { ws.set_zoom(&id, applied) } else { ws.clear_zoom(&id) }
        }
        self.changed(cx);
    }

    pub(crate) fn set_focused_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ws) = self.state.active_mut()
            && ws.focused_container_id.as_deref() != Some(id)
        {
            ws.focused_container_id = Some(id.to_owned());
            self.changed(cx);
        }
    }

    pub(crate) fn toggle_focus_mode(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let target = id.or_else(|| self.state.active().and_then(|w| w.focused_container_id.clone()));
        let closing_now = matches!(self.focus_anim, Some((_, true)));
        if self.focus_mode.is_some() && self.focus_mode == target && !closing_now {
            // Collapse back into the tile, then drop focus mode (see `settle_focus_anim`).
            if self.focus_from.is_some() {
                self.focus_anim = Some((Instant::now(), true));
            } else {
                self.focus_mode = None;
            }
        } else {
            // The tile it grows out of, relative to the grid.
            let grid = *self.grid_bounds.borrow();
            self.focus_from = target.as_ref().and_then(|id| {
                let tile = *self.pane_bounds.borrow().get(id)?;
                let grid = grid?;
                Some(Bounds::new(tile.origin - grid.origin, tile.size))
            });
            self.focus_anim = self.focus_from.map(|_| (Instant::now(), false));
            // The lifted pane takes the keyboard (its header button may lift a pane
            // other than the focused one).
            if let (Some(id), Some(ws)) = (target.as_deref(), self.state.active_mut())
                && ws.container(id).is_some()
                && ws.focused_container_id.as_deref() != Some(id)
            {
                ws.focused_container_id = Some(id.to_owned());
                self.changed(cx);
            }
            self.focus_mode = target;
        }
        self.focus_pending = true;
        cx.notify();
    }

    /// Advance focus mode's reveal: finish a close, or keep frames coming.
    fn settle_focus_anim(&mut self, window: &mut Window) {
        if self.focus_mode.is_none() {
            self.focus_anim = None;
            self.focus_from = None;
            return;
        }
        if let Some((start, closing)) = self.focus_anim {
            if start.elapsed() >= crate::grid::FOCUS_ANIM {
                self.focus_anim = None;
                if closing {
                    self.focus_mode = None;
                    self.focus_from = None;
                }
            } else {
                window.request_animation_frame();
            }
        }
    }

    // ── Dragging ────────────────────────────────────────────────────────────

    pub(crate) fn on_drag_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.drag {
            None => {}
            Some(Drag::Pane { pos, start, active, .. }) => {
                // Released outside the window: the mouse-up never reached us.
                if ev.pressed_button != Some(MouseButton::Left) {
                    self.drag = None;
                    cx.notify();
                    return;
                }
                *pos = ev.position;
                if !*active && (ev.position - *start).magnitude() > f64::from(PANE_DRAG_THRESHOLD * crate::ui::zoom()) {
                    *active = true;
                }
                if *active {
                    cx.notify();
                }
            }
            Some(Drag::Sidebar) => {
                // The width is kept (and saved) unscaled: it zooms with the rest.
                self.settings.sidebar_width = crate::ui::unpx(ev.position.x).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                cx.notify();
            }
            Some(Drag::Split { path, horizontal }) => {
                let Some(b) = self.split_bounds.borrow().get(path).copied() else {
                    return;
                };
                let (pos, start, len) = if *horizontal { (ev.position.x, b.left(), b.size.width) } else { (ev.position.y, b.top(), b.size.height) };
                if len <= px(0.) {
                    return;
                }
                // SplitGrid's minimum share is 10%.
                let ratio = ((pos - start) / len).clamp(0.1, 0.9);
                let path = path.clone();
                if let Some(split) = self.state.active_mut().and_then(|w| w.layout_tree.as_mut()).and_then(|t| t.split_at_mut(&path)) {
                    split.ratio = ratio;
                    cx.notify();
                }
            }
        }
    }

    /// SPLITGRID_DEBUG_LAYOUT=1: print the UI zoom and the key layout numbers
    /// (last frame's) whenever they change, to check that zoom scales them alike.
    fn debug_log_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("SPLITGRID_DEBUG_LAYOUT").is_none() {
            return;
        }
        let mut out = format!(
            "zoom={:.3} rem={:?} viewport={:?} sidebar_width={} ({:?} on screen) grid={:?}",
            crate::ui::zoom(),
            window.rem_size(),
            window.viewport_size(),
            self.settings.sidebar_width,
            px(self.settings.sidebar_width),
            *self.grid_bounds.borrow()
        );
        let mut ids: Vec<String> = self.pane_bounds.borrow().keys().cloned().collect();
        ids.sort();
        for id in ids {
            let b = self.pane_bounds.borrow().get(&id).copied();
            out.push_str(&format!("\n  pane {id}: {b:?}"));
            if let Some(p) = self.panes.get(&id) {
                let (cols, rows) = p.view.read(cx).grid_size();
                out.push_str(&format!(" terminal {cols}x{rows} font {}", p.view.read(cx).font_size()));
            }
            if let Some(g) = self.browser_panes.get(&id).and_then(|e| e.pane.read(cx).debug_geometry()) {
                out.push_str(&format!(" browser {g}"));
            }
        }
        if self.debug_layout_seen.as_deref() != Some(out.as_str()) {
            eprintln!("[layout-debug] {out}");
            self.debug_layout_seen = Some(out);
        }
    }

    /// The pane under `pos` a dragged pane would swap with (never itself).
    pub(crate) fn pane_drop_target(&self, source: &str, pos: Point<Pixels>) -> Option<String> {
        let tree = self.state.active()?.layout_tree.as_ref()?;
        let bounds = self.pane_bounds.borrow();
        tree.leaf_ids().into_iter().find(|id| id != source && bounds.get(id).is_some_and(|b| b.contains(&pos)))
    }

    pub(crate) fn on_drag_end(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Split { .. }) => self.changed(cx),
            Some(Drag::Pane { id, pos, active: true, .. }) => {
                // Dropped on another workspace in the sidebar: it moves there, running.
                if let Some(ws) = self.workspace_drop_target(&id, pos) {
                    if self.state.move_pane(&id, &ws) {
                        if self.focus_mode.as_deref() == Some(id.as_str()) {
                            self.focus_mode = None;
                        }
                        self.focus_pending = true;
                        self.changed(cx);
                    }
                } else if let Some(target) = self.pane_drop_target(&id, pos)
                    && let Some(tree) = self.state.active_mut().and_then(|w| w.layout_tree.as_mut())
                    && tree.swap_leaves(&id, &target)
                {
                    self.changed(cx);
                }
                cx.notify();
            }
            Some(Drag::Pane { .. }) => {}
            Some(Drag::Sidebar) => {
                self.save_settings();
                cx.notify();
            }
            None => {}
        }
    }
}

fn load_state(store: &Store, env: &str) -> WorkspaceState {
    let mut state = store.load(env).unwrap_or_else(|e| {
        log::error!("loading environment {env}: {e:#}");
        WorkspaceState::default()
    });
    if state.workspaces.is_empty() {
        state.workspaces.push(Workspace::new("Workspace 1", None));
    }
    state.normalize();
    state
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_panes(window, cx);
        self.ensure_proc_poll(cx);
        if self.settings_tab == Some(crate::settings::SettingsTab::General) {
            self.ensure_appearance_controls(window, cx);
        }
        if self.focus_pending && self.renaming.is_none() && self.image_preview.is_none() && self.popover.is_none() && self.editor.is_none() && self.settings_tab.is_none() && self.sync_settings.is_none() && !self.procs.overlay_open() {
            self.focus_pending = false;
            self.focus_active_pane(window, cx);
        }
        self.split_bounds.borrow_mut().clear();
        self.ws_row_bounds.borrow_mut().clear();
        self.settle_focus_anim(window);
        self.settle_layout_anim(window, cx);
        // Web views are native views over the window: which may show this frame.
        self.ensure_browser_panes(window, cx);
        self.present_browser_panes(cx);
        self.debug_log_layout(window, cx);

        let sidebar = self.render_sidebar(window, cx);
        let grid = self.render_grid_area(window, cx);
        // macOS fullscreen has no traffic lights and no window to drag, so
        // no title strip either (WorkspaceBar hides itself the same way).
        let title = (!window.is_fullscreen()).then(|| self.render_title_strip(cx));
        let status = self.render_status_bar(cx);
        let popover = self.popover.clone().map(|p| self.render_popover(p, window, cx));
        let collapsed_preview = self.render_collapsed_preview(window, cx);
        let picker = self.env_picker_open.then(|| self.render_env_picker(window, cx));
        let settings = self.render_settings(window, cx);
        let editor = self.render_editor(window, cx);
        let sync_settings = self.render_sync_settings(window, cx);
        let dragging = matches!(self.drag, Some(Drag::Split { .. } | Drag::Sidebar | Drag::Pane { active: true, .. }));
        let pane_drag = self.render_pane_drag(cx);
        let procs_overlay = self.render_procs_overlay(window, cx);
        let image_preview = self.render_image_preview(window, cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_primary())
            .text_color(TEXT_PRIMARY)
            .font_family(crate::theme::ui_font())
            .text_size(px(13.))
            .when(dragging, |d| d.cursor_grabbing())
            .track_focus(&self.root_focus)
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| this.split(Direction::Horizontal, None, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| this.split(Direction::Vertical, None, cx)))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| {
                if let Some(id) = this.state.active().and_then(|w| w.focused_container_id.clone()) {
                    this.close_pane(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewWorkspace, window, cx| this.new_workspace(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &NextWorkspace, window, cx| this.cycle_workspace(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevWorkspace, window, cx| this.cycle_workspace(-1, window, cx)))
            .on_action(cx.listener(|this, _: &ToggleFocusMode, _, cx| this.toggle_focus_mode(None, cx)))
            .on_action(cx.listener(|this, _: &OpenEnvironments, _, cx| this.toggle_env_picker(false, cx)))
            .on_action(cx.listener(|this, _: &NewWindow, _, cx| this.toggle_env_picker(true, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_focused(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_focused(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom_focused(None, cx)))
            // The View menu's Zoom items own ⌘+ / ⌘− / ⌘0 as macOS key equivalents, so
            // the key arrives as UiZoom* even inside a pane: there it is the pane's font
            // size (as in SplitGrid); elsewhere it bubbles to the app's window zoom.
            .on_action(cx.listener(|this, _: &UiZoomIn, window, cx| this.pane_or_ui_zoom(Some(1.), window, cx)))
            .on_action(cx.listener(|this, _: &UiZoomOut, window, cx| this.pane_or_ui_zoom(Some(-1.), window, cx)))
            .on_action(cx.listener(|this, _: &UiZoomReset, window, cx| this.pane_or_ui_zoom(None, window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(crate::settings::SettingsTab::Ssh, cx)))
            .on_action(cx.listener(|this, _: &Quit, window, cx| this.confirm_quit(window, cx)))
            .children(title)
            .child(div().flex_1().min_h_0().flex().child(sidebar).child(grid))
            .child(status)
            .children(popover)
            .children(collapsed_preview)
            .children(picker)
            .children(settings)
            .children(editor)
            .children(sync_settings)
            .children(pane_drag)
            .children(procs_overlay)
            .children(image_preview)
    }
}
