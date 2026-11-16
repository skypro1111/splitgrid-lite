//! The facade behind [`crate::HistoryService`] (port of src/main/history/service.ts).
//!
//! CONSENT: constructing it does nothing. Until `enable()` it downloads
//! nothing, spawns nothing (process or thread), watches nothing and writes
//! nothing; `estimate()` only stat()s transcript files.
//!
//! Threads (all started by `enable()`, all gone after `disable()`):
//!   - one background loop: scan + embed passes, then a distillation pass,
//!     woken by `kick()`, the transcript watcher (debounced) and the rescan
//!     interval;
//!   - a status emitter, alive only while there are changes to push, which
//!     calls the `on_status` listeners at most about once a second;
//!   - the runtime's idle timer and llama-server, on demand.
//!
//! Every public method may block (disk, git, llama-server); queries run on the
//! caller's thread against the shared, internally locked stores.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::distill::{
    ClaudeCliDistillRunner, DISTILL_MODEL_LABEL, DistillRunResult, DistillRunner, HAIKU_PRICE_LOW,
    resolve_claude_executable,
};
use crate::git_why::{commits_for_file, match_edits_to_commits};
use crate::indexer::{DistillScanResult, HistoryIndexer, IndexerOptions};
use crate::runtime::{
    EmbedKind, EmbeddingRuntime, EmbeddingRuntimeOptions, InstallProgress, RuntimeLike,
    current_platform,
};
use crate::util::{CancelToken, is_cancelled_error, now_ms, path_str};
use crate::{
    DistillState, FileSession, HistoryConfig, HistoryDistillEstimate, HistoryDistillStatus,
    HistoryEstimate, HistoryProgress, HistoryStatus, HistoryWhyEntry, HistoryWorkspaceStats,
    IndexedWorkspace, ProgressState, SearchHit, SessionSummary, ShownChunk,
};

/// Back-off between background passes when nothing asks for one sooner.
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Claude Code appends to a transcript on every turn; coalesce those writes.
pub const WATCH_DEBOUNCE: Duration = Duration::from_secs(30);
/// Settings re-renders on every status push; one per second is plenty.
pub const STATUS_THROTTLE: Duration = Duration::from_secs(1);
/// The emitter thread exits after this long with nothing to push.
const EMITTER_LINGER: Duration = Duration::from_secs(5);

// Rough first-import cost model for the consent prompt (see service.ts).
const TOKENS_PER_BYTE: f64 = 0.003;
const TOKENS_PER_SECOND: f64 = 2_500.0;
const INDEX_BYTES_PER_BYTE: f64 = 0.05;

// Decision notes: back-off after a failure that says nothing about the input.
const DISTILL_BACKOFF_MIN_MS: i64 = 5 * 60_000;
const DISTILL_BACKOFF_MAX_MS: i64 = 60 * 60_000;

/// Test seams (Electron's HistoryServiceDeps): a fake runtime / runner / clock.
#[derive(Default, Clone)]
pub struct ServiceDeps {
    pub runtime: Option<Arc<dyn RuntimeLike>>,
    pub distill_runner: Option<Arc<dyn DistillRunner>>,
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

/// Claude Code's project-dir naming: every non-alphanumeric UTF-16 unit becomes `-`.
pub fn claude_project_slug(folder: &str) -> String {
    folder
        .chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() {
                vec![c]
            } else {
                vec!['-'; c.len_utf16()]
            }
        })
        .collect()
}

fn jsonl_bytes(dir: &Path) -> (u64, u64) {
    fn walk(d: &Path, depth: u32, files: &mut u64, bytes: &mut u64) {
        let Ok(entries) = std::fs::read_dir(d) else {
            return;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_file() && name.ends_with(".jsonl") {
                if let Ok(m) = std::fs::metadata(e.path()) {
                    *bytes += m.len();
                    *files += 1;
                }
            } else if ft.is_dir() && depth < 2 && name != "tool-results" {
                walk(&e.path(), depth + 1, files, bytes);
            }
        }
    }
    let (mut files, mut bytes) = (0, 0);
    walk(dir, 0, &mut files, &mut bytes);
    (files, bytes)
}

