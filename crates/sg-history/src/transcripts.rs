//! Turning Claude Code's JSONL session store into retrievable chunks
//! (port of src/main/history/transcripts.ts).
//!
//! The store records far more than the conversation; this module reduces a
//! transcript to turns (one real prompt plus everything the assistant wrote
//! until the next one) with deterministic ids, so re-parsing a grown file
//! yields the same ids and the store re-embeds only chunks whose hash moved.
//!
//! PARITY: chunk text and hashes must be byte-identical to the Electron app's,
//! because both apps write the same per-workspace DBs. Lengths are therefore
//! measured in UTF-16 code units (JavaScript `length`) and every regex keeps
//! JavaScript's ASCII `\b` / `\w` / `\d` and its `\s` set.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::ChunkKind;
use crate::util::{
    JS_WS, is_js_ws, js_trim, len16, mtime_ms, one_line, parse_ts, path_str, sha1_hex, truthy,
};

// ─── Tunables ────────────────────────────────────────────────────────────────

/// A prompt shorter than this ("ok", "yes") is folded into the current turn.
pub const MIN_PROMPT_CHARS: usize = 15;
/// Upper bound on one chunk's text; a longer turn is split into parts.
pub const MAX_CHUNK_CHARS: usize = 18_000;
/// One-line tool markers are cut to this length.
pub const TOOL_MARKER_CHARS: usize = 100;
const CONTINUATION_PROMPT_CHARS: usize = 300;
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

const SYSTEM_PREFIXES: [&str; 12] = [
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<command-stdout>",
    "<command-stderr>",
    "<system-reminder>",
    "<user-prompt-submit-hook>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "[Request interrupted",
    "Caveat:",
];

// ─── Types ───────────────────────────────────────────────────────────────────

/// One Claude Code transcript file's metadata.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SessionMeta {
    pub id: String,
    pub file: String,
    pub parent_session_id: Option<String>,
    pub cwds: Vec<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub title: Option<String>,
    pub git_branch: Option<String>,
}

/// One retrievable unit (see src/shared/history.ts TranscriptChunk).
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptChunk {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub ts: i64,
    pub kind: ChunkKind,
    pub text: String,
    pub files: Vec<String>,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTranscript {
    pub meta: SessionMeta,
    pub chunks: Vec<TranscriptChunk>,
}

/// One transcript file found under the projects dir.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptFile {
    pub id: String,
    pub file: String,
    pub parent_session_id: Option<String>,
    pub size: u64,
    pub mtime_ms: f64,
}

/// Diagnostics hook: (rule name, original match).
pub type RedactHook<'a> = &'a mut dyn FnMut(&str, &str);

#[derive(Default)]
pub struct ParseOptions<'a> {
    pub max_chunk_chars: Option<usize>,
    pub min_prompt_chars: Option<usize>,
    /// Diagnostics: called once per redacted secret with the rule name and the original match.
    pub on_redact: Option<RedactHook<'a>>,
}

#[derive(Clone, Debug, Default)]
pub struct TranscriptSource {
    pub file: String,
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
}

impl From<&str> for TranscriptSource {
    fn from(file: &str) -> Self {
        Self {
            file: file.to_string(),
            ..Default::default()
        }
    }
}

impl From<&TranscriptFile> for TranscriptSource {
    fn from(f: &TranscriptFile) -> Self {
        Self {
            file: f.file.clone(),
            id: Some(f.id.clone()),
            parent_session_id: f.parent_session_id.clone(),
        }
    }
}

// ─── File discovery ──────────────────────────────────────────────────────────

fn list_dir(dir: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn stat_file(file: PathBuf, id: String, parent: Option<String>) -> Option<TranscriptFile> {
    let meta = std::fs::metadata(&file).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(TranscriptFile {
        id,
        file: path_str(&file),
        parent_session_id: parent,
        size: meta.len(),
        mtime_ms: mtime_ms(&meta),
    })
}

/// Every main-session and subagent transcript under `projects_dir`:
///   `<slug>/<sessionId>.jsonl` and `<slug>/<sessionId>/subagents/agent-<id>.jsonl`.
/// The slug is never decoded; membership comes from the records' `cwd`.
/// Sorted by path. Stat only, no reads.
pub fn discover_transcript_files(projects_dir: &Path) -> Vec<TranscriptFile> {
    let mut found = Vec::new();
    for project in list_dir(projects_dir) {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for entry in list_dir(&project.path()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_file() && name.ends_with(".jsonl") {
                let id = name[..name.len() - 6].to_string();
                found.extend(stat_file(entry.path(), id, None));
            } else if ft.is_dir() {
                for sub in list_dir(&entry.path().join("subagents")) {
                    let sub_name = sub.file_name().to_string_lossy().into_owned();
                    if !sub.file_type().is_ok_and(|t| t.is_file()) || !sub_name.ends_with(".jsonl")
                    {
                        continue;
                    }
                    let id = sub_name[..sub_name.len() - 6].to_string();
                    found.extend(stat_file(sub.path(), id, Some(name.clone())));
                }
            }
        }
    }
    found.sort_by(|a, b| a.file.cmp(&b.file));
    found
}

// ─── Envelope interpretation (agent/transcript-text.ts) ──────────────────────

enum StoredText {
    Text,
    Command(String),
    Output(String),
    Hidden,
}

fn tag_content(source: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = source.find(&open)? + open.len();
    let end = source[start..].find(&close)? + start;
    Some(js_trim(&source[start..end]).to_string())
}

/// Inline wrappers that can ride along inside a real prompt.
fn strip_inline_noise(text: &str) -> String {
    let mut out = text.to_string();
    for re in [
        r"(?s)<system-reminder>.*?</system-reminder>",
        r"(?s)<local-command-caveat>.*?</local-command-caveat>",
        r"(?s)<user-prompt-submit-hook>.*?</user-prompt-submit-hook>",
    ] {
        out = cached_regex(re).replace_all(&out, "").into_owned();
    }
    js_trim(&out).to_string()
}

fn cached_regex(pattern: &'static str) -> &'static Regex {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: LazyLock<Mutex<HashMap<&'static str, &'static Regex>>> =
        LazyLock::new(Default::default);
    let mut map = CACHE.lock().unwrap();
    map.entry(pattern)
        .or_insert_with(|| Box::leak(Box::new(Regex::new(pattern).unwrap())))
}

fn interpret_stored_user_text(raw: &str) -> StoredText {
    let source = js_trim(raw);
    if source.is_empty() {
        return StoredText::Hidden;
    }
    if let Some(name) = tag_content(source, "command-name") {
        let args = tag_content(source, "command-args").unwrap_or_default();
        let text = if args.is_empty() {
            name
        } else {
            format!("{name} {args}")
        };
        return StoredText::Command(js_trim(&text).to_string());
    }
    if let Some(stdout) = tag_content(source, "local-command-stdout")
        .or_else(|| tag_content(source, "command-stdout"))
    {
        return if stdout.is_empty() {
            StoredText::Hidden
        } else {
            StoredText::Output(stdout)
        };
    }
    if source.starts_with("<task-notification>") {
        let summary = tag_content(source, "summary")
            .or_else(|| tag_content(source, "status"))
            .unwrap_or_default();
        return if summary.is_empty() {
            StoredText::Hidden
        } else {
            StoredText::Output(summary)
        };
    }
    let cleaned = strip_inline_noise(source);
    if cleaned.is_empty() || cached_regex(r"^<[a-z][a-z0-9-]*>").is_match(&cleaned) {
        return StoredText::Hidden;
    }
    StoredText::Text
}

enum UserTextKind {
    Prompt(String),
    Command(String),
    Notification(String),
    Ignore,
}

fn classify_user_text(raw: &str) -> UserTextKind {
    let source = js_trim(raw);
    if source.is_empty() {
        return UserTextKind::Ignore;
    }
    if source.starts_with("<task-notification>") {
        return UserTextKind::Notification(match interpret_stored_user_text(source) {
            StoredText::Output(t) | StoredText::Command(t) => t,
            StoredText::Text => strip_inline_noise(source),
            StoredText::Hidden => String::new(),
        });
    }
    if source.starts_with("<command-name>") || source.starts_with("<command-message>") {
        return match interpret_stored_user_text(source) {
            StoredText::Command(t) if !t.is_empty() => UserTextKind::Command(t),
            _ => UserTextKind::Ignore,
        };
    }
    if SYSTEM_PREFIXES.iter().any(|p| source.starts_with(p)) {
        let rest = strip_inline_noise(source);
        if rest.is_empty() || SYSTEM_PREFIXES.iter().any(|p| rest.starts_with(p)) {
            return UserTextKind::Ignore;
        }
        return UserTextKind::Prompt(rest);
    }
    let cleaned = strip_inline_noise(source);
    if cleaned.is_empty() {
        UserTextKind::Ignore
    } else {
        UserTextKind::Prompt(cleaned)
    }
}

fn peer_message_body(text: &str) -> String {
    let lead = cached_regex_owned(format!(
        "^[{JS_WS}]*Another Claude session sent a message:[{JS_WS}]*"
    ));
    let out = lead.replacen(text, 1, "");
    let out = cached_regex(r"<agent-message(?:[^A-Za-z0-9_>][^>]*)?>").replace_all(&out, "");
    let out = out.replace("</agent-message>", "");
    js_trim(&out).to_string()
}

fn cached_regex_owned(pattern: String) -> &'static Regex {
    cached_regex(Box::leak(pattern.into_boxed_str()))
}

// ─── Record helpers ──────────────────────────────────────────────────────────

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `stringField`: a string whose trim is non-empty (returned untrimmed).
fn string_field<'a>(input: Option<&'a Value>, key: &str) -> Option<&'a str> {
    let s = input?.as_object()?.get(key)?.as_str()?;
    if js_trim(s).is_empty() { None } else { Some(s) }
}

