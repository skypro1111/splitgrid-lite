// Port of the service / indexer parts of src/main/history/indexer.test.ts and
// of distill-service.test.ts. All fixtures are synthetic and live in a
// tempdir; the runtime and the distillation runner are fakes, so nothing is
// downloaded and no llama-server or claude process is ever started.

use super::*;
use crate::distill::{
    DISTILL_OUTPUT_TOKENS_PER_GROUP, DISTILL_PROMPT_OVERHEAD_TOKENS, DISTILL_SETTLE_MS,
    DistillUsage, DistilledNote, call_cost_usd, tokens_for_chars,
};
use crate::runtime::Embedder;
use crate::{ChunkKind, DecisionNoteKind, HISTORY_EMBED_DIM, HistoryService, NoteRef};
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, AtomicUsize};

const WS_A: &str = "/work/alpha";
const WS_B: &str = "/work/beta";
const T0: i64 = 1_772_323_200_000; // 2026-03-01T00:00:00.000Z

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Deterministic fake embedding keyed on the words (fakeVector in the TS tests).
fn fake_vector(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; HISTORY_EMBED_DIM];
    let lower = text.to_lowercase();
    for w in lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
    {
        let mut h: u32 = 0;
        for u in w.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(u32::from(u));
        }
        v[(h as usize) % HISTORY_EMBED_DIM] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let n = if n > 0.0 { n } else { 1.0 };
    v.iter().map(|x| x / n).collect()
}

#[derive(Default)]
struct FakeRuntime {
    installed: AtomicBool,
    installs: AtomicUsize,
    embed_calls: AtomicUsize,
    fail_install: Mutex<Option<String>>,
    create_on_install: Mutex<Option<PathBuf>>,
    fail_embed: AtomicBool,
    /// While true, embed() blocks.
    gate: Mutex<bool>,
    gate_cv: Condvar,
}

impl Embedder for FakeRuntime {
    fn embed(&self, texts: &[String], _kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let mut g = self.gate.lock().unwrap();
        while *g {
            g = self.gate_cv.wait(g).unwrap();
        }
        drop(g);
        if self.fail_embed.load(Ordering::SeqCst) {
            bail!("server down");
        }
        self.embed_calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|t| fake_vector(t)).collect())
    }
}

