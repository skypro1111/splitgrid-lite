//! Workspace history (the Electron app's src/main/history): an OPT-IN archive
//! of this machine's Claude Code sessions, one index per workspace, searchable
//! by agents through `sg-history`.
//!
//! CONSENT: building a [`HistoryService`] does nothing. Until `enable()` it
//! downloads nothing, spawns nothing, watches nothing and writes nothing;
//! `estimate()` only stat()s transcript files.
//!
//! DATA IS SHARED WITH THE ELECTRON APP: `<data dir>/history` (runtime/, db/)
//! is the same folder, with the same file names, SQLite schema and
//! `user_version`, and the same pinned llama.cpp build + model, so either app
//! can open what the other wrote. A DB with a newer schema is left alone.
//!
//! API CONTRACT: the app (src/history_ui, src/agent/history_cmd.rs) is
//! written against these signatures. Every method may block (disk, git,
//! llama-server): call them off the UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod distill;
pub mod git_why;
pub mod indexer;
pub mod runtime;
mod service;
pub mod store;
pub mod transcripts;
mod util;

pub use service::{ServiceDeps, claude_project_slug};
pub use util::{CancelToken, is_cancelled_error};

/// Embedding width stored and searched (EmbeddingGemma, MRL-truncated).
pub const HISTORY_EMBED_DIM: usize = 256;

/// A workspace whose folder sessions belong to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedWorkspace {
    pub id: String,
    pub working_directory: Option<PathBuf>,
}

pub struct HistoryConfig {
    /// `<data dir>/history`.
    pub root_dir: PathBuf,
    /// `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`.
    pub projects_dir: PathBuf,
    /// The current workspaces, read whenever the indexer needs them.
    pub workspaces: Arc<dyn Fn() -> Vec<IndexedWorkspace> + Send + Sync>,
    /// AppSettings.claudeBinaryPath (None/empty = find `claude`), for decision notes.
    pub claude_binary: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Turn,
    CompactSummary,
    Decision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionNoteKind {
    Decision,
    Rejected,
    Gotcha,
    Convention,
}

impl ChunkKind {
    /// The `chunks.kind` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            ChunkKind::Turn => "turn",
            ChunkKind::CompactSummary => "compact_summary",
            ChunkKind::Decision => "decision",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "turn" => Some(ChunkKind::Turn),
            "compact_summary" => Some(ChunkKind::CompactSummary),
            "decision" => Some(ChunkKind::Decision),
            _ => None,
        }
    }
}