/// `x?.kind` where `x` must be an object holding `kind` (any JSON value, null included).
fn origin_kind(origin: Option<&Value>) -> Option<&Value> {
    origin?.as_object()?.get("kind")
}

fn kind_is(k: Option<&Value>, s: &str) -> bool {
    k.and_then(Value::as_str) == Some(s)
}

fn user_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(s)) => {
            if js_trim(s).is_empty() {
                vec![]
            } else {
                vec![s.clone()]
            }
        }
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| str_field(b, "type") == Some("text"))
            .filter_map(|b| str_field(b, "text"))
            .filter(|t| !js_trim(t).is_empty())
            .map(str::to_string)
            .collect(),
        _ => vec![],
    }
}

fn tool_marker(name: &str, input: Option<&Value>) -> String {
    let pick = |key: &str| string_field(input, key);
    let detail: Option<String> = match name {
        "Bash" | "PowerShell" => pick("description")
            .or_else(|| pick("command"))
            .map(str::to_string),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => pick("file_path")
            .or_else(|| pick("notebook_path"))
            .map(str::to_string),
        "Glob" | "Grep" => match (pick("pattern"), pick("path")) {
            (Some(p), Some(w)) => Some(format!("{p} in {w}")),
            (Some(p), None) => Some(p.to_string()),
            (None, w) => w.map(str::to_string),
        },
        "WebFetch" => pick("url").map(str::to_string),
        "WebSearch" => pick("query").map(str::to_string),
        "Task" | "Agent" => pick("description")
            .or_else(|| pick("subagent_type"))
            .map(str::to_string),
        "Skill" => pick("skill")
            .or_else(|| pick("command"))
            .map(str::to_string),
        "TodoWrite" => None,
        _ => [
            "command",
            "file_path",
            "path",
            "url",
            "query",
            "description",
            "pattern",
            "prompt",
        ]
        .iter()
        .find_map(|k| pick(k))
        .map(str::to_string),
    };
    match detail {
        Some(d) => format!("[{name}] {}", one_line(&d, TOOL_MARKER_CHARS)),
        None => format!("[{name}]"),
    }
}

fn is_win_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    matches!(b.first(), Some(b'/' | b'\\'))
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
}

fn edited_file(name: &str, input: Option<&Value>, cwd: Option<&str>) -> Option<String> {
    if !EDIT_TOOLS.contains(&name) {
        return None;
    }
    let raw = string_field(input, "file_path").or_else(|| string_field(input, "notebook_path"))?;
    match cwd {
        Some(cwd) if !cwd.is_empty() && !raw.starts_with('/') && !is_win_absolute(raw) => {
            Some(posix_normalize(&format!("{cwd}/{raw}")))
        }
        _ => Some(raw.to_string()),
    }
}

// ─── Chunk building ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    User,
    Assistant,
    Summary,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::User => "User: ",
            Role::Assistant => "Assistant: ",
            Role::Summary => "Compaction summary: ",
        }
    }
}

#[derive(Clone, Debug)]
struct Piece {
    role: Role,
    text: String,
    ts: i64,
    files: Vec<String>,
}

struct Turn {
    seq: i64,
    kind: ChunkKind,
    ts: i64,
    opener: Option<String>,
    opener_queued: bool,
    pieces: Vec<Piece>,
    has_assistant_text: bool,
}

fn piece_cost(piece: &Piece, prev: Option<Role>) -> usize {
    match prev {
        Some(p) if p == piece.role => 1 + len16(&piece.text),
        None => piece.role.label().len() + len16(&piece.text),
        Some(_) => 2 + piece.role.label().len() + len16(&piece.text),
    }
}

fn render_pieces<'a>(pieces: impl Iterator<Item = &'a Piece>) -> String {
    let mut out = String::new();
    let mut prev: Option<Role> = None;
    for piece in pieces {
        if Some(piece.role) != prev {
            if prev.is_some() {
                out.push_str("\n\n");
            }
            out.push_str(piece.role.label());
        } else {
            out.push('\n');
        }
        out.push_str(&piece.text);
        prev = Some(piece.role);
    }
    out
}

