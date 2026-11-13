//! Workspace SFTP sync — the Electron app's `sync:run-now`, `sftp:push-paths`,
//! `sftp:pull-paths` and `sync:watch-workspace` (ipc-handlers.ts +
//! sftp-sync-manager.ts), over the same `sync` field of the workspace state, so
//! a config made in the Electron app works here unchanged.
//!
//! The semantics are Electron's, deliberately including what it does NOT do:
//! - **Sync now** uploads every local file under the working directory to every
//!   enabled target (`<remotePath>/<relative path>`). It never deletes, never
//!   downloads and does not compare trees: the local copy always wins.
//! - **Push** uploads the given workspace paths to every enabled target;
//!   **pull** downloads them from the FIRST enabled target, overwriting local
//!   files (the remote wins). No conflict detection either way.
//! - **Auto sync** (sync enabled + a target): a file watcher mirrors local
//!   changes — created/changed files are uploaded, created folders made,
//!   deleted paths deleted on every target. Nothing already there at start is
//!   uploaded (that's what Sync now is for).
//! - Excluded everywhere: `.git`; `.splitgrid` on Sync now; `.gitignore` (the
//!   root one) when `useGitIgnore` is on; the watcher also never looks inside
//!   `node_modules`, `__pycache__`, `target`, `.DS_Store`.
//!
//! Planning (what to transfer) is pure — [`plan_walk`], [`exclusion`],
//! [`remote_file`] — and tested without a server; the transfers run on the SSH
//! runtime over a small per-(workspace, target) session pool that reconnects
//! once on a connection error, as Electron's `withRetry` does.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::model::connections::{AuthMethod, ConnectionStore};
use crate::model::workspace::Workspace;
use crate::sftp::SftpClient;
use crate::ssh::{HostKeyChecker, SshAuth, SshParams};

/// Parallel transfers per batch (Electron's SFTP_CONCURRENCY).
pub const CONCURRENCY: usize = 10;
/// Sync-now log entries kept on the workspace.
const MAX_LOGS: usize = 400;
/// Quiet time after the last file event before the watcher uploads.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(700);
/// Never watched (Electron's ALWAYS_IGNORE_SEGMENTS).
pub const ALWAYS_IGNORE_SEGMENTS: [&str; 5] = [".git", "node_modules", "__pycache__", "target", ".DS_Store"];

pub type Connections = Arc<Mutex<ConnectionStore>>;

// ─── Config (workspace.sync) ────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub use_git_ignore: bool,
    #[serde(default)]
    pub targets: Vec<SyncTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<Value>>,
    /// fileStates and anything newer, kept as found.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncTarget {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub remote_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_error: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SyncTarget {
    /// Enabled, with a connection and a remote root: what transfers go to.
    pub fn is_active(&self) -> bool {
        self.enabled && self.connection_id.as_deref().is_some_and(|c| !c.is_empty()) && !self.remote_path.trim().is_empty()
    }

    /// The remote root without trailing slashes.
    pub fn remote_root(&self) -> String {
        let r = self.remote_path.replace('\\', "/");
        let t = r.trim_end_matches('/');
        if t.is_empty() && r.starts_with('/') { "/".into() } else { t.to_owned() }
    }
}

impl SyncConfig {
    pub fn of(ws: &Workspace) -> Option<SyncConfig> {
        ws.extra.get("sync").and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn store(&self, ws: &mut Workspace) {
        if let Ok(v) = serde_json::to_value(self) {
            ws.extra.insert("sync".into(), v);
        }
    }

    /// Sync is on and some target is enabled (what push/pull/sync need).
    pub fn configured(&self) -> bool {
        self.enabled && self.targets.iter().any(|t| t.enabled)
    }

    /// The auto-sync watcher runs for this config (Electron's `eligible`).
    pub fn watchable(&self) -> bool {
        self.enabled && self.targets.iter().any(|t| t.enabled && t.connection_id.as_deref().is_some_and(|c| !c.is_empty()))
    }

    /// Record a Sync now result the way the Electron settings card does:
    /// per-target last sync, plus one `sync-now` log line (newest last, 400 kept).
    pub fn record_sync_now(&mut self, root: &str, result: &Value, now: i64) {
        let results = result["targetResults"].as_array().cloned().unwrap_or_default();
        for r in &results {
            let Some(t) = self.targets.iter_mut().find(|t| Some(t.id.as_str()) == r["targetId"].as_str()) else { continue };
            let ok = r["ok"].as_bool().unwrap_or(false);
            t.last_sync_at = Some(now);
            t.last_sync_status = Some(if ok { "success" } else { "error" }.into());
            t.last_sync_error = (!ok).then(|| r["error"].as_str().unwrap_or("Sync failed").to_owned());
        }
        let logs = self.logs.get_or_insert_with(Vec::new);
        logs.push(json!({
            "id": format!("{now}-{}", crate::model::workspace::new_id("l").trim_start_matches("l-")),
            "at": now,
            "action": "sync-now",
            "filePath": root,
            "ok": results.iter().all(|r| r["ok"].as_bool() == Some(true)),
            "message": summary(result),
        }));
        if logs.len() > MAX_LOGS {
            let drop = logs.len() - MAX_LOGS;
            logs.drain(..drop);
        }
    }
}

/// "scanned 12, uploaded 12, skipped 3" (+ " (cancelled)").
pub fn summary(result: &Value) -> String {
    let n = |k: &str| result[k].as_u64().unwrap_or(0);
    format!(
        "scanned {}, uploaded {}, skipped {}{}",
        n("scanned"),
        n("uploaded"),
        n("skippedByGitIgnore"),
        if result["cancelled"].as_bool() == Some(true) { " (cancelled)" } else { "" }
    )
}

// ─── Planning (pure) ────────────────────────────────────────────────────────

/// The root `.gitignore`, when the config honours it (None without the file).
pub struct Matcher(ignore::gitignore::Gitignore);

impl Matcher {
    pub fn load(root: &Path, use_git_ignore: bool) -> Option<Matcher> {
        if !use_git_ignore {
            return None;
        }
        let text = std::fs::read_to_string(root.join(".gitignore")).ok()?;
        Self::from_rules(root, &text)
    }

