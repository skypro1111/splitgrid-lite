//! Workspace history in this app (the Electron app's history wiring in
//! ipc-handlers.ts): the one [`HistoryService`] of the process, the last status
//! it reported, and the config it is built with.
//!
//! Consent is `historyEnabled` in the shared `app-settings.json` (decision
//! notes: `historyDistill`); only Settings → Workspace history writes them, and
//! only once the service call they stand for succeeded. At startup the service
//! is resumed when consent was given earlier (in either app); otherwise it is
//! built and left alone, which does nothing.
//!
//! Every service method may block (disk, git, llama-server), so the UI calls
//! them through [`blocking`], on a thread of their own.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui_kit::{App, Global};
use serde_json::{Map, Value};
use sg_history::{HistoryConfig, HistoryService, HistoryStatus, IndexedWorkspace};

use crate::model::store::Store;

/// Status pushes ask for the workspace list every second while indexing, and
/// it is read from every environment's state file, so it is cached briefly.
const WORKSPACES_TTL: Duration = Duration::from_secs(5);

/// The process's history service and what it last reported.
pub struct Hub {
    pub service: HistoryService,
    pub status: Option<HistoryStatus>,
}

impl Global for Hub {}

/// Bumped by every disable: an enable still installing when the user turns the
/// feature off must not persist consent once it finishes (Electron's historyEpoch).
static EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn epoch() -> u64 {
    EPOCH.load(Ordering::SeqCst)
}

pub fn bump_epoch() {
    EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// (historyEnabled, historyDistill) as `app-settings.json` has them.
pub fn flags(all: &Map<String, Value>) -> (bool, bool) {
    let on = |k: &str| all.get(k).and_then(Value::as_bool).unwrap_or(false);
    (on("historyEnabled"), on("historyDistill"))
}

/// Where Claude Code keeps its transcripts: `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`.
pub fn projects_dir(claude_config_dir: Option<&str>, home: &Path) -> PathBuf {
    match claude_config_dir.map(str::trim) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("projects"),
        _ => home.join(".claude/projects"),
    }
}

/// AppSettings.claudeBinaryPath; unset or blank means "find `claude`".
pub fn claude_binary(all: &Map<String, Value>) -> Option<PathBuf> {
    all.get("claudeBinaryPath").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Every environment's workspaces, not just the open windows': sessions of a
/// workspace in another environment are indexed too.
pub fn workspaces_in(store: &Store) -> Vec<IndexedWorkspace> {
    store
        .list_envs()
        .into_iter()
        .filter_map(|env| store.load(&env.id).ok())
        .flat_map(|state| state.workspaces)
        .map(|w| IndexedWorkspace {
            id: w.id,
            working_directory: w.working_directory.filter(|d| !d.trim().is_empty()).map(|d| PathBuf::from(crate::grid::expand_tilde(&d))),
        })
        .collect()
}

fn cached_workspaces(store: Store) -> impl Fn() -> Vec<IndexedWorkspace> + Send + Sync {
    let cache: Mutex<Option<(Instant, Vec<IndexedWorkspace>)>> = Mutex::new(None);
    move || {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < WORKSPACES_TTL
        {
            return list.clone();
        }
        let list = workspaces_in(&store);
        *cache = Some((Instant::now(), list.clone()));
        list
    }
}

pub fn config(store: &Store) -> HistoryConfig {
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let settings = store.clone();
    HistoryConfig {
        root_dir: store.dir().join("history"),
        projects_dir: projects_dir(std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(), &home),
        workspaces: Arc::new(cached_workspaces(store.clone())),
        claude_binary: Arc::new(move || claude_binary(&settings.app_settings())),
        log: Arc::new(|line| log::info!("[history] {line}")),
    }
}

enum Update {
    /// The first status, read once at startup (a push may already be newer).
    Seed(HistoryStatus),
    Push(HistoryStatus),
}

/// Build the process's service; resume it when consent was given in an earlier run.
pub fn init(store: &Store, cx: &mut App) {
    let service = HistoryService::new(config(store));
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Update>();
    let push = tx.clone();
    service.on_status(Box::new(move |s| {
        let _ = push.unbounded_send(Update::Push(s.clone()));
    }));
    cx.set_global(Hub { service: service.clone(), status: None });
    // Stop llama-server and the loop on quit (consent stays as it was).
    let on_quit = service.clone();
    cx.on_app_quit(move |_| {
        on_quit.dispose();
        async {}
    })
    .detach();
    let _ = SERVICE.set(service.clone());
    cx.spawn(async move |cx| {
        while let Some(update) = rx.next().await {
            cx.update(|cx| {
                let hub = cx.global_mut::<Hub>();
                match update {
                    Update::Seed(s) if hub.status.is_none() => hub.status = Some(s),
                    Update::Seed(_) => return,
                    Update::Push(s) => hub.status = Some(s),
                }
                cx.refresh_windows();
            });
        }
    })
    .detach();

    let (enabled, distill) = flags(&store.app_settings());
    let spawned = std::thread::Builder::new().name("history-startup".into()).spawn(move || {
        // Consent given in an earlier run (in either app): bring it back up.
        // Without it, nothing.
        if enabled {
            service.resume_if_enabled(true, distill);
        }
        let _ = tx.unbounded_send(Update::Seed(service.status()));
    });
    if let Err(e) = spawned {
        log::error!("history: starting: {e}");
    }
}

/// The service, when it was built.
pub fn service(cx: &App) -> Option<HistoryService> {
    cx.try_global::<Hub>().map(|h| h.service.clone())
}

pub fn status(cx: &App) -> Option<&HistoryStatus> {
    cx.try_global::<Hub>().and_then(|h| h.status.as_ref())
}

/// Run a blocking service call on a thread of its own. None when it panicked
/// (or the thread could not start), so a caller never waits forever.
pub fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl Future<Output = Option<T>> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new().name("history-call".into()).spawn(move || {
        let _ = tx.send(f());
    });
    if let Err(e) = spawned {
        log::error!("history: starting a call: {e}");
    }
    async move { rx.await.ok() }
}