/// Cut an oversized piece into slices of at most `max` UTF-16 units, preferring line breaks.
fn slice_piece(piece: Piece, max: usize) -> Vec<Piece> {
    if len16(&piece.text) <= max {
        return vec![piece];
    }
    let units: Vec<u16> = piece.text.encode_utf16().collect();
    let mut slices: Vec<Piece> = Vec::new();
    let mut start = 0usize;
    while start < units.len() {
        let mut end = (start + max).min(units.len());
        if end < units.len() {
            // lastIndexOf('\n', end): the last newline at or before `end`.
            let upto = end.min(units.len() - 1);
            if let Some(nl) = units[..=upto].iter().rposition(|&u| u == u16::from(b'\n'))
                && (nl as f64) > start as f64 + max as f64 / 2.0
            {
                end = nl + 1;
            }
            let code = units[end - 1];
            if (0xD800..=0xDBFF).contains(&code) {
                end -= 1;
            }
        }
        let text = String::from_utf16_lossy(&units[start..end]);
        let slice = js_trim(&text);
        if !slice.is_empty() {
            let files = if slices.is_empty() {
                piece.files.clone()
            } else {
                Vec::new()
            };
            slices.push(Piece {
                role: piece.role,
                text: slice.to_string(),
                ts: piece.ts,
                files,
            });
        }
        start = end;
    }
    slices
}

pub(crate) fn hash_chunk(text: &str, files: &[String]) -> String {
    sha1_hex(&[text.as_bytes(), b"\0", files.join("\n").as_bytes()])
}

/// Render a turn into one or more chunks, packed greedily from the start so a
/// growing turn only changes its last part. Part 0 keeps the bare `<sid>:<seq>` id.
fn chunks_for_turn(turn: Turn, session_id: &str, budget: usize) -> Vec<TranscriptChunk> {
    let header: Option<Piece> = turn.opener.as_ref().map(|opener| Piece {
        role: if turn.kind == ChunkKind::CompactSummary {
            Role::Summary
        } else {
            Role::User
        },
        text: format!(
            "{} (continued)",
            one_line(opener, CONTINUATION_PROMPT_CHARS)
        ),
        ts: turn.ts,
        files: vec![],
    });
    let header_cost = header.as_ref().map_or(0, |h| piece_cost(h, None));
    let max_piece = 1000usize.max(budget.saturating_sub(header_cost + 64));
    let pieces: Vec<Piece> = turn
        .pieces
        .into_iter()
        .flat_map(|p| slice_piece(p, max_piece))
        .collect();

    // A part is a list of indices into `pieces`, optionally led by the header.
    let mut parts: Vec<(bool, Vec<usize>)> = Vec::new();
    let mut current: (bool, Vec<usize>) = (false, Vec::new());
    let mut current_len = 0usize;
    let mut prev: Option<Role> = None;
    for (i, piece) in pieces.iter().enumerate() {
        let mut cost = piece_cost(piece, prev);
        if !current.1.is_empty() && current_len + cost > budget {
            parts.push(std::mem::replace(
                &mut current,
                (header.is_some(), Vec::new()),
            ));
            current_len = header_cost;
            prev = header.as_ref().map(|h| h.role);
            cost = piece_cost(piece, prev);
        }
        current.1.push(i);
        current_len += cost;
        prev = Some(piece.role);
    }
    if !current.1.is_empty() {
        parts.push(current);
    }

    parts
        .into_iter()
        .enumerate()
        .map(|(index, (with_header, idx))| {
            let iter = header
                .iter()
                .filter(|_| with_header)
                .chain(idx.iter().map(|&i| &pieces[i]));
            let text = render_pieces(iter);
            let mut files: Vec<String> = Vec::new();
            for &i in &idx {
                for f in &pieces[i].files {
                    if !files.contains(f) {
                        files.push(f.clone());
                    }
                }
            }
            let first_ts = idx.first().map_or(0, |&i| pieces[i].ts);
            let hash = hash_chunk(&text, &files);
            TranscriptChunk {
                id: if index == 0 {
                    format!("{session_id}:{}", turn.seq)
                } else {
                    format!("{session_id}:{}.{index}", turn.seq)
                },
                session_id: session_id.to_string(),
                seq: turn.seq,
                ts: if first_ts != 0 { first_ts } else { turn.ts },
                kind: turn.kind,
                text,
                files,
                hash,
            }
        })
        .collect()
}

/// Record types never read; recognised from the line's first bytes and skipped
/// before decoding. Only has to be safe, not complete.
const SKIPPED_TYPES: [&str; 19] = [
    "file-history-snapshot",
    "file-history-delta",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "queue-operation",
    "mode",
    "permission-mode",
    "atis-latch",
    "bridge-session",
    "last-prompt",
    "agent-name",
    "cost-state",
    "tools-used",
    "frame-link",
    "progress",
    "worktree-state",
    "artifact-comment-monitor",
    "artifact-autoreact-ledger",
];
const TYPE_PREFIX: &[u8] = b"{\"type\":\"";

fn skipped_by_prefix(line: &[u8]) -> bool {
    let head = &line[..line.len().min(48)];
    if !head.starts_with(TYPE_PREFIX) {
        return false;
    }
    let rest = &head[TYPE_PREFIX.len()..];
    let Some(close) = rest.iter().position(|&b| b == b'"') else {
        return false;
    };
    std::str::from_utf8(&rest[..close]).is_ok_and(|t| SKIPPED_TYPES.contains(&t))
}

fn resolve_source(source: &TranscriptSource) -> (String, String, Option<String>) {
    let file = source.file.clone();
    let path = Path::new(&file);
    let id = source.id.clone().unwrap_or_else(|| {
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        base.strip_suffix(".jsonl")
            .map(str::to_string)
            .unwrap_or(base)
    });
    let mut parent = source.parent_session_id.clone();
    if parent.is_none() {
        let dir = path.parent();
        if dir
            .and_then(Path::file_name)
            .is_some_and(|n| n == "subagents")
        {
            parent = dir
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned());
        }
    }
    (file, id, parent.filter(|p| !p.is_empty()))
}

struct Parser<'o, 'a> {
    redact: &'o mut ParseOptions<'a>,
    min_prompt: usize,
    turns: Vec<Turn>,
    current: Option<usize>,
    next_seq: i64,
}

impl Parser<'_, '_> {
    fn redact(&mut self, text: &str) -> String {
        match self.redact.on_redact.as_deref_mut() {
            Some(cb) => redact_secrets_with(text, cb),
            None => redact_secrets(text),
        }
    }

    fn cur(&mut self) -> Option<&mut Turn> {
        self.current.map(|i| &mut self.turns[i])
    }

    fn open_turn(
        &mut self,
        kind: ChunkKind,
        opener: Option<String>,
        ts: i64,
        queued: bool,
    ) -> usize {
        let mut turn = Turn {
            seq: self.next_seq,
            kind,
            ts,
            opener: opener.clone(),
            opener_queued: queued,
            pieces: vec![],
            has_assistant_text: false,
        };
        self.next_seq += 1;
        if let Some(text) = opener {
            let role = if kind == ChunkKind::CompactSummary {
                Role::Summary
            } else {
                Role::User
            };
            turn.pieces.push(Piece {
                role,
                text,
                ts,
                files: vec![],
            });
        }
        self.turns.push(turn);
        self.turns.len() - 1
    }

    fn open_prompt(&mut self, raw: &str, ts: i64, queued: bool, exempt: bool) {
        let text = self.redact(js_trim(raw));
        if text.is_empty() {
            return;
        }
        let min_prompt = self.min_prompt;
        if let Some(cur) = self.cur() {
            if !queued && cur.opener_queued && cur.opener.as_deref() == Some(text.as_str()) {
                return;
            }
            if !exempt && len16(&text) < min_prompt {
                cur.pieces.push(Piece {
                    role: Role::User,
                    text,
                    ts,
                    files: vec![],
                });
                return;
            }
        }
        self.current = Some(self.open_turn(ChunkKind::Turn, Some(text), ts, queued));
    }

