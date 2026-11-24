//! SplitGrid (native): workspaces of terminal grids, on gpui.

mod agent;
mod anim;
mod app;
mod app_settings;
mod assets;
mod background;
mod browser;
mod chrome;
mod collapsed;
mod commands;
mod grid;
mod history;
mod ide;
mod layout_anim;
mod model;
mod overlay;
mod procs;
mod prompts;
mod ptyd;
mod secrets;
mod settings;
mod sftp;
mod sftp_format;
mod sftp_pane;
mod sftp_sync;
mod sidebar;
mod single_instance;
mod sql_pane;
mod ssh;
mod ssh_prompt;
mod ssh_ui;
mod sync_settings;
mod terminal;
mod theme;
mod ui;
mod windows;

use gpui_kit::*;

use app::AppView;
use model::store::{Store, app_support_dir};

/// env_logger, minus known-harmless gpui messages: an activation change
/// arriving for a window that is being opened or has just closed (logged with
/// an empty target, so a per-module filter can't catch it), and the fonts that
/// fail to load while Settings → Appearance probes every installed family once
/// (`theme::FONT_PROBE`).
fn init_logging() {
    struct Quiet(env_logger::Logger);
    impl log::Log for Quiet {
        fn enabled(&self, m: &log::Metadata) -> bool {
            self.0.enabled(m)
        }
        fn log(&self, r: &log::Record) {
            if r.target().is_empty() && r.args().to_string().starts_with("handling active status change") {
                return;
            }
            if theme::FONT_PROBE.load(std::sync::atomic::Ordering::Relaxed) && r.target().contains("text_system") {
                return;
            }
            self.0.log(r)
        }
        fn flush(&self) {
            self.0.flush()
        }
    }
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build();
    log::set_max_level(inner.filter());
    let _ = log::set_boxed_logger(Box::new(Quiet(inner)));
}

fn main() {
    agent::scrub_inherited_env();
    init_logging();

    // One data directory with the Electron app: the same workspaces,
    // environments, connections, host keys and credential key.
    let data_dir = app_support_dir("SplitGrid");
    // One Lite per data dir: a second launch hands over to the first and exits.
    let _instance = match single_instance::acquire(&data_dir) {
        Ok(lock) => lock,
        Err(pid) => {
            log::info!("SplitGrid Lite already runs (pid {pid:?}): bringing it forward");
            if let Some(pid) = pid {
                single_instance::activate(pid);
            }
            std::process::exit(0);
        }
    };
    let store = match Store::open(data_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open the data directory: {e:#}");
            std::process::exit(1);
        }
    };

    // Persistent terminals: sessions under the data dir; install the daemon and
    // bring up the menu-bar helper when it is in use.
    ptyd::init(&data_dir);
    std::thread::spawn(ptyd::startup);

    // The agent hooks and skills Agent integrations ask for (off: nothing is touched).
    let installer = agent::install::Installer::new(&data_dir);
    std::thread::spawn(move || installer.startup());

    let connections = match model::connections::ConnectionStore::open(&data_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot open saved connections: {e:#}");
            std::process::exit(1);
        }
    };

    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        // Settings → Appearance: the background colour and fonts, before anything draws.
        let appearance = store.app_settings();
        theme::set_palette(background::derive(Some(&background::from_settings(&appearance))));
        theme::set_fonts(app_settings::fonts(&appearance));
        theme::apply_to_components(cx);
        // Whole-window UI zoom (⌘+ / ⌘− / ⌘0 outside the panes), as last saved.
        // SPLITGRID_DEBUG_UI_ZOOM=<factor> overrides it for this run (not saved).
        let level = match std::env::var("SPLITGRID_DEBUG_UI_ZOOM").ok().and_then(|v| v.parse::<f32>().ok()).filter(|f| *f > 0.) {
            Some(f) => f.ln() / 1.2f32.ln(),
            None => store.settings().ui_zoom_level,
        };
        ui::apply(level, cx);
        app::bind_keys(cx);
        ide::bind_keys(cx);
        sql_pane::bind_keys(cx);
        browser::bind_keys(cx);
        // The focus-mode shortcut the user picked (Settings → Shortcuts), over the default.
        app_settings::apply_focus_hotkey(&store.app_settings(), cx);
        // sg-browser's console/network hooks: before any pane exists, so every page gets them.
        browser::bridge::init(cx);
        cx.set_menus(vec![
            Menu { name: "SplitGrid".into(), items: vec![MenuItem::action("Quit SplitGrid", app::Quit)], disabled: false },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Window", app::NewWindow),
                    MenuItem::separator(),
                    MenuItem::action("Open Env…", app::OpenEnvironments),
                    MenuItem::action("Settings…", app::OpenSettings),
                ],
                disabled: false,
            },
            // Electron's default View menu: whole-window zoom.
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Actual Size", app::UiZoomReset),
                    MenuItem::action("Zoom In", app::UiZoomIn),
                    MenuItem::action("Zoom Out", app::UiZoomOut),
                ],
                disabled: false,
            },
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // The receiver agents in our terminals talk to (sg-terminal, hooks); one for all windows.
        let ext = std::sync::Arc::new(agent::ext_channel::Channel::new(&data_dir));
        let (agent, requests) = match agent::server::start(Some(ext.clone())) {
            Ok((port, requests)) => (Some(std::rc::Rc::new(agent::AgentConfig::new(port, &data_dir).with_ext(ext))), Some(requests)),
            Err(e) => {
                log::error!("agent bridge: {e}");
                (None, None)
            }
        };
        let shared = windows::Shared { store, connections: std::sync::Arc::new(std::sync::Mutex::new(connections)), agent };
        // Workspace history: one service for the process, resumed only when
        // consent was given earlier (app-settings.json historyEnabled).
        history::init(&shared.store, cx);
        windows::init(shared, requests, cx);
        // Agent notifications: the click handler, and permission when they are on.
        let all = cx.global::<windows::Shared>().store.app_settings();
        agent::notify::init(all.get("systemNotifications").and_then(serde_json::Value::as_bool) != Some(false), cx);
        // ⌘⇧N / File → New Window: a window asks which environment to open
        // (AppView handles it). Reaching here means no window is open: reopen the last one.
        cx.on_action(|_: &app::NewWindow, cx| {
            let env = cx.global::<windows::Shared>().store.settings().active_environment;
            windows::open_env(Some(env), cx);
        });
        let options = windows::options(cx);
        open_window(options, cx, |window, cx| cx.new(|cx| AppView::new(None, window, cx))).expect("failed to open the window");
        // Development aid for screenshots: SPLITGRID_DEBUG_IDE=<folder> opens an IDE pane on it.
        if let Some(dir) = std::env::var_os("SPLITGRID_DEBUG_IDE") {
            let opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1100.), px(720.)), cx))),
                titlebar: Some(TitlebarOptions { title: Some("IDE".into()), ..Default::default() }),
                ..Default::default()
            };
            let _ = open_window(opts, cx, |window, cx| {
                // …and SPLITGRID_DEBUG_IDE_FILE=<file> opens a file in it.
                let mut state = ide::IdeState::default();
                if let Ok(file) = std::env::var("SPLITGRID_DEBUG_IDE_FILE") {
                    state.tabs.push(ide::TabState { file_path: file, is_preview: false });
                }
                cx.new(|cx| ide::IdePane::new(std::path::PathBuf::from(dir), state, 13., window, cx))
            });
        }
        cx.activate(true);
    });
}