impl RuntimeLike for FakeRuntime {
    fn is_supported(&self) -> bool {
        true
    }
    fn is_installed(&self) -> bool {
        self.installed.load(Ordering::SeqCst)
    }
    fn download_bytes(&self) -> u64 {
        1000
    }
    fn install(&self, _cancel: &CancelToken, _p: &mut dyn FnMut(InstallProgress)) -> Result<()> {
        if let Some(dir) = self.create_on_install.lock().unwrap().as_ref() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(e) = self.fail_install.lock().unwrap().clone() {
            bail!("{e}");
        }
        std::thread::sleep(Duration::from_millis(20));
        self.installs.fetch_add(1, Ordering::SeqCst);
        self.installed.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn uninstall(&self) -> Result<()> {
        self.installed.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) {}
}

type Answer =
    Box<dyn Fn(&str, usize, &CancelToken, &HashSet<i64>) -> DistillRunResult + Send + Sync>;

/// Records every run; by default answers with one decision citing the first seq.
struct FakeRunner {
    inputs: Mutex<Vec<String>>,
    answer: Mutex<Option<Answer>>,
}

fn default_answer(input: &str, cancel: &CancelToken, seqs: &HashSet<i64>) -> DistillRunResult {
    if cancel.is_cancelled() {
        return DistillRunResult::aborted();
    }
    let seq = *seqs.iter().min().unwrap();
    DistillRunResult::Ok {
        notes: vec![DistilledNote {
            kind: DecisionNoteKind::Decision,
            title: format!(
                "Chose the {} approach",
                if input.contains("alpha") {
                    "alpha"
                } else {
                    "other"
                }
            ),
            why: "Because of the tests.".into(),
            files: vec![],
            sources: vec![seq],
        }],
        usage: DistillUsage {
            cost_usd: 0.001,
            input_tokens: 1000.0,
            output_tokens: 50.0,
            model: None,
        },
    }
}

fn block_until_cancelled(cancel: &CancelToken) -> DistillRunResult {
    while !cancel.is_cancelled() {
        std::thread::sleep(Duration::from_millis(5));
    }
    DistillRunResult::aborted()
}

impl FakeRunner {
    fn new(answer: Option<Answer>) -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(vec![]),
            answer: Mutex::new(answer),
        })
    }
    fn count(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

impl DistillRunner for FakeRunner {
    fn run(&self, input: &str, cancel: &CancelToken, seqs: &HashSet<i64>) -> DistillRunResult {
        let n = {
            let mut i = self.inputs.lock().unwrap();
            i.push(input.to_string());
            i.len()
        };
        let answer = self.answer.lock().unwrap();
        match answer.as_ref() {
            Some(f) => f(input, n, cancel, seqs),
            None => default_answer(input, cancel, seqs),
        }
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    projects: PathBuf,
    clock: AtomicI64,
    now: Arc<AtomicI64>,
    workspaces: Arc<Mutex<Vec<IndexedWorkspace>>>,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let projects = root.join("projects");
        let fx = Self {
            _dir: dir,
            root,
            projects,
            clock: AtomicI64::new(T0),
            now: Arc::new(AtomicI64::new(0)),
            workspaces: Arc::new(Mutex::new(vec![
                IndexedWorkspace {
                    id: "ws-a".into(),
                    working_directory: Some(WS_A.into()),
                },
                IndexedWorkspace {
                    id: "ws-b".into(),
                    working_directory: Some(WS_B.into()),
                },
                IndexedWorkspace {
                    id: "ws-none".into(),
                    working_directory: None,
                },
            ])),
        };
        std::fs::create_dir_all(fx.projects.join(claude_project_slug(WS_A))).unwrap();
        std::fs::create_dir_all(fx.projects.join(claude_project_slug(WS_B))).unwrap();
        fx
    }

    fn line(&self, kind: &str, text: &str, cwd: &str, sid: &str) -> String {
        let ts = self.clock.fetch_add(1000, Ordering::SeqCst) + 1000;
        let message = if kind == "user" {
            serde_json::json!({ "role": "user", "content": text })
        } else {
            serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
        };
        serde_json::json!({ "type": kind, "message": message, "cwd": cwd, "sessionId": sid, "timestamp": iso(ts), "uuid": format!("u{ts}") })
            .to_string()
            + "\n"
    }

    fn turn(&self, prompt: &str, answer: &str, cwd: &str, sid: &str) -> String {
        self.line("user", prompt, cwd, sid) + &self.line("assistant", answer, cwd, sid)
    }

    fn file(&self, ws: &str, sid: &str) -> PathBuf {
        self.projects
            .join(claude_project_slug(ws))
            .join(format!("{sid}.jsonl"))
    }

    fn write(&self, ws: &str, sid: &str, content: &str) {
        std::fs::write(self.file(ws, sid), content).unwrap();
    }

    fn append(&self, ws: &str, sid: &str, content: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.file(ws, sid))
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
    }

    fn seed_index_fixtures(&self) {
        let a = self.turn(
            "why do we write the settings file atomically",
            "Because a crash mid-write left it truncated.",
            &format!("{WS_A}/src"),
            "s-alpha",
        );
        self.write(WS_A, "s-alpha", &a);
        let b = self.turn(
            "set up the database migration for invoices",
            "Added the invoices migration.",
            WS_B,
            "s-beta",
        );
        self.write(WS_B, "s-beta", &b);
    }

    fn seed_distill_fixtures(&self) {
        self.write(
            WS_A,
            "s-alpha",
            &self.turn(
                "alpha: how should the settings file be written",
                "Atomically, via rename.",
                WS_A,
                "s-alpha",
            ),
        );
        self.write(
            WS_B,
            "s-beta",
            &self.turn(
                "beta: plan the invoices migration please",
                "Added the invoices migration.",
                WS_B,
                "s-beta",
            ),
        );
        *self.workspaces.lock().unwrap() = vec![
            IndexedWorkspace {
                id: "ws-a".into(),
                working_directory: Some(WS_A.into()),
            },
            IndexedWorkspace {
                id: "ws-b".into(),
                working_directory: Some(WS_B.into()),
            },
        ];
        // Every fixture session has settled.
        self.now.store(
            self.clock.load(Ordering::SeqCst) + DISTILL_SETTLE_MS * 10,
            Ordering::SeqCst,
        );
    }

    fn config(&self) -> HistoryConfig {
        let ws = self.workspaces.clone();
        HistoryConfig {
            root_dir: self.root.join("history"),
            projects_dir: self.projects.clone(),
            workspaces: Arc::new(move || ws.lock().unwrap().clone()),
            claude_binary: Arc::new(|| None),
            log: Arc::new(|_| {}),
        }
    }

    fn service(
        &self,
        runtime: Arc<FakeRuntime>,
        runner: Option<Arc<FakeRunner>>,
    ) -> HistoryService {
        let now = self.now.clone();
        HistoryService::with_deps(
            self.config(),
            ServiceDeps {
                runtime: Some(runtime),
                distill_runner: runner.map(|r| r as Arc<dyn DistillRunner>),
                now: Some(Arc::new(move || {
                    let n = now.load(Ordering::SeqCst);
                    if n == 0 { now_ms() } else { n }
                })),
            },
        )
    }

    fn indexer(&self, embedder: Arc<dyn Embedder>, batch: usize) -> HistoryIndexer {
        let ws = self.workspaces.clone();
        HistoryIndexer::new(IndexerOptions {
            projects_dir: self.projects.clone(),
            data_dir: self.root.join("db"),
            workspaces: Arc::new(move || ws.lock().unwrap().clone()),
            embedder,
            batch_size: batch,
            log: Arc::new(|_| {}),
        })
    }
}