impl DecisionNoteKind {
    pub const ALL: [DecisionNoteKind; 4] = [
        DecisionNoteKind::Decision,
        DecisionNoteKind::Rejected,
        DecisionNoteKind::Gotcha,
        DecisionNoteKind::Convention,
    ];
    /// The `note_meta.note_kind` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionNoteKind::Decision => "decision",
            DecisionNoteKind::Rejected => "rejected",
            DecisionNoteKind::Gotcha => "gotcha",
            DecisionNoteKind::Convention => "convention",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteRef {
    pub kind: DecisionNoteKind,
    pub sources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub chunk_id: String,
    pub session_id: String,
    pub ts: i64,
    pub kind: ChunkKind,
    pub snippet: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NoteRef>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEstimate {
    pub transcript_files: u64,
    pub transcript_bytes: u64,
    pub download_bytes: u64,
    pub disk_bytes: u64,
    pub first_import_minutes: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryWorkspaceStats {
    pub workspace_id: String,
    pub sessions: u64,
    pub chunks: u64,
    pub embedded_chunks: u64,
    pub db_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    #[default]
    Off,
    Installing,
    Indexing,
    Idle,
    Paused,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryProgress {
    pub state: ProgressState,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub embedded_chunks: Option<u64>,
    pub total_chunks: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistillState {
    #[default]
    Off,
    Idle,
    Running,
    Waiting,
    Paused,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDistillStatus {
    pub enabled: bool,
    pub state: DistillState,
    pub groups_distilled: u64,
    pub groups_pending: u64,
    pub groups_failed: u64,
    pub groups_unsettled: u64,
    pub notes: u64,
    pub spent_usd: f64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStatus {
    pub enabled: bool,
    pub progress: HistoryProgress,
    pub workspaces: Vec<HistoryWorkspaceStats>,
    pub distill: Option<HistoryDistillStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDistillEstimate {
    pub model: String,
    pub price_input_per_m_tok: f64,
    pub price_output_per_m_tok: f64,
    pub pending_groups: u64,
    pub pending_input_tokens: u64,
    pub pending_output_tokens: u64,
    pub backlog_usd: f64,
    pub unsettled_groups: u64,
    pub monthly_groups: u64,
    pub monthly_input_tokens: u64,
    pub monthly_output_tokens: u64,
    pub monthly_usd: f64,
    pub spent_usd: f64,
    pub notes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSession {
    pub session_id: String,
    pub chunk_id: String,
    pub ts: i64,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShownChunk {
    pub chunk_id: String,
    pub ts: i64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub chunks: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhyCommit {
    pub sha: String,
    pub ts: i64,
    pub subject: String,
    pub author: String,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub uncommitted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhyEdit {
    pub session_id: String,
    pub chunk_id: String,
    pub ts: i64,
    pub title: Option<String>,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryWhyEntry {
    pub commit: WhyCommit,
    pub edits: Vec<WhyEdit>,
}

/// `~/Library/Application Support/SplitGrid/history`: the folder the Electron
/// app uses too (its `userData` is `…/Application Support/SplitGrid`), so the
/// runtime and the per-workspace DBs are shared. None when $HOME is unset.
pub fn default_root_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/Application Support/SplitGrid/history"))
}

/// `$CLAUDE_CONFIG_DIR/projects` when the variable is set (and not blank),
/// else `~/.claude/projects` — the same rule as the Electron app.
pub fn default_projects_dir() -> Option<PathBuf> {
    if let Some(dir) =
        std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.to_string_lossy().trim().is_empty())
    {
        return Some(PathBuf::from(dir).join("projects"));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// The facade over the whole feature (Electron's WorkspaceHistoryService).
/// Cheap to clone; one per process.
#[derive(Clone)]
pub struct HistoryService {
    inner: Arc<service::Inner>,
}

impl HistoryService {
    /// Does nothing until `enable()` / `resume_if_enabled()`.
    pub fn new(cfg: HistoryConfig) -> Self {
        Self::with_deps(cfg, ServiceDeps::default())
    }

    /// `new` with injected parts (fake runtime / distill runner / clock), for tests.
    pub fn with_deps(cfg: HistoryConfig, deps: ServiceDeps) -> Self {
        Self {
            inner: service::Inner::new(cfg, deps),
        }
    }

    /// At startup: `app-settings.json` already says historyEnabled (consent was
    /// given, in either app) — start the background loop without re-installing
    /// anything the user didn't agree to (install only if missing, as Electron does).
    /// Returns at once; the work happens on a background thread.
    pub fn resume_if_enabled(&self, enabled: bool, distill: bool) {
        if !enabled {
            return;
        }
        // Set before enable() so the first background pass already includes it.
        if distill {
            self.inner.set_distill_enabled(true);
        }
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            if let Err(e) = inner.enable() {
                inner.log_line(&format!("[history] resume failed: {e}"));
            }
        });
    }

    pub fn status(&self) -> HistoryStatus {
        self.inner.status()
    }
    /// Called on every status change (throttled ~1/s), from a background thread.
    pub fn on_status(&self, f: Box<dyn Fn(&HistoryStatus) + Send + Sync>) {
        self.inner.on_status(Arc::from(f));
    }
    pub fn estimate(&self) -> anyhow::Result<HistoryEstimate> {
        self.inner.estimate()
    }
    /// Consent given: install the runtime (download + sha256 verify), then index
    /// in the background. Returns once installed (or failed).
    pub fn enable(&self) -> anyhow::Result<()> {
        self.inner.enable()
    }
    /// Stop everything; with `delete_data` also remove the runtime and every index.
    pub fn disable(&self, delete_data: bool) -> anyhow::Result<()> {
        self.inner.disable(delete_data)
    }
    pub fn pause(&self) {
        self.inner.pause()
    }
    pub fn resume(&self) {
        self.inner.resume()
    }

    /// App quit: stop llama-server and the background loop and close the DBs.
    /// Consent is kept (a later `enable()` / next launch picks up again).
    pub fn dispose(&self) {
        self.inner.dispose()
    }

    /// A workspace was added or its folder changed: look for its sessions soon
    /// (after the watch debounce). No-op while disabled.
    pub fn notify_workspaces_changed(&self) {
        self.inner.notify_workspaces_changed()
    }

    // Agent queries, always scoped to one workspace.
    pub fn search(
        &self,
        workspace_id: &str,
        query: &str,
        limit: Option<usize>,
        since: Option<i64>,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.inner.search(workspace_id, query, limit, since)
    }
    pub fn sessions_for_file(
        &self,
        workspace_id: &str,
        file: &str,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<FileSession>> {
        self.inner.sessions_for_file(workspace_id, file, limit)
    }
    pub fn show_chunk(
        &self,
        workspace_id: &str,
        chunk_id: &str,
        radius: Option<usize>,
    ) -> anyhow::Result<Vec<ShownChunk>> {
        self.inner.show_chunk(workspace_id, chunk_id, radius)
    }
    pub fn list_sessions(
        &self,
        workspace_id: &str,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        self.inner.list_sessions(workspace_id, limit)
    }
    pub fn why(
        &self,
        workspace_id: &str,
        file: &str,
        line: Option<u32>,
    ) -> anyhow::Result<Vec<HistoryWhyEntry>> {
        self.inner.why(workspace_id, file, line)
    }

    // Decision notes (AppSettings.historyDistill), only meaningful while enabled.
    pub fn distill_estimate(&self) -> anyhow::Result<HistoryDistillEstimate> {
        self.inner.distill_estimate()
    }
    pub fn set_distill_enabled(&self, enabled: bool) {
        self.inner.set_distill_enabled(enabled)
    }
    pub fn delete_notes(&self) -> anyhow::Result<()> {
        self.inner.delete_notes()
    }
}