static SERVICE: std::sync::OnceLock<HistoryService> = std::sync::OnceLock::new();
static WORKSPACES_SEEN: Mutex<Option<std::collections::HashMap<String, u64>>> = Mutex::new(None);

/// After a window saved its environment's workspaces: when their ids or folders changed,
/// let the indexer look for the new folders' sessions (a no-op while off).
pub fn workspaces_saved(env_id: &str, state: &crate::model::workspace::WorkspaceState) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for w in &state.workspaces {
        w.id.hash(&mut h);
        w.working_directory.hash(&mut h);
    }
    let sig = h.finish();
    let mut seen = WORKSPACES_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_default().insert(env_id.to_owned(), sig).is_some_and(|old| old != sig)
        && let Some(service) = SERVICE.get()
    {
        service.notify_workspaces_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_dir_follows_claude_config_dir() {
        let home = Path::new("/home/me");
        assert_eq!(projects_dir(None, home), PathBuf::from("/home/me/.claude/projects"));
        assert_eq!(projects_dir(Some("  "), home), PathBuf::from("/home/me/.claude/projects"));
        assert_eq!(projects_dir(Some("/cfg/claude"), home), PathBuf::from("/cfg/claude/projects"));
    }

    #[test]
    fn flags_and_binary_come_from_app_settings() {
        let all: Map<String, Value> = serde_json::from_value(json!({ "historyEnabled": true, "claudeBinaryPath": " " })).unwrap();
        assert_eq!(flags(&all), (true, false));
        assert_eq!(claude_binary(&all), None);
        let all: Map<String, Value> = serde_json::from_value(json!({ "historyDistill": true, "claudeBinaryPath": "/opt/claude" })).unwrap();
        assert_eq!(flags(&all), (false, true));
        assert_eq!(claude_binary(&all), Some(PathBuf::from("/opt/claude")));
        assert_eq!(flags(&Map::new()), (false, false));
    }

    #[test]
    fn workspaces_span_every_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        let state = |ws: Value| serde_json::from_value::<crate::model::workspace::WorkspaceState>(json!({ "workspaces": ws })).unwrap();
        store.save("default", &state(json!([{ "id": "w1", "name": "One", "workingDirectory": "/src/one" }]))).unwrap();
        let other = store.create_env("Other").unwrap();
        store.save(&other, &state(json!([{ "id": "w2", "name": "Two" }, { "id": "w3", "name": "Three", "workingDirectory": "" }]))).unwrap();

        let mut list = workspaces_in(&store);
        list.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            list,
            vec![
                IndexedWorkspace { id: "w1".into(), working_directory: Some(PathBuf::from("/src/one")) },
                IndexedWorkspace { id: "w2".into(), working_directory: None },
                IndexedWorkspace { id: "w3".into(), working_directory: None },
            ]
        );

        // Cached for a while: a workspace added meanwhile shows up only after it expires.
        let cached = cached_workspaces(store.clone());
        assert_eq!(cached().len(), 3);
        store.save("default", &state(json!([{ "id": "w1", "name": "One" }, { "id": "w4", "name": "Four" }]))).unwrap();
        assert_eq!(cached().len(), 3);
    }

    #[test]
    fn blocking_survives_a_panicking_call() {
        assert_eq!(futures::executor::block_on(blocking(|| 7)), Some(7));
        let none: Option<()> = futures::executor::block_on(blocking(|| panic!("boom")));
        assert_eq!(none, None);
    }
}