#[track_caller]
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn state(s: &HistoryService) -> ProgressState {
    s.status().progress.state
}

fn distill(s: &HistoryService) -> HistoryDistillStatus {
    s.status().distill.unwrap()
}

fn enabled(s: &HistoryService) {
    s.enable().unwrap();
    wait_for("idle", || state(s) == ProgressState::Idle);
}

// ─── HistoryIndexer ──────────────────────────────────────────────────────────

fn sessions(ix: &HistoryIndexer, ws: &str) -> Vec<String> {
    ix.store(ws, false)
        .unwrap()
        .unwrap()
        .list_sessions(10, None)
        .unwrap()
        .into_iter()
        .map(|s| s.meta.id)
        .collect()
}

#[test]
fn indexer_routes_sessions_by_cwd() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 16);
    assert_eq!(ix.scan(&|| false).unwrap(), 2);
    assert_eq!(sessions(&ix, "ws-a"), vec!["s-alpha"]);
    assert_eq!(sessions(&ix, "ws-b"), vec!["s-beta"]);
    assert!(ix.store("ws-none", false).unwrap().is_none());
    ix.close();
}

#[test]
fn indexer_skips_unchanged_and_adds_only_new_chunks() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 16);
    ix.scan(&|| false).unwrap();
    ix.embed_pending(&|| false, &|| {}).unwrap();
    assert_eq!(ix.scan(&|| false).unwrap(), 0);
    ix.close();
    assert!(fx.root.join("db/files.json").exists());

    // A fresh indexer (app restart) reuses files.json and the DB file states.
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 16);
    assert_eq!(ix.scan(&|| false).unwrap(), 0);
    fx.append(
        WS_A,
        "s-alpha",
        &fx.turn(
            "and what about the workspace file",
            "Same helper, writeFileAtomicSync.",
            WS_A,
            "s-alpha",
        ),
    );
    assert_eq!(ix.scan(&|| false).unwrap(), 1);
    assert_eq!(ix.progress().unwrap(), (3, 2));
    ix.close();
}