    fn push_assistant(&mut self, text: &str, ts: i64, is_text: bool, files: Vec<String>) {
        if self.current.is_none() {
            self.current = Some(self.open_turn(ChunkKind::Turn, None, ts, false));
        }
        let text = self.redact(text);
        let cur = self.cur().expect("current turn");
        cur.pieces.push(Piece {
            role: Role::Assistant,
            text,
            ts,
            files,
        });
        if is_text {
            cur.has_assistant_text = true;
        }
    }

    fn push_notification(&mut self, summary: &str, ts: i64) {
        if self.current.is_none() {
            return;
        }
        let marker = if summary.is_empty() {
            "[task-notification]".to_string()
        } else {
            format!(
                "[task-notification] {}",
                one_line(summary, TOOL_MARKER_CHARS * 2)
            )
        };
        let text = self.redact(&marker);
        self.cur().expect("current turn").pieces.push(Piece {
            role: Role::Assistant,
            text,
            ts,
            files: vec![],
        });
    }

    fn handle_user(&mut self, record: &Value, ts: i64) {
        let message = record.get("message");
        let texts = user_texts(message.and_then(|m| m.get("content")));
        if texts.is_empty() {
            return;
        }
        if truthy(record.get("isCompactSummary")) {
            let text = self.redact(js_trim(&texts.join("\n")));
            if !text.is_empty() {
                self.open_turn(ChunkKind::CompactSummary, Some(text), ts, false);
            }
            return;
        }
        let origin = origin_kind(record.get("origin"));
        let turn_origin = record.get("turnOrigin").and_then(Value::as_str);
        let joined = texts.join("\n");
        if kind_is(origin, "peer") || turn_origin == Some("peer") {
            let body = peer_message_body(&joined);
            if !body.is_empty() {
                self.open_prompt(&format!("[peer message] {body}"), ts, false, true);
            }
            return;
        }
        if truthy(record.get("isMeta")) {
            if turn_origin == Some("scheduled") {
                let body = strip_inline_noise(&joined);
                if !body.is_empty() {
                    self.open_prompt(&format!("[scheduled] {body}"), ts, false, true);
                }
            } else if kind_is(origin, "coordinator") {
                let body = strip_inline_noise(&joined);
                if !body.is_empty() {
                    self.open_prompt(&body, ts, false, true);
                }
            }
            return;
        }
        if kind_is(origin, "task-notification") || turn_origin == Some("task_notification") {
            let text = match classify_user_text(&joined) {
                UserTextKind::Notification(t) | UserTextKind::Prompt(t) => t,
                _ => String::new(),
            };
            self.push_notification(&text, ts);
            return;
        }
        let mut prompts: Vec<String> = Vec::new();
        for text in &texts {
            match classify_user_text(text) {
                UserTextKind::Prompt(t) => prompts.push(t),
                UserTextKind::Command(t) => self.open_prompt(&t, ts, false, true),
                UserTextKind::Notification(t) => self.push_notification(&t, ts),
                UserTextKind::Ignore => {}
            }
        }
        if !prompts.is_empty() {
            self.open_prompt(&prompts.join("\n"), ts, false, false);
        }
    }