    pub fn from_rules(root: &Path, rules: &str) -> Option<Matcher> {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        for line in rules.lines() {
            let _ = b.add_line(None, line);
        }
        b.build().ok().map(Matcher)
    }

    /// `rel` is the POSIX path under the root.
    pub fn ignores(&self, rel: &str, is_dir: bool) -> bool {
        !rel.is_empty() && self.0.matched_path_or_any_parents(rel, is_dir).is_ignore()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// `.git` and friends: never synced, not counted.
    Builtin,
    /// Matched `.gitignore`: counted as skipped.
    Ignored,
}

/// Whether entry `name` at `rel` stays out of a sync (Electron's makeSyncExcluder,
/// plus the walk's own `extra` names). The root (`rel` empty) never does.
pub fn exclusion(name: &str, rel: &str, is_dir: bool, matcher: Option<&Matcher>, extra: &[&str]) -> Option<Skip> {
    if name == ".git" || extra.contains(&name) {
        return Some(Skip::Builtin);
    }
    if matcher.is_some_and(|m| m.ignores(rel, is_dir)) {
        return Some(Skip::Ignored);
    }
    None
}

/// What a walk found to transfer: POSIX paths relative to the root.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub files: Vec<String>,
    pub skipped: usize,
}

/// Walk from `start` (a relative POSIX dir, "" = root) with `list(dir) -> [(name, is_dir)]`
/// (regular files and real folders only), skipping excluded entries without
/// descending into them.
pub fn plan_walk(start: &str, list: &mut dyn FnMut(&str) -> Vec<(String, bool)>, matcher: Option<&Matcher>, extra: &[&str]) -> Plan {
    let mut plan = Plan::default();
    let mut stack = vec![start.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries = list(&dir);
        entries.sort();
        for (name, is_dir) in entries.into_iter().rev() {
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            match exclusion(&name, &rel, is_dir, matcher, extra) {
                Some(Skip::Ignored) => plan.skipped += 1,
                Some(Skip::Builtin) => {}
                None if is_dir => stack.push(rel),
                None => plan.files.push(rel),
            }
        }
    }
    plan
}

/// The local lister for [`plan_walk`]: regular files and real folders (symlinks are skipped, as in Electron).
pub fn local_lister(root: &Path) -> impl FnMut(&str) -> Vec<(String, bool)> + '_ {
    move |rel: &str| {
        let dir = if rel.is_empty() { root.to_owned() } else { root.join(rel) };
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        rd.flatten()
            .filter_map(|e| {
                let ft = e.file_type().ok()?;
                (ft.is_dir() || ft.is_file()).then(|| (e.file_name().to_string_lossy().into_owned(), ft.is_dir()))
            })
            .collect()
    }
}

/// `<root>/<rel>` with no doubled slashes.
pub fn remote_file(root: &str, rel: &str) -> String {
    let joined = format!("{}/{}", root.trim_end_matches('/'), rel);
    let mut out = String::with_capacity(joined.len());
    for ch in joined.chars() {
        if ch == '/' && out.ends_with('/') {
            continue;
        }
        out.push(ch);
    }
    out
}

/// A local path's POSIX path under `root` (None outside it; "" for the root).
pub fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.iter().any(|p| p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

/// A name a server sent that must not become a local path component.
fn unsafe_name(name: &str) -> bool {
    name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') || name.contains('\0')
}

// ─── Connections + session pool ─────────────────────────────────────────────

/// How to reach one saved connection.
#[derive(Clone)]
pub struct ConnSpec {
    pub params: SshParams,
    pub check: HostKeyChecker,
    signature: String,
}

/// Resolve a saved connection (its secrets opened here, never handed to a shell).
/// A password connection without a saved password fails clearly instead of
/// prompting: these runs are unattended, as in the Electron app.
pub fn conn_spec(store: &Connections, id: &str) -> Result<ConnSpec, String> {
    let mut s = store.lock().unwrap();
    s.sync_from_disk();
    let c = s.get(id).cloned().ok_or_else(|| "Saved SSH connection not found.".to_owned())?;
    let auth = match c.auth_method {
        AuthMethod::Password => SshAuth::Password(s.password(&c).filter(|p| !p.is_empty()).ok_or_else(|| {
            format!("No saved password for \"{}\" ({}). Save it in the connection's settings in SplitGrid, then retry.", c.label, c.address())
        })?),
        AuthMethod::PrivateKey => SshAuth::Key {
            path: PathBuf::from(crate::grid::expand_tilde(c.private_key_path.as_deref().unwrap_or("~/.ssh/id_rsa"))),
            passphrase: s.passphrase(&c),
        },
    };
    drop(s);
    let signature = format!(
        "{}|{}|{}|{:?}|{}",
        c.host,
        c.port,
        c.username,
        c.auth_method,
        c.private_key_path.clone().unwrap_or_default()
    );
    let (host, port, st) = (c.host.clone(), c.port, store.clone());
    let check: HostKeyChecker = Arc::new(move |fp: &str| st.lock().unwrap().check_host_key(&host, port, fp));
    Ok(ConnSpec { params: SshParams { host: c.host, port: c.port, username: c.username, auth, offer_password: None }, check, signature })
}

type Slot = Arc<tokio::sync::Mutex<Option<(String, Arc<SftpClient>)>>>;

fn pool() -> &'static Mutex<HashMap<String, Slot>> {
    static POOL: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    POOL.get_or_init(Default::default)
}

