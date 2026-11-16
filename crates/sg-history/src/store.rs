//! Per-workspace SQLite store (port of src/main/history/store.ts).
//!
//! One DB file per workspace holding sessions, turn-level chunks, a trigram
//! FTS5 index over chunk text and the chunks' 256-d embeddings. Dense search is
//! brute force over an in-memory cache. The DB is an ARCHIVE (Claude Code
//! prunes old transcripts), hence 0600 and the schema-version check that
//! refuses to touch a DB written by a newer build.
//!
//! SHARED WITH THE ELECTRON APP: schema, `meta.schema_version`, FTS table,
//! triggers and the little-endian f32 vector BLOB are identical, and the DB
//! runs in WAL with a busy timeout so both apps can write it at once.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};

use crate::transcripts::{SessionMeta, TranscriptChunk};
use crate::util::{js_round, js_trim, lower_chars};
use crate::{ChunkKind, DecisionNoteKind, HISTORY_EMBED_DIM, NoteRef, SearchHit};

/// v2 added decision notes (note_meta, distill_groups).
pub const SCHEMA_VERSION: i64 = 2;

const FUSION_CANDIDATES: usize = 100;
const RRF_K: f64 = 60.0;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;
const MAX_QUERY_TERMS: usize = 16;
const SNIPPET_CHARS: i64 = 300;
const SNIPPET_MAX_LINES: usize = 8;
const MIN_TERM_CHARS: usize = 3;
/// `-Number.MAX_SAFE_INTEGER`, the TS "no since" bound.
const NO_SINCE: i64 = -9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    Hybrid,
    Dense,
    Lexical,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileState {
    pub size: u64,
    /// Stored rounded to whole milliseconds.
    pub mtime_ms: f64,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredSession {
    pub meta: SessionMeta,
    pub chunk_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredChunk {
    pub chunk: TranscriptChunk,
    pub embedded: bool,
    pub session_title: Option<String>,
    pub note: Option<NoteRef>,
}

/// A decision note to store (distill::build_note_chunks).
#[derive(Clone, Debug, PartialEq)]
pub struct NoteInput {
    pub chunk: TranscriptChunk,
    pub note_kind: DecisionNoteKind,
    pub sources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DistillGroupState {
    pub group: i64,
    pub input_hash: String,
    pub ok: bool,
    pub attempts: i64,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub notes: i64,
    pub distilled_at: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct DistillRunRecord {
    pub input_hash: String,
    pub ok: bool,
    pub counts_as_attempt: bool,
    pub cost_usd: f64,
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub at: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidateChunk {
    pub id: String,
    pub seq: i64,
    pub hash: String,
    pub chars: i64,
    pub ts: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DistillCandidate {
    pub session_id: String,
    pub title: Option<String>,
    pub last_ts: i64,
    pub chunks: Vec<CandidateChunk>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingEmbedding {
    pub id: String,
    pub text: String,
    pub hash: String,
}

#[derive(Clone, Debug)]
pub struct ChunkVector {
    pub id: String,
    pub vector: Vec<f32>,
    /// When given, stored only if the chunk still has this hash.
    pub hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileHistoryChunk {
    pub chunk_id: String,
    pub seq: i64,
    pub ts: i64,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileHistoryEntry {
    pub session_id: String,
    pub session_title: Option<String>,
    pub git_branch: Option<String>,
    pub last_ts: i64,
    pub chunks: Vec<FileHistoryChunk>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HistoryStats {
    pub sessions: u64,
    pub chunks: u64,
    pub embedded_chunks: u64,
    pub db_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub text: String,
    pub vector: Option<Vec<f32>>,
    pub limit: Option<usize>,
    pub since: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkText {
    pub id: String,
    pub seq: i64,
    pub ts: i64,
    pub text: String,
}

// ─── Schema ──────────────────────────────────────────────────────────────────

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
  path       TEXT PRIMARY KEY,
  size       INTEGER NOT NULL,
  mtime_ms   INTEGER NOT NULL,
  session_id TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  file       TEXT NOT NULL,
  cwds       TEXT NOT NULL DEFAULT '[]',
  title      TEXT,
  git_branch TEXT,
  first_ts   INTEGER NOT NULL DEFAULT 0,
  last_ts    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_last_ts ON sessions(last_ts);
CREATE TABLE IF NOT EXISTS chunks (
  rid        INTEGER PRIMARY KEY,
  id         TEXT NOT NULL UNIQUE,
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  ts         INTEGER NOT NULL,
  kind       TEXT NOT NULL,
  text       TEXT NOT NULL,
  hash       TEXT NOT NULL,
  vec        BLOB
);
CREATE INDEX IF NOT EXISTS chunks_session_seq ON chunks(session_id, seq);
CREATE INDEX IF NOT EXISTS chunks_ts ON chunks(ts);
CREATE TABLE IF NOT EXISTS chunk_files (
  chunk_rid INTEGER NOT NULL REFERENCES chunks(rid) ON DELETE CASCADE,
  path      TEXT NOT NULL,
  PRIMARY KEY (chunk_rid, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chunk_files_path ON chunk_files(path);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text, content='chunks', content_rowid='rid', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.rid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.rid, old.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE OF text ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.rid, old.text);
  INSERT INTO chunks_fts(rowid, text) VALUES (new.rid, new.text);
END;
-- Decision notes are rows of chunks (kind 'decision') so FTS, vectors, show and
-- search need nothing new; this side table says which group made them and
-- which turns they cite. Deleting the chunk row cascades here.
CREATE TABLE IF NOT EXISTS note_meta (
  chunk_rid INTEGER PRIMARY KEY REFERENCES chunks(rid) ON DELETE CASCADE,
  grp       INTEGER NOT NULL,
  note_kind TEXT NOT NULL,
  sources   TEXT NOT NULL DEFAULT '[]'
);
-- One row per distilled (or attempted) group of a session: the input hash
-- decides whether a group needs distilling again.
CREATE TABLE IF NOT EXISTS distill_groups (
  session_id    TEXT NOT NULL,
  grp           INTEGER NOT NULL,
  input_hash    TEXT NOT NULL,
  status        TEXT NOT NULL,
  attempts      INTEGER NOT NULL DEFAULT 0,
  cost_usd      REAL NOT NULL DEFAULT 0,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  notes         INTEGER NOT NULL DEFAULT 0,
  distilled_at  INTEGER NOT NULL,
  error         TEXT,
  PRIMARY KEY (session_id, grp)
) WITHOUT ROWID;
";

// ─── Query parsing ───────────────────────────────────────────────────────────

static TERM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""([^"]+)"|`([^`]+)`|([\p{L}\p{N}_]+(?:[.\-/:#@][\p{L}\p{N}_]+)*)"#).unwrap()
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryTerm {
    /// Lower-cased term text.
    pub text: String,
    /// Looks like code / an exact token rather than a natural-language word.
    pub identifier: bool,
}

/// Identifier-like: a quoted phrase (decided by the caller), camelCase /
/// PascalCase with an inner capital, snake_case, anything with a digit, dotted
/// / slashed paths and hosts, ASCII kebab-case. Plain words in any script and
/// bare acronyms are not.
pub fn is_identifier_like(raw: &str) -> bool {
    static RES: LazyLock<[Regex; 5]> = LazyLock::new(|| {
        [
            Regex::new(r"\p{N}").unwrap(),
            Regex::new(r"\p{Ll}\p{Lu}|\p{Lu}{2}\p{Ll}").unwrap(),
            Regex::new(r"[\p{L}\p{N}][./:#@][\p{L}\p{N}]").unwrap(),
            Regex::new(r"^[A-Za-z0-9]+(?:-[A-Za-z0-9]+)+$").unwrap(),
            Regex::new("_").unwrap(),
        ]
    });
    RES.iter().any(|re| re.is_match(raw))
}

pub fn parse_query_terms(text: &str) -> Vec<QueryTerm> {
    let clean: String = text
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out: Vec<QueryTerm> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for m in TERM_RE.captures_iter(&clean) {
        let quoted = m.get(1).or(m.get(2));
        let raw = js_trim(quoted.or(m.get(3)).map_or("", |g| g.as_str()));
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        out.push(QueryTerm {
            text: lower,
            identifier: quoted.is_some() || is_identifier_like(raw),
        });
        if out.len() >= MAX_QUERY_TERMS {
            break;
        }
    }
    out
}

/// FTS5 MATCH expression: each term double-quoted (inner quotes doubled),
/// OR-ed. None when no term is long enough for the trigram index.
pub fn build_match_expression(terms: &[QueryTerm]) -> Option<String> {
    let parts: Vec<String> = terms
        .iter()
        .filter(|t| t.text.chars().count() >= MIN_TERM_CHARS)
        .map(|t| format!("\"{}\"", t.text.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" OR "))
    }
}

// ─── Snippets ────────────────────────────────────────────────────────────────

fn index_of(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

fn last_index_of(hay: &[char], c: char, from: i64) -> i64 {
    if from < 0 {
        return -1;
    }
    let upto = (from as usize).min(hay.len().saturating_sub(1));
    if hay.is_empty() {
        return -1;
    }
    hay[..=upto]
        .iter()
        .rposition(|&x| x == c)
        .map_or(-1, |i| i as i64)
}

/// ~300 chars / a few lines around the best lexical match (the longest
/// matching term, identifiers first); the start of the chunk otherwise.
pub fn make_snippet(text: &str, terms: &[QueryTerm]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower = lower_chars(text);
    let len = chars.len() as i64;
    let mut ordered: Vec<&QueryTerm> = terms.iter().collect();
    ordered.sort_by(|a, b| {
        b.identifier
            .cmp(&a.identifier)
            .then(b.text.chars().count().cmp(&a.text.chars().count()))
    });
    let mut at: i64 = -1;
    for t in ordered {
        if t.text.is_empty() {
            continue;
        }
        let needle: Vec<char> = t.text.chars().collect();
        if let Some(i) = index_of(&lower, &needle, 0) {
            at = i as i64;
            break;
        }
    }
    let mut start: i64 = 0;
    if at > 0 {
        start = (at - SNIPPET_CHARS / 3).max(0);
        let nl = last_index_of(&chars, '\n', at);
        if nl >= start - 40 && nl < at {
            start = nl + 1;
        } else if start > 0
            && let Some(sp) = index_of(&chars, &[' '], start as usize)
            && (sp as i64) < at
        {
            start = sp as i64 + 1;
        }
    }
    let mut end = len.min(start + SNIPPET_CHARS);
    if end < len {
        let sp = last_index_of(&chars, ' ', end);
        if sp > start + SNIPPET_CHARS / 2 {
            end = sp;
        }
    }
    let mut body: String = chars[start as usize..end as usize].iter().collect();
    let lines: Vec<&str> = body.split('\n').collect();
    if lines.len() > SNIPPET_MAX_LINES {
        body = lines[..SNIPPET_MAX_LINES].join("\n");
        end = start + body.chars().count() as i64;
    }
    static NL3: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
    let body = NL3.replace_all(&body, "\n\n");
    let body = js_trim(&body);
    format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < len { "…" } else { "" }
    )
}

// ─── Vector cache ────────────────────────────────────────────────────────────

const D: usize = HISTORY_EMBED_DIM;

#[derive(Clone, Copy, Debug)]
struct Ranked {
    rid: i64,
    score: f64,
    exact: bool,
}

/// Every embedded chunk's vector in one contiguous buffer, patched in place by
/// writes; a failed transaction drops it and it reloads on the next search.
struct VectorCache {
    data: Vec<f32>,
    rids: Vec<i64>,
    ts: Vec<i64>,
    slot_by_rid: HashMap<i64, usize>,
}

impl VectorCache {
    fn with_capacity(n: usize) -> Self {
        Self {
            data: Vec::with_capacity(n * D),
            rids: Vec::with_capacity(n),
            ts: Vec::with_capacity(n),
            slot_by_rid: HashMap::new(),
        }
    }

    fn set(&mut self, rid: i64, ts: i64, vec: &[f32]) {
        match self.slot_by_rid.get(&rid) {
            Some(&slot) => {
                self.ts[slot] = ts;
                self.data[slot * D..(slot + 1) * D].copy_from_slice(vec);
            }
            None => {
                self.slot_by_rid.insert(rid, self.rids.len());
                self.rids.push(rid);
                self.ts.push(ts);
                self.data.extend_from_slice(vec);
            }
        }
    }

    fn set_ts(&mut self, rid: i64, ts: i64) {
        if let Some(&slot) = self.slot_by_rid.get(&rid) {
            self.ts[slot] = ts;
        }
    }

    fn remove(&mut self, rid: i64) {
        let Some(slot) = self.slot_by_rid.remove(&rid) else {
            return;
        };
        let last = self.rids.len() - 1;
        if slot != last {
            let moved = self.rids[last];
            self.rids[slot] = moved;
            self.ts[slot] = self.ts[last];
            self.data.copy_within(last * D..(last + 1) * D, slot * D);
            self.slot_by_rid.insert(moved, slot);
        }
        self.rids.pop();
        self.ts.pop();
        self.data.truncate(last * D);
    }

    /// Top-k slots by dot product (= cosine: both sides are unit length).
    fn top_k(&self, query: &[f32], k: usize, since: Option<i64>) -> Vec<Ranked> {
        let min_ts = since.unwrap_or(i64::MIN);
        let mut best: Vec<Ranked> = Vec::with_capacity(k + 1);
        let mut floor = f64::NEG_INFINITY;
        for (s, &ts) in self.ts.iter().enumerate() {
            if ts < min_ts {
                continue;
            }
            let row = &self.data[s * D..(s + 1) * D];
            let mut dot = 0f64;
            for j in 0..D {
                dot += f64::from(row[j]) * f64::from(query[j]);
            }
            if best.len() >= k && dot <= floor {
                continue;
            }
            let mut i = best.len();
            while i > 0 && best[i - 1].score < dot {
                i -= 1;
            }
            best.insert(
                i,
                Ranked {
                    rid: self.rids[s],
                    score: dot,
                    exact: false,
                },
            );
            if best.len() > k {
                best.pop();
            }
            if best.len() >= k {
                floor = best[best.len() - 1].score;
            }
        }
        best
    }
}

fn normalized(vec: &[f32]) -> Result<Vec<f32>> {
    if vec.len() != D {
        bail!("history: expected a {D}-d vector, got {}", vec.len());
    }
    let sum: f64 = vec.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    if sum.is_nan() || sum <= 0.0 || !sum.is_finite() {
        return Ok(vec![0.0; D]);
    }
    let inv = 1.0 / sum.sqrt();
    Ok(vec.iter().map(|&x| (f64::from(x) * inv) as f32).collect())
}

fn vec_to_blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn blob_to_vec(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() != D * 4 {
        return None;
    }
    Some(
        blob.as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
    )
}

fn parse_json_strings(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .and_then(|v| v.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn clamp_limit(limit: Option<usize>, fallback: usize, max: usize) -> usize {
    match limit {
        None => fallback,
        Some(l) => l.clamp(1, max),
    }
}

fn note_ref(kind: Option<String>, sources: Option<String>) -> Option<NoteRef> {
    let kind = DecisionNoteKind::parse(kind.as_deref()?)?;
    Some(NoteRef {
        kind,
        sources: parse_json_strings(sources.as_deref()),
    })
}

// ─── Store ───────────────────────────────────────────────────────────────────

struct Inner {
    conn: Option<Connection>,
    cache: Option<VectorCache>,
}

impl Inner {
    fn conn(&self) -> Result<&Connection> {
        self.conn
            .as_ref()
            .ok_or_else(|| anyhow!("history: store is closed"))
    }

    /// `BEGIN IMMEDIATE` … `COMMIT`; nested calls join the outer transaction.
    /// A failure rolls back and drops the vector cache (its in-place patches
    /// may now disagree with the DB).
    fn tx<T>(&mut self, f: impl FnOnce(&mut Inner) -> Result<T>) -> Result<T> {
        if !self.conn()?.is_autocommit() {
            return f(self);
        }
        self.conn()?.execute_batch("BEGIN IMMEDIATE")?;
        let result = f(self).and_then(|v| {
            self.conn()?.execute_batch("COMMIT")?;
            Ok(v)
        });
        if result.is_err() {
            if let Ok(c) = self.conn() {
                let _ = c.execute_batch("ROLLBACK");
            }
            self.cache = None;
        }
        result
    }
}

/// The DB of one workspace. Thread-safe: every method locks it for its own duration.
pub struct HistoryStore {
    inner: Mutex<Inner>,
    db_path: String,
}

#[derive(Debug)]
struct ChunkRow {
    rid: i64,
    id: String,
    session_id: String,
    seq: i64,
    ts: i64,
    kind: String,
    text: String,
    hash: String,
    embedded: bool,
    title: Option<String>,
    note_kind: Option<String>,
    sources: Option<String>,
}

impl HistoryStore {
    /// Open (creating if needed) the store at `db_path`; `":memory:"` for tests.
    /// The file is pre-created 0600 so its -wal / -shm siblings inherit it.
    pub fn open(db_path: &str) -> Result<Self> {
        let in_memory = db_path.is_empty() || db_path == ":memory:";
        if !in_memory {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            if !Path::new(db_path).exists() {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(db_path);
            }
            let _ = std::fs::set_permissions(db_path, std::fs::Permissions::from_mode(0o600));
        }
        let conn = if in_memory {
            Connection::open_in_memory()?
        } else {
            Connection::open(db_path)?
        };
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        // Refuse a DB from a newer build before writing anything to it.
        let version: Option<i64> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map(|v| v as i64);
        if let Some(v) = version
            && v > SCHEMA_VERSION
        {
            bail!("history: {db_path} has schema v{v}, newer than supported v{SCHEMA_VERSION}");
        }
        if !in_memory {
            conn.execute_batch("PRAGMA journal_mode = WAL")?;
        }
        // NORMAL under WAL can lose the last commits on power loss but never
        // corrupts; everything here is re-derivable from transcripts on disk.
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
        let mut inner = Inner {
            conn: Some(conn),
            cache: None,
        };
        inner.tx(|i| {
            let c = i.conn()?;
            c.execute_batch(SCHEMA_SQL)?;
            c.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?)",
                [SCHEMA_VERSION.to_string()],
            )?;
            Ok(())
        })?;
        Ok(Self {
            inner: Mutex::new(inner),
            db_path: if in_memory {
                ":memory:".into()
            } else {
                db_path.to_string()
            },
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn close(&self) {
        let mut inner = self.lock();
        inner.cache = None;
        if let Some(c) = inner.conn.take() {
            let _ = c.close();
        }
    }

    // ─── Files ─────────────────────────────────────────────────────────────

    pub fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st =
            c.prepare_cached("SELECT size, mtime_ms, session_id FROM files WHERE path = ?")?;
        Ok(st
            .query_row([path], |r| {
                Ok(FileState {
                    size: r.get::<_, i64>(0)?.max(0) as u64,
                    mtime_ms: r.get::<_, i64>(1)? as f64,
                    session_id: r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                })
            })
            .optional()?)
    }

    pub fn set_file_state(&self, path: &str, state: &FileState) -> Result<()> {
        let inner = self.lock();
        inner
            .conn()?
            .prepare_cached(
                "INSERT INTO files(path, size, mtime_ms, session_id) VALUES (?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime_ms = excluded.mtime_ms,
             session_id = excluded.session_id",
            )?
            .execute(params![
                path,
                state.size as i64,
                js_round(state.mtime_ms),
                state.session_id
            ])?;
        Ok(())
    }

    // ─── Sessions & chunks ─────────────────────────────────────────────────

    pub fn upsert_session(&self, meta: &SessionMeta) -> Result<()> {
        let inner = self.lock();
        inner.conn()?.prepare_cached(
            "INSERT INTO sessions(id, parent_id, file, cwds, title, git_branch, first_ts, last_ts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET parent_id = excluded.parent_id, file = excluded.file,
             cwds = excluded.cwds, title = excluded.title, git_branch = excluded.git_branch,
             first_ts = excluded.first_ts, last_ts = excluded.last_ts",
        )?
        .execute(params![
            meta.id,
            meta.parent_session_id,
            meta.file,
            serde_json::to_string(&meta.cwds)?,
            meta.title,
            meta.git_branch,
            meta.first_ts,
            meta.last_ts
        ])?;
        Ok(())
    }

    /// Make the session's stored (non-note) chunks exactly `chunks`, in one
    /// transaction: new ids inserted, changed hashes updated (vector cleared),
    /// missing ids deleted. Returns the ids that need (re)embedding.
    pub fn replace_session_chunks(
        &self,
        session_id: &str,
        chunks: &[TranscriptChunk],
    ) -> Result<Vec<String>> {
        let mut inner = self.lock();
        inner.tx(|i| {
            let c = i.conn.as_ref().expect("open");
            let mut existing: HashMap<String, (i64, String, i64, i64, String)> = HashMap::new();
            let mut order: Vec<String> = Vec::new();
            {
                let mut st = c.prepare_cached(
                    "SELECT rid, id, hash, seq, ts, kind FROM chunks WHERE session_id = ? AND kind != 'decision'",
                )?;
                let rows = st.query_map([session_id], |r| {
                    Ok((r.get::<_, String>(1)?, (r.get(0)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                })?;
                for row in rows {
                    let (id, v) = row?;
                    order.push(id.clone());
                    existing.insert(id, v);
                }
            }
            let mut to_embed = Vec::new();
            let mut seen: HashSet<&str> = HashSet::new();
            let mut cache_ops: Vec<(i64, Option<i64>)> = Vec::new(); // (rid, Some(ts)=set_ts | None=remove)
            for ch in chunks {
                if ch.session_id != session_id {
                    bail!("history: chunk {} belongs to {}, not {session_id}", ch.id, ch.session_id);
                }
                if ch.kind == ChunkKind::Decision {
                    bail!("history: {} is a note; use replace_group_notes", ch.id);
                }
                if !seen.insert(ch.id.as_str()) {
                    continue;
                }
                let ts = ch.ts;
                match existing.get(&ch.id) {
                    None => {
                        let rid: i64 = c
                            .prepare_cached(
                                "INSERT INTO chunks(id, session_id, seq, ts, kind, text, hash) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING rid",
                            )?
                            .query_row(params![ch.id, session_id, ch.seq, ts, ch.kind.as_str(), ch.text, ch.hash], |r| r.get(0))?;
                        let mut add = c.prepare_cached("INSERT OR IGNORE INTO chunk_files(chunk_rid, path) VALUES (?, ?)")?;
                        for f in &ch.files {
                            add.execute(params![rid, f])?;
                        }
                        to_embed.push(ch.id.clone());
                    }
                    Some((rid, hash, _, _, _)) if *hash != ch.hash => {
                        c.prepare_cached("UPDATE chunks SET seq = ?, ts = ?, kind = ?, text = ?, hash = ?, vec = NULL WHERE rid = ?")?
                            .execute(params![ch.seq, ts, ch.kind.as_str(), ch.text, ch.hash, rid])?;
                        c.prepare_cached("DELETE FROM chunk_files WHERE chunk_rid = ?")?.execute([rid])?;
                        let mut add = c.prepare_cached("INSERT OR IGNORE INTO chunk_files(chunk_rid, path) VALUES (?, ?)")?;
                        for f in &ch.files {
                            add.execute(params![rid, f])?;
                        }
                        cache_ops.push((*rid, None));
                        to_embed.push(ch.id.clone());
                    }
                    Some((rid, _, seq, prev_ts, kind)) if *seq != ch.seq || *prev_ts != ts || kind != ch.kind.as_str() => {
                        c.prepare_cached("UPDATE chunks SET seq = ?, ts = ?, kind = ? WHERE rid = ?")?
                            .execute(params![ch.seq, ts, ch.kind.as_str(), rid])?;
                        cache_ops.push((*rid, Some(ts)));
                    }
                    Some(_) => {}
                }
            }
            for id in &order {
                if seen.contains(id.as_str()) {
                    continue;
                }
                let rid = existing[id].0;
                c.prepare_cached("DELETE FROM chunks WHERE rid = ?")?.execute([rid])?;
                cache_ops.push((rid, None));
            }
            if let Some(cache) = i.cache.as_mut() {
                for (rid, op) in cache_ops {
                    match op {
                        Some(ts) => cache.set_ts(rid, ts),
                        None => cache.remove(rid),
                    }
                }
            }
            Ok(to_embed)
        })
    }

    /// Chunks still lacking a vector, oldest first.
    pub fn pending_embeddings(&self, limit: usize) -> Result<Vec<PendingEmbedding>> {
        let n = clamp_limit(Some(limit), 32, 10_000);
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st = c.prepare_cached(
            "SELECT id, text, hash FROM chunks WHERE vec IS NULL ORDER BY ts, rid LIMIT ?",
        )?;
        let rows = st.query_map([n as i64], |r| {
            Ok(PendingEmbedding {
                id: r.get(0)?,
                text: r.get(1)?,
                hash: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Store vectors (re-normalized). Returns how many were stored.
    pub fn set_vectors(&self, items: &[ChunkVector]) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        let mut inner = self.lock();
        inner.tx(|i| {
            let mut stored = 0;
            let mut patches: Vec<(i64, i64, Vec<f32>)> = Vec::new();
            {
                let c = i.conn()?;
                for item in items {
                    let vec = normalized(&item.vector)?;
                    let blob = vec_to_blob(&vec);
                    let row: Option<(i64, i64)> = match &item.hash {
                        Some(h) => c
                            .prepare_cached("UPDATE chunks SET vec = ? WHERE id = ? AND hash = ? RETURNING rid, ts")?
                            .query_row(params![blob, item.id, h], |r| Ok((r.get(0)?, r.get(1)?)))
                            .optional()?,
                        None => c
                            .prepare_cached("UPDATE chunks SET vec = ? WHERE id = ? RETURNING rid, ts")?
                            .query_row(params![blob, item.id], |r| Ok((r.get(0)?, r.get(1)?)))
                            .optional()?,
                    };
                    let Some((rid, ts)) = row else { continue };
                    stored += 1;
                    patches.push((rid, ts, vec));
                }
            }
            if let Some(cache) = i.cache.as_mut() {
                for (rid, ts, vec) in patches {
                    cache.set(rid, ts, &vec);
                }
            }
            Ok(stored)
        })
    }

    fn chunk_from_row(c: &Connection, r: ChunkRow) -> Result<StoredChunk> {
        let mut st =
            c.prepare_cached("SELECT path FROM chunk_files WHERE chunk_rid = ? ORDER BY path")?;
        let files = st
            .query_map([r.rid], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(StoredChunk {
            chunk: TranscriptChunk {
                id: r.id,
                session_id: r.session_id,
                seq: r.seq,
                ts: r.ts,
                kind: ChunkKind::parse(&r.kind).unwrap_or(ChunkKind::Turn),
                text: r.text,
                files,
                hash: r.hash,
            },
            embedded: r.embedded,
            session_title: r.title.filter(|t| !t.is_empty()),
            note: note_ref(r.note_kind, r.sources),
        })
    }

    fn read_chunk_row(r: &rusqlite::Row<'_>, with_note: bool) -> rusqlite::Result<ChunkRow> {
        Ok(ChunkRow {
            rid: r.get(0)?,
            id: r.get(1)?,
            session_id: r.get(2)?,
            seq: r.get(3)?,
            ts: r.get(4)?,
            kind: r.get(5)?,
            text: r.get(6)?,
            hash: r.get(7)?,
            embedded: r.get::<_, i64>(8)? != 0,
            title: r.get(9)?,
            note_kind: if with_note { r.get(10)? } else { None },
            sources: if with_note { r.get(11)? } else { None },
        })
    }

    fn get_chunk_locked(c: &Connection, id: &str) -> Result<Option<StoredChunk>> {
        let row = c
            .prepare_cached(
                "SELECT c.rid, c.id, c.session_id, c.seq, c.ts, c.kind, c.text, c.hash,
                 c.vec IS NOT NULL AS embedded, s.title, m.note_kind, m.sources
                 FROM chunks c LEFT JOIN sessions s ON s.id = c.session_id
                 LEFT JOIN note_meta m ON m.chunk_rid = c.rid WHERE c.id = ?",
            )?
            .query_row([id], |r| Self::read_chunk_row(r, true))
            .optional()?;
        row.map(|r| Self::chunk_from_row(c, r)).transpose()
    }

    pub fn get_chunk(&self, id: &str) -> Result<Option<StoredChunk>> {
        let inner = self.lock();
        Self::get_chunk_locked(inner.conn()?, id)
    }

    /// A session's (non-note) chunks in order; with `around`, only the
    /// `radius` (default 2) chunks on each side of it. Anchoring on a note
    /// returns just that note.
    pub fn get_session_chunks(
        &self,
        session_id: &str,
        around: Option<&str>,
        radius: Option<usize>,
    ) -> Result<Vec<StoredChunk>> {
        let inner = self.lock();
        let c = inner.conn()?;
        let base = "SELECT c.rid, c.id, c.session_id, c.seq, c.ts, c.kind, c.text, c.hash,
                    c.vec IS NOT NULL AS embedded, s.title
                    FROM chunks c LEFT JOIN sessions s ON s.id = c.session_id
                    WHERE c.session_id = ? AND c.kind != 'decision'";
        let rows: Vec<ChunkRow> = if let Some(anchor_id) = around {
            let anchor: Option<(i64, String)> = c
                .prepare_cached("SELECT seq, kind FROM chunks WHERE id = ? AND session_id = ?")?
                .query_row([anchor_id, session_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((seq, kind)) = anchor else {
                return Ok(vec![]);
            };
            if kind == "decision" {
                return Ok(Self::get_chunk_locked(c, anchor_id)?.into_iter().collect());
            }
            let radius = radius.unwrap_or(2) as i64;
            let mut st =
                c.prepare_cached(&format!("{base} AND c.seq BETWEEN ? AND ? ORDER BY c.seq"))?;
            st.query_map(params![session_id, seq - radius, seq + radius], |r| {
                Self::read_chunk_row(r, false)
            })?
            .collect::<rusqlite::Result<_>>()?
        } else {
            let mut st = c.prepare_cached(&format!("{base} ORDER BY c.seq"))?;
            st.query_map([session_id], |r| Self::read_chunk_row(r, false))?
                .collect::<rusqlite::Result<_>>()?
        };
        rows.into_iter()
            .map(|r| Self::chunk_from_row(c, r))
            .collect()
    }

    /// Newest sessions first, optionally only those active since `since`.
    pub fn list_sessions(&self, limit: usize, since: Option<i64>) -> Result<Vec<StoredSession>> {
        let n = clamp_limit(Some(limit), 20, 10_000);
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st = c.prepare_cached(
            "SELECT s.id, s.parent_id, s.file, s.cwds, s.title, s.git_branch, s.first_ts, s.last_ts,
             (SELECT COUNT(*) FROM chunks c WHERE c.session_id = s.id AND c.kind != 'decision') AS chunk_count
             FROM sessions s WHERE s.last_ts >= ? ORDER BY s.last_ts DESC, s.id LIMIT ?",
        )?;
        let rows = st.query_map(params![since.unwrap_or(NO_SINCE), n as i64], |r| {
            Ok(StoredSession {
                meta: SessionMeta {
                    id: r.get(0)?,
                    parent_session_id: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                    file: r.get(2)?,
                    cwds: parse_json_strings(r.get::<_, Option<String>>(3)?.as_deref()),
                    title: r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
                    git_branch: r.get::<_, Option<String>>(5)?.filter(|s| !s.is_empty()),
                    first_ts: r.get(6)?,
                    last_ts: r.get(7)?,
                },
                chunk_count: r.get::<_, i64>(8)?.max(0) as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sessions whose turns created/edited `path`, newest edit first. An
    /// absolute path matches exactly; a relative one also as a path suffix.
    pub fn sessions_for_file(&self, path: &str, limit: usize) -> Result<Vec<FileHistoryEntry>> {
        let n = clamp_limit(Some(limit), 10, MAX_LIMIT);
        let rel = path.strip_prefix("./").unwrap_or(path);
        let b = path.as_bytes();
        let is_abs = path.starts_with('/')
            || path.starts_with("\\\\")
            || (b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && matches!(b[2], b'/' | b'\\'));
        let filter = if is_abs {
            "f.path = ?"
        } else {
            "(f.path = ? OR f.path LIKE ? ESCAPE '\\')"
        };
        let sql = format!(
            "SELECT c.id, c.session_id, c.seq, c.ts, c.text, s.title, s.git_branch
             FROM chunk_files f JOIN chunks c ON c.rid = f.chunk_rid
             LEFT JOIN sessions s ON s.id = c.session_id
             WHERE {filter} ORDER BY c.ts DESC, c.seq DESC LIMIT ?"
        );
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st = c.prepare_cached(&sql)?;
        type Row = (
            String,
            String,
            i64,
            i64,
            String,
            Option<String>,
            Option<String>,
        );
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Row> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        };
        let cap = (n * 50) as i64;
        let rows: Vec<Row> = if is_abs {
            st.query_map(params![path, cap], map)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            let escaped: String = rel
                .chars()
                .flat_map(|ch| {
                    if matches!(ch, '\\' | '%' | '_') {
                        vec!['\\', ch]
                    } else {
                        vec![ch]
                    }
                })
                .collect();
            st.query_map(params![rel, format!("%/{escaped}"), cap], map)?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut entries: Vec<FileHistoryEntry> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (id, sid, seq, ts, text, title, branch) in rows {
            let idx = match index.get(&sid) {
                Some(&i) => i,
                None => {
                    if entries.len() >= n {
                        continue;
                    }
                    entries.push(FileHistoryEntry {
                        session_id: sid.clone(),
                        session_title: title.filter(|t| !t.is_empty()),
                        git_branch: branch.filter(|t| !t.is_empty()),
                        last_ts: ts,
                        chunks: vec![],
                    });
                    index.insert(sid, entries.len() - 1);
                    entries.len() - 1
                }
            };
            let e = &mut entries[idx];
            if !e.chunks.iter().any(|ch| ch.chunk_id == id) {
                e.chunks.push(FileHistoryChunk {
                    chunk_id: id,
                    seq,
                    ts,
                    snippet: make_snippet(&text, &[]),
                });
            }
        }
        Ok(entries)
    }

    /// Remove a session with its chunks, FTS rows, vectors and file-state rows.
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        inner.tx(|i| {
            let c = i.conn()?;
            let rids: Vec<i64> = c
                .prepare_cached("SELECT rid FROM chunks WHERE session_id = ?")?
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            c.execute("DELETE FROM chunks WHERE session_id = ?", [id])?;
            c.execute("DELETE FROM sessions WHERE id = ?", [id])?;
            c.execute("DELETE FROM files WHERE session_id = ?", [id])?;
            c.execute("DELETE FROM distill_groups WHERE session_id = ?", [id])?;
            if let Some(cache) = i.cache.as_mut() {
                rids.iter().for_each(|&r| cache.remove(r));
            }
            Ok(())
        })
    }

    pub fn stats(&self) -> Result<HistoryStats> {
        let inner = self.lock();
        let c = inner.conn()?;
        let (sessions, chunks, embedded): (i64, i64, i64) = c.query_row(
            "SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM chunks),
             (SELECT COUNT(*) FROM chunks WHERE vec IS NOT NULL)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let db_bytes = if self.db_path == ":memory:" {
            let pc: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let ps: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            (pc * ps) as u64
        } else {
            [self.db_path.clone(), format!("{}-wal", self.db_path)]
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum()
        };
        Ok(HistoryStats {
            sessions: sessions as u64,
            chunks: chunks as u64,
            embedded_chunks: embedded as u64,
            db_bytes,
        })
    }

    // ─── Search ────────────────────────────────────────────────────────────

    fn ensure_cache(inner: &mut Inner) -> Result<&VectorCache> {
        if inner.cache.is_none() {
            let cache = {
                let c = inner.conn()?;
                let n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE vec IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?;
                let mut cache = VectorCache::with_capacity((n as usize).max(1024));
                let mut st = c.prepare("SELECT rid, ts, vec FROM chunks WHERE vec IS NOT NULL")?;
                let mut rows = st.query([])?;
                while let Some(r) = rows.next()? {
                    let blob: Vec<u8> = r.get(2)?;
                    if let Some(v) = blob_to_vec(&blob) {
                        cache.set(r.get(0)?, r.get(1)?, &v);
                    }
                }
                cache
            };
            inner.cache = Some(cache);
        }
        Ok(inner.cache.as_ref().expect("cache"))
    }

    fn lexical_ranks(c: &Connection, m: &str, since: Option<i64>, k: usize) -> Result<Vec<Ranked>> {
        let mut st = c.prepare_cached(
            "SELECT c.rid AS rid, bm25(chunks_fts) AS r
             FROM chunks_fts JOIN chunks c ON c.rid = chunks_fts.rowid
             WHERE chunks_fts MATCH ? AND c.ts >= ?
             ORDER BY r LIMIT ?",
        )?;
        let rows = st.query_map(params![m, since.unwrap_or(NO_SINCE), k as i64], |r| {
            Ok(Ranked {
                rid: r.get(0)?,
                score: -r.get::<_, f64>(1)?,
                exact: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Terms too short for the trigram index, with no vector to fall back on: a LIKE scan.
    fn like_ranks(
        c: &Connection,
        terms: &[QueryTerm],
        since: Option<i64>,
        k: usize,
    ) -> Result<Vec<Ranked>> {
        let words: Vec<&str> = terms
            .iter()
            .map(|t| t.text.as_str())
            .filter(|t| !t.is_empty())
            .take(4)
            .collect();
        if words.is_empty() {
            return Ok(vec![]);
        }
        let clause = words
            .iter()
            .map(|_| "c.text LIKE ? ESCAPE '\\'")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT c.rid AS rid FROM chunks c WHERE ({clause}) AND c.ts >= ? ORDER BY c.ts DESC LIMIT ?"
        );
        let mut values: Vec<rusqlite::types::Value> = words
            .iter()
            .map(|w| {
                let esc: String = w
                    .chars()
                    .flat_map(|ch| {
                        if matches!(ch, '\\' | '%' | '_') {
                            vec!['\\', ch]
                        } else {
                            vec![ch]
                        }
                    })
                    .collect();
                rusqlite::types::Value::Text(format!("%{esc}%"))
            })
            .collect();
        values.push(since.unwrap_or(NO_SINCE).into());
        values.push((k as i64).into());
        let mut st = c.prepare_cached(&sql)?;
        let rids: Vec<i64> = st
            .query_map(rusqlite::params_from_iter(values), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rids
            .into_iter()
            .enumerate()
            .map(|(i, rid)| Ranked {
                rid,
                score: 1.0 / (i as f64 + 1.0),
                exact: false,
            })
            .collect())
    }

    /// Identifier-gated hybrid search (see store.ts for the measurements):
    /// the lexical leg runs only on identifier-like terms; with none, results
    /// are dense only; with some, dense and lexical top-100 are fused by RRF
    /// (k=60, equal weights). Without a vector, lexical over all terms (bm25),
    /// or a LIKE scan when nothing is ≥3 chars. Ties: a chunk with an exact
    /// identifier wins, then the newer one.
    pub fn search(&self, q: &SearchQuery, mode: Option<SearchMode>) -> Result<Vec<SearchHit>> {
        let limit = clamp_limit(q.limit, DEFAULT_LIMIT, MAX_LIMIT);
        let since = q.since;
        let terms = parse_query_terms(&q.text);
        let mode = mode.unwrap_or(if q.vector.is_some() {
            SearchMode::Hybrid
        } else {
            SearchMode::Lexical
        });
        if mode != SearchMode::Lexical && q.vector.is_none() {
            bail!(
                "history: {} search needs a query vector",
                if mode == SearchMode::Dense {
                    "dense"
                } else {
                    "hybrid"
                }
            );
        }
        let mut inner = self.lock();
        let ranked: Vec<Ranked> = if mode == SearchMode::Lexical {
            let c = inner.conn()?;
            match build_match_expression(&terms) {
                Some(m) => Self::lexical_ranks(c, &m, since, limit)?,
                None => Self::like_ranks(c, &terms, since, limit)?,
            }
        } else {
            let qv = normalized(q.vector.as_deref().unwrap_or_default())?;
            let k = if mode == SearchMode::Dense {
                limit
            } else {
                FUSION_CANDIDATES
            };
            let dense = Self::ensure_cache(&mut inner)?.top_k(&qv, k, since);
            let ident: Vec<QueryTerm> = terms.iter().filter(|t| t.identifier).cloned().collect();
            let m = if mode == SearchMode::Hybrid {
                build_match_expression(&ident)
            } else {
                None
            };
            match m {
                None => dense.into_iter().take(limit).collect(),
                Some(m) => {
                    let lexical = Self::lexical_ranks(inner.conn()?, &m, since, FUSION_CANDIDATES)?;
                    let mut fused: Vec<Ranked> = Vec::new();
                    let mut at: HashMap<i64, usize> = HashMap::new();
                    let mut add = |rid: i64, rank: usize, exact: bool| {
                        let i = *at.entry(rid).or_insert_with(|| {
                            fused.push(Ranked {
                                rid,
                                score: 0.0,
                                exact: false,
                            });
                            fused.len() - 1
                        });
                        fused[i].score += 1.0 / (RRF_K + rank as f64 + 1.0);
                        if exact {
                            fused[i].exact = true;
                        }
                    };
                    dense
                        .iter()
                        .enumerate()
                        .for_each(|(i, h)| add(h.rid, i, false));
                    lexical
                        .iter()
                        .enumerate()
                        .for_each(|(i, h)| add(h.rid, i, true));
                    fused
                }
            }
        };
        Self::hydrate(inner.conn()?, ranked, &terms, limit)
    }

    fn hydrate(
        c: &Connection,
        mut ranked: Vec<Ranked>,
        terms: &[QueryTerm],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        // Cut to the top `limit` plus anything tied with the last one, so the
        // recency tie-break sees every contender, before touching rows.
        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let cutoff = if ranked.is_empty() {
            f64::NEG_INFINITY
        } else {
            ranked[limit.min(ranked.len()) - 1].score
        };
        let mut st = c.prepare_cached(
            "SELECT c.id, c.session_id, c.ts, c.kind, c.text, s.title, m.note_kind, m.sources
             FROM chunks c LEFT JOIN sessions s ON s.id = c.session_id
             LEFT JOIN note_meta m ON m.chunk_rid = c.rid WHERE c.rid = ?",
        )?;
        type Row = (
            String,
            String,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let mut rows: Vec<(Ranked, Row)> = Vec::new();
        for (i, h) in ranked.into_iter().enumerate() {
            if !(i < limit || h.score == cutoff) {
                continue;
            }
            let row: Option<Row> = st
                .query_row([h.rid], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })
                .optional()?;
            if let Some(row) = row {
                rows.push((h, row));
            }
        }
        rows.sort_by(|(ha, ra), (hb, rb)| {
            hb.score
                .partial_cmp(&ha.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(hb.exact.cmp(&ha.exact))
                .then(rb.2.cmp(&ra.2))
        });
        Ok(rows
            .into_iter()
            .take(limit)
            .map(
                |(h, (id, sid, ts, kind, text, title, note_kind, sources))| SearchHit {
                    chunk_id: id,
                    session_id: sid,
                    ts,
                    kind: ChunkKind::parse(&kind).unwrap_or(ChunkKind::Turn),
                    snippet: make_snippet(&text, terms),
                    score: h.score,
                    session_title: title.filter(|t| !t.is_empty()),
                    note: note_ref(note_kind, sources),
                },
            )
            .collect())
    }

    // ─── Decision notes ────────────────────────────────────────────────────

    /// Main sessions (no subagents) with their turn chunks' lengths and hashes,
    /// in seq order. Compaction summaries are excluded.
    pub fn distill_candidates(&self) -> Result<Vec<DistillCandidate>> {
        let inner = self.lock();
        let c = inner.conn()?;
        let mut out: Vec<DistillCandidate> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        {
            let mut st = c.prepare_cached(
                "SELECT id, title, last_ts FROM sessions WHERE parent_id IS NULL",
            )?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (id, title, last_ts) = row?;
                by_id.insert(id.clone(), out.len());
                out.push(DistillCandidate {
                    session_id: id,
                    title: title.filter(|t| !t.is_empty()),
                    last_ts,
                    chunks: vec![],
                });
            }
        }
        let mut st = c.prepare_cached(
            "SELECT session_id, id, seq, hash, length(text) AS chars, ts FROM chunks
             WHERE kind = 'turn' ORDER BY session_id, seq, rid",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let sid: String = r.get(0)?;
            if let Some(&i) = by_id.get(&sid) {
                out[i].chunks.push(CandidateChunk {
                    id: r.get(1)?,
                    seq: r.get(2)?,
                    hash: r.get(3)?,
                    chars: r.get(4)?,
                    ts: r.get(5)?,
                });
            }
        }
        out.retain(|c| !c.chunks.is_empty());
        Ok(out)
    }

    /// Text of the given chunks, in the order given (missing ids skipped).
    pub fn get_chunk_texts(&self, ids: &[String]) -> Result<Vec<ChunkText>> {
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st = c.prepare_cached("SELECT id, seq, ts, text FROM chunks WHERE id = ?")?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(row) = st
                .query_row([id], |r| {
                    Ok(ChunkText {
                        id: r.get(0)?,
                        seq: r.get(1)?,
                        ts: r.get(2)?,
                        text: r.get(3)?,
                    })
                })
                .optional()?
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    fn group_states_locked(
        c: &Connection,
        session_id: &str,
    ) -> Result<BTreeMap<i64, DistillGroupState>> {
        let mut st = c.prepare_cached(
            "SELECT grp, input_hash, status, attempts, cost_usd, input_tokens, output_tokens, notes, distilled_at, error
             FROM distill_groups WHERE session_id = ?",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(DistillGroupState {
                group: r.get(0)?,
                input_hash: r.get(1)?,
                ok: r.get::<_, String>(2)? == "ok",
                attempts: r.get(3)?,
                cost_usd: r.get(4)?,
                input_tokens: r.get(5)?,
                output_tokens: r.get(6)?,
                notes: r.get(7)?,
                distilled_at: r.get(8)?,
                error: r.get::<_, Option<String>>(9)?.filter(|e| !e.is_empty()),
            })
        })?;
        let mut map = BTreeMap::new();
        for row in rows {
            let s = row?;
            map.insert(s.group, s);
        }
        Ok(map)
    }

    /// Distillation state of every group of a session, by group index.
    pub fn group_states(&self, session_id: &str) -> Result<BTreeMap<i64, DistillGroupState>> {
        let inner = self.lock();
        Self::group_states_locked(inner.conn()?, session_id)
    }

    fn delete_group_notes(i: &mut Inner, session_id: &str, group: i64) -> Result<()> {
        let rids: Vec<i64> = {
            let c = i.conn()?;
            let rids: Vec<i64> = c
                .prepare_cached("SELECT c.rid FROM chunks c JOIN note_meta m ON m.chunk_rid = c.rid WHERE c.session_id = ? AND m.grp = ?")?
                .query_map(params![session_id, group], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut del = c.prepare_cached("DELETE FROM chunks WHERE rid = ?")?;
            for r in &rids {
                del.execute([r])?;
            }
            rids
        };
        if let Some(cache) = i.cache.as_mut() {
            rids.iter().for_each(|&r| cache.remove(r));
        }
        Ok(())
    }

    /// Make the notes of one group exactly `notes` (old ones go, new ones are
    /// inserted unembedded); with `run`, the group's state is recorded in the
    /// same transaction.
    pub fn replace_group_notes(
        &self,
        session_id: &str,
        group: i64,
        notes: &[NoteInput],
        run: Option<&DistillRunRecord>,
    ) -> Result<Vec<String>> {
        let mut inner = self.lock();
        inner.tx(|i| {
            Self::delete_group_notes(i, session_id, group)?;
            let c = i.conn()?;
            let mut ids = Vec::new();
            for n in notes {
                if n.chunk.session_id != session_id {
                    bail!("history: note {} belongs to {}, not {session_id}", n.chunk.id, n.chunk.session_id);
                }
                let rid: i64 = c
                    .prepare_cached(
                        "INSERT INTO chunks(id, session_id, seq, ts, kind, text, hash) VALUES (?, ?, ?, ?, 'decision', ?, ?) RETURNING rid",
                    )?
                    .query_row(params![n.chunk.id, session_id, n.chunk.seq, n.chunk.ts, n.chunk.text, n.chunk.hash], |r| r.get(0))?;
                c.prepare_cached("INSERT INTO note_meta(chunk_rid, grp, note_kind, sources) VALUES (?, ?, ?, ?)")?
                    .execute(params![rid, group, n.note_kind.as_str(), serde_json::to_string(&n.sources)?])?;
                ids.push(n.chunk.id.clone());
            }
            if let Some(run) = run {
                let run = DistillRunRecord { ok: true, ..run.clone() };
                Self::record_group_run_locked(c, session_id, group, &run, Some(notes.len() as i64))?;
            }
            Ok(ids)
        })
    }

    /// The stored notes of one group, in insertion order.
    pub fn get_group_notes(&self, session_id: &str, group: i64) -> Result<Vec<NoteInput>> {
        let inner = self.lock();
        let c = inner.conn()?;
        let mut st = c.prepare_cached(
            "SELECT c.id, c.session_id, c.seq, c.ts, c.text, c.hash, m.note_kind, m.sources
             FROM chunks c JOIN note_meta m ON m.chunk_rid = c.rid
             WHERE c.session_id = ? AND m.grp = ? ORDER BY c.rid",
        )?;
        let rows = st.query_map(params![session_id, group], |r| {
            Ok((
                TranscriptChunk {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    seq: r.get(2)?,
                    ts: r.get(3)?,
                    kind: ChunkKind::Decision,
                    text: r.get(4)?,
                    files: vec![],
                    hash: r.get(5)?,
                },
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (chunk, kind, sources) = row?;
            out.push(NoteInput {
                chunk,
                note_kind: DecisionNoteKind::parse(&kind).unwrap_or(DecisionNoteKind::Decision),
                sources: parse_json_strings(Some(&sources)),
            });
        }
        Ok(out)
    }

    fn record_group_run_locked(
        c: &Connection,
        session_id: &str,
        group: i64,
        run: &DistillRunRecord,
        notes: Option<i64>,
    ) -> Result<()> {
        let prev = Self::group_states_locked(c, session_id)?.remove(&group);
        let same_input = prev
            .as_ref()
            .is_some_and(|p| p.input_hash == run.input_hash);
        let attempts = if run.ok {
            0
        } else {
            (if same_input {
                prev.as_ref().map_or(0, |p| p.attempts)
            } else {
                0
            }) + i64::from(run.counts_as_attempt)
        };
        let stored_notes = if run.ok {
            notes.unwrap_or(0)
        } else {
            prev.as_ref().map_or(0, |p| p.notes)
        };
        c.prepare_cached(
            "INSERT INTO distill_groups(session_id, grp, input_hash, status, attempts, cost_usd, input_tokens,
               output_tokens, notes, distilled_at, error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(session_id, grp) DO UPDATE SET input_hash = excluded.input_hash, status = excluded.status,
               attempts = excluded.attempts, cost_usd = distill_groups.cost_usd + excluded.cost_usd,
               input_tokens = distill_groups.input_tokens + excluded.input_tokens,
               output_tokens = distill_groups.output_tokens + excluded.output_tokens,
               notes = excluded.notes, distilled_at = excluded.distilled_at, error = excluded.error",
        )?
        .execute(params![
            session_id,
            group,
            run.input_hash,
            if run.ok { "ok" } else { "error" },
            attempts,
            if run.cost_usd.is_finite() { run.cost_usd } else { 0.0 },
            js_round(run.input_tokens),
            js_round(run.output_tokens),
            stored_notes,
            run.at,
            if run.ok { None } else { Some(run.error.clone().unwrap_or_else(|| "failed".into())) },
        ])?;
        if run.cost_usd > 0.0 {
            c.prepare_cached(
                "INSERT INTO meta(key, value) VALUES ('distill_spent_usd', ?)
                 ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(meta.value AS REAL) + CAST(excluded.value AS REAL) AS TEXT)",
            )?
            .execute([run.cost_usd.to_string()])?;
        }
        Ok(())
    }

    /// Record one distillation run of a group (see store.ts recordGroupRun).
    pub fn record_group_run(
        &self,
        session_id: &str,
        group: i64,
        run: &DistillRunRecord,
    ) -> Result<()> {
        let mut inner = self.lock();
        inner.tx(|i| Self::record_group_run_locked(i.conn()?, session_id, group, run, None))
    }

    /// Drop the notes and state of groups at index >= `group_count`.
    pub fn prune_groups(&self, session_id: &str, group_count: i64) -> Result<()> {
        let mut inner = self.lock();
        let stale: Vec<i64> = Self::group_states_locked(inner.conn()?, session_id)?
            .into_keys()
            .filter(|&g| g >= group_count)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        inner.tx(|i| {
            for g in stale {
                Self::delete_group_notes(i, session_id, g)?;
                i.conn()?.execute(
                    "DELETE FROM distill_groups WHERE session_id = ? AND grp = ?",
                    params![session_id, g],
                )?;
            }
            Ok(())
        })
    }

    /// Delete every note and every group's state; returns how many notes went.
    pub fn delete_notes(&self) -> Result<usize> {
        let mut inner = self.lock();
        inner.tx(|i| {
            let rids: Vec<i64> = {
                let c = i.conn()?;
                let rids: Vec<i64> = c
                    .prepare_cached("SELECT rid FROM chunks WHERE kind = 'decision'")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                c.execute("DELETE FROM chunks WHERE kind = 'decision'", [])?;
                c.execute("DELETE FROM distill_groups", [])?;
                rids
            };
            if let Some(cache) = i.cache.as_mut() {
                rids.iter().for_each(|&r| cache.remove(r));
            }
            Ok(rids.len())
        })
    }

    /// Notes stored and total spent on distillation in this DB.
    pub fn distill_totals(&self) -> Result<(u64, f64)> {
        let inner = self.lock();
        let (notes, spent): (i64, Option<String>) = inner.conn()?.query_row(
            "SELECT (SELECT COUNT(*) FROM chunks WHERE kind = 'decision'),
             (SELECT value FROM meta WHERE key = 'distill_spent_usd')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let spent = spent
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        Ok((notes as u64, spent))
    }
}

impl Drop for HistoryStore {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
