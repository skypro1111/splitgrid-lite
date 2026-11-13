//! One window per environment (as in the Electron app): what the windows
//! share, which environment each one shows, and opening a new one.
//!
//! Two windows on the same environment would overwrite each other's state
//! file, so opening an environment that is already open brings its window
//! forward instead.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::*;
use serde_json::json;

use crate::agent::AgentConfig;
use crate::agent::server::Request;
use crate::app::AppView;
use crate::model::connections::ConnectionStore;
use crate::model::store::Store;

/// What every window shares: the data directory, the saved connections, the agent bridge.
#[derive(Clone)]
pub struct Shared {
    pub store: Store,
    pub connections: Arc<Mutex<ConnectionStore>>,
    pub agent: Option<Rc<AgentConfig>>,
}

impl Global for Shared {}

/// The open windows and the environment each shows. Any handle: the first
/// window's root is gpui-kit's `Root` wrapping the AppView (see [`app_view`]).
#[derive(Default)]
struct EnvWindows(Vec<(AnyWindowHandle, String)>);

impl Global for EnvWindows {}

pub fn init(shared: Shared, requests: Option<UnboundedReceiver<Request>>, cx: &mut App) {
    cx.set_global(shared);
    cx.set_global(EnvWindows::default());
    // Agents' requests go to the window that holds the calling terminal.
    if let Some(mut requests) = requests {
        cx.spawn(async move |cx| {
            while let Some(req) = requests.next().await {
                cx.update(|cx| dispatch(req, cx));
            }
        })
        .detach();
    }
}

fn dispatch(req: Request, cx: &mut App) {
    let terminal = req.body["terminal"].as_str().or_else(|| req.query("terminal")).map(str::to_owned).unwrap_or_default();
    let handles: Vec<AnyWindowHandle> = cx.global::<EnvWindows>().0.iter().map(|(h, _)| *h).collect();
    let owner = handles.into_iter().find_map(|h| app_view(h, cx).filter(|v| v.read(cx).owns_terminal(&terminal)).map(|v| (h, v)));
    match owner {
        Some((handle, view)) => {
            let _ = handle.update(cx, |_, window, cx| view.update(cx, |view, cx| view.handle_agent_request(req, window, cx)));
        }
        // Hooks from a terminal that's gone: nothing to do. Commands: say so.
        None if req.kind == "hook" => req.respond(json!({})),
        None => req.respond(json!({ "ok": false, "error": "unknown_terminal", "message": "cannot locate the calling terminal's workspace" })),
    }
}

/// The AppView a window shows: its root, or the root inside gpui-kit's `Root`.
fn app_view(handle: AnyWindowHandle, cx: &mut App) -> Option<Entity<AppView>> {
    let root = handle.update(cx, |root, _, _| root).ok()?;
    if let Ok(view) = root.clone().downcast::<AppView>() {
        return Some(view);
    }
    root.downcast::<gpui_kit::base::Root>().ok()?.read(cx).view().clone().downcast::<AppView>().ok()
}

/// The window showing `env`, if any.
pub fn window_for(env: &str, cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<EnvWindows>()?.0.iter().find(|(_, e)| e == env).map(|(h, _)| *h)
}

/// Environments open in windows other than `except`.
pub fn open_elsewhere(except: AnyWindowHandle, cx: &App) -> Vec<String> {
    cx.try_global::<EnvWindows>()
        .map(|w| w.0.iter().filter(|(h, _)| *h != except).map(|(_, e)| e.clone()).collect())
        .unwrap_or_default()
}

/// Record which environment a window shows (on open and on every switch).
pub fn set_env(handle: AnyWindowHandle, env: &str, cx: &mut App) {
    let list = &mut cx.global_mut::<EnvWindows>().0;
    match list.iter_mut().find(|(h, _)| *h == handle) {
        Some(entry) => entry.1 = env.to_owned(),
        None => list.push((handle, env.to_owned())),
    }
}

pub fn forget(handle: AnyWindowHandle, cx: &mut App) {
    if let Some(w) = cx.try_global::<EnvWindows>().is_some().then(|| cx.global_mut::<EnvWindows>()) {
        w.0.retain(|(h, _)| *h != handle);
    }
}

pub fn options(cx: &App) -> WindowOptions {
    // Each new window a little down and right of the last, like macOS cascades.
    let n = cx.try_global::<EnvWindows>().map_or(0, |w| w.0.len()) as f32;
    let mut bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    bounds.origin += point(px(28. * n), px(28. * n));
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Like Electron's `hiddenInset`: the traffic lights sit inside our 36px title strip.
        titlebar: Some(TitlebarOptions {
            title: Some("SplitGrid".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(14.), px(12.))),
        }),
        window_min_size: Some(size(px(900.), px(600.))),
        ..Default::default()
    }
}

/// Show `env` in its own window: bring its window forward when it has one,
/// else open one. `None` creates a new environment for the new window.
pub fn open_env(env: Option<String>, cx: &mut App) {
    if let Some(handle) = env.as_deref().and_then(|e| window_for(e, cx)) {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }
    let env = match env {
        Some(e) => e,
        None => {
            let store = &cx.global::<Shared>().store;
            let name = format!("Environment {}", store.list_envs().len() + 1);
            match store.create_env(&name) {
                Ok(id) => id,
                Err(e) => {
                    log::error!("creating an environment: {e:#}");
                    return;
                }
            }
        }
    };
    if let Err(e) = cx.open_window(options(cx), |window, cx| cx.new(|cx| AppView::new(Some(env), window, cx))) {
        log::error!("opening a window: {e:#}");
    }
}

/// A clicked agent notification: bring the app and its environment's window
/// forward (reopening it if it was closed), then show the workspace and terminal.
pub fn reveal_terminal(target: crate::agent::notify::Target, cx: &mut App) {
    cx.activate(true);
    if window_for(&target.env, cx).is_none() {
        if !cx.global::<Shared>().store.list_envs().iter().any(|e| e.id == target.env) {
            return;
        }
        open_env(Some(target.env.clone()), cx);
    }
    let Some(handle) = window_for(&target.env, cx) else { return };
    let Some(view) = app_view(handle, cx) else { return };
    let _ = handle.update(cx, |_, window, cx| {
        window.activate_window();
        view.update(cx, |view, cx| view.reveal_terminal(&target.workspace, &target.terminal, window, cx));
    });
}