    fn handle_assistant(&mut self, record: &Value, ts: i64) {
        let message = record.get("message");
        if truthy(record.get("isApiErrorMessage"))
            || message.and_then(|m| m.get("model")).and_then(Value::as_str) == Some("<synthetic>")
        {
            return;
        }
        let cwd = record.get("cwd").and_then(Value::as_str);
        match message.and_then(|m| m.get("content")) {
            Some(Value::String(s)) => {
                let t = js_trim(s);
                if !t.is_empty() {
                    self.push_assistant(t, ts, true, vec![]);
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match str_field(block, "type") {
                        Some("text") => {
                            let t = js_trim(str_field(block, "text").unwrap_or(""));
                            if !t.is_empty() {
                                self.push_assistant(t, ts, true, vec![]);
                            }
                        }
                        Some("thinking") => {
                            let t = js_trim(str_field(block, "thinking").unwrap_or(""));
                            if !t.is_empty() {
                                self.push_assistant(&format!("[thinking] {t}"), ts, true, vec![]);
                            }
                        }
                        Some("tool_use") => {
                            let name = str_field(block, "name")
                                .filter(|n| !n.is_empty())
                                .unwrap_or("tool");
                            let input = block.get("input");
                            let edited = edited_file(name, input, cwd);
                            self.push_assistant(
                                &tool_marker(name, input),
                                ts,
                                false,
                                edited.into_iter().collect(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_attachment(&mut self, record: &Value, ts: i64) {
        let Some(att) = record.get("attachment") else {
            return;
        };
        if str_field(att, "type") != Some("queued_command") || truthy(att.get("isMeta")) {
            return;
        }
        let text = match att.get("prompt") {
            Some(Value::String(s)) => s.clone(),
            other => user_texts(other).join("\n"),
        };
        if js_trim(&text).is_empty() {
            return;
        }
        let mode = str_field(att, "commandMode");
        if mode == Some("task-notification") {
            let t = match classify_user_text(&text) {
                UserTextKind::Notification(t) | UserTextKind::Prompt(t) => t,
                _ => String::new(),
            };
            self.push_notification(&t, ts);
            return;
        }
        if mode != Some("prompt") {
            return;
        }
        let origin = origin_kind(att.get("origin"));
        if kind_is(origin, "peer") {
            let body = peer_message_body(&text);
            if !body.is_empty() {
                self.open_prompt(&format!("[peer message] {body}"), ts, true, true);
            }
            return;
        }
        if origin.is_some() && !kind_is(origin, "human") {
            return;
        }
        match classify_user_text(&text) {
            UserTextKind::Prompt(t) => self.open_prompt(&t, ts, true, false),
            UserTextKind::Command(t) => self.open_prompt(&t, ts, true, true),
            _ => {}
        }
    }
}

/// Parse transcript content (already read) into session meta and chunks.
/// Lines that are not valid JSON (a truncated tail of a live file) are skipped.
pub fn parse_transcript_content(
    content: &[u8],
    source: &TranscriptSource,
    mut opts: ParseOptions<'_>,
) -> ParsedTranscript {
    let (file, id, parent_session_id) = resolve_source(source);
    let budget = opts.max_chunk_chars.unwrap_or(MAX_CHUNK_CHARS).max(2000);
    let min_prompt = opts.min_prompt_chars.unwrap_or(MIN_PROMPT_CHARS);

    let mut cwds: Vec<String> = Vec::new();
    let mut cwd_set: HashSet<String> = HashSet::new();
    let mut add_cwd = |cwd: Option<&Value>| {
        if let Some(c) = cwd.and_then(Value::as_str)
            && !c.is_empty()
            && cwd_set.insert(c.to_string())
        {
            cwds.push(c.to_string());
        }
    };
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut seen: HashSet<String> = HashSet::new();

    let mut p = Parser {
        redact: &mut opts,
        min_prompt,
        turns: Vec::new(),
        current: None,
        next_seq: 0,
    };

    for line in content.split(|&b| b == b'\n') {
        if line.is_empty() || skipped_by_prefix(line) {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !record.is_object() {
            continue;
        }
        let rtype = str_field(&record, "type");
        if matches!(rtype, Some("user" | "assistant"))
            && let Some(uuid) = str_field(&record, "uuid").filter(|u| !u.is_empty())
            && !seen.insert(uuid.to_string())
        {
            continue;
        }
        let ts = parse_ts(record.get("timestamp"));
        if ts != 0 {
            if first_ts == 0 || ts < first_ts {
                first_ts = ts;
            }
            if ts > last_ts {
                last_ts = ts;
            }
        }
        add_cwd(record.get("cwd"));
        if let Some(b) = str_field(&record, "gitBranch").filter(|b| !b.is_empty()) {
            git_branch = Some(b.to_string());
        }
        match rtype {
            Some("user") => p.handle_user(&record, ts),
            Some("assistant") => p.handle_assistant(&record, ts),
            Some("attachment") => p.handle_attachment(&record, ts),
            Some("ai-title") => {
                if let Some(t) = str_field(&record, "aiTitle")
                    .map(js_trim)
                    .filter(|t| !t.is_empty())
                {
                    ai_title = Some(t.to_string());
                }
            }
            Some("custom-title") => {
                if let Some(t) = str_field(&record, "customTitle")
                    .map(js_trim)
                    .filter(|t| !t.is_empty())
                {
                    custom_title = Some(t.to_string());
                }
            }
            Some("relocated") => add_cwd(record.get("relocatedCwd")),
            _ => {}
        }
    }

    let turns = std::mem::take(&mut p.turns);
    let mut chunks = Vec::new();
    for turn in turns {
        if turn.kind == ChunkKind::Turn && !turn.has_assistant_text {
            continue;
        }
        chunks.extend(chunks_for_turn(turn, &id, budget));
    }
    let title = custom_title.or(ai_title).map(|t| redact_secrets(&t));
    ParsedTranscript {
        meta: SessionMeta {
            id,
            file,
            parent_session_id,
            cwds,
            first_ts,
            last_ts,
            title,
            git_branch,
        },
        chunks,
    }
}

/// Read and parse one transcript. A subagent transcript without a title takes
/// it from the sibling `agent-<id>.meta.json` (the task description).
pub fn parse_transcript(source: &TranscriptSource) -> std::io::Result<ParsedTranscript> {
    let (file, id, parent) = resolve_source(source);
    let resolved = TranscriptSource {
        file: file.clone(),
        id: Some(id),
        parent_session_id: parent.clone(),
    };
    let bytes = std::fs::read(&file)?;
    let mut parsed = parse_transcript_content(&bytes, &resolved, ParseOptions::default());
    if parsed.meta.title.is_none() && parent.is_some() {
        let meta_file = match file.strip_suffix(".jsonl") {
            Some(stem) => format!("{stem}.meta.json"),
            None => file.clone(),
        };
        if let Ok(raw) = std::fs::read_to_string(&meta_file)
            && let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(d) = v
                .get("description")
                .and_then(Value::as_str)
                .map(js_trim)
                .filter(|d| !d.is_empty())
        {
            parsed.meta.title = Some(redact_secrets(d));
        }
    }
    Ok(parsed)
}

// ─── Redaction ───────────────────────────────────────────────────────────────

const REDACTED: &str = "[REDACTED]";

const PLACEHOLDER_WORDS: [&str; 30] = [
    "null",
    "none",
    "nil",
    "undefined",
    "true",
    "false",
    "string",
    "str",
    "number",
    "int",
    "integer",
    "boolean",
    "bool",
    "any",
    "object",
    "unknown",
    "required",
    "optional",
    "password",
    "passwd",
    "secret",
    "token",
    "redacted",
    "hidden",
    "masked",
    "example",
    "placeholder",
    "changeme",
    "empty",
    "default",
];

fn is_placeholder(value: &str) -> bool {
    let v = js_trim(value);
    if v.is_empty() {
        return true;
    }
    if v.starts_with(['$', '%', '<', '@', '#', '{', '[']) {
        return true;
    }
    if v.contains(REDACTED) {
        return true;
    }
    if [
        "process.env",
        "os.environ",
        "getenv",
        "import.meta.env",
        "secrets.",
    ]
    .iter()
    .any(|s| v.contains(s))
    {
        return true;
    }
    if v.chars()
        .all(|c| matches!(c, '*' | 'x' | 'X' | '•' | '.' | '…'))
    {
        return true;
    }
    let lower = v.to_ascii_lowercase();
    if ["your", "my", "some", "the"]
        .iter()
        .any(|w| lower.starts_with(w) && matches!(lower.as_bytes().get(w.len()), Some(b'_' | b'-')))
    {
        return true;
    }
    let lower = v.to_lowercase();
    PLACEHOLDER_WORDS.contains(&lower.as_str())
}

const STRONG_KEYWORDS: [&str; 8] = [
    "PASSWORD",
    "[Pp]assword",
    "PASSWD",
    "[Pp]asswd",
    "PASSPHRASE",
    "[Pp]assphrase",
    "SSHPASS",
    "[Ss]shpass",
];
const TOKEN_KEYWORDS: [&str; 15] = [
    "CLIENT[_-]?SECRET",
    "[Cc]lient[_-]?[Ss]ecret",
    "SECRET",
    "[Ss]ecret",
    "TOKEN",
    "[Tt]oken",
    "API[_-]?KEY",
    "[Aa]pi[_-]?[Kk]ey",
    "ACCESS[_-]?KEY",
    "[Aa]ccess[_-]?[Kk]ey",
    "AUTH[_-]?KEY",
    "[Aa]uth[_-]?[Kk]ey",
    "PRIVATE[_-]?KEY",
    "[Pp]rivate[_-]?[Kk]ey",
    // Bare `PASS` only in env form (`DB_PASS=…`, enforced below).
    "PASS",
];

static NON_SECRET_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:types?|counts?|limits?|url|uri|files?|paths?|dir|names?|ids?|endpoint|expiry|expires|expiration|ttl|len|length|size|budget|usage|kind|header|field|prefix|env|var|variable|mode|policy|hint|label|placeholder|regex|pattern|min|max|required|reset|rules?|strength|valid|validator|input|prompt|manager|store|provider|ref|arn|version|format|style|class|icon|button|dialog|form)$").unwrap()
});

fn fancy(pattern: &str) -> fancy_regex::Regex {
    fancy_regex::RegexBuilder::new(pattern)
        .backtrack_limit(10_000_000)
        .build()
        .expect("valid redaction regex")
}

static ASSIGNMENT_RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    let keyword: Vec<&str> = STRONG_KEYWORDS
        .iter()
        .chain(TOKEN_KEYWORDS.iter())
        .copied()
        .collect();
    let keyword = format!("(?:{})", keyword.join("|"));
    fancy(&format!(
        "(?:(?<![A-Za-z0-9])|(?<=[a-z0-9])(?=[A-Z]))({keyword}(?:[_.-][A-Za-z0-9]+|[A-Z][a-z0-9]+)*)\
         ([\"']?[ \\t]*(?:=>|:=|=|:)[ \\t]*)\
         (?:\"([^\"\\n]+)\"|'([^'\\n]+)'|`([^`\\n]+)`|([^{JS_WS}\"'`,;)}}\\]&<>]+))"
    ))
});

static FLAG_RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy(&format!(
        "(?i)(?<![A-Za-z0-9_-])(--(?:[a-z0-9]+-)*(?:password|passwd|passphrase|pass|secret|client-secret|token|api-key|apikey|access-key|auth-token|private-key))\
         ([ \\t]+)(?:'([^'\\n]+)'|\"([^\"\\n]+)\"|([^{JS_WS}'\"-][^{JS_WS}'\"]*))"
    ))
});

fn is_strong_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["password", "passwd", "passphrase", "sshpass"]
        .iter()
        .any(|w| k.contains(w))
}

/// `key.split(/[_.-]|(?<=[a-z0-9])(?=[A-Z][a-z0-9])/)`, last part when there are several.
fn last_key_segment(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '_' | '.' | '-') {
            parts.push(std::mem::take(&mut cur));
            continue;
        }
        let hump = i > 0
            && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit())
            && c.is_ascii_uppercase()
            && chars
                .get(i + 1)
                .is_some_and(|n| n.is_ascii_lowercase() || n.is_ascii_digit());
        if hump {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    parts.push(cur);
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    if parts.len() > 1 {
        parts.last().cloned().unwrap_or_default()
    } else {
        String::new()
    }
}

