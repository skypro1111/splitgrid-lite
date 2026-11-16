//! Per-workspace indexer (port of src/main/history/indexer.ts and the query
//! half of indexer-host.ts).
//!
//! One pass: stat every transcript, parse files that changed since last seen
//! (remembering their cwds in `files.json`), write sessions + chunks into the
//! DB of every workspace whose folder contains one of those cwds, then embed
//! whatever chunks lack a vector, a batch at a time. A transcript that ran in
//! two overlapping workspace folders lands in both DBs.
//!
//! Thread-safety: every store locks itself per operation and `files.json` is
//! read at the start of each scan, so queries from other threads interleave
//! with a running pass (the slow parts — parsing, embedding — hold no lock).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::distill::{
    DISTILL_GROUP_CHARS, DISTILL_MAX_ATTEMPTS, DISTILL_OUTPUT_TOKENS_PER_GROUP,
    DISTILL_PROMPT_OVERHEAD_TOKENS, DistillGroup, DistillRunResult, build_note_chunks,
    call_cost_usd, group_session_chunks, is_session_settled, render_group_input, tokens_for_chars,
};
use crate::runtime::{EmbedKind, Embedder};
use crate::store::{ChunkVector, DistillGroupState, DistillRunRecord, HistoryStore, SearchQuery};
use crate::transcripts::{
    ParsedTranscript, TranscriptFile, TranscriptSource, discover_transcript_files,
    parse_transcript, transcript_belongs_to,
};
use crate::util::{js_round, js_trim, len16, path_str, write_atomic};
use crate::{
    FileSession, HistoryWorkspaceStats, IndexedWorkspace, SearchHit, SessionSummary, ShownChunk,
    WhyEdit,
};

const MONTH_MS: i64 = 30 * 86_400_000;
pub const SEEN_FILE: &str = "files.json";
pub const DB_SUFFIX: &str = ".sqlite";

/// What is known about a transcript file without re-reading it (files.json, shared with Electron).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct SeenFile {
    size: u64,
    mtime_ms: f64,
    #[serde(default)]
    cwds: Vec<String>,
}

/// Workspace ids end up in a file name: anything outside a safe charset is replaced.
pub fn db_file_name(workspace_id: &str) -> String {
    let safe: String = workspace_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // JS replaces per UTF-16 unit: an astral char becomes two underscores.
    let safe: String = workspace_id
        .chars()
        .zip(safe.chars())
        .flat_map(|(orig, s)| {
            if s == '_' && orig.len_utf16() == 2 {
                vec!['_', '_']
            } else {
                vec![s]
            }
        })
        .collect();
    format!("{safe}{DB_SUFFIX}")
}

pub struct IndexerOptions {
    /// Claude Code's projects dir.
    pub projects_dir: PathBuf,
    /// Where the per-workspace DBs and files.json live (`<root>/db`).
    pub data_dir: PathBuf,
    /// Current workspaces, read on every pass.
    pub workspaces: Arc<dyn Fn() -> Vec<IndexedWorkspace> + Send + Sync>,
    pub embedder: Arc<dyn Embedder>,
    /// Chunks per embedding request (default 16).
    pub batch_size: usize,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

/// One group to distill; the same group in overlapping workspaces is distilled once and written to all.
#[derive(Clone, Debug, PartialEq)]
pub struct DistillPlanItem {
    pub workspace_id: String,
    pub session_id: String,
    pub group: i64,
    pub input_hash: String,
    pub also_workspace_ids: Vec<String>,
}

/// A plan item's rendered input, plus what commit needs to place the notes.
#[derive(Clone, Debug, PartialEq)]
pub struct DistillInputResult {
    pub text: String,
    pub chars: usize,
    pub seqs: Vec<i64>,
    pub ts_by_seq: Vec<(i64, i64)>,
    pub first_seq: i64,
    pub last_ts: i64,
}

/// Counts behind HistoryDistillStatus and HistoryDistillEstimate (one DB pass, no model).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DistillScanResult {
    pub plan: Vec<DistillPlanItem>,
    pub groups_distilled: u64,
    pub groups_pending: u64,
    pub groups_failed: u64,
    pub groups_unsettled: u64,
    pub notes: u64,
    pub spent_usd: f64,
    pub pending_input_tokens: i64,
    pub pending_output_tokens: i64,
    pub backlog_usd: f64,
    pub monthly_groups: i64,
    pub monthly_input_tokens: i64,
    pub monthly_output_tokens: i64,
    pub monthly_usd: f64,
}