#[test]
fn indexer_embeds_and_stops_between_batches() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 1);
    ix.scan(&|| false).unwrap();
    let batches = AtomicUsize::new(0);
    ix.embed_pending(&|| batches.load(Ordering::SeqCst) >= 1, &|| {
        batches.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    assert_eq!(ix.progress().unwrap().1, 1);
    ix.embed_pending(&|| false, &|| {}).unwrap();
    assert_eq!(ix.progress().unwrap(), (2, 2));
    ix.close();
}

#[test]
fn indexer_keeps_history_after_transcript_deletion() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 16);
    ix.scan(&|| false).unwrap();
    std::fs::remove_file(fx.file(WS_A, "s-alpha")).unwrap();
    ix.scan(&|| false).unwrap();
    assert_eq!(sessions(&ix, "ws-a").len(), 1);
    let seen = std::fs::read_to_string(fx.root.join("db/files.json")).unwrap();
    assert!(!seen.contains("s-alpha"), "bookkeeping entry dropped");
    ix.close();
}

#[test]
fn files_json_uses_the_electron_shape() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let ix = fx.indexer(Arc::new(FakeRuntime::default()), 16);
    ix.scan(&|| false).unwrap();
    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.root.join("db/files.json")).unwrap())
            .unwrap();
    let entry = &seen[path_str(&fx.file(WS_B, "s-beta"))];
    assert!(entry["size"].as_u64().unwrap() > 0);
    assert!(entry["mtimeMs"].as_f64().unwrap() > 0.0);
    assert_eq!(entry["cwds"], serde_json::json!([WS_B]));
    assert!(fx.root.join("db/ws-b.sqlite").exists());
    ix.close();
}

// ─── WorkspaceHistoryService ─────────────────────────────────────────────────

#[test]
fn touches_nothing_before_consent() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let rt = Arc::new(FakeRuntime::default());
    let svc = fx.service(rt.clone(), None);
    let st = svc.status();
    assert!(!st.enabled);
    assert_eq!(st.progress.state, ProgressState::Off);
    let est = svc.estimate().unwrap();
    assert_eq!(est.transcript_files, 2);
    assert_eq!(est.download_bytes, 1000);
    assert!(est.first_import_minutes >= 1.0);
    assert_eq!(rt.installs.load(Ordering::SeqCst), 0);
    assert!(!fx.root.join("history").exists());
    assert_eq!(
        svc.search("ws-a", "atomic", None, None)
            .unwrap_err()
            .to_string(),
        "history_disabled"
    );
    svc.set_distill_enabled(true);
    assert!(!fx.root.join("history").exists());
    assert!(svc.inner.bg.lock().unwrap().is_none(), "no background loop");
    assert!(!svc.inner.emit.lock().unwrap().running, "no status thread");
}

#[test]
fn enable_installs_once_indexes_and_scopes_search() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let rt = Arc::new(FakeRuntime::default());
    let svc = fx.service(rt.clone(), None);
    let pushes = Arc::new(AtomicUsize::new(0));
    let p = pushes.clone();
    svc.on_status(Box::new(move |_| {
        p.fetch_add(1, Ordering::SeqCst);
    }));
    let (a, b) = (svc.clone(), svc.clone());
    let t1 = std::thread::spawn(move || a.enable());
    let t2 = std::thread::spawn(move || b.enable());
    t1.join().unwrap().unwrap();
    t2.join().unwrap().unwrap();
    assert_eq!(rt.installs.load(Ordering::SeqCst), 1);
    wait_for("idle", || state(&svc) == ProgressState::Idle);
    let hits = svc
        .search(
            "ws-a",
            "why is the settings file written atomically",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        hits.iter()
            .map(|h| h.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s-alpha"]
    );
    assert!(
        !svc.search("ws-b", "settings file atomically", None, None)
            .unwrap()
            .iter()
            .any(|h| h.session_id == "s-alpha")
    );
    let mut ids: Vec<String> = svc
        .status()
        .workspaces
        .into_iter()
        .map(|w| w.workspace_id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["ws-a", "ws-b"]);
    wait_for("a status push", || pushes.load(Ordering::SeqCst) > 0);
    svc.disable(false).unwrap();
    assert!(svc.inner.bg.lock().unwrap().is_none());
    assert_eq!(state(&svc), ProgressState::Off);
}

#[test]
fn falls_back_to_lexical_when_query_cannot_be_embedded() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let rt = Arc::new(FakeRuntime::default());
    let svc = fx.service(rt.clone(), None);
    enabled(&svc);
    rt.fail_embed.store(true, Ordering::SeqCst);
    let hits = svc
        .search("ws-b", "\"invoices migration\"", None, None)
        .unwrap();
    assert_eq!(hits[0].session_id, "s-beta");
    svc.disable(false).unwrap();
}