/// The cached session for `key` (opened, or reopened when dead or the connection changed).
pub async fn session(key: &str, spec: &ConnSpec) -> Result<Arc<SftpClient>> {
    let slot = pool().lock().unwrap().entry(key.to_owned()).or_default().clone();
    let mut guard = slot.lock().await;
    if let Some((sig, client)) = guard.as_ref()
        && *sig == spec.signature
        && !client.is_closed()
    {
        return Ok(client.clone());
    }
    let client = SftpClient::open(spec.params.clone(), spec.check.clone()).await?;
    *guard = Some((spec.signature.clone(), client.clone()));
    Ok(client)
}

fn invalidate(key: &str) {
    if let Some(slot) = pool().lock().unwrap().get(key).cloned()
        && let Ok(mut g) = slot.try_lock()
    {
        *g = None;
    }
}

/// Electron's isConnectionError keywords.
fn is_connection_error(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_lowercase();
    ["not connected", "no sftp", "channel", "socket", "eof", "timeout", "timed out", "reset", "broken pipe", "ended", "closed", "disconnect"]
        .iter()
        .any(|k| m.contains(k))
}

/// Run `op` on the session, reconnecting once on a connection error.
pub async fn with_retry<T, F, Fut>(key: &str, spec: &ConnSpec, op: F) -> Result<T>
where
    F: Fn(Arc<SftpClient>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let client = session(key, spec).await?;
    match op(client).await {
        Err(e) if is_connection_error(&e) => {
            log::warn!("sftp: connection error, reconnecting: {e:#}");
            invalidate(key);
            let client = session(key, spec).await?;
            op(client).await
        }
        other => other,
    }
}

/// Upload one file, making its remote folder first (Electron's syncFile).
pub async fn put_file(key: &str, spec: &ConnSpec, local: &Path, remote: &str) -> Result<()> {
    with_retry(key, spec, |c| async move {
        c.ensure_dir_all(&crate::sftp::parent(remote)).await;
        c.upload(local, remote, &|_| {}).await
    })
    .await
}

/// Download one file, making its local folder first (Electron's pullFileDirect).
pub async fn get_file(key: &str, spec: &ConnSpec, remote: &str, local: &Path) -> Result<()> {
    if let Some(dir) = local.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    with_retry(key, spec, |c| async move { c.download(remote, local, &|_| {}).await }).await
}

// ─── Jobs ───────────────────────────────────────────────────────────────────