fn is_secret_value(key: &str, value: &str, quoted: bool, env_style: bool) -> bool {
    let v = js_trim(value);
    if is_placeholder(v) {
        return false;
    }
    let suffix = last_key_segment(key);
    if !suffix.is_empty() && NON_SECRET_SUFFIX.is_match(&suffix) {
        return false;
    }
    let strong = is_strong_key(key);
    if len16(v) < if strong { 4 } else { 8 } {
        return false;
    }
    if !strong && v.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if cached_regex(r"^(?i:[a-z][a-z0-9+.-]*:)?//").is_match(v)
        || cached_regex(r"^~?\.{0,2}/").is_match(v)
    {
        return false;
    }
    if !quoted {
        if v.starts_with(['=', '>', ':']) {
            return false;
        }
        if cached_regex(r"^[A-Za-z_$][A-Za-z0-9_$.]*\(").is_match(v) {
            return false;
        }
        if v.ends_with('-') {
            return false;
        }
        if !env_style
            && cached_regex(r"^[A-Za-z_][A-Za-z_]*(?:\.[A-Za-z_][A-Za-z_]*)*$").is_match(v)
        {
            return false;
        }
    } else {
        if v.split(is_js_ws).filter(|s| !s.is_empty()).count() >= 3 {
            return false;
        }
        if cached_regex(r"^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$").is_match(v) {
            return false;
        }
    }
    true
}

/// Groups of one match: index 0 is the whole match.
type Groups<'t> = Vec<Option<&'t str>>;

enum Engine {
    Plain(Regex),
    /// A look-around rule. JS caps every value group at 4096 UTF-16 units
    /// (`{1,4096}`); a repeat that large overflows the regex NFA, so the
    /// patterns use `+` and the cap is applied here: an over-long quoted group
    /// means "no match at this position" (the search goes on one char later,
    /// as the JS engine would), an over-long bare group is cut to the cap
    /// (it is the pattern's last element, so JS simply stops there).
    Fancy {
        re: &'static fancy_regex::Regex,
        quoted: &'static [usize],
        bare: usize,
    },
    Pem,
}

const VALUE_CAP: usize = 4096;

/// Byte length of the longest prefix of `s` within `cap` UTF-16 units.
fn prefix_within(s: &str, cap: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > cap {
            return i;
        }
    }
    s.len()
}

/// Builds the replacement from a match's groups (reporting the secret), or None to leave it.
type Replacer = fn(&Groups<'_>, &mut dyn FnMut(&str)) -> Option<String>;

struct Rule {
    name: &'static str,
    engine: Engine,
    /// Return the replacement, or None to leave the match alone.
    replace: Replacer,
    /// Cheap literal prefilter: the rule can only match when one of these occurs
    /// (case-insensitively). Empty = always run.
    needles: &'static [&'static str],
}

fn whole(g: &Groups<'_>, report: &mut dyn FnMut(&str)) -> Option<String> {
    report(g[0].unwrap_or(""));
    Some(REDACTED.to_string())
}

fn prefix_secret(g: &Groups<'_>, report: &mut dyn FnMut(&str)) -> Option<String> {
    let (prefix, secret) = (g[1].unwrap_or(""), g[2]?);
    if secret.is_empty() {
        return None;
    }
    report(secret);
    Some(format!("{prefix}{REDACTED}"))
}

fn strip_quotes(s: &str) -> &str {
    let s = s.strip_prefix(['\'', '"']).unwrap_or(s);
    s.strip_suffix(['\'', '"']).unwrap_or(s)
}

fn ascii(pattern: String) -> Engine {
    Engine::Plain(Regex::new(&pattern).expect("valid redaction regex"))
}