#[test]
fn failed_install_is_an_error_and_stays_off() {
    let fx = Fx::new();
    let rt = Arc::new(FakeRuntime::default());
    *rt.fail_install.lock().unwrap() = Some("sha256 mismatch".into());
    let svc = fx.service(rt, None);
    assert!(
        svc.enable()
            .unwrap_err()
            .to_string()
            .contains("sha256 mismatch")
    );
    let st = svc.status();
    assert!(!st.enabled);
    assert_eq!(st.progress.state, ProgressState::Error);
    assert_eq!(st.progress.error.as_deref(), Some("sha256 mismatch"));
}

#[test]
fn disable_with_delete_removes_everything_even_after_failed_enable() {
    let fx = Fx::new();
    let rt = Arc::new(FakeRuntime::default());
    *rt.fail_install.lock().unwrap() = Some("network".into());
    *rt.create_on_install.lock().unwrap() = Some(fx.root.join("history/runtime"));
    let svc = fx.service(rt, None);
    assert!(svc.enable().unwrap_err().to_string().contains("network"));
    assert!(fx.root.join("history").exists());
    svc.disable(true).unwrap();
    assert!(!fx.root.join("history").exists());
    assert_eq!(state(&svc), ProgressState::Off);
}

#[test]
fn pause_stops_indexing_and_resume_finishes() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let rt = Arc::new(FakeRuntime::default());
    *rt.gate.lock().unwrap() = true;
    let svc = fx.service(rt.clone(), None);
    svc.enable().unwrap();
    svc.pause();
    *rt.gate.lock().unwrap() = false;
    rt.gate_cv.notify_all();
    wait_for("paused", || state(&svc) == ProgressState::Paused);
    std::thread::sleep(Duration::from_millis(50));
    assert!(svc.status().progress.embedded_chunks.unwrap_or(0) < 2);
    svc.resume();
    wait_for("idle with all embedded", || {
        let p = svc.status().progress;
        p.state == ProgressState::Idle && p.embedded_chunks == Some(2)
    });
    svc.disable(false).unwrap();
}

#[test]
fn resume_if_enabled_brings_the_service_back() {
    let fx = Fx::new();
    fx.seed_index_fixtures();
    let svc = fx.service(Arc::new(FakeRuntime::default()), None);
    svc.resume_if_enabled(false, false);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!svc.status().enabled);
    svc.resume_if_enabled(true, false);
    wait_for("idle", || state(&svc) == ProgressState::Idle);
    assert!(svc.status().enabled);
    svc.dispose();
    assert!(svc.status().enabled, "dispose keeps consent");
    svc.disable(false).unwrap();
}

#[test]
fn queries_show_list_file_and_why() {
    let fx = Fx::new();
    fx.write(WS_A, "s-edit", &{
        let mut s = fx.line("user", "rename the helper in the settings module", WS_A, "s-edit");
        let ts = fx.clock.fetch_add(1000, Ordering::SeqCst) + 1000;
        s += &(serde_json::json!({ "type": "assistant", "cwd": WS_A, "sessionId": "s-edit", "timestamp": iso(ts), "uuid": "edit-1",
            "message": { "role": "assistant", "content": [
                { "type": "text", "text": "Renamed it." },
                { "type": "tool_use", "id": "t1", "name": "Edit", "input": { "file_path": "src/settings.ts" } }
            ] } })
        .to_string()
            + "\n");
        s
    });
    let svc = fx.service(Arc::new(FakeRuntime::default()), None);
    enabled(&svc);
    let files = svc
        .sessions_for_file("ws-a", "src/settings.ts", None)
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].session_id, "s-edit");
    assert_eq!(files[0].chunk_id, "s-edit:0");
    let shown = svc.show_chunk("ws-a", "s-edit:0", None).unwrap();
    assert!(shown[0].text.contains("[Edit] src/settings.ts"));
    let list = svc.list_sessions("ws-a", None).unwrap();
    assert_eq!(list[0].session_id, "s-edit");
    assert_eq!(list[0].chunks, 1);
    // Not a git repo: no commits, no error.
    assert!(svc.why("ws-a", "src/settings.ts", None).unwrap().is_empty());
    assert!(svc.list_sessions("ws-unknown", None).unwrap().is_empty());
    svc.disable(false).unwrap();
}