/// Everything a sync run needs, detached from the UI.
#[derive(Clone)]
pub struct Job {
    pub workspace_id: String,
    pub local_root: PathBuf,
    pub sync: SyncConfig,
    pub connections: Connections,
    pub cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn new(ws: &Workspace, sync: SyncConfig, connections: Connections) -> Option<Job> {
        let root = ws.working_directory.as_deref().filter(|d| !d.is_empty())?;
        Some(Job {
            workspace_id: ws.id.clone(),
            local_root: PathBuf::from(crate::grid::expand_tilde(root)),
            sync,
            connections,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    fn key(&self, target_id: &str) -> String {
        format!("{}:{}", self.workspace_id, target_id)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Each active target's (id, remote root, connection), connected up front so a
    /// dead host fails once instead of once per file. Failures: (id, error).
    async fn connect_targets(&self) -> (Vec<(String, String, ConnSpec)>, Vec<(String, String)>) {
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        for t in self.sync.targets.iter().filter(|t| t.is_active()) {
            let spec = match conn_spec(&self.connections, t.connection_id.as_deref().unwrap_or_default()) {
                Ok(s) => s,
                Err(e) => {
                    failed.push((t.id.clone(), e));
                    continue;
                }
            };
            match session(&self.key(&t.id), &spec).await {
                Ok(_) => ok.push((t.id.clone(), t.remote_root(), spec)),
                Err(e) => failed.push((t.id.clone(), format!("{e:#}"))),
            }
        }
        (ok, failed)
    }
}

#[derive(Default)]
struct Agg {
    ok: bool,
    count: usize,
    error: Option<String>,
}

/// One target's line of a result; `error` only when there is one (as Electron's JSON drops undefined).
fn target_result(id: &str, a: &Agg, count_key: &str) -> Value {
    let mut v = json!({ "targetId": id, "ok": a.ok, count_key: a.count });
    if let Some(e) = &a.error {
        v["error"] = e.clone().into();
    }
    v
}

/// Upload `files` (relative paths) to every connected target, CONCURRENCY at a time.
async fn upload_all(job: &Job, files: &[String], targets: &[(String, String, ConnSpec)], agg: &Mutex<HashMap<String, Agg>>) {
    let stream = futures::stream::iter(files.iter()).take_while(|_| futures::future::ready(!job.cancelled()));
    stream
        .for_each_concurrent(CONCURRENCY, |rel| async move {
            let local = job.local_root.join(rel);
            for (id, root, spec) in targets {
                let res = put_file(&job.key(id), spec, &local, &remote_file(root, rel)).await;
                let mut a = agg.lock().unwrap();
                let entry = a.entry(id.clone()).or_insert(Agg { ok: true, ..Default::default() });
                match res {
                    Ok(()) => entry.count += 1,
                    Err(e) => {
                        entry.ok = false;
                        entry.error = Some(format!("{e:#}"));
                    }
                }
            }
        })
        .await;
}

/// Sync now: upload the whole working directory to every enabled target.
pub async fn sync_now(job: Job) -> Value {
    if !job.sync.enabled || job.sync.targets.is_empty() {
        return json!({ "scanned": 0, "uploaded": 0, "skippedByGitIgnore": 0, "targetResults": [] });
    }
    let matcher = Matcher::load(&job.local_root, job.sync.use_git_ignore);
    let root = job.local_root.clone();
    let plan = plan_walk("", &mut local_lister(&root), matcher.as_ref(), &[".splitgrid"]);

    let agg: Mutex<HashMap<String, Agg>> = Mutex::new(job.sync.targets.iter().map(|t| (t.id.clone(), Agg { ok: true, ..Default::default() })).collect());
    let (targets, failed) = job.connect_targets().await;
    for (id, e) in failed {
        if !plan.files.is_empty()
            && let Some(a) = agg.lock().unwrap().get_mut(&id)
        {
            a.ok = false;
            a.error = Some(e);
        }
    }
    upload_all(&job, &plan.files, &targets, &agg).await;

    let agg = agg.into_inner().unwrap();
    let results: Vec<Value> = job
        .sync
        .targets
        .iter()
        .filter_map(|t| agg.get(&t.id).map(|a| target_result(&t.id, a, "uploaded")))
        .collect();
    json!({
        "scanned": plan.files.len(),
        "uploaded": agg.values().map(|a| a.count).sum::<usize>(),
        "skippedByGitIgnore": plan.skipped,
        "cancelled": job.cancelled(),
        "targetResults": results,
    })
}

/// The files under the given local paths (folders walked), relative to the root.
fn plan_paths(job: &Job, paths: &[PathBuf], matcher: Option<&Matcher>) -> Plan {
    let mut plan = Plan::default();
    let root = job.local_root.clone();
    for p in paths {
        let Some(rel) = rel_of(&root, p) else { continue };
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(meta) = std::fs::metadata(p) else { continue };
        match exclusion(&name, &rel, meta.is_dir(), matcher, &[]) {
            Some(Skip::Ignored) => plan.skipped += 1,
            Some(Skip::Builtin) => {}
            None if meta.is_dir() => {
                let sub = plan_walk(&rel, &mut local_lister(&root), matcher, &[]);
                plan.files.extend(sub.files);
                plan.skipped += sub.skipped;
            }
            None if meta.is_file() => plan.files.push(rel),
            None => {}
        }
    }
    plan
}

/// Push specific workspace paths to every enabled target.
pub async fn push_paths(job: Job, paths: Vec<PathBuf>) -> Value {
    if !job.sync.enabled || job.sync.targets.is_empty() {
        return json!({ "pushed": 0, "total": 0, "targetResults": [] });
    }
    let matcher = Matcher::load(&job.local_root, job.sync.use_git_ignore);
    let plan = plan_paths(&job, &paths, matcher.as_ref());
    let agg: Mutex<HashMap<String, Agg>> =
        Mutex::new(job.sync.targets.iter().filter(|t| t.enabled).map(|t| (t.id.clone(), Agg { ok: true, ..Default::default() })).collect());
    let (targets, failed) = job.connect_targets().await;
    for (id, e) in failed {
        if let Some(a) = agg.lock().unwrap().get_mut(&id) {
            a.ok = false;
            a.error = Some(e);
        }
    }
    upload_all(&job, &plan.files, &targets, &agg).await;
    let agg = agg.into_inner().unwrap();
    let results: Vec<Value> = job
        .sync
        .targets
        .iter()
        .filter_map(|t| agg.get(&t.id).map(|a| target_result(&t.id, a, "pushed")))
        .collect();
    json!({
        "pushed": agg.values().map(|a| a.count).max().unwrap_or(0),
        "total": plan.files.len(),
        "cancelled": job.cancelled(),
        "skippedByGitIgnore": plan.skipped,
        "targetResults": results,
    })
}

/// Pull specific workspace paths from the first enabled target (remote wins).
pub async fn pull_paths(job: Job, paths: Vec<PathBuf>) -> Value {
    if !job.sync.enabled || job.sync.targets.is_empty() {
        return json!({ "pulled": 0, "targetResults": [] });
    }
    let Some(target) = job.sync.targets.iter().find(|t| t.is_active()).cloned() else {
        return json!({ "pulled": 0, "targetResults": [] });
    };
    let fail = |e: String| json!({ "pulled": 0, "targetResults": [{ "targetId": target.id, "ok": false, "error": e }] });
    let spec = match conn_spec(&job.connections, target.connection_id.as_deref().unwrap_or_default()) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let key = job.key(&target.id);
    let remote_root = target.remote_root();
    let root = job.local_root.clone();
    let matcher = Matcher::load(&root, job.sync.use_git_ignore);
    let mut skipped = 0usize;
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for p in &paths {
        if job.cancelled() {
            break;
        }
        let Some(rel) = rel_of(&root, p) else { continue };
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let remote = remote_file(&remote_root, &rel);
        let is_dir = match std::fs::metadata(p) {
            Ok(m) => m.is_dir(),
            Err(_) => with_retry(&key, &spec, |c| {
                let remote = remote.clone();
                async move { c.is_dir(&remote).await }
            })
            .await
            .unwrap_or(false),
        };
        match exclusion(&name, &rel, is_dir, matcher.as_ref(), &[]) {
            Some(Skip::Ignored) => {
                skipped += 1;
                continue;
            }
            Some(Skip::Builtin) => continue,
            None => {}
        }
        if !is_dir {
            files.push((remote, p.clone()));
            continue;
        }
        // Walk the remote folder with the same exclusions.
        let mut stack = vec![(remote, p.clone())];
        while let Some((rdir, ldir)) = stack.pop() {
            if job.cancelled() {
                break;
            }
            let _ = tokio::fs::create_dir_all(&ldir).await;
            let entries = match with_retry(&key, &spec, |c| {
                let rdir = rdir.clone();
                async move { c.list(&rdir).await }
            })
            .await
            {
                Ok(e) => e,
                Err(e) => return json!({ "pulled": 0, "cancelled": job.cancelled(), "skippedByGitIgnore": skipped,
                                        "targetResults": [{ "targetId": target.id, "ok": false, "pulled": 0, "error": format!("{e:#}") }] }),
            };
            for e in entries {
                if unsafe_name(&e.name) {
                    continue;
                }
                let (rp, lp) = (crate::sftp::join(&rdir, &e.name), ldir.join(&e.name));
                let rel = rel_of(&root, &lp).unwrap_or_default();
                match exclusion(&e.name, &rel, e.is_dir, matcher.as_ref(), &[]) {
                    Some(Skip::Ignored) => skipped += 1,
                    Some(Skip::Builtin) => {}
                    None if e.is_dir => stack.push((rp, lp)),
                    None => files.push((rp, lp)),
                }
            }
        }
    }

    if files.is_empty() {
        return json!({ "pulled": 0, "cancelled": job.cancelled(), "skippedByGitIgnore": skipped,
                       "targetResults": [{ "targetId": target.id, "ok": true, "pulled": 0 }] });
    }
    let errors: Mutex<Vec<String>> = Mutex::default();
    let done = std::sync::atomic::AtomicUsize::new(0);
    futures::stream::iter(files.iter())
        .take_while(|_| futures::future::ready(!job.cancelled()))
        .for_each_concurrent(CONCURRENCY, |(rp, lp)| {
            let (key, spec, errors, done, root) = (&key, &spec, &errors, &done, &root);
            async move {
                done.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = get_file(key, spec, rp, lp).await {
                    errors.lock().unwrap().push(format!("{}: {e:#}", rel_of(root, lp).unwrap_or_default()));
                }
            }
        })
        .await;
    let errors = errors.into_inner().unwrap();
    let pulled = done.load(Ordering::Relaxed) - errors.len();
    let mut tr = json!({ "targetId": target.id, "ok": errors.is_empty(), "pulled": pulled });
    if !errors.is_empty() {
        tr["error"] = errors.iter().take(5).cloned().collect::<Vec<_>>().join("; ").into();
    }
    json!({ "pulled": pulled, "cancelled": job.cancelled(), "skippedByGitIgnore": skipped, "targetResults": [tr] })
}

// ─── Running syncs (for the menu's Sync now / Cancel) ──────────────────────

fn running() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Marks a Sync now of a workspace as running until dropped (also when a timeout drops it).
pub struct Running(String);

impl Drop for Running {
    fn drop(&mut self) {
        running().lock().unwrap().remove(&self.0);
    }
}

/// Mark a Sync now for `ws` as running (None if one already is).
pub fn begin(ws: &str, cancel: Arc<AtomicBool>) -> Option<Running> {
    let mut r = running().lock().unwrap();
    if r.contains_key(ws) {
        return None;
    }
    r.insert(ws.to_owned(), cancel);
    Some(Running(ws.to_owned()))
}

pub fn is_running(ws: &str) -> bool {
    running().lock().unwrap().contains_key(ws)
}

pub fn cancel(ws: &str) {
    if let Some(c) = running().lock().unwrap().get(ws) {
        c.store(true, Ordering::Relaxed);
    }
}

// ─── Auto sync (file watcher) ───────────────────────────────────────────────

struct Watch {
    owner: String,
    signature: String,
    _watcher: notify::RecommendedWatcher,
}

fn watches() -> &'static Mutex<HashMap<String, Watch>> {
    static W: OnceLock<Mutex<HashMap<String, Watch>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// What a watched workspace's sync depends on (a change restarts its watcher).
fn watch_signature(root: &str, sync: &SyncConfig) -> String {
    let targets: Vec<String> =
        sync.targets.iter().map(|t| format!("{}:{}:{}:{}", t.id, t.enabled, t.connection_id.clone().unwrap_or_default(), t.remote_path)).collect();
    format!("{root}|{}|{}", sync.use_git_ignore, targets.join(","))
}

/// Keep one watcher per workspace of `owner` (a window's environment) whose sync
/// is on — background workspaces included, as in Electron — and stop the rest.
pub fn reconcile_watchers(owner: &str, workspaces: &[Workspace], connections: &Connections) {
    let mut desired: HashMap<String, (PathBuf, SyncConfig, String)> = HashMap::new();
    for ws in workspaces {
        let (Some(root), Some(sync)) = (ws.working_directory.as_deref().filter(|d| !d.is_empty()), SyncConfig::of(ws)) else { continue };
        if !sync.watchable() {
            continue;
        }
        let root = crate::grid::expand_tilde(root);
        let sig = watch_signature(&root, &sync);
        desired.insert(ws.id.clone(), (PathBuf::from(root), sync, sig));
    }
    let mut w = watches().lock().unwrap();
    w.retain(|id, watch| watch.owner != owner || desired.get(id).is_some_and(|d| d.2 == watch.signature));
    for (id, (root, sync, sig)) in desired {
        if w.contains_key(&id) {
            continue;
        }
        match start_watch(id.clone(), root, sync, connections.clone()) {
            Ok(watcher) => {
                w.insert(id, Watch { owner: owner.to_owned(), signature: sig, _watcher: watcher });
            }
            Err(e) => log::error!("auto sync: watching workspace {id}: {e:#}"),
        }
    }
}

/// Stop every watcher of `owner` (its window closed).
pub fn stop_watchers(owner: &str) {
    watches().lock().unwrap().retain(|_, w| w.owner != owner);
}

/// A changed path under `root` the watcher should never look at.
pub fn watch_ignored(root: &Path, path: &Path, matcher: Option<&Matcher>) -> bool {
    let Some(rel) = rel_of(root, path) else { return true };
    if rel.is_empty() {
        return false;
    }
    if rel.split('/').any(|p| ALWAYS_IGNORE_SEGMENTS.contains(&p)) {
        return true;
    }
    matcher.is_some_and(|m| m.ignores(&rel, path.is_dir()))
}

fn start_watch(ws_id: String, root: PathBuf, sync: SyncConfig, connections: Connections) -> Result<notify::RecommendedWatcher> {
    use notify::Watcher as _;
    let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, bool)>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        // A created or renamed-in folder gets its whole content pushed.
        let structural = matches!(ev.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(notify::event::ModifyKind::Name(_)));
        for p in ev.paths {
            let _ = tx.send((p, structural));
        }
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    let job = Job { workspace_id: ws_id.clone(), local_root: root.clone(), sync, connections, cancel: Arc::new(AtomicBool::new(false)) };
    std::thread::Builder::new().name(format!("auto-sync {ws_id}")).spawn(move || {
        let mut pending: HashMap<PathBuf, bool> = HashMap::new();
        loop {
            match rx.recv_timeout(WATCH_DEBOUNCE) {
                Ok((p, structural)) => {
                    *pending.entry(p).or_default() |= structural;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !pending.is_empty() {
                        let batch = std::mem::take(&mut pending);
                        crate::ssh::runtime().block_on(flush(&job, batch));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })?;
    Ok(watcher)
}

/// What a batch of watcher events becomes (pure, on the current disk state).
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub dirs: Vec<String>,
    pub files: Vec<String>,
    pub deletes: Vec<String>,
}

pub fn classify(root: &Path, batch: &HashMap<PathBuf, bool>, matcher: Option<&Matcher>) -> Changes {
    let mut c = Changes::default();
    let mut seen = HashSet::new();
    let mut paths: Vec<_> = batch.iter().collect();
    paths.sort();
    for (path, structural) in paths {
        if watch_ignored(root, path, matcher) {
            continue;
        }
        let Some(rel) = rel_of(root, path).filter(|r| !r.is_empty()) else { continue };
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_file() => {
                if seen.insert(rel.clone()) {
                    c.files.push(rel);
                }
            }
            Ok(m) if m.is_dir() && *structural => {
                c.dirs.push(rel.clone());
                let sub = plan_walk(&rel, &mut local_lister(root), matcher, &ALWAYS_IGNORE_SEGMENTS);
                for f in sub.files {
                    if seen.insert(f.clone()) {
                        c.files.push(f);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => c.deletes.push(rel),
        }
    }
    c
}

async fn flush(job: &Job, batch: HashMap<PathBuf, bool>) {
    let matcher = Matcher::load(&job.local_root, job.sync.use_git_ignore);
    let changes = classify(&job.local_root, &batch, matcher.as_ref());
    if changes == Changes::default() {
        return;
    }
    let (targets, failed) = job.connect_targets().await;
    for (id, e) in &failed {
        log::warn!("auto sync {}: target {id}: {e}", job.workspace_id);
    }
    let mut errors = 0usize;
    for (id, root, spec) in &targets {
        let key = job.key(id);
        for d in &changes.dirs {
            let dir = remote_file(root, d);
            if with_retry(&key, spec, |c| {
                let dir = dir.clone();
                async move {
                    c.ensure_dir_all(&dir).await;
                    Ok(())
                }
            })
            .await
            .is_err()
            {
                errors += 1;
            }
        }
    }
    let agg = Mutex::new(HashMap::new());
    upload_all(job, &changes.files, &targets, &agg).await;
    for (id, root, spec) in &targets {
        for d in &changes.deletes {
            let path = remote_file(root, d);
            let res = with_retry(&job.key(id), spec, |c| {
                let path = path.clone();
                async move { c.remove_if_exists(&path).await }
            })
            .await;
            if let Err(e) = res {
                errors += 1;
                log::warn!("auto sync: delete {path}: {e:#}");
            }
        }
    }
    errors += agg.into_inner().unwrap().values().filter(|a: &&Agg| !a.ok).count();
    log::info!(
        "auto sync {}: {} dirs, {} files, {} deletes, {} errors",
        job.workspace_id,
        changes.dirs.len(),
        changes.files.len(),
        changes.deletes.len(),
        errors + failed.len()
    );
}

// ─── App glue ───────────────────────────────────────────────────────────────

impl crate::app::AppView {
    /// Start/stop this window's auto-sync watchers to match its workspaces.
    pub(crate) fn reconcile_sync_watchers(&mut self, cx: &mut gpui_kit::Context<Self>) {
        reconcile_watchers(&cx.entity_id().to_string(), &self.state.workspaces, &self.connections);
    }

    /// Store a finished Sync now on its workspace (target status + log line).
    pub(crate) fn record_sync_now(&mut self, ws_id: &str, result: &Value, cx: &mut gpui_kit::Context<Self>) {
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) else { return };
        let Some(mut sync) = SyncConfig::of(ws) else { return };
        let root = ws.working_directory.clone().unwrap_or_else(|| ".".into());
        sync.record_sync_now(&root, result, crate::model::workspace::now_ms());
        sync.store(ws);
        self.changed(cx);
    }

    /// The workspace menu's "Sync now": the whole working directory to every target.
    pub(crate) fn sync_workspace_now(&mut self, ws_id: &str, cx: &mut gpui_kit::Context<Self>) {
        if is_running(ws_id) {
            cancel(ws_id);
            return;
        }
        let Some(ws) = self.state.workspaces.iter().find(|w| w.id == ws_id) else { return };
        let Some(job) = SyncConfig::of(ws).filter(SyncConfig::configured).and_then(|s| Job::new(ws, s, self.connections.clone())) else { return };
        let Some(guard) = begin(ws_id, job.cancel.clone()) else { return };
        let ran = ws_id.to_owned();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::sftp::call(async move {
                let _guard = guard;
                Ok(sync_now(job).await)
            })
            .await
            .unwrap_or_else(|e| json!({ "targetResults": [], "error": format!("{e:#}") }));
            let _ = this.update(cx, |this, cx| this.record_sync_now(&ran, &result, cx));
        })
        .detach();
    }

    /// The menu line for a workspace's sync: (label, detail) — None without a configured sync.
    pub(crate) fn sync_menu_entry(&self, ws: &Workspace) -> Option<(String, String)> {
        let sync = SyncConfig::of(ws).filter(SyncConfig::configured)?;
        if is_running(&ws.id) {
            return Some(("Cancel sync".into(), "syncing…".into()));
        }
        let last = sync.targets.iter().filter(|t| t.enabled).filter_map(|t| t.last_sync_at.map(|at| (at, t))).max_by_key(|(at, _)| *at);
        let detail = match last {
            None => "never synced".into(),
            Some((at, _)) => {
                let failed = sync.targets.iter().any(|t| t.enabled && t.last_sync_status.as_deref() == Some("error"));
                let ago = (crate::model::workspace::now_ms() - at).max(0) / 1000;
                let when = match ago {
                    0..60 => "just now".to_owned(),
                    60..3600 => format!("{}m ago", ago / 60),
                    3600..86400 => format!("{}h ago", ago / 3600),
                    _ => format!("{}d ago", ago / 86400),
                };
                if failed { format!("failed · {when}") } else { when }
            }
        };
        Some(("Sync now".into(), detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(&str, &[(&str, bool)])]) -> impl FnMut(&str) -> Vec<(String, bool)> {
        let map: HashMap<String, Vec<(String, bool)>> =
            entries.iter().map(|(d, list)| (d.to_string(), list.iter().map(|(n, b)| (n.to_string(), *b)).collect())).collect();
        move |dir: &str| map.get(dir).cloned().unwrap_or_default()
    }

    #[test]
    fn walk_skips_git_and_gitignored_without_descending() {
        let root = Path::new("/ws");
        let m = Matcher::from_rules(root, "node_modules/\n*.log\ndist\n!keep.log\n").unwrap();
        let mut list = tree(&[
            ("", &[(".git", true), ("src", true), ("node_modules", true), ("a.log", false), ("keep.log", false), ("README.md", false), ("dist", true), (".splitgrid", true)]),
            ("src", &[("main.rs", false), ("x.log", false), ("deep", true)]),
            ("src/deep", &[("y.txt", false)]),
            ("node_modules", &[("pkg", true)]),
            ("dist", &[("bundle.js", false)]),
            (".splitgrid", &[("state", false)]),
        ]);
        let plan = plan_walk("", &mut list, Some(&m), &[".splitgrid"]);
        let mut files = plan.files.clone();
        files.sort();
        assert_eq!(files, ["README.md", "keep.log", "src/deep/y.txt", "src/main.rs"]);
        // a.log, node_modules, dist, src/x.log — .git and .splitgrid aren't counted.
        assert_eq!(plan.skipped, 4);
    }

    #[test]
    fn walk_without_gitignore_takes_everything_but_git() {
        let mut list = tree(&[("", &[(".git", true), ("node_modules", true), ("a", false)]), ("node_modules", &[("p.js", false)])]);
        let plan = plan_walk("", &mut list, None, &[]);
        let mut files = plan.files;
        files.sort();
        assert_eq!(files, ["a", "node_modules/p.js"]);
    }

    #[test]
    fn walk_from_a_subfolder_keeps_root_relative_paths() {
        let root = Path::new("/ws");
        let m = Matcher::from_rules(root, "/src/gen\n").unwrap();
        let mut list = tree(&[("src", &[("gen", true), ("a.rs", false)]), ("src/gen", &[("b.rs", false)])]);
        let plan = plan_walk("src", &mut list, Some(&m), &[]);
        assert_eq!(plan.files, ["src/a.rs"]);
        assert_eq!(plan.skipped, 1);
    }

    #[test]
    fn remote_paths_join_cleanly() {
        assert_eq!(remote_file("/srv/app", "a/b.txt"), "/srv/app/a/b.txt");
        assert_eq!(remote_file("/srv/app/", "a"), "/srv/app/a");
        assert_eq!(remote_file("/", "a"), "/a");
        assert_eq!(remote_file("/srv//app", "x"), "/srv/app/x");
        let t = SyncTarget { remote_path: "/srv/app///".into(), ..Default::default() };
        assert_eq!(t.remote_root(), "/srv/app");
        assert_eq!(SyncTarget { remote_path: "/".into(), ..Default::default() }.remote_root(), "/");
    }

    #[test]
    fn rel_paths_stay_under_the_root() {
        let root = Path::new("/ws/proj");
        assert_eq!(rel_of(root, Path::new("/ws/proj/a/b")).as_deref(), Some("a/b"));
        assert_eq!(rel_of(root, Path::new("/ws/proj")).as_deref(), Some(""));
        assert_eq!(rel_of(root, Path::new("/ws/other")), None);
    }

    #[test]
    fn electron_config_round_trips_with_unknown_fields() {
        let raw = json!({
            "enabled": true, "useGitIgnore": true,
            "targets": [{ "id": "t1", "name": "prod", "enabled": true, "connectionId": "c1", "remotePath": "/srv/app",
                          "lastSyncAt": 5, "lastSyncStatus": "success", "future": 1 }],
            "fileStates": { "a": { "status": "synced", "updatedAt": 1 } },
            "logs": []
        });
        let cfg: SyncConfig = serde_json::from_value(raw.clone()).unwrap();
        assert!(cfg.configured() && cfg.watchable() && cfg.targets[0].is_active());
        assert_eq!(serde_json::to_value(&cfg).unwrap(), raw);
    }

    #[test]
    fn targets_need_connection_and_root_to_be_active() {
        let t = |enabled, conn: Option<&str>, path: &str| SyncTarget {
            enabled,
            connection_id: conn.map(str::to_owned),
            remote_path: path.into(),
            ..Default::default()
        };
        assert!(t(true, Some("c"), "/x").is_active());
        assert!(!t(false, Some("c"), "/x").is_active());
        assert!(!t(true, None, "/x").is_active());
        assert!(!t(true, Some("c"), "  ").is_active());
        let cfg = SyncConfig { enabled: true, targets: vec![t(true, None, "")], ..Default::default() };
        assert!(cfg.configured() && !cfg.watchable());
    }

    #[test]
    fn sync_now_is_recorded_like_the_settings_card() {
        let mut cfg = SyncConfig {
            enabled: true,
            targets: vec![
                SyncTarget { id: "a".into(), ..Default::default() },
                SyncTarget { id: "b".into(), last_sync_error: Some("old".into()), ..Default::default() },
            ],
            ..Default::default()
        };
        let result = json!({ "scanned": 3, "uploaded": 5, "skippedByGitIgnore": 1, "cancelled": false,
            "targetResults": [{ "targetId": "a", "ok": false, "error": "boom" }, { "targetId": "b", "ok": true }] });
        cfg.record_sync_now("/ws", &result, 42);
        assert_eq!(cfg.targets[0].last_sync_status.as_deref(), Some("error"));
        assert_eq!(cfg.targets[0].last_sync_error.as_deref(), Some("boom"));
        assert_eq!((cfg.targets[1].last_sync_at, cfg.targets[1].last_sync_error.as_deref()), (Some(42), None));
        let log = &cfg.logs.as_ref().unwrap()[0];
        assert_eq!(log["message"], "scanned 3, uploaded 5, skipped 1");
        assert_eq!((log["ok"].as_bool(), log["action"].as_str()), (Some(false), Some("sync-now")));
        for _ in 0..450 {
            cfg.record_sync_now("/ws", &result, 43);
        }
        assert_eq!(cfg.logs.as_ref().unwrap().len(), MAX_LOGS);
    }

    #[test]
    fn watcher_batches_classify_against_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("new/sub")).unwrap();
        std::fs::write(root.join("new/sub/f.txt"), b"x").unwrap();
        std::fs::write(root.join("changed.txt"), b"x").unwrap();
        std::fs::write(root.join("skip.log"), b"x").unwrap();
        std::fs::create_dir_all(root.join("node_modules/p")).unwrap();
        std::fs::write(root.join("node_modules/p/i.js"), b"x").unwrap();
        let m = Matcher::from_rules(&root, "*.log\n").unwrap();
        let batch: HashMap<PathBuf, bool> = [
            (root.join("new"), true),
            (root.join("changed.txt"), false),
            (root.join("gone.txt"), false),
            (root.join("skip.log"), false),
            (root.join("node_modules/p/i.js"), false),
            (root.clone(), false),
        ]
        .into_iter()
        .collect();
        let c = classify(&root, &batch, Some(&m));
        assert_eq!(c.dirs, ["new"]);
        assert_eq!(c.files, ["changed.txt", "new/sub/f.txt"]);
        assert_eq!(c.deletes, ["gone.txt"]);
    }

    #[test]
    fn connection_errors_are_recognised() {
        assert!(is_connection_error(&anyhow::anyhow!("Channel closed")));
        assert!(is_connection_error(&anyhow::anyhow!("Timed out connecting")));
        assert!(!is_connection_error(&anyhow::anyhow!("No such file")));
    }
}