#[derive(Default)]
struct State {
    enabled: bool,
    paused: bool,
    progress: HistoryProgress,
    workspace_stats: Vec<HistoryWorkspaceStats>,
    installing: bool,
    install_gen: u64,
    install_error: Option<String>,
    install_cancel: Option<CancelToken>,
    // Background loop.
    kick_pending: bool,
    debounce_at: Option<Instant>,
    rescan_at: Option<Instant>,
    // Decision notes.
    distill_on: bool,
    distill_cancel: Option<CancelToken>,
    distill_retry_at: i64,
    distill_backoff_ms: i64,
    distill_scanned: bool,
    distill: HistoryDistillStatus,
}

impl State {
    fn should_stop(&self) -> bool {
        !self.enabled || self.paused
    }
}

#[derive(Default)]
struct EmitState {
    pending: bool,
    force: bool,
    last: Option<Instant>,
    running: bool,
}

struct Background {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    _watcher: Option<notify::RecommendedWatcher>,
}

type Listener = Arc<dyn Fn(&HistoryStatus) + Send + Sync>;

pub(crate) struct Inner {
    cfg: HistoryConfig,
    deps: ServiceDeps,
    runtime: OnceLock<Arc<dyn RuntimeLike>>,
    runner: OnceLock<Arc<dyn DistillRunner>>,
    indexer: Mutex<Option<Arc<HistoryIndexer>>>,
    state: Mutex<State>,
    cv: Condvar,
    listeners: Mutex<Vec<Listener>>,
    emit: Mutex<EmitState>,
    emit_cv: Condvar,
    bg: Mutex<Option<Background>>,
}

