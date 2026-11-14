//! SplitGrid (native): workspaces of terminal grids, on gpui.

mod agent;
mod anim;
mod app;
mod app_settings;
mod assets;
mod browser;
mod chrome;
mod grid;
mod ide;
mod layout_anim;
mod model;
mod secrets;
mod settings;
mod sftp;
mod sftp_format;
mod sftp_pane;
mod sftp_sync;
mod ssh;
mod ssh_prompt;
mod ssh_ui;
mod sidebar;
mod sql_pane;
mod terminal;
mod theme;
mod ui;
mod windows;

use gpui_kit::*;

use app::AppView;
use model::store::{Store, app_support_dir};

/// env_logger, minus one known-harmless gpui error: an activation change
/// arriving for a window that is being opened or has just closed. gpui logs it
/// with an empty target, so a per-module filter can't catch it.
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
    let store = match Store::open(data_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open the data directory: {e:#}");
            std::process::exit(1);
        }
    };

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
        let (agent, requests) = match agent::server::start() {
            Ok((port, requests)) => (Some(std::rc::Rc::new(agent::AgentConfig::new(port, &data_dir))), Some(requests)),
            Err(e) => {
                log::error!("agent bridge: {e}");
                (None, None)
            }
        };
        let shared = windows::Shared { store, connections: std::sync::Arc::new(std::sync::Mutex::new(connections)), agent };
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