// ─── Decision notes ──────────────────────────────────────────────────────────

#[test]
fn distill_is_off_by_default() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(None);
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    wait_for("pending counted", || distill(&svc).groups_pending == 2);
    let d = distill(&svc);
    assert!(!d.enabled);
    assert_eq!(d.state, DistillState::Off);
    assert_eq!(runner.count(), 0);
    svc.disable(false).unwrap();
}

#[test]
fn distill_estimate_from_the_db() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(None);
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    let est = svc.distill_estimate().unwrap();
    assert_eq!(runner.count(), 0);
    assert_eq!(est.pending_groups, 2);
    assert_eq!(
        est.pending_output_tokens,
        2 * DISTILL_OUTPUT_TOKENS_PER_GROUP as u64
    );
    let ix_root = HistoryIndexer::new(IndexerOptions {
        projects_dir: fx.projects.clone(),
        data_dir: fx.root.join("history/db"),
        workspaces: fx.config().workspaces,
        embedder: Arc::new(FakeRuntime::default()),
        batch_size: 16,
        log: Arc::new(|_| {}),
    });
    let chars: Vec<i64> = ["ws-a", "ws-b"]
        .iter()
        .map(|id| {
            ix_root
                .store(id, false)
                .unwrap()
                .unwrap()
                .distill_candidates()
                .unwrap()[0]
                .chunks
                .iter()
                .map(|c| c.chars + 10)
                .sum()
        })
        .collect();
    ix_root.close();
    let inputs: Vec<i64> = chars
        .iter()
        .map(|&c| tokens_for_chars(c) + DISTILL_PROMPT_OVERHEAD_TOKENS)
        .collect();
    assert_eq!(est.pending_input_tokens, (inputs[0] + inputs[1]) as u64);
    let expected: f64 = inputs
        .iter()
        .map(|&t| call_cost_usd(t as f64, DISTILL_OUTPUT_TOKENS_PER_GROUP as f64))
        .sum();
    assert!((est.backlog_usd - expected).abs() < 1e-10);
    assert_eq!(est.price_input_per_m_tok, 0.1);
    assert_eq!(est.price_output_per_m_tok, 0.5);
    assert_eq!(est.monthly_groups, 2);
    assert!(est.monthly_usd > 0.0);
    assert_eq!(est.spent_usd, 0.0);
    assert_eq!(est.model, "Claude Haiku 5.5");

    fx.now.fetch_add(60 * 86_400_000, Ordering::SeqCst);
    assert_eq!(svc.distill_estimate().unwrap().monthly_groups, 0);
    svc.disable(false).unwrap();
}

#[test]
fn distills_each_group_once_into_searchable_notes() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(None);
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("2 distilled", || {
        let d = distill(&svc);
        d.state == DistillState::Idle
            && d.groups_distilled == 2
            && d.groups_pending == 0
            && d.notes == 2
    });
    assert_eq!(runner.count(), 2);
    assert!(runner.inputs.lock().unwrap()[0].contains("[#0]\nUser: "));
    assert!((distill(&svc).spent_usd - 0.002).abs() < 1e-9);

    let hits = svc
        .search("ws-a", "Chose the alpha approach", None, None)
        .unwrap();
    assert_eq!(hits[0].kind, ChunkKind::Decision);
    assert_eq!(hits[0].chunk_id, "s-alpha:n0.0");
    assert_eq!(
        hits[0].note,
        Some(NoteRef {
            kind: DecisionNoteKind::Decision,
            sources: vec!["s-alpha:0".into()]
        })
    );
    let shown = svc.show_chunk("ws-a", "s-alpha:n0.0", None).unwrap();
    assert_eq!(
        shown
            .iter()
            .map(|c| c.chunk_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s-alpha:n0.0"]
    );
    assert_eq!(svc.list_sessions("ws-a", None).unwrap()[0].chunks, 1);

    // Same input: never paid for twice.
    svc.set_distill_enabled(false);
    svc.set_distill_enabled(true);
    wait_for("idle", || distill(&svc).state == DistillState::Idle);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(runner.count(), 2);
    svc.disable(false).unwrap();
}