pub struct HistoryIndexer {
    opts: IndexerOptions,
    stores: Mutex<HashMap<String, Arc<HistoryStore>>>,
    /// Workspaces whose DB could not be opened (e.g. a newer schema), logged once.
    unusable: Mutex<HashSet<String>>,
}

impl HistoryIndexer {
    pub fn new(opts: IndexerOptions) -> Self {
        Self {
            opts,
            stores: Mutex::new(HashMap::new()),
            unusable: Mutex::new(HashSet::new()),
        }
    }

    pub fn db_path(&self, workspace_id: &str) -> PathBuf {
        self.opts.data_dir.join(db_file_name(workspace_id))
    }

    /// The workspace's store, opened on first use; None when it has no DB yet and `create` is false.
    pub fn store(&self, workspace_id: &str, create: bool) -> Result<Option<Arc<HistoryStore>>> {
        let mut stores = self.stores.lock().unwrap();
        if let Some(s) = stores.get(workspace_id) {
            return Ok(Some(s.clone()));
        }
        let file = self.db_path(workspace_id);
        if !create && !file.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.opts.data_dir)?;
        let store = Arc::new(HistoryStore::open(&path_str(&file))?);
        stores.insert(workspace_id.to_string(), store.clone());
        Ok(Some(store))
    }

    /// `store()` for the background passes: a DB that can't be opened (a newer
    /// schema written by a newer build) is skipped, not fatal for the others.
    fn usable_store(&self, workspace_id: &str, create: bool) -> Option<Arc<HistoryStore>> {
        match self.store(workspace_id, create) {
            Ok(s) => s,
            Err(e) => {
                if self
                    .unusable
                    .lock()
                    .unwrap()
                    .insert(workspace_id.to_string())
                {
                    (self.opts.log)(&format!("history: workspace {workspace_id} skipped: {e}"));
                }
                None
            }
        }
    }

    /// Workspaces that have a folder, the only ones history can apply to.
    fn indexable(&self) -> Vec<(String, String)> {
        (self.opts.workspaces)()
            .into_iter()
            .filter_map(|w| {
                let folder = w.working_directory.map(|p| path_str(&p))?;
                if js_trim(&folder).is_empty() {
                    None
                } else {
                    Some((w.id, folder))
                }
            })
            .collect()
    }

    fn load_seen(&self) -> HashMap<String, SeenFile> {
        std::fs::read_to_string(self.opts.data_dir.join(SEEN_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, serde_json::Value>>(&raw).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| serde_json::from_value::<SeenFile>(v).ok().map(|s| (k, s)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_seen(&self, seen: &HashMap<String, SeenFile>) -> Result<()> {
        std::fs::create_dir_all(&self.opts.data_dir)?;
        write_atomic(
            &self.opts.data_dir.join(SEEN_FILE),
            serde_json::to_string(seen)?.as_bytes(),
            Some(0o600),
        )?;
        Ok(())
    }

    fn parse(&self, file: &TranscriptFile) -> Option<ParsedTranscript> {
        match parse_transcript(&TranscriptSource::from(file)) {
            Ok(p) => Some(p),
            Err(e) => {
                // Vanished between stat and read, or unreadable: skip it this pass.
                (self.opts.log)(&format!("history: skip {}: {e}", file.file));
                None
            }
        }
    }

    /// Bring every workspace DB up to date with the transcripts on disk.
    /// Returns the number of chunks that now need embedding.
    pub fn scan(&self, should_stop: &dyn Fn() -> bool) -> Result<usize> {
        let workspaces = self.indexable();
        if workspaces.is_empty() {
            return Ok(0);
        }
        let files = discover_transcript_files(&self.opts.projects_dir);
        let mut seen = self.load_seen();
        let mut present: HashSet<String> = HashSet::new();
        let mut pending = 0usize;
        let mut dirty = false;

        for file in &files {
            if should_stop() {
                break;
            }
            present.insert(file.file.clone());
            let mut parsed: Option<ParsedTranscript> = None;
            let changed = seen
                .get(&file.file)
                .is_none_or(|p| p.size != file.size || p.mtime_ms != file.mtime_ms);
            if changed {
                let Some(p) = self.parse(file) else { continue };
                seen.insert(
                    file.file.clone(),
                    SeenFile {
                        size: file.size,
                        mtime_ms: file.mtime_ms,
                        cwds: p.meta.cwds.clone(),
                    },
                );
                parsed = Some(p);
                dirty = true;
            }
            let cwds = seen[&file.file].cwds.clone();
            for (ws_id, folder) in &workspaces {
                if !transcript_belongs_to(&cwds, Some(folder), None) {
                    continue;
                }
                let Some(store) = self.usable_store(ws_id, true) else {
                    continue;
                };
                // The DB keeps mtime rounded to whole ms (as the Electron app writes it).
                if let Some(state) = store.get_file_state(&file.file)?
                    && state.size == file.size
                    && state.mtime_ms as i64 == js_round(file.mtime_ms)
                {
                    continue;
                }
                if parsed.is_none() {
                    parsed = self.parse(file);
                }
                let Some(p) = parsed.as_ref() else { break };
                store.upsert_session(&p.meta)?;
                pending += store.replace_session_chunks(&p.meta.id, &p.chunks)?.len();
                store.set_file_state(
                    &file.file,
                    &crate::store::FileState {
                        size: file.size,
                        mtime_ms: file.mtime_ms,
                        session_id: Some(p.meta.id.clone()),
                    },
                )?;
            }
        }

        // Transcripts Claude Code deleted are kept in the DBs on purpose (that is
        // the point of the archive); only their bookkeeping entry goes.
        if !should_stop() {
            let before = seen.len();
            seen.retain(|k, _| present.contains(k));
            dirty |= seen.len() != before;
        }
        if dirty {
            self.save_seen(&seen)?;
        }
        Ok(pending)
    }

    /// Embed chunks without a vector, across all workspace DBs, until none are
    /// left or `should_stop` turns true. `on_batch` runs after each batch.
    pub fn embed_pending(&self, should_stop: &dyn Fn() -> bool, on_batch: &dyn Fn()) -> Result<()> {
        for (ws_id, _) in self.indexable() {
            let Some(store) = self.usable_store(&ws_id, false) else {
                continue;
            };
            loop {
                if should_stop() {
                    return Ok(());
                }
                let batch = store.pending_embeddings(self.opts.batch_size)?;
                if batch.is_empty() {
                    break;
                }
                let texts: Vec<String> = batch.iter().map(|b| b.text.clone()).collect();
                let vectors = self.opts.embedder.embed(&texts, EmbedKind::Document)?;
                if should_stop() {
                    return Ok(());
                }
                let items: Vec<ChunkVector> = batch
                    .into_iter()
                    .zip(vectors)
                    .map(|(b, v)| ChunkVector {
                        id: b.id,
                        vector: v,
                        hash: Some(b.hash),
                    })
                    .collect();
                store.set_vectors(&items)?;
                on_batch();
            }
        }
        Ok(())
    }

    // ─── Decision notes ────────────────────────────────────────────────────

    /// One pass over every workspace DB: group each main session, compare
    /// group hashes with the stored state, and count (a group counts once
    /// even in several overlapping workspaces). With `write`, groups whose
    /// session shrank are pruned and a pending group already distilled in
    /// another DB gets that DB's notes copied. Without it nothing is written.
    pub fn distill_scan(&self, now: i64, write: bool) -> Result<DistillScanResult> {
        struct Entry {
            ws_id: String,
            store: Arc<HistoryStore>,
            session_id: String,
            last_ts: i64,
            group: DistillGroup,
            state: Option<DistillGroupState>,
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut done_by_hash: HashMap<String, (Arc<HistoryStore>, String, i64)> = HashMap::new();
        let mut recent_by_session: HashMap<String, i64> = HashMap::new();
        let mut out = DistillScanResult::default();
        for (ws_id, _) in self.indexable() {
            let Some(store) = self.usable_store(&ws_id, false) else {
                continue;
            };
            let (notes, spent) = store.distill_totals()?;
            out.notes += notes;
            out.spent_usd += spent;
            for cand in store.distill_candidates()? {
                let groups = group_session_chunks(&cand.chunks, DISTILL_GROUP_CHARS);
                if write {
                    store.prune_groups(&cand.session_id, groups.len() as i64)?;
                }
                let states = store.group_states(&cand.session_id)?;
                for group in groups {
                    let state = states.get(&group.index).cloned();
                    if state
                        .as_ref()
                        .is_some_and(|s| s.ok && s.input_hash == group.input_hash)
                    {
                        done_by_hash.insert(
                            group.input_hash.clone(),
                            (store.clone(), cand.session_id.clone(), group.index),
                        );
                    }
                    entries.push(Entry {
                        ws_id: ws_id.clone(),
                        store: store.clone(),
                        session_id: cand.session_id.clone(),
                        last_ts: cand.last_ts,
                        group,
                        state,
                    });
                }
                recent_by_session
                    .entry(cand.session_id.clone())
                    .or_insert_with(|| {
                        cand.chunks
                            .iter()
                            .filter(|c| c.ts >= now - MONTH_MS)
                            .map(|c| c.chars)
                            .sum()
                    });
            }
        }

        // Best status per (session, group) across workspaces: done > failed > pending > unsettled.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum S {
            Unsettled,
            Pending,
            Failed,
            Done,
        }
        let mut status: HashMap<(String, i64), S> = HashMap::new();
        let mut plan: Vec<(DistillPlanItem, i64)> = Vec::new();
        let mut plan_by_hash: HashMap<String, usize> = HashMap::new();
        for e in &entries {
            let same_input = e
                .state
                .as_ref()
                .is_some_and(|s| s.input_hash == e.group.input_hash);
            let s = if e.state.as_ref().is_some_and(|s| s.ok) && same_input {
                S::Done
            } else if e
                .state
                .as_ref()
                .is_some_and(|s| !s.ok && s.attempts >= DISTILL_MAX_ATTEMPTS)
                && same_input
            {
                S::Failed
            } else if !is_session_settled(e.last_ts, now) {
                S::Unsettled
            } else if let Some((src_store, src_sid, src_group)) =
                done_by_hash.get(&e.group.input_hash)
            {
                if write {
                    let notes = src_store.get_group_notes(src_sid, *src_group)?;
                    e.store.replace_group_notes(
                        &e.session_id,
                        e.group.index,
                        &notes,
                        Some(&DistillRunRecord {
                            input_hash: e.group.input_hash.clone(),
                            ok: true,
                            at: now,
                            ..Default::default()
                        }),
                    )?;
                }
                // Without `write`: copied for free on the next real pass.
                S::Done
            } else {
                match plan_by_hash.get(&e.group.input_hash) {
                    Some(&i) => {
                        let item = &mut plan[i].0;
                        if item.workspace_id != e.ws_id
                            && !item.also_workspace_ids.contains(&e.ws_id)
                        {
                            item.also_workspace_ids.push(e.ws_id.clone());
                        }
                    }
                    None => {
                        plan_by_hash.insert(e.group.input_hash.clone(), plan.len());
                        plan.push((
                            DistillPlanItem {
                                workspace_id: e.ws_id.clone(),
                                session_id: e.session_id.clone(),
                                group: e.group.index,
                                input_hash: e.group.input_hash.clone(),
                                also_workspace_ids: vec![],
                            },
                            e.last_ts,
                        ));
                        let input =
                            tokens_for_chars(e.group.chars) + DISTILL_PROMPT_OVERHEAD_TOKENS;
                        out.pending_input_tokens += input;
                        out.pending_output_tokens += DISTILL_OUTPUT_TOKENS_PER_GROUP;
                        out.backlog_usd +=
                            call_cost_usd(input as f64, DISTILL_OUTPUT_TOKENS_PER_GROUP as f64);
                    }
                }
                S::Pending
            };
            let key = (e.session_id.clone(), e.group.index);
            if status.get(&key).is_none_or(|prev| s > *prev) {
                status.insert(key, s);
            }
        }
        for s in status.values() {
            match s {
                S::Done => out.groups_distilled += 1,
                S::Failed => out.groups_failed += 1,
                S::Pending => out.groups_pending += 1,
                S::Unsettled => out.groups_unsettled += 1,
            }
        }
        // Newest sessions first: the most recent decisions are the ones agents ask about.
        plan.sort_by(|(a, at), (b, bt)| {
            bt.cmp(at)
                .then_with(|| a.session_id.cmp(&b.session_id))
                .then(a.group.cmp(&b.group))
        });
        out.plan = plan.into_iter().map(|(p, _)| p).collect();

        // Ongoing rate: the last 30 days of new conversation, grouped per session.
        for chars in recent_by_session.into_values() {
            if chars <= 0 {
                continue;
            }
            let groups = (chars + DISTILL_GROUP_CHARS - 1) / DISTILL_GROUP_CHARS;
            let input = tokens_for_chars(chars) + groups * DISTILL_PROMPT_OVERHEAD_TOKENS;
            let output = groups * DISTILL_OUTPUT_TOKENS_PER_GROUP;
            out.monthly_groups += groups;
            out.monthly_input_tokens += input;
            out.monthly_output_tokens += output;
            out.monthly_usd +=
                call_cost_usd(input as f64 / groups as f64, output as f64 / groups as f64)
                    * groups as f64;
        }
        Ok(out)
    }

    /// Render a planned group, or None when the session changed since planning.
    pub fn distill_input(&self, item: &DistillPlanItem) -> Result<Option<DistillInputResult>> {
        let Some(store) = self.usable_store(&item.workspace_id, false) else {
            return Ok(None);
        };
        let Some(cand) = store
            .distill_candidates()?
            .into_iter()
            .find(|c| c.session_id == item.session_id)
        else {
            return Ok(None);
        };
        let groups = group_session_chunks(&cand.chunks, DISTILL_GROUP_CHARS);
        let Some(group) = groups
            .get(item.group as usize)
            .filter(|g| g.input_hash == item.input_hash)
        else {
            return Ok(None);
        };
        let chunks = store.get_chunk_texts(&group.chunk_ids)?;
        let mut ts_by_seq: Vec<(i64, i64)> = Vec::new();
        for c in &chunks {
            match ts_by_seq.iter_mut().find(|(s, _)| *s == c.seq) {
                Some(e) => e.1 = e.1.max(c.ts),
                None => ts_by_seq.push((c.seq, c.ts.max(0))),
            }
        }
        let pairs: Vec<(i64, &str)> = chunks.iter().map(|c| (c.seq, c.text.as_str())).collect();
        let text = render_group_input(&pairs, cand.title.as_deref());
        Ok(Some(DistillInputResult {
            chars: len16(&text),
            text,
            seqs: ts_by_seq.iter().map(|(s, _)| *s).collect(),
            last_ts: ts_by_seq.iter().map(|(_, t)| *t).fold(0, i64::max),
            ts_by_seq,
            first_seq: group.first_seq,
        }))
    }

    /// Store a run's outcome for the planned group in every workspace that
    /// holds it; the cost is recorded once. Returns the number of notes written.
    pub fn distill_commit(
        &self,
        item: &DistillPlanItem,
        input: &DistillInputResult,
        result: &DistillRunResult,
        now: i64,
    ) -> Result<usize> {
        let usage = result.usage().cloned().unwrap_or_default();
        let notes = match result {
            DistillRunResult::Ok { notes, .. } => build_note_chunks(
                &item.session_id,
                item.group,
                input.first_seq,
                input.last_ts,
                notes,
                &input.ts_by_seq,
            ),
            DistillRunResult::Failed { .. } => vec![],
        };
        let targets = std::iter::once(&item.workspace_id).chain(item.also_workspace_ids.iter());
        for (i, ws_id) in targets.enumerate() {
            let Some(store) = self.usable_store(ws_id, false) else {
                continue;
            };
            let run = DistillRunRecord {
                input_hash: item.input_hash.clone(),
                ok: result.is_ok(),
                counts_as_attempt: matches!(
                    result,
                    DistillRunResult::Failed {
                        input_specific: true,
                        ..
                    }
                ),
                cost_usd: if i == 0 { usage.cost_usd } else { 0.0 },
                input_tokens: if i == 0 { usage.input_tokens } else { 0.0 },
                output_tokens: if i == 0 { usage.output_tokens } else { 0.0 },
                at: now,
                error: match result {
                    DistillRunResult::Failed { error, .. } => Some(error.clone()),
                    DistillRunResult::Ok { .. } => None,
                },
            };
            if result.is_ok() {
                store.replace_group_notes(&item.session_id, item.group, &notes, Some(&run))?;
            } else {
                store.record_group_run(&item.session_id, item.group, &run)?;
            }
        }
        Ok(notes.len())
    }

    /// Delete every decision note in every workspace DB; returns how many went.
    pub fn delete_notes(&self) -> Result<usize> {
        let mut n = 0;
        for (ws_id, _) in self.indexable() {
            if let Some(store) = self.usable_store(&ws_id, false) {
                n += store.delete_notes()?;
            }
        }
        Ok(n)
    }

    /// (total chunks, embedded chunks) across indexable workspaces.
    pub fn progress(&self) -> Result<(u64, u64)> {
        let (mut total, mut embedded) = (0, 0);
        for (ws_id, _) in self.indexable() {
            if let Some(store) = self.usable_store(&ws_id, false) {
                let s = store.stats()?;
                total += s.chunks;
                embedded += s.embedded_chunks;
            }
        }
        Ok((total, embedded))
    }

    /// Per-workspace stats of every workspace that has a DB.
    pub fn stats(&self) -> Result<Vec<HistoryWorkspaceStats>> {
        let mut out = Vec::new();
        for ws in (self.opts.workspaces)() {
            if let Some(store) = self.usable_store(&ws.id, false) {
                let s = store.stats()?;
                out.push(HistoryWorkspaceStats {
                    workspace_id: ws.id,
                    sessions: s.sessions,
                    chunks: s.chunks,
                    embedded_chunks: s.embedded_chunks,
                    db_bytes: s.db_bytes,
                });
            }
        }
        Ok(out)
    }

    // ─── Queries (indexer-host.ts) ─────────────────────────────────────────

    pub fn search(
        &self,
        workspace_id: &str,
        text: &str,
        vector: Option<Vec<f32>>,
        limit: Option<usize>,
        since: Option<i64>,
    ) -> Result<Vec<SearchHit>> {
        let Some(store) = self.store(workspace_id, false)? else {
            return Ok(vec![]);
        };
        store.search(
            &SearchQuery {
                text: text.to_string(),
                vector,
                limit,
                since,
            },
            None,
        )
    }

    /// One line per session (its newest edit of the file).
    pub fn sessions_for_file(
        &self,
        workspace_id: &str,
        file: &str,
        limit: usize,
    ) -> Result<Vec<FileSession>> {
        let Some(store) = self.store(workspace_id, false)? else {
            return Ok(vec![]);
        };
        Ok(store
            .sessions_for_file(file, limit)?
            .into_iter()
            .filter_map(|e| {
                let c = e.chunks.into_iter().next()?;
                Some(FileSession {
                    session_id: e.session_id,
                    chunk_id: c.chunk_id,
                    ts: c.ts,
                    title: e.session_title,
                })
            })
            .collect())
    }

    /// Every session edit of the file (`why` attributes edits to commits by time).
    pub fn file_edits(&self, workspace_id: &str, file: &str) -> Result<Vec<WhyEdit>> {
        let Some(store) = self.store(workspace_id, false)? else {
            return Ok(vec![]);
        };
        Ok(store
            .sessions_for_file(file, 200)?
            .into_iter()
            .flat_map(|e| {
                let (sid, title) = (e.session_id, e.session_title);
                e.chunks.into_iter().map(move |c| WhyEdit {
                    session_id: sid.clone(),
                    chunk_id: c.chunk_id,
                    ts: c.ts,
                    title: title.clone(),
                    snippet: c.snippet,
                })
            })
            .collect())
    }

    pub fn show_chunk(
        &self,
        workspace_id: &str,
        chunk_id: &str,
        radius: usize,
    ) -> Result<Vec<ShownChunk>> {
        let Some(store) = self.store(workspace_id, false)? else {
            return Ok(vec![]);
        };
        let Some(chunk) = store.get_chunk(chunk_id)? else {
            return Ok(vec![]);
        };
        Ok(store
            .get_session_chunks(&chunk.chunk.session_id, Some(chunk_id), Some(radius))?
            .into_iter()
            .map(|c| ShownChunk {
                chunk_id: c.chunk.id,
                ts: c.chunk.ts,
                text: c.chunk.text,
            })
            .collect())
    }

    pub fn list_sessions(&self, workspace_id: &str, limit: usize) -> Result<Vec<SessionSummary>> {
        let Some(store) = self.store(workspace_id, false)? else {
            return Ok(vec![]);
        };
        Ok(store
            .list_sessions(limit, None)?
            .into_iter()
            .map(|s| SessionSummary {
                session_id: s.meta.id,
                title: s.meta.title,
                first_ts: s.meta.first_ts,
                last_ts: s.meta.last_ts,
                chunks: s.chunk_count,
            })
            .collect())
    }

    pub fn close(&self) {
        for (_, s) in self.stores.lock().unwrap().drain() {
            s.close();
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.opts.data_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_file_names_match_electron() {
        assert_eq!(db_file_name("1f0c-ab_9"), "1f0c-ab_9.sqlite");
        assert_eq!(db_file_name("a/b.c"), "a_b_c.sqlite");
        assert_eq!(db_file_name("é😀"), "___.sqlite");
    }
}