impl Inner {
    pub(crate) fn new(cfg: HistoryConfig, deps: ServiceDeps) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            deps,
            runtime: OnceLock::new(),
            runner: OnceLock::new(),
            indexer: Mutex::new(None),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            listeners: Mutex::new(Vec::new()),
            emit: Mutex::new(EmitState::default()),
            emit_cv: Condvar::new(),
            bg: Mutex::new(None),
        })
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, msg: &str) {
        (self.cfg.log)(msg)
    }

    pub(crate) fn log_line(&self, msg: &str) {
        self.log(msg)
    }

    fn now(&self) -> i64 {
        self.deps.now.as_ref().map_or_else(now_ms, |f| f())
    }

    /// Created lazily; the real runtime only computes paths when built.
    fn runtime(&self) -> Arc<dyn RuntimeLike> {
        self.runtime
            .get_or_init(|| {
                self.deps.runtime.clone().unwrap_or_else(|| {
                    let log = self.cfg.log.clone();
                    EmbeddingRuntime::new(EmbeddingRuntimeOptions {
                        root_dir: self.cfg.root_dir.join("runtime"),
                        idle: None,
                        threads: None,
                        platform: None,
                        log: Some(log),
                    }) as Arc<dyn RuntimeLike>
                })
            })
            .clone()
    }

    fn indexer(&self) -> Arc<HistoryIndexer> {
        let mut slot = self.indexer.lock().unwrap();
        slot.get_or_insert_with(|| {
            Arc::new(HistoryIndexer::new(IndexerOptions {
                projects_dir: self.cfg.projects_dir.clone(),
                data_dir: self.cfg.root_dir.join("db"),
                workspaces: self.cfg.workspaces.clone(),
                embedder: self.runtime(),
                batch_size: 16,
                log: self.cfg.log.clone(),
            }))
        })
        .clone()
    }

    fn runner(&self) -> Arc<dyn DistillRunner> {
        self.runner
            .get_or_init(|| {
                self.deps.distill_runner.clone().unwrap_or_else(|| {
                    let binary = self.cfg.claude_binary.clone();
                    Arc::new(ClaudeCliDistillRunner::new(
                        Arc::new(move || resolve_claude_executable(binary().as_deref())),
                        // Outside every workspace folder: no project context is picked up.
                        self.cfg.root_dir.join("distill"),
                        self.cfg.log.clone(),
                    )) as Arc<dyn DistillRunner>
                })
            })
            .clone()
    }

    // ─── Status ────────────────────────────────────────────────────────────

    fn distill_status(s: &State) -> HistoryDistillStatus {
        let state = if !s.distill_on {
            DistillState::Off
        } else if s.paused {
            DistillState::Paused
        } else {
            s.distill.state
        };
        HistoryDistillStatus {
            enabled: s.distill_on,
            state,
            ..s.distill.clone()
        }
    }

    pub(crate) fn status(&self) -> HistoryStatus {
        let s = self.st();
        HistoryStatus {
            enabled: s.enabled,
            progress: s.progress.clone(),
            workspaces: s.workspace_stats.clone(),
            distill: Some(Self::distill_status(&s)),
        }
    }

    pub(crate) fn on_status(&self, f: Listener) {
        self.listeners.lock().unwrap().push(f);
    }

    fn set_progress(self: &Arc<Self>, f: impl FnOnce(&mut HistoryProgress), force: bool) {
        f(&mut self.st().progress);
        self.request_emit(force);
    }

    /// Push the current status to the listeners from the emitter thread:
    /// forced pushes go out at once, the rest at most once per STATUS_THROTTLE.
    fn request_emit(self: &Arc<Self>, force: bool) {
        let mut e = self.emit.lock().unwrap();
        e.pending = true;
        e.force |= force;
        if e.running {
            self.emit_cv.notify_all();
            return;
        }
        e.running = true;
        let me = self.clone();
        if std::thread::Builder::new()
            .name("sg-history-status".into())
            .spawn(move || me.emitter())
            .is_err()
        {
            e.running = false;
        }
    }

    fn emitter(self: Arc<Self>) {
        let mut e = self.emit.lock().unwrap();
        loop {
            if !e.pending {
                let (g, timeout) = self.emit_cv.wait_timeout(e, EMITTER_LINGER).unwrap();
                e = g;
                if !e.pending && timeout.timed_out() {
                    e.running = false;
                    return;
                }
                continue;
            }
            let now = Instant::now();
            if !e.force
                && let Some(last) = e.last
                && now - last < STATUS_THROTTLE
            {
                e = self
                    .emit_cv
                    .wait_timeout(e, STATUS_THROTTLE - (now - last))
                    .unwrap()
                    .0;
                continue;
            }
            e.pending = false;
            e.force = false;
            e.last = Some(now);
            drop(e);
            let status = self.status();
            let listeners: Vec<Listener> = self.listeners.lock().unwrap().clone();
            for l in listeners {
                // A broken listener must not stop indexing.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| l(&status)));
            }
            e = self.emit.lock().unwrap();
        }
    }

    // ─── Consent ───────────────────────────────────────────────────────────

    /// Stat-only: the slug of each workspace folder names Claude Code's project
    /// dir for sessions started there. Reads no transcript, touches no network.
    pub(crate) fn estimate(&self) -> Result<HistoryEstimate> {
        let mut slugs: Vec<String> = Vec::new();
        for w in (self.cfg.workspaces)() {
            if let Some(f) = w.working_directory.map(|p| path_str(&p)) {
                let f = f.trim();
                if !f.is_empty() {
                    let slug = claude_project_slug(f);
                    if !slugs.contains(&slug) {
                        slugs.push(slug);
                    }
                }
            }
        }
        let (mut files, mut bytes) = (0u64, 0u64);
        if let Ok(dirs) = std::fs::read_dir(&self.cfg.projects_dir) {
            for d in dirs.flatten() {
                let name = d.file_name().to_string_lossy().into_owned();
                if !slugs
                    .iter()
                    .any(|s| name == *s || name.starts_with(&format!("{s}-")))
                {
                    continue;
                }
                let (f, b) = jsonl_bytes(&d.path());
                files += f;
                bytes += b;
            }
        }
        let rt = self.runtime();
        let index_bytes = (bytes as f64 * INDEX_BYTES_PER_BYTE).round() as u64;
        Ok(HistoryEstimate {
            transcript_files: files,
            transcript_bytes: bytes,
            download_bytes: if rt.is_installed() {
                0
            } else {
                rt.download_bytes()
            },
            disk_bytes: rt.download_bytes() + index_bytes,
            first_import_minutes: ((bytes as f64 * TOKENS_PER_BYTE) / TOKENS_PER_SECOND / 60.0)
                .round()
                .max(1.0),
        })
    }

    /// Consent given. A second call while installing joins it; a call while
    /// already running is a no-op. Returns once installed (or paused).
    pub(crate) fn enable(self: &Arc<Self>) -> Result<()> {
        let supported = self.runtime().is_supported();
        {
            let mut s = self.st();
            if s.enabled && !s.installing {
                return Ok(());
            }
            if s.installing {
                let generation = s.install_gen;
                while s.installing && s.install_gen == generation {
                    s = self.cv.wait(s).unwrap();
                }
                return match &s.install_error {
                    Some(e) => bail!("{e}"),
                    None => Ok(()),
                };
            }
            if !supported {
                let (p, a) = current_platform();
                bail!("Workspace history is not available on {p}-{a} yet.");
            }
            s.enabled = true;
            s.paused = false;
            s.installing = true;
            s.install_gen += 1;
            s.install_error = None;
        }
        self.run_install()
    }

    fn run_install(self: &Arc<Self>) -> Result<()> {
        let r = self.install_runtime();
        let mut s = self.st();
        s.installing = false;
        s.install_error = r.as_ref().err().map(|e| e.to_string());
        self.cv.notify_all();
        r
    }

    fn install_runtime(self: &Arc<Self>) -> Result<()> {
        let rt = self.runtime();
        if !rt.is_installed() {
            let token = CancelToken::new();
            self.st().install_cancel = Some(token.clone());
            let total = rt.download_bytes();
            self.set_progress(
                |p| {
                    *p = HistoryProgress {
                        state: ProgressState::Installing,
                        downloaded_bytes: Some(0),
                        total_bytes: Some(total),
                        ..Default::default()
                    }
                },
                true,
            );
            let me = self.clone();
            let r = rt.install(&token, &mut |p: InstallProgress| {
                me.set_progress(
                    |pr| {
                        *pr = HistoryProgress {
                            state: ProgressState::Installing,
                            downloaded_bytes: Some(p.downloaded_bytes),
                            total_bytes: Some(p.total_bytes),
                            ..Default::default()
                        }
                    },
                    false,
                )
            });
            self.st().install_cancel = None;
            if let Err(e) = r {
                let (paused, enabled) = {
                    let s = self.st();
                    (s.paused, s.enabled)
                };
                if is_cancelled_error(&e) && (paused || !enabled) {
                    // Paused or turned off mid-download: resume() / enable() picks it up.
                    if enabled {
                        self.set_progress(
                            |p| {
                                *p = HistoryProgress {
                                    state: ProgressState::Paused,
                                    ..Default::default()
                                }
                            },
                            true,
                        );
                    }
                    return Ok(());
                }
                self.st().enabled = false;
                let msg = e.to_string();
                self.set_progress(
                    |p| {
                        *p = HistoryProgress {
                            state: ProgressState::Error,
                            error: Some(msg),
                            ..Default::default()
                        }
                    },
                    true,
                );
                return Err(e);
            }
        }
        if !self.st().enabled {
            return Ok(());
        }
        self.start_background();
        Ok(())
    }

    pub(crate) fn disable(self: &Arc<Self>, delete_data: bool) -> Result<()> {
        {
            let mut s = self.st();
            s.enabled = false;
            s.paused = false;
            if let Some(t) = &s.install_cancel {
                t.cancel();
            }
            // History off means distillation off too; re-enabling asks again.
            s.distill_on = false;
            if let Some(t) = &s.distill_cancel {
                t.cancel();
            }
            s.distill_scanned = false;
        }
        let bg = self.stop_background();
        {
            let mut s = self.st();
            while s.installing {
                s = self.cv.wait(s).unwrap();
            }
        }
        self.shutdown(bg);
        if delete_data {
            // Even when never fully enabled: a failed or cancelled install
            // leaves partial downloads that must not linger.
            if let Some(rt) = self.runtime.get() {
                let _ = rt.uninstall();
            }
            match std::fs::remove_dir_all(&self.cfg.root_dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        self.set_progress(
            |p| {
                *p = HistoryProgress {
                    state: ProgressState::Off,
                    ..Default::default()
                }
            },
            true,
        );
        Ok(())
    }

    pub(crate) fn pause(self: &Arc<Self>) {
        {
            let mut s = self.st();
            if !s.enabled || s.paused {
                return;
            }
            s.paused = true;
            // An install in flight is cancelled; verified files are kept.
            if let Some(t) = &s.install_cancel {
                t.cancel();
            }
            if let Some(t) = &s.distill_cancel {
                t.cancel();
            }
        }
        self.set_progress(|p| p.state = ProgressState::Paused, true);
    }

    pub(crate) fn resume(self: &Arc<Self>) {
        {
            let mut s = self.st();
            if !s.enabled || !s.paused {
                return;
            }
            s.paused = false;
        }
        if !self.runtime().is_installed() {
            {
                let mut s = self.st();
                if s.installing {
                    return;
                }
                s.installing = true;
                s.install_gen += 1;
                s.install_error = None;
            }
            let me = self.clone();
            std::thread::spawn(move || {
                if let Err(e) = me.run_install() {
                    me.log(&format!("[history] resume install failed: {e}"));
                }
            });
            return;
        }
        self.set_progress(|p| p.state = ProgressState::Indexing, true);
        self.kick();
    }

    /// App quit: stop the server and the loop, close the DBs; consent stays.
    pub(crate) fn dispose(self: &Arc<Self>) {
        let was = {
            let mut s = self.st();
            let was = s.enabled;
            s.enabled = false;
            if let Some(t) = &s.install_cancel {
                t.cancel();
            }
            if let Some(t) = &s.distill_cancel {
                t.cancel();
            }
            was
        };
        let bg = self.stop_background();
        self.shutdown(bg);
        self.st().enabled = was;
    }

    /// A workspace was added or its folder changed: look for its sessions soon.
    pub(crate) fn notify_workspaces_changed(self: &Arc<Self>) {
        let ok = {
            let s = self.st();
            s.enabled && !s.installing
        };
        if ok {
            self.schedule_scan();
        }
    }

    fn shutdown(&self, bg: Option<Background>) {
        // The loop sees `enabled`/`paused` and returns between batches.
        if let Some(bg) = bg {
            let _ = bg.thread.join();
        }
        if let Some(rt) = self.runtime.get() {
            rt.stop();
        }
        if let Some(ix) = self.indexer.lock().unwrap().take() {
            ix.close();
        }
        self.st().workspace_stats.clear();
    }

    // ─── Background loop ───────────────────────────────────────────────────

    fn start_background(self: &Arc<Self>) {
        if let Some(old) = self.stop_background() {
            // Not joined here: it sees its stop flag and exits on its own.
            drop(old);
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let watcher = (|| -> Option<notify::RecommendedWatcher> {
            use notify::Watcher;
            let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res
                    && ev
                        .paths
                        .iter()
                        .any(|p| p.extension().is_some_and(|e| e == "jsonl"))
                    && let Some(me) = weak.upgrade()
                {
                    me.schedule_scan();
                }
            })
            .ok()?;
            // No projects dir yet (Claude Code never ran): the interval covers it.
            w.watch(&self.cfg.projects_dir, notify::RecursiveMode::Recursive)
                .ok()?;
            Some(w)
        })();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut s = self.st();
            s.rescan_at = Some(Instant::now() + RESCAN_INTERVAL);
            s.debounce_at = None;
        }
        let me = self.clone();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sg-history-index".into())
            .spawn(move || me.background_loop(&flag))
            .expect("spawn history loop");
        *self.bg.lock().unwrap() = Some(Background {
            stop,
            thread,
            _watcher: watcher,
        });
        self.kick();
    }

    /// Stop the watcher and timers and tell the loop to exit; returns it for joining.
    fn stop_background(&self) -> Option<Background> {
        let bg = self.bg.lock().unwrap().take();
        if let Some(b) = &bg {
            b.stop.store(true, Ordering::SeqCst);
        }
        let mut s = self.st();
        s.debounce_at = None;
        s.rescan_at = None;
        self.cv.notify_all();
        bg
    }

    fn schedule_scan(&self) {
        let mut s = self.st();
        if s.debounce_at.is_none() {
            s.debounce_at = Some(Instant::now() + WATCH_DEBOUNCE);
            self.cv.notify_all();
        }
    }

    /// Run a scan + embed pass now, or right after the current one.
    fn kick(&self) {
        let mut s = self.st();
        if s.should_stop() {
            return;
        }
        s.kick_pending = true;
        self.cv.notify_all();
    }

    fn background_loop(self: Arc<Self>, stop: &AtomicBool) {
        loop {
            {
                let mut s = self.st();
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let now = Instant::now();
                    if s.kick_pending {
                        if !s.should_stop() {
                            break;
                        }
                        s.kick_pending = false;
                    }
                    if s.debounce_at.is_some_and(|d| d <= now) {
                        s.debounce_at = None;
                        s.kick_pending = !s.should_stop();
                        continue;
                    }
                    if s.rescan_at.is_some_and(|r| r <= now) {
                        s.rescan_at = Some(now + RESCAN_INTERVAL);
                        s.kick_pending = !s.should_stop();
                        continue;
                    }
                    let next = [s.debounce_at, s.rescan_at].into_iter().flatten().min();
                    s = match next {
                        Some(t) => self.cv.wait_timeout(s, t - now).unwrap().0,
                        None => self.cv.wait(s).unwrap(),
                    };
                }
            }
            self.run_passes();
            if stop.load(Ordering::SeqCst) {
                return;
            }
            self.run_distill();
        }
    }

    fn should_stop(&self) -> bool {
        self.st().should_stop()
    }

    fn run_passes(self: &Arc<Self>) {
        let indexer = self.indexer();
        let stop = || self.should_stop();
        let result = (|| -> Result<()> {
            loop {
                self.st().kick_pending = false;
                self.set_progress(|p| p.state = ProgressState::Indexing, false);
                indexer.scan(&stop)?;
                if stop() {
                    break;
                }
                self.refresh_counts();
                indexer.embed_pending(&stop, &|| self.refresh_counts())?;
                let again = {
                    let s = self.st();
                    s.kick_pending && !s.should_stop()
                };
                if !again {
                    break;
                }
            }
            self.refresh_counts();
            let (enabled, paused) = {
                let s = self.st();
                (s.enabled, s.paused)
            };
            if enabled {
                self.set_progress(
                    |p| {
                        p.state = if paused {
                            ProgressState::Paused
                        } else {
                            ProgressState::Idle
                        }
                    },
                    true,
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            if !self.st().enabled {
                return;
            }
            self.log(&format!("[history] indexing pass failed: {e}"));
            // Stay enabled: the next watch event or interval retries.
            let msg = e.to_string();
            self.set_progress(
                |p| {
                    p.state = ProgressState::Error;
                    p.error = Some(msg);
                },
                true,
            );
        }
    }

    /// Re-read per-workspace stats and fold them into progress.
    fn refresh_counts(self: &Arc<Self>) {
        let Some(ix) = self.indexer.lock().unwrap().clone() else {
            return;
        };
        let Ok(stats) = ix.stats() else { return };
        let (total, embedded) = stats
            .iter()
            .fold((0, 0), |(t, e), s| (t + s.chunks, e + s.embedded_chunks));
        let enabled = {
            let mut s = self.st();
            s.workspace_stats = stats;
            s.enabled
        };
        if enabled {
            self.set_progress(
                |p| {
                    p.total_chunks = Some(total);
                    p.embedded_chunks = Some(embedded);
                },
                false,
            );
        }
    }

    // ─── Agent queries ─────────────────────────────────────────────────────

    fn ensure_enabled(&self) -> Result<Arc<HistoryIndexer>> {
        if !self.st().enabled {
            bail!("history_disabled");
        }
        Ok(self.indexer())
    }

    pub(crate) fn search(
        &self,
        workspace_id: &str,
        query: &str,
        limit: Option<usize>,
        since: Option<i64>,
    ) -> Result<Vec<SearchHit>> {
        let ix = self.ensure_enabled()?;
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let vector = match self.runtime().embed(&[query.to_string()], EmbedKind::Query) {
            Ok(mut v) if !v.is_empty() => Some(v.remove(0)),
            Ok(_) => None,
            Err(e) => {
                // Runtime unavailable (still installing, crashed): lexical results beat none.
                self.log(&format!(
                    "[history] query embedding failed, lexical only: {e}"
                ));
                None
            }
        };
        ix.search(workspace_id, query, vector, limit, since)
    }

    pub(crate) fn sessions_for_file(
        &self,
        workspace_id: &str,
        file: &str,
        limit: Option<usize>,
    ) -> Result<Vec<FileSession>> {
        self.ensure_enabled()?
            .sessions_for_file(workspace_id, file, limit.unwrap_or(10))
    }

    pub(crate) fn show_chunk(
        &self,
        workspace_id: &str,
        chunk_id: &str,
        radius: Option<usize>,
    ) -> Result<Vec<ShownChunk>> {
        self.ensure_enabled()?
            .show_chunk(workspace_id, chunk_id, radius.unwrap_or(1))
    }

    pub(crate) fn list_sessions(
        &self,
        workspace_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SessionSummary>> {
        self.ensure_enabled()?
            .list_sessions(workspace_id, limit.unwrap_or(20))
    }

    pub(crate) fn why(
        &self,
        workspace_id: &str,
        file: &str,
        line: Option<u32>,
    ) -> Result<Vec<HistoryWhyEntry>> {
        let ix = self.ensure_enabled()?;
        // git needs an absolute path; a relative one is taken as workspace-relative.
        let mut abs = PathBuf::from(file);
        if abs.is_relative()
            && let Some(folder) = (self.cfg.workspaces)()
                .into_iter()
                .find(|w| w.id == workspace_id)
                .and_then(|w: IndexedWorkspace| w.working_directory)
        {
            abs = folder.join(file);
        }
        let commits = commits_for_file(&abs, line);
        let edits = ix.file_edits(workspace_id, file)?;
        Ok(match_edits_to_commits(&commits, &edits, now_ms()))
    }

    // ─── Decision notes ────────────────────────────────────────────────────
    // CONSENT, second level: distillation runs Claude Haiku through the user's
    // Claude Code login. Off unless set_distill_enabled(true), and only while
    // history is enabled and not paused.

    fn set_distill(self: &Arc<Self>, f: impl FnOnce(&mut HistoryDistillStatus), force: bool) {
        let enabled = {
            let mut s = self.st();
            f(&mut s.distill);
            s.enabled
        };
        if enabled {
            self.request_emit(force);
        }
    }

    fn apply_distill_scan(self: &Arc<Self>, scan: &DistillScanResult) {
        self.st().distill_scanned = true;
        self.set_distill(
            |d| {
                d.groups_distilled = scan.groups_distilled;
                d.groups_pending = scan.groups_pending;
                d.groups_failed = scan.groups_failed;
                d.groups_unsettled = scan.groups_unsettled;
                d.notes = scan.notes;
                d.spent_usd = scan.spent_usd;
            },
            false,
        );
    }

    pub(crate) fn set_distill_enabled(self: &Arc<Self>, enabled: bool) {
        let kick = {
            let mut s = self.st();
            if s.distill_on == enabled {
                return;
            }
            s.distill_on = enabled;
            s.distill_retry_at = 0;
            s.distill_backoff_ms = 0;
            if !enabled && let Some(t) = &s.distill_cancel {
                t.cancel();
            }
            enabled && s.enabled && !s.installing
        };
        self.set_distill(
            |d| {
                d.enabled = enabled;
                d.state = if enabled {
                    DistillState::Idle
                } else {
                    DistillState::Off
                };
                d.error = None;
            },
            true,
        );
        if kick {
            self.kick();
        }
    }

    pub(crate) fn distill_estimate(self: &Arc<Self>) -> Result<HistoryDistillEstimate> {
        let ix = self.ensure_enabled()?;
        let scan = ix.distill_scan(self.now(), false)?;
        self.apply_distill_scan(&scan);
        Ok(HistoryDistillEstimate {
            model: DISTILL_MODEL_LABEL.into(),
            price_input_per_m_tok: HAIKU_PRICE_LOW.input,
            price_output_per_m_tok: HAIKU_PRICE_LOW.output,
            pending_groups: scan.plan.len() as u64,
            pending_input_tokens: scan.pending_input_tokens.max(0) as u64,
            pending_output_tokens: scan.pending_output_tokens.max(0) as u64,
            backlog_usd: scan.backlog_usd,
            unsettled_groups: scan.groups_unsettled,
            monthly_groups: scan.monthly_groups.max(0) as u64,
            monthly_input_tokens: scan.monthly_input_tokens.max(0) as u64,
            monthly_output_tokens: scan.monthly_output_tokens.max(0) as u64,
            monthly_usd: scan.monthly_usd,
            spent_usd: scan.spent_usd,
            notes: scan.notes,
        })
    }

    /// Refused while distillation is on: it would immediately pay to redo everything.
    pub(crate) fn delete_notes(self: &Arc<Self>) -> Result<()> {
        let ix = self.ensure_enabled()?;
        if self.st().distill_on {
            bail!("Turn decision notes off before deleting them.");
        }
        ix.delete_notes()?;
        self.refresh_counts();
        let scan = ix.distill_scan(self.now(), false)?;
        self.apply_distill_scan(&scan);
        Ok(())
    }

    fn distill_should_stop(&self) -> bool {
        let s = self.st();
        s.should_stop() || !s.distill_on || s.kick_pending
    }

    /// One distillation pass after each index pass: plan, then distill the
    /// pending groups newest-session first, one at a time. Stops for pause /
    /// disable / off and yields to a scan asked for meanwhile.
    fn run_distill(self: &Arc<Self>) {
        let (distill_on, scanned, retry_at) = {
            let s = self.st();
            if s.should_stop() || s.kick_pending || s.progress.state == ProgressState::Error {
                return;
            }
            (s.distill_on, s.distill_scanned, s.distill_retry_at)
        };
        let ix = self.indexer();
        if !distill_on {
            // Off: still show how many notes exist and what was spent, once per enable.
            if !scanned && let Ok(scan) = ix.distill_scan(self.now(), false) {
                self.apply_distill_scan(&scan);
            }
            return;
        }
        if self.now() < retry_at {
            return;
        }
        let result = (|| -> Result<()> {
            let scan = ix.distill_scan(self.now(), true)?;
            self.apply_distill_scan(&scan);
            for item in &scan.plan {
                if self.distill_should_stop() {
                    break;
                }
                let Some(input) = ix.distill_input(item)? else {
                    continue;
                };
                if self.distill_should_stop() {
                    break;
                }
                self.set_distill(
                    |d| {
                        d.state = DistillState::Running;
                        d.error = None;
                    },
                    false,
                );
                let token = CancelToken::new();
                self.st().distill_cancel = Some(token.clone());
                if self.distill_should_stop() {
                    token.cancel();
                }
                let seqs = input.seqs.iter().copied().collect();
                let result = self.runner().run(&input.text, &token, &seqs);
                self.st().distill_cancel = None;
                if matches!(result, DistillRunResult::Failed { aborted: true, .. }) {
                    break;
                }
                let written = ix.distill_commit(item, &input, &result, self.now())?;
                let cost = result.usage().map_or(0.0, |u| u.cost_usd);
                match &result {
                    DistillRunResult::Ok { .. } => {
                        self.st().distill_backoff_ms = 0;
                        self.set_distill(
                            |d| {
                                d.spent_usd += cost;
                                d.notes += written as u64;
                                d.groups_distilled += 1;
                                d.groups_pending = d.groups_pending.saturating_sub(1);
                            },
                            false,
                        );
                        // Embed the new notes now so they are searchable right away.
                        if written > 0 {
                            let _ =
                                ix.embed_pending(&|| self.should_stop(), &|| self.refresh_counts());
                        }
                    }
                    DistillRunResult::Failed {
                        error,
                        input_specific,
                        ..
                    } => {
                        self.log(&format!(
                            "[history] distilling {} group {} failed: {error}",
                            item.session_id, item.group
                        ));
                        self.set_distill(|d| d.spent_usd += cost, false);
                        if *input_specific {
                            continue; // counted as an attempt; the next group may be fine
                        }
                        let now = self.now();
                        {
                            let mut s = self.st();
                            s.distill_backoff_ms = (s.distill_backoff_ms * 2)
                                .clamp(DISTILL_BACKOFF_MIN_MS, DISTILL_BACKOFF_MAX_MS);
                            s.distill_retry_at = now + s.distill_backoff_ms;
                        }
                        let error = error.clone();
                        self.set_distill(
                            |d| {
                                d.state = DistillState::Waiting;
                                d.error = Some(error);
                            },
                            true,
                        );
                        return Ok(());
                    }
                }
            }
            // A pass that yielded or was stopped is re-planned by the next one.
            if !self.distill_should_stop() {
                let scan = ix.distill_scan(self.now(), false)?;
                self.apply_distill_scan(&scan);
            }
            self.set_distill(
                |d| {
                    d.state = DistillState::Idle;
                    d.error = None;
                },
                true,
            );
            Ok(())
        })();
        if let Err(e) = result {
            if !self.st().enabled {
                return;
            }
            self.log(&format!("[history] distillation pass failed: {e}"));
            let msg = e.to_string();
            self.set_distill(
                |d| {
                    d.state = DistillState::Error;
                    d.error = Some(msg);
                },
                true,
            );
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