#[test]
fn redistills_only_changed_groups_once_settled() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(None);
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("2 distilled", || distill(&svc).groups_distilled == 2);

    fx.append(
        WS_A,
        "s-alpha",
        &fx.turn(
            "alpha: and also make the writes fsync",
            "Added fsync before rename.",
            WS_A,
            "s-alpha",
        ),
    );
    fx.now
        .store(fx.clock.load(Ordering::SeqCst) + 60_000, Ordering::SeqCst); // just moved: not settled
    svc.set_distill_enabled(false);
    svc.set_distill_enabled(true);
    wait_for("unsettled", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.groups_unsettled == 1
    });
    assert_eq!(runner.count(), 2);
    assert_eq!(distill(&svc).notes, 2);

    fx.now.store(
        fx.clock.load(Ordering::SeqCst) + DISTILL_SETTLE_MS + 1,
        Ordering::SeqCst,
    );
    svc.set_distill_enabled(false);
    svc.set_distill_enabled(true);
    wait_for("third run", || runner.count() == 3);
    assert!(runner.inputs.lock().unwrap()[2].contains("make the writes fsync"));
    wait_for("settled again", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.groups_distilled == 2 && d.groups_unsettled == 0
    });
    svc.disable(false).unwrap();
}

#[test]
fn overlapping_workspaces_share_one_run() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    *fx.workspaces.lock().unwrap() = vec![
        IndexedWorkspace {
            id: "ws-a".into(),
            working_directory: Some(WS_A.into()),
        },
        IndexedWorkspace {
            id: "ws-parent".into(),
            working_directory: Some("/work".into()),
        },
    ];
    let runner = FakeRunner::new(None);
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("done", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.groups_pending == 0 && d.groups_distilled == 2
    });
    assert_eq!(runner.count(), 2);
    assert_eq!(
        svc.search("ws-a", "Chose the alpha approach", None, None)
            .unwrap()[0]
            .kind,
        ChunkKind::Decision
    );
    assert_eq!(
        svc.search("ws-parent", "Chose the alpha approach", None, None)
            .unwrap()[0]
            .kind,
        ChunkKind::Decision
    );
    assert!((distill(&svc).spent_usd - 0.002).abs() < 1e-9);
    svc.disable(false).unwrap();
}

#[test]
fn pause_aborts_the_run_and_resume_continues() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let block = Arc::new(AtomicBool::new(true));
    let started = Arc::new(AtomicUsize::new(0));
    let (b, s) = (block.clone(), started.clone());
    let runner = FakeRunner::new(Some(Box::new(move |input, _n, cancel, seqs| {
        if !b.load(Ordering::SeqCst) {
            return default_answer(input, cancel, seqs);
        }
        s.fetch_add(1, Ordering::SeqCst);
        block_until_cancelled(cancel)
    })));
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("first run", || started.load(Ordering::SeqCst) == 1);
    wait_for("running", || distill(&svc).state == DistillState::Running);
    svc.pause();
    assert_eq!(distill(&svc).state, DistillState::Paused);
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(distill(&svc).groups_distilled, 0);

    block.store(false, Ordering::SeqCst);
    svc.resume();
    wait_for("both distilled", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.groups_distilled == 2
    });
    svc.disable(false).unwrap();
}

#[test]
fn disabling_history_aborts_the_run_and_turns_distill_off() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let aborted = Arc::new(AtomicBool::new(false));
    let a = aborted.clone();
    let runner = FakeRunner::new(Some(Box::new(move |_i, _n, cancel, _s| {
        let r = block_until_cancelled(cancel);
        a.store(true, Ordering::SeqCst);
        r
    })));
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("started", || runner.count() == 1);
    svc.disable(false).unwrap();
    assert!(aborted.load(Ordering::SeqCst));
    let d = distill(&svc);
    assert!(!d.enabled);
    assert_eq!(d.state, DistillState::Off);
    assert_eq!(runner.count(), 1);
}

