//! Small helpers shared by the modules: JavaScript string semantics (the
//! Electron app measures and slices text in UTF-16 code units, and the chunk
//! text/hashes written to the shared DBs must come out byte-identical), a
//! cancellation token, atomic file writes and subprocess plumbing.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

// ─── JavaScript string semantics ─────────────────────────────────────────────

/// Characters JavaScript's `\s` and `String.prototype.trim` treat as white space.
pub(crate) fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// The body of a regex character class equal to JavaScript's `\s`.
pub(crate) const JS_WS: &str = "\t\n\u{0B}\u{0C}\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

/// `s.length` in JavaScript: UTF-16 code units.
pub(crate) fn len16(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)` in JavaScript (n in UTF-16 units). A cut through a surrogate
/// pair leaves a lone surrogate in JS, which Node encodes as U+FFFD when the
/// string is hashed or stored; the same replacement is produced here.
pub(crate) fn slice16(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if units + w > n {
            if units < n {
                out.push('\u{FFFD}');
            }
            break;
        }
        units += w;
        out.push(c);
    }
    out
}

/// `text.replace(/\s+/g, ' ')` (JS white space).
pub(crate) fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if is_js_ws(c) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// `oneLine`: whitespace collapsed, trimmed, cut to `max` UTF-16 units plus `…`.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let flat = collapse_ws(text);
    let flat = js_trim(&flat);
    if len16(flat) > max {
        format!("{}…", slice16(flat, max))
    } else {
        flat.to_string()
    }
}

/// `toLowerCase()` that keeps one char per char (so offsets in the lowered text
/// line up with the original); differs from JS only for the handful of chars
/// whose lowercase form is longer.
pub(crate) fn lower_chars(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

pub(crate) fn sha1_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    hex::encode(h.finalize())
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn truthy(v: Option<&serde_json::Value>) -> bool {
    use serde_json::Value;
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `Date.parse` for the ISO-8601 timestamps Claude Code writes (ms since epoch, 0 when unknown).
pub(crate) fn parse_ts(v: Option<&serde_json::Value>) -> i64 {
    let Some(s) = v.and_then(|v| v.as_str()) else {
        return 0;
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_millis();
    }
    // `2026-01-01T00:00:00` without an offset is local time in JS; Claude
    // Code always writes `Z`, so the remaining forms are best-effort UTC.
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return dt.and_utc().timestamp_millis();
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp_millis())
            .unwrap_or(0);
    }
    0
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Node's `stat.mtimeMs`: `sec * 1e3 + nsec / 1e6`, as a double. files.json is
/// shared with the Electron app, so the value must be computed the same way.
pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as f64 * 1000.0 + meta.mtime_nsec() as f64 / 1e6
}

/// JavaScript `Math.round` (halves round up).
pub(crate) fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

// ─── Cancellation ────────────────────────────────────────────────────────────

/// AbortSignal stand-in: cheap to clone, checked between units of work.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The error a cancelled install / download returns.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}
impl std::error::Error for Cancelled {}

pub fn is_cancelled_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

// ─── Files ───────────────────────────────────────────────────────────────────

/// writeFileAtomicSync: a sibling `<file>.<pid>.tmp`, renamed over the target.
pub(crate) fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(m) = mode {
            opts.mode(m);
        }
        let mut f = opts.open(&tmp)?;
        std::io::Write::write_all(&mut f, data)?;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, data)?;
    }
    Ok(())
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ─── Processes ───────────────────────────────────────────────────────────────

/// `os.setPriority(pid, PRIORITY_BELOW_NORMAL)`: background work never competes
/// with the user's foreground apps. Best effort.
pub(crate) fn lower_priority(pid: u32) {
    // SAFETY: plain syscall on a pid we spawned.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 10);
    }
}

pub(crate) fn signal(pid: u32, sig: i32) {
    // SAFETY: plain syscall on a pid we spawned.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Read a pipe on a helper thread, keeping at most `cap` bytes (the tail when
/// `keep_tail`, else the head).
pub(crate) fn drain<R: Read + Send + 'static>(
    mut r: R,
    cap: usize,
    keep_tail: bool,
) -> std::thread::JoinHandle<(Vec<u8>, usize)> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut total = 0usize;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    if keep_tail {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > cap {
                            buf.drain(..buf.len() - cap);
                        }
                    } else if buf.len() < cap {
                        let take = n.min(cap - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        (buf, total)
    })
}

pub(crate) struct CmdOutput {
    pub ok: bool,
    pub stdout: String,
}

/// Run a short command to completion with a timeout (killed when it overruns).
pub(crate) fn run_command(mut cmd: Command, timeout: Duration) -> CmdOutput {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            return CmdOutput {
                ok: false,
                stdout: String::new(),
            };
        }
    };
    let out = drain(
        child.stdout.take().expect("piped"),
        128 * 1024 * 1024,
        false,
    );
    let err = drain(child.stderr.take().expect("piped"), 64 * 1024, true);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let stdout = String::from_utf8_lossy(&out.join().map(|r| r.0).unwrap_or_default()).into_owned();
    let _ = err.join();
    CmdOutput {
        ok: status.is_some_and(|s| s.success()),
        stdout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_slicing_matches_js() {
        assert_eq!(len16("ab😀cd"), 6);
        assert_eq!(slice16("ab😀cd", 2), "ab");
        assert_eq!(slice16("ab😀cd", 3), "ab\u{FFFD}");
        assert_eq!(slice16("ab😀cd", 4), "ab😀");
        assert_eq!(one_line("  a \n\t b  ", 100), "a b");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(js_trim("\u{FEFF} x \u{3000}"), "x");
    }

    #[test]
    fn parses_iso_timestamps() {
        let v = serde_json::json!("2026-01-01T00:00:01.000Z");
        assert_eq!(parse_ts(Some(&v)), 1_767_225_601_000);
        assert_eq!(parse_ts(Some(&serde_json::json!(5))), 0);
        assert_eq!(parse_ts(None), 0);
        assert_eq!(js_round(123.5), 124);
        assert_eq!(js_round(123.4), 123);
    }
}