/// JavaScript (non-unicode) `\b`.
const B: &str = r"(?-u:\b)";

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        Rule {
            name: "pem-private-key",
            engine: Engine::Pem,
            replace: whole,
            needles: &["PRIVATE KEY"],
        },
        Rule {
            name: "pem-private-key",
            engine: ascii(format!(
                r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----(?:(?:[{JS_WS}]|\\n)*[A-Za-z0-9+/=]{{8,}})*"
            )),
            replace: whole,
            needles: &["PRIVATE KEY"],
        },
        Rule {
            name: "anthropic-key",
            engine: ascii(format!(r"{B}sk-ant-[A-Za-z0-9_-]{{20,}}")),
            replace: whole,
            needles: &["sk-ant-"],
        },
        Rule {
            name: "openai-key",
            engine: ascii(format!(
                r"{B}sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{{20,}}"
            )),
            replace: whole,
            needles: &["sk-"],
        },
        Rule {
            // `\bsk-(?=[A-Za-z0-9]*\d)[A-Za-z0-9]{20,}\b`: the look-ahead is the
            // digit check below (the run it scans is exactly group 1).
            name: "openai-key",
            engine: ascii(format!(r"{B}sk-([A-Za-z0-9]{{20,}}){B}")),
            replace: |g, report| {
                if !g[1]?.bytes().any(|b| b.is_ascii_digit()) {
                    return None;
                }
                whole(g, report)
            },
            needles: &["sk-"],
        },
        Rule {
            name: "stripe-key",
            engine: ascii(format!(r"{B}[rs]k_(?:live|test)_[A-Za-z0-9]{{16,}}{B}")),
            replace: whole,
            needles: &["k_live_", "k_test_"],
        },
        Rule {
            name: "github-token",
            engine: ascii(format!(r"{B}gh[pousr]_[A-Za-z0-9]{{30,}}{B}")),
            replace: whole,
            needles: &["gh"],
        },
        Rule {
            name: "github-token",
            engine: ascii(format!(r"{B}github_pat_[A-Za-z0-9_]{{30,}}{B}")),
            replace: whole,
            needles: &["github_pat_"],
        },
        Rule {
            name: "gitlab-token",
            engine: ascii(format!(r"{B}glpat-[A-Za-z0-9_-]{{20,}}{B}")),
            replace: whole,
            needles: &["glpat-"],
        },
        Rule {
            name: "huggingface-token",
            engine: ascii(format!(r"{B}hf_[A-Za-z0-9]{{30,}}{B}")),
            replace: whole,
            needles: &["hf_"],
        },
        Rule {
            name: "google-api-key",
            engine: ascii(format!(r"{B}AIza[0-9A-Za-z_-]{{35}}{B}")),
            replace: whole,
            needles: &["AIza"],
        },
        Rule {
            name: "aws-access-key",
            engine: ascii(format!(r"{B}(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{{16}}{B}")),
            replace: whole,
            needles: &[],
        },
        Rule {
            name: "slack-token",
            engine: ascii(format!(r"{B}xox[abposr]-[A-Za-z0-9-]{{10,}}")),
            replace: whole,
            needles: &["xox"],
        },
        Rule {
            name: "jwt",
            engine: ascii(format!(
                r"{B}eyJ[A-Za-z0-9_-]{{8,}}\.[A-Za-z0-9_-]{{8,}}\.[A-Za-z0-9_-]{{8,}}"
            )),
            replace: whole,
            needles: &["eyJ"],
        },
        Rule {
            name: "auth-header",
            engine: ascii(format!(
                r#"(?i){B}(authorization["']?[ \t]*[:=][ \t]*["']?(?:bearer|basic|token|bot|digest)[ \t]+)([A-Za-z0-9._~+/-]{{8,}}=*)"#
            )),
            replace: |g, report| {
                let secret = g[2]?;
                if secret.is_empty() || is_placeholder(secret) {
                    return None;
                }
                report(secret);
                Some(format!("{}{REDACTED}", g[1].unwrap_or("")))
            },
            needles: &["authorization"],
        },
        Rule {
            // `\b(Bearer[ \t]+)((?=[A-Za-z0-9._~+/-]*\d)[A-Za-z0-9._~+/-]{20,}=*)`:
            // the look-ahead is the digit check on the token run.
            name: "bearer-token",
            engine: ascii(format!(r"{B}(Bearer[ \t]+)([A-Za-z0-9._~+/-]{{20,}}=*)")),
            replace: |g, report| {
                if !g[2]?.bytes().any(|b| b.is_ascii_digit()) {
                    return None;
                }
                prefix_secret(g, report)
            },
            needles: &["Bearer"],
        },
        Rule {
            name: "url-credentials",
            engine: ascii(format!(
                r#"(?i){B}([a-z][a-z0-9+.-]{{1,20}}://)([^{JS_WS}:@/?#'"]{{1,128}}):([^{JS_WS}@/?#'"]{{1,256}})@"#
            )),
            replace: |g, report| {
                let pass = g[3]?;
                if pass.is_empty()
                    || is_placeholder(pass)
                    || (pass.len() <= 5 && pass.bytes().all(|b| b.is_ascii_digit()))
                {
                    return None;
                }
                report(pass);
                Some(format!(
                    "{}{}:{REDACTED}@",
                    g[1].unwrap_or(""),
                    g[2].unwrap_or("")
                ))
            },
            needles: &["://"],
        },
        Rule {
            name: "sshpass",
            engine: ascii(format!(
                r#"{B}(sshpass[ \t]+-p[ \t]*)('[^'\n]*'|"[^"\n]*"|[^{JS_WS}'"]+)"#
            )),
            replace: |g, report| {
                let secret = g[2]?;
                if secret.is_empty() || is_placeholder(strip_quotes(secret)) {
                    return None;
                }
                report(secret);
                Some(format!("{}{REDACTED}", g[1].unwrap_or("")))
            },
            needles: &["sshpass"],
        },
        Rule {
            name: "mysql-password",
            engine: ascii(format!(
                r#"{B}((?:mysql|mysqldump|mysqladmin|mariadb|mariadb-dump){B}[^\n|;&]*?[ \t]-p)('[^'\n]+'|"[^"\n]+"|[^{JS_WS}'"]+)"#
            )),
            replace: |g, report| {
                let secret = g[2]?;
                if secret.is_empty() || is_placeholder(strip_quotes(secret)) {
                    return None;
                }
                report(secret);
                Some(format!("{}{REDACTED}", g[1].unwrap_or("")))
            },
            needles: &["mysql", "mariadb"],
        },
        Rule {
            name: "assignment",
            engine: Engine::Fancy {
                re: &ASSIGNMENT_RE,
                quoted: &[3, 4, 5],
                bare: 6,
            },
            replace: |g, report| {
                let (key, sep) = (g[1]?, g[2]?);
                let quoted_value = g[3].or(g[4]).or(g[5]);
                let value = quoted_value.or(g[6])?;
                let env_style = key
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    && sep == "=";
                if key == "PASS" && !env_style {
                    return None;
                }
                if !is_secret_value(key, value, quoted_value.is_some(), env_style) {
                    return None;
                }
                report(value);
                let quote = if g[3].is_some() {
                    "\""
                } else if g[4].is_some() {
                    "'"
                } else if g[5].is_some() {
                    "`"
                } else {
                    ""
                };
                Some(format!("{key}{sep}{quote}{REDACTED}{quote}"))
            },
            needles: &["pass", "secret", "token", "key"],
        },
        Rule {
            name: "cli-flag",
            engine: Engine::Fancy {
                re: &FLAG_RE,
                quoted: &[3, 4],
                bare: 5,
            },
            replace: |g, report| {
                let (flag, space) = (g[1]?, g[2].unwrap_or(""));
                let quoted_value = g[3].or(g[4]);
                let value = quoted_value.or(g[5])?;
                if !is_secret_value(
                    flag.trim_start_matches('-'),
                    value,
                    quoted_value.is_some(),
                    false,
                ) {
                    return None;
                }
                report(value);
                let quote = if g[3].is_some() {
                    "'"
                } else if g[4].is_some() {
                    "\""
                } else {
                    ""
                };
                Some(format!("{flag}{space}{quote}{REDACTED}{quote}"))
            },
            needles: &["--"],
        },
    ]
});

/// The first PEM block (rule 1) at or after `from`: `-----BEGIN <words>PRIVATE
/// KEY[ BLOCK]-----`, then the nearest matching END within 20000 UTF-16 units.
fn find_pem(text: &str, from: usize) -> Option<(usize, usize)> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"-----BEGIN ((?:[A-Z0-9]+ )*)PRIVATE KEY((?: BLOCK)?)-----").unwrap()
    });
    let mut at = from;
    while let Some(c) = HEAD.captures_at(text, at) {
        let m = c.get(0).unwrap();
        let end_marker = format!("-----END {}PRIVATE KEY{}-----", &c[1], &c[2]);
        // The END must start within 20000 UTF-16 units of the header's end.
        let body = &text[m.end()..];
        let mut limit = body.len();
        let mut units = 0;
        for (i, ch) in body.char_indices() {
            if units > 20_000 {
                limit = i;
                break;
            }
            units += ch.len_utf16();
        }
        if units <= 20_000 {
            limit = body.len();
        }
        if let Some(pos) = body.find(&end_marker).filter(|&p| p <= limit) {
            return Some((m.start(), m.end() + pos + end_marker.len()));
        }
        // No match starting here; the regex would retry from the next position.
        at = m.start() + 1;
    }
    None
}