#[test]
fn turning_distill_off_aborts_and_keeps_notes() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(Some(Box::new(|input, n, cancel, seqs| {
        if n == 1 {
            default_answer(input, cancel, seqs)
        } else {
            block_until_cancelled(cancel)
        }
    })));
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("second run", || runner.count() == 2);
    svc.set_distill_enabled(false);
    wait_for("off with one note", || {
        let d = distill(&svc);
        !d.enabled && d.state == DistillState::Off && d.notes == 1
    });
    let a = svc
        .search("ws-a", "Chose the alpha approach", None, None)
        .unwrap();
    let b = svc
        .search("ws-b", "Chose the other approach", None, None)
        .unwrap();
    assert!(
        a.iter()
            .chain(b.iter())
            .any(|h| h.kind == ChunkKind::Decision)
    );
    svc.disable(false).unwrap();
}

#[test]
fn non_input_failure_backs_off() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let runner = FakeRunner::new(Some(Box::new(|_i, _n, _c, _s| {
        DistillRunResult::failed("Claude AI usage limit reached", false)
    })));
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("waiting", || distill(&svc).state == DistillState::Waiting);
    assert!(distill(&svc).error.unwrap().contains("usage limit"));
    assert_eq!(runner.count(), 1);
    svc.pause();
    svc.resume();
    wait_for("idle", || state(&svc) == ProgressState::Idle);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(runner.count(), 1);
    svc.disable(false).unwrap();
}

#[test]
fn malformed_output_gives_up_after_three_attempts() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    *fx.workspaces.lock().unwrap() = vec![IndexedWorkspace {
        id: "ws-a".into(),
        working_directory: Some(WS_A.into()),
    }];
    let runner = FakeRunner::new(Some(Box::new(|_i, _n, _c, _s| DistillRunResult::Failed {
        error: "Claude Code returned no structured notes".into(),
        input_specific: true,
        aborted: false,
        usage: Some(DistillUsage {
            cost_usd: 0.0005,
            input_tokens: 900.0,
            output_tokens: 10.0,
            model: None,
        }),
    })));
    let svc = fx.service(Arc::new(FakeRuntime::default()), Some(runner.clone()));
    enabled(&svc);
    for i in 1..=3 {
        svc.set_distill_enabled(true);
        wait_for("a run", || runner.count() == i);
        wait_for("idle", || distill(&svc).state == DistillState::Idle);
        svc.set_distill_enabled(false);
    }
    svc.set_distill_enabled(true);
    wait_for("given up", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.groups_failed == 1 && d.groups_pending == 0
    });
    assert_eq!(runner.count(), 3);
    assert!((distill(&svc).spent_usd - 0.0015).abs() < 1e-9);
    svc.disable(false).unwrap();
}

#[test]
fn delete_notes_refused_while_on_keeps_spent() {
    let fx = Fx::new();
    fx.seed_distill_fixtures();
    let svc = fx.service(
        Arc::new(FakeRuntime::default()),
        Some(FakeRunner::new(None)),
    );
    enabled(&svc);
    svc.set_distill_enabled(true);
    wait_for("notes", || {
        let d = distill(&svc);
        d.state == DistillState::Idle && d.notes == 2
    });
    assert!(svc.delete_notes().unwrap_err().to_string().contains("off"));
    svc.set_distill_enabled(false);
    svc.delete_notes().unwrap();
    let d = distill(&svc);
    assert_eq!((d.notes, d.groups_distilled, d.groups_pending), (0, 0, 2));
    assert!((d.spent_usd - 0.002).abs() < 1e-9);
    assert!(
        !svc.search("ws-a", "Chose the alpha approach", None, None)
            .unwrap()
            .iter()
            .any(|h| h.kind == ChunkKind::Decision)
    );
    svc.disable(false).unwrap();
}

#[test]
fn project_slug_matches_claude_code() {
    assert_eq!(claude_project_slug("/work/alpha"), "-work-alpha");
    assert_eq!(
        claude_project_slug("/Users/me/My Drive/x.y"),
        "-Users-me-My-Drive-x-y"
    );
    assert_eq!(claude_project_slug("/a/😀"), "-a---");
}