fn apply_rule(rule: &Rule, text: &str, on_hit: &mut dyn FnMut(&str, &str)) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut report = |secret: &str| on_hit(rule.name, secret);
    let mut emit = |start: usize, end: usize, groups: Groups<'_>, out: &mut String| {
        out.push_str(&text[last..start]);
        match (rule.replace)(&groups, &mut report) {
            Some(rep) => out.push_str(&rep),
            None => out.push_str(&text[start..end]),
        }
        last = end;
    };
    match &rule.engine {
        Engine::Plain(re) => {
            for c in re.captures_iter(text) {
                let m = c.get(0).unwrap();
                let groups: Groups<'_> =
                    (0..c.len()).map(|i| c.get(i).map(|g| g.as_str())).collect();
                emit(m.start(), m.end(), groups, &mut out);
            }
        }
        Engine::Fancy { re, quoted, bare } => {
            let mut pos = 0;
            while pos <= text.len() {
                let Ok(Some(c)) = re.captures_from_pos(text, pos) else {
                    break;
                };
                let m = c.get(0).unwrap();
                if quoted.iter().any(|&i| {
                    c.get(i)
                        .is_some_and(|g| crate::util::len16(g.as_str()) > VALUE_CAP)
                }) {
                    pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                let mut end = m.end();
                let mut groups: Groups<'_> =
                    (0..c.len()).map(|i| c.get(i).map(|g| g.as_str())).collect();
                if let Some(g) = c.get(*bare) {
                    let keep = prefix_within(g.as_str(), VALUE_CAP);
                    if keep < g.as_str().len() {
                        end = g.start() + keep;
                        groups[*bare] = Some(&text[g.start()..end]);
                        groups[0] = Some(&text[m.start()..end]);
                    }
                }
                emit(m.start(), end, groups, &mut out);
                pos = if end > m.start() { end } else { end + 1 };
            }
        }
        Engine::Pem => {
            let mut at = 0;
            while let Some((s, e)) = find_pem(text, at) {
                emit(s, e, vec![Some(&text[s..e])], &mut out);
                at = e;
            }
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Replace credentials in `text` with `[REDACTED]`. Leans towards leaving
/// ordinary code and prose alone.
pub fn redact_secrets(text: &str) -> String {
    redact_secrets_with(text, &mut |_, _| {})
}

/// [`redact_secrets`] reporting each hit (rule name, original match) for diagnostics.
pub fn redact_secrets_with(text: &str, on_hit: &mut dyn FnMut(&str, &str)) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();
    let mut lower: Option<String> = None;
    for rule in RULES.iter() {
        if !rule.needles.is_empty() {
            let l = lower.get_or_insert_with(|| out.to_ascii_lowercase());
            if !rule
                .needles
                .iter()
                .any(|n| l.contains(&n.to_ascii_lowercase()))
            {
                continue;
            }
        }
        let next = apply_rule(rule, &out, on_hit);
        if next != out {
            out = next;
            lower = None;
        }
    }
    out
}

// ─── Workspace matching ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Darwin,
    Linux,
    Win32,
}

pub struct BelongsToOptions<'a> {
    pub platform: Platform,
    /// A failure means "use the path as given".
    pub realpath: &'a dyn Fn(&str) -> Option<String>,
}

fn default_realpath(p: &str) -> Option<String> {
    std::fs::canonicalize(p).ok().map(|p| path_str(&p))
}

/// Node's `path.posix.normalize`.
pub(crate) fn posix_normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let absolute = p.starts_with('/');
    let trailing = p.ends_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|s| *s != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            s => stack.push(s),
        }
    }
    let mut out = stack.join("/");
    if out.is_empty() {
        return if absolute {
            "/".into()
        } else if trailing {
            "./".into()
        } else {
            ".".into()
        };
    }
    if trailing {
        out.push('/');
    }
    if absolute { format!("/{out}") } else { out }
}

/// A simplified `path.win32.normalize` (drive / UNC / rooted / relative).
fn win_normalize(p: &str) -> (String, String) {
    let p = p.replace('/', "\\");
    let (root, rest) =
        if p.len() >= 2 && p.as_bytes()[0].is_ascii_alphabetic() && p.as_bytes()[1] == b':' {
            if p[2..].starts_with('\\') {
                (p[..3].to_string(), &p[3..])
            } else {
                (p[..2].to_string(), &p[2..])
            }
        } else if p.starts_with('\\') {
            ("\\".to_string(), p.trim_start_matches('\\'))
        } else {
            (String::new(), p.as_str())
        };
    let trailing = rest.ends_with('\\');
    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|s| *s != "..") {
                    stack.pop();
                } else if root.is_empty() {
                    stack.push("..");
                }
            }
            s => stack.push(s),
        }
    }
    let mut body = stack.join("\\");
    if body.is_empty() && root.is_empty() {
        body = ".".into();
    }
    if trailing && !body.is_empty() {
        body.push('\\');
    }
    (format!("{root}{body}"), root)
}

/// True when any `cwd` the session ran in is `folder` or lies inside it: on a
/// path boundary, case-insensitive on macOS/Windows, comparing both the paths
/// as recorded and their realpaths.
pub fn transcript_belongs_to(
    cwds: &[String],
    folder: Option<&str>,
    opts: Option<&BelongsToOptions<'_>>,
) -> bool {
    let Some(folder) = folder.map(js_trim).filter(|f| !f.is_empty()) else {
        return false;
    };
    let default = BelongsToOptions {
        platform: Platform::Darwin,
        realpath: &default_realpath,
    };
    let opts = opts.unwrap_or(&default);
    let win = opts.platform == Platform::Win32;
    let sep = if win { '\\' } else { '/' };
    let fold = win || opts.platform == Platform::Darwin;

    let normalize = |raw: &str| -> String {
        use unicode_normalization::UnicodeNormalization;
        let (mut n, root) = if win {
            win_normalize(raw)
        } else {
            let n = posix_normalize(raw);
            let root = if n.starts_with('/') {
                "/".to_string()
            } else {
                String::new()
            };
            (n, root)
        };
        while n.len() > root.len() && n.ends_with(sep) {
            n.pop();
        }
        let n: String = n.nfc().collect();
        if fold { n.to_lowercase() } else { n }
    };
    let variants = |raw: &str| -> Vec<String> {
        let mut out = vec![normalize(raw)];
        if let Some(real) = (opts.realpath)(raw) {
            let real = normalize(&real);
            if real != out[0] {
                out.push(real);
            }
        }
        out
    };
    let inside = |child: &str, parent: &str| {
        child == parent
            || if parent.ends_with(sep) {
                child.starts_with(parent)
            } else {
                child.starts_with(&format!("{parent}{sep}"))
            }
    };
    let folders = variants(folder);
    cwds.iter().filter(|c| !c.is_empty()).any(|cwd| {
        variants(cwd)
            .iter()
            .any(|cand| folders.iter().any(|f| inside(cand, f)))
    })
}

#[cfg(test)]
#[path = "transcripts_tests.rs"]
mod tests;
