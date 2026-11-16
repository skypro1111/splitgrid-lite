//! Local embedding runtime (port of src/main/history/runtime.ts).
//!
//! Embeddings come from llama.cpp's `llama-server` running EmbeddingGemma 2
//! (Q8_0 GGUF) on loopback. Nothing ships with the app: the binary and the
//! model are fetched only by `install()`, after consent. Constructing an
//! [`EmbeddingRuntime`] touches neither the network nor the disk and spawns
//! nothing.
//!
//! SHARED WITH THE ELECTRON APP: same pins (URL + sha256 + size), same layout
//! (`bin/`, `models/`, `installed.json`, `tmp/`), so either app can use the
//! runtime the other installed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Once, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::HISTORY_EMBED_DIM;
use crate::util::{CancelToken, Cancelled, lower_priority, path_str, signal, write_atomic};

// ─── Pinned manifest ─────────────────────────────────────────────────────────
// llama.cpp b11960 (commit 8642541b2), validated by the embeddinggemma-2 spike.

pub const LLAMA_CPP_TAG: &str = "b11960";

#[derive(Debug)]
pub struct LlamaAsset {
    /// `darwin-arm64` | `darwin-x64` | `linux-x64` | `win32-x64` (installed.json's `platform`).
    pub key: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    pub archive: Archive,
    /// Archive-relative path of the server binary; its directory is what gets kept.
    pub server_path: &'static str,
    /// Shared libs that come along with the server (regex on the file name).
    pub lib_pattern: &'static str,
    /// Libs whose absence means a broken extract.
    pub required_libs: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archive {
    TarGz,
    Zip,
}

pub static LLAMA_ASSETS: [LlamaAsset; 4] = [
    LlamaAsset {
        key: "darwin-arm64",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-macos-arm64.tar.gz",
        sha256: "58e5a5e1d7c2bf66cedd9c29e0323f3e570660a70a1e2156d32eed08f4cc1e3c",
        size: 16_465_074,
        archive: Archive::TarGz,
        server_path: "llama-b11960/llama-server",
        lib_pattern: r"\.dylib$",
        required_libs: &[
            "libllama-server-impl.dylib",
            "libllama.0.dylib",
            "libggml-base.0.dylib",
            "libggml-metal.0.dylib",
        ],
    },
    LlamaAsset {
        key: "darwin-x64",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-macos-x64.tar.gz",
        sha256: "d1e6c9ddcaed9a3b1bb8f3d946bdb3481c7b919357a4029dcb747762397f1da5",
        size: 12_205_764,
        archive: Archive::TarGz,
        server_path: "llama-b11960/llama-server",
        lib_pattern: r"\.dylib$",
        required_libs: &[
            "libllama-server-impl.dylib",
            "libllama.0.dylib",
            "libggml-base.0.dylib",
            "libggml-cpu.0.dylib",
        ],
    },
    LlamaAsset {
        key: "linux-x64",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-x64.tar.gz",
        sha256: "31cc3a6bc638ac6c48dc4b1b74888f1eaadf3d14227c1e5f67daf9a49541d97a",
        size: 22_686_210,
        archive: Archive::TarGz,
        server_path: "llama-b11960/llama-server",
        lib_pattern: r"\.so(\.\d+)*$",
        required_libs: &[
            "libllama-server-impl.so",
            "libllama.so.0",
            "libggml-base.so.0",
            "libggml-cpu-x64.so",
        ],
    },
    LlamaAsset {
        key: "win32-x64",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-win-cpu-x64.zip",
        sha256: "73f962f82850d244db47eaacb9d913637027b82399124324298576d58c5f4287",
        size: 24_407_745,
        archive: Archive::Zip,
        server_path: "llama-server.exe",
        lib_pattern: r"(?i)\.dll$",
        required_libs: &[
            "llama-server-impl.dll",
            "llama.dll",
            "ggml-base.dll",
            "ggml-cpu-x64.dll",
            "libomp.dll",
        ],
    },
];

pub struct ModelAsset {
    pub repo: &'static str,
    pub revision: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// Pinned to a commit; sha256 is the LFS oid.
pub const MODEL_ASSET: ModelAsset = ModelAsset {
    repo: "ggml-org/embeddinggemma-2-GGUF",
    revision: "58faf90940886df2d62be555c8765d00dac362c5",
    file: "embeddinggemma-2-Q8_0.gguf",
    url: "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/58faf90940886df2d62be555c8765d00dac362c5/embeddinggemma-2-Q8_0.gguf",
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    size: 309_855_456,
};

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Whether an extracted file is kept next to the server: the server itself,
/// its shared libs, and of the `-impl` libs only the server's.
pub fn is_kept_file(asset: &LlamaAsset, name: &str) -> bool {
    if name == basename(asset.server_path) {
        return true;
    }
    if !Regex::new(asset.lib_pattern).is_ok_and(|re| re.is_match(name)) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    !lower.contains("-impl.") || lower.contains("llama-server-impl.")
}

/// The release asset for a platform/arch (Node naming), or None where history isn't offered.
pub fn select_llama_asset(platform: &str, arch: &str) -> Option<&'static LlamaAsset> {
    let key = format!("{platform}-{arch}");
    LLAMA_ASSETS.iter().find(|a| a.key == key)
}

/// `(process.platform, process.arch)` of this build.
pub fn current_platform() -> (&'static str, &'static str) {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    (platform, arch)
}

// ─── Embedding input/output shaping ──────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedKind {
    Document,
    Query,
}

pub const DOCUMENT_PREFIX: &str = "title: none | text: ";
pub const QUERY_PREFIX: &str = "task: search result | query: ";

pub fn with_prefix(text: &str, kind: EmbedKind) -> String {
    format!(
        "{}{text}",
        if kind == EmbedKind::Query {
            QUERY_PREFIX
        } else {
            DOCUMENT_PREFIX
        }
    )
}

/// ~20k chars keeps nearly every input inside the 8192-token ubatch.
pub const MAX_INPUT_CHARS: usize = 20_000;
const MIN_RETRY_CHARS: usize = 256;

/// Cut to `max_chars` UTF-16 units without splitting a surrogate pair.
pub fn clamp_input(text: &str, max_chars: usize) -> String {
    if crate::util::len16(text) <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > max_chars {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out
}

/// Matryoshka truncation to `dim` dims, re-normalised (as runtime.ts computes it).
pub fn truncate_normalize(vec: &[f64], dim: usize) -> Result<Vec<f32>> {
    if vec.len() < dim {
        bail!("embedding has {} dims, need at least {dim}", vec.len());
    }
    let mut out: Vec<f32> = vec[..dim].iter().map(|&v| v as f32).collect();
    let sum: f64 = vec[..dim].iter().map(|v| v * v).sum();
    let norm = sum.sqrt();
    if norm > 0.0 {
        for x in &mut out {
            *x = (f64::from(*x) / norm) as f32;
        }
    }
    Ok(out)
}

/// Group inputs into requests by item count and char budget; never an empty batch.
pub fn make_batches(texts: &[String], max_items: usize, max_chars: usize) -> Vec<Vec<String>> {
    let mut batches = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut chars = 0;
    for t in texts {
        let len = crate::util::len16(t);
        if !cur.is_empty() && (cur.len() >= max_items || chars + len > max_chars) {
            batches.push(std::mem::take(&mut cur));
            chars = 0;
        }
        cur.push(t.clone());
        chars += len;
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

/// Produces L2-normalized HISTORY_EMBED_DIM vectors, one per input, in order.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>>;
}

// ─── Idle shutdown ───────────────────────────────────────────────────────────

struct IdleState {
    active: usize,
    deadline: Option<Instant>,
    thread: bool,
}

/// Reference-counted idle timer: `busy()` on every request start, `release()`
/// on its end; it arms only once nothing is in flight. One helper thread
/// exists only while the timer is armed.
pub struct IdleTimer {
    ms: Duration,
    on_idle: Arc<dyn Fn() + Send + Sync>,
    shared: Arc<(Mutex<IdleState>, Condvar)>,
}

impl IdleTimer {
    pub fn new(ms: Duration, on_idle: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            ms,
            on_idle,
            shared: Arc::new((
                Mutex::new(IdleState {
                    active: 0,
                    deadline: None,
                    thread: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn busy(&self) {
        let mut s = self.shared.0.lock().unwrap();
        s.active += 1;
        s.deadline = None;
        self.shared.1.notify_all();
    }

    pub fn release(&self) {
        let mut s = self.shared.0.lock().unwrap();
        s.active = s.active.saturating_sub(1);
        if s.active == 0 {
            s.deadline = Some(Instant::now() + self.ms);
            self.shared.1.notify_all();
            if !s.thread {
                s.thread = true;
                let shared = self.shared.clone();
                let on_idle = self.on_idle.clone();
                std::thread::Builder::new()
                    .name("sg-history-idle".into())
                    .spawn(move || Self::run(shared, on_idle))
                    .expect("spawn idle timer");
            }
        }
    }

    pub fn cancel(&self) {
        let mut s = self.shared.0.lock().unwrap();
        s.deadline = None;
        self.shared.1.notify_all();
    }

    pub fn armed(&self) -> bool {
        self.shared.0.lock().unwrap().deadline.is_some()
    }

    fn run(shared: Arc<(Mutex<IdleState>, Condvar)>, on_idle: Arc<dyn Fn() + Send + Sync>) {
        let (lock, cv) = &*shared;
        let mut s = lock.lock().unwrap();
        loop {
            let Some(deadline) = s.deadline else {
                s.thread = false;
                return;
            };
            let now = Instant::now();
            if now >= deadline {
                s.deadline = None;
                let fire = s.active == 0;
                s.thread = false;
                drop(s);
                if fire {
                    on_idle();
                }
                return;
            }
            s = cv.wait_timeout(s, deadline - now).unwrap().0;
        }
    }
}

// ─── Verified download ───────────────────────────────────────────────────────

pub fn sha256_file(file: &Path) -> Result<String> {
    let mut f = std::fs::File::open(file)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

fn resolve_location(base: &ureq::http::Uri, location: &str) -> String {
    let scheme = base.scheme_str().unwrap_or("https");
    let authority = base.authority().map(|a| a.as_str()).unwrap_or("");
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        let path = base.path();
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        format!("{scheme}://{authority}{dir}{location}")
    }
}

fn http_agent(recv_body: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent("SplitGrid")
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(recv_body))
        .build();
    ureq::Agent::new_with_config(config)
}

/// The connection dropped mid-body: worth another try, resuming from what arrived.
#[derive(Debug)]
struct Interrupted(String);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Interrupted {}

/// How many times a dropped download is resumed before giving up.
const DOWNLOAD_ATTEMPTS: u32 = 5;

/// Stream `url` into `part` while hashing; returns the sha256 hex. Bytes
/// already in `part` (an interrupted attempt) are kept and the rest asked for
/// with a Range request; a server that ignores it restarts the file.
fn fetch_to_file(
    url: &str,
    part: &Path,
    cancel: &CancelToken,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<String> {
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let agent = http_agent(Duration::from_secs(3600));
    let mut url = url.to_string();
    for hops in 0..=10 {
        let uri: ureq::http::Uri = url
            .parse()
            .map_err(|_| anyhow!("Invalid download URL: {url}"))?;
        let scheme = uri.scheme_str().unwrap_or("");
        let host = uri.host().unwrap_or("");
        if scheme != "https" && !(scheme == "http" && is_loopback_host(host)) {
            bail!(
                "Refusing non-https download: {scheme}://{}",
                uri.authority().map(|a| a.as_str()).unwrap_or("")
            );
        }
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        let where_ = format!(
            "{}{}",
            uri.authority().map(|a| a.as_str()).unwrap_or(""),
            uri.path()
        );
        let mut req = agent.get(&url).header("accept", "*/*");
        if have > 0 {
            req = req.header("range", format!("bytes={have}-"));
        }
        let res = req.call().map_err(|e| anyhow::Error::new(Interrupted(format!("{e} downloading {where_}"))))?;
        let status = res.status().as_u16();
        if (300..400).contains(&status)
            && let Some(loc) = res.headers().get("location").and_then(|v| v.to_str().ok())
        {
            if hops >= 10 {
                bail!("Too many redirects downloading {where_}");
            }
            url = resolve_location(&uri, loc);
            continue;
        }
        let resumed = have > 0 && status == 206;
        if status != 200 && !resumed {
            bail!("HTTP {status} downloading {where_}");
        }
        let mut reader = res.into_body().into_reader();
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut file = if resumed {
            // Hash what is already there, then append.
            let mut prev = std::fs::File::open(part)?;
            loop {
                let n = prev.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            std::fs::OpenOptions::new().append(true).open(part)?
        } else {
            // A server that ignored the Range: start over (progress counts
            // those bytes twice, which the bar clamps).
            std::fs::File::create(part)?
        };
        loop {
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(_) if cancel.is_cancelled() => return Err(Cancelled.into()),
                Err(e) => return Err(Interrupted(format!("Download failed: {where_}: {e}")).into()),
            };
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            on_bytes(n as u64);
        }
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        file.sync_all().ok();
        return Ok(hex::encode(hash.finalize()));
    }
    bail!("Too many redirects downloading {url}")
}

/// Download `url` to `dest`, accepting it only if its sha256 matches. Streams
/// into `<dest>.part` and renames into place, so `dest` is either absent or
/// verified. Redirects must stay https (plain http only on loopback, for tests).
pub fn download_verified(
    url: &str,
    dest: &Path,
    sha256: &str,
    cancel: &CancelToken,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<()> {
    let part = PathBuf::from(format!("{}.part", dest.display()));
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&part);
    let result = (|| {
        let mut attempt = 1;
        let actual = loop {
            match fetch_to_file(url, &part, cancel, on_bytes) {
                Err(e) if e.is::<Interrupted>() && attempt < DOWNLOAD_ATTEMPTS => {
                    attempt += 1;
                    // Back off a little; a cancel during the wait ends it.
                    let until = std::time::Instant::now() + Duration::from_secs(2 * u64::from(attempt));
                    while std::time::Instant::now() < until {
                        if cancel.is_cancelled() {
                            return Err(Cancelled.into());
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                other => break other?,
            }
        };
        if actual != sha256.to_ascii_lowercase() {
            bail!(
                "Checksum mismatch for {}: expected sha256 {sha256}, got {actual}. The download was corrupted or tampered with and has been discarded.",
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
        }
        let _ = std::fs::remove_file(dest);
        std::fs::rename(&part, dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

// ─── Extraction ──────────────────────────────────────────────────────────────

/// The system extractor for an archive (tar on macOS/Linux; unzip for a zip).
pub fn extract_command(archive: Archive, file: &Path, dest_dir: &Path) -> (String, Vec<String>) {
    match archive {
        Archive::Zip => (
            "unzip".into(),
            vec![
                "-q".into(),
                "-o".into(),
                path_str(file),
                "-d".into(),
                path_str(dest_dir),
            ],
        ),
        Archive::TarGz => (
            "tar".into(),
            vec![
                "-xzf".into(),
                path_str(file),
                "-C".into(),
                path_str(dest_dir),
            ],
        ),
    }
}

fn run_tool(cmd: &str, args: &[String]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = stderr
        .chars()
        .rev()
        .take(2000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    bail!(
        "{} exited with {}: {}",
        basename(cmd),
        out.status.code().map_or("signal".into(), |c| c.to_string()),
        tail.trim()
    )
}

// ─── Server process ──────────────────────────────────────────────────────────

/// llama-server flags (checked against `--help` of the pinned build).
pub fn server_args(model_path: &str, port: u16, threads: usize) -> Vec<String> {
    [
        "-m",
        model_path,
        "--embedding",
        "--pooling",
        "mean",
        "-c",
        "8192",
        "-b",
        "8192",
        "-ub",
        "8192",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "-np",
        "1",
        "-t",
        &threads.to_string(),
        "--offline",
        "--no-webui",
        "--cache-ram",
        "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_threads() -> usize {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (n / 2).clamp(1, 8)
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    hex::encode(buf)
}

/// Every llama-server we started, SIGKILLed at process exit so a quitting
/// app never leaves a 300 MB orphan behind.
static LIVE_PIDS: LazyLock<Mutex<Vec<u32>>> = LazyLock::new(Default::default);

extern "C" fn kill_live_servers() {
    if let Ok(pids) = LIVE_PIDS.try_lock() {
        for &pid in pids.iter() {
            signal(pid, libc::SIGKILL);
        }
    }
}

fn track_pid(pid: u32) {
    static ONCE: Once = Once::new();
    // SAFETY: registering a plain extern "C" fn with no captured state.
    ONCE.call_once(|| unsafe {
        libc::atexit(kill_live_servers);
    });
    LIVE_PIDS.lock().unwrap().push(pid);
}

fn untrack_pid(pid: u32) {
    LIVE_PIDS.lock().unwrap().retain(|&p| p != pid);
}

/// atexit doesn't run when the app is killed (a signal, a crash, a force
/// quit), and macOS has no parent-death signal: a tiny shell watchdog kills
/// the server once this process is gone, and leaves on its own when the
/// server stops first. Costs one sleeping `sh` per running server.
fn watch_parent(server: u32) {
    let script = r#"while kill -0 "$1" 2>/dev/null; do kill -0 "$2" 2>/dev/null || exit 0; sleep 1; done; kill -9 "$2" 2>/dev/null"#;
    let spawned = Command::new("/bin/sh")
        .args(["-c", script, "sg-history-watchdog", &std::process::id().to_string(), &server.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Reap it whenever it ends, so it never lingers as a zombie.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// SIGTERM, then SIGKILL after 3 s.
fn kill_child(mut child: Child) {
    let pid = child.id();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        signal(pid, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
    untrack_pid(pid);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct InstalledPin {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    revision: Option<String>,
    sha256: String,
}

/// `installed.json`, shared with the Electron app.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledRecord {
    version: u32,
    platform: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    llama: Option<InstalledPin>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    model: Option<InstalledPin>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallPhase {
    Download,
    Extract,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstallProgress {
    pub phase: InstallPhase,
    /// Bytes downloaded / total across what this install actually fetches.
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

pub struct EmbeddingRuntimeOptions {
    /// Install root (`<root_dir>/runtime`); not created until install().
    pub root_dir: PathBuf,
    /// Stop the server after this long with no request in flight (default 5 min).
    pub idle: Option<Duration>,
    pub threads: Option<usize>,
    /// Node-style platform/arch overrides for tests.
    pub platform: Option<(&'static str, &'static str)>,
    pub log: Option<LogFn>,
}

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug)]
enum RequestError {
    /// The server's answer, not a crash.
    Http(String),
    /// An input tokenised past the ubatch.
    TooLarge(String),
    /// Process died / connection reset: worth one restart.
    Transport(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Http(m) | RequestError::TooLarge(m) | RequestError::Transport(m) => {
                f.write_str(m)
            }
        }
    }
}

#[derive(Default)]
struct ServerState {
    child: Option<Child>,
    ready: bool,
    port: u16,
    api_key: String,
    /// Bumped by stop_server() so an in-progress start knows it was cancelled.
    generation: u64,
    stderr_tail: Option<Arc<Mutex<String>>>,
}

pub struct EmbeddingRuntime {
    pub root_dir: PathBuf,
    pub asset: Option<&'static LlamaAsset>,
    platform: (&'static str, &'static str),
    threads: usize,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    idle: IdleTimer,
    server: Mutex<ServerState>,
    start_lock: Mutex<()>,
    install_lock: Mutex<()>,
    install_cancel: Mutex<Option<CancelToken>>,
}

impl EmbeddingRuntime {
    pub fn new(opts: EmbeddingRuntimeOptions) -> Arc<Self> {
        let platform = opts.platform.unwrap_or_else(current_platform);
        let log = opts.log.unwrap_or_else(|| Arc::new(|_: &str| {}));
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let w = weak.clone();
            let idle_log = log.clone();
            let on_idle: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(rt) = w.upgrade() {
                    idle_log("history runtime: idle, stopping llama-server");
                    rt.stop_server();
                }
            });
            Self {
                root_dir: opts.root_dir,
                asset: select_llama_asset(platform.0, platform.1),
                platform,
                threads: opts.threads.unwrap_or_else(default_threads),
                log,
                idle: IdleTimer::new(opts.idle.unwrap_or(Duration::from_secs(5 * 60)), on_idle),
                server: Mutex::new(ServerState::default()),
                start_lock: Mutex::new(()),
                install_lock: Mutex::new(()),
                install_cancel: Mutex::new(None),
            }
        })
    }

    // ── Paths ──
    pub fn bin_dir(&self) -> PathBuf {
        self.root_dir.join("bin")
    }
    pub fn server_path(&self) -> PathBuf {
        self.bin_dir().join(basename(
            self.asset.map_or("llama-server", |a| a.server_path),
        ))
    }
    pub fn model_path(&self) -> PathBuf {
        self.root_dir.join("models").join(MODEL_ASSET.file)
    }
    fn record_path(&self) -> PathBuf {
        self.root_dir.join("installed.json")
    }
    fn tmp_dir(&self) -> PathBuf {
        self.root_dir.join("tmp")
    }

    pub fn is_supported(&self) -> bool {
        self.asset.is_some()
    }

    /// Bytes a fresh install downloads (for the consent prompt).
    pub fn download_bytes(&self) -> u64 {
        self.asset.map_or(0, |a| a.size) + MODEL_ASSET.size
    }

    // ── Install state ──
    fn read_record(&self) -> Option<InstalledRecord> {
        let raw = std::fs::read_to_string(self.record_path()).ok()?;
        serde_json::from_str::<InstalledRecord>(&raw)
            .ok()
            .filter(|r| r.version == 1)
    }

    fn binary_present(&self, asset: &LlamaAsset) -> bool {
        self.server_path().exists()
            && asset
                .required_libs
                .iter()
                .all(|lib| self.bin_dir().join(lib).exists())
    }

    fn model_present(&self) -> bool {
        std::fs::metadata(self.model_path()).is_ok_and(|m| m.len() == MODEL_ASSET.size)
    }

    fn binary_current(&self, rec: Option<&InstalledRecord>, asset: &LlamaAsset) -> bool {
        rec.is_some_and(|r| {
            r.platform == asset.key && r.llama.as_ref().is_some_and(|l| l.sha256 == asset.sha256)
        }) && self.binary_present(asset)
    }

    fn model_current(&self, rec: Option<&InstalledRecord>) -> bool {
        rec.is_some_and(|r| {
            r.model
                .as_ref()
                .is_some_and(|m| m.sha256 == MODEL_ASSET.sha256)
        }) && self.model_present()
    }

    /// Cheap check: the recorded manifest matches this build's pins and the files are there.
    pub fn is_installed(&self) -> bool {
        let Some(asset) = self.asset else {
            return false;
        };
        let rec = self.read_record();
        self.binary_current(rec.as_ref(), asset) && self.model_current(rec.as_ref())
    }

    /// Download, verify and unpack llama-server + the model (only what is
    /// missing or out of date). Call only after consent. Concurrent calls run
    /// one after the other; the second finds everything in place.
    pub fn install(
        &self,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<()> {
        let _guard = self.install_lock.lock().unwrap_or_else(|e| e.into_inner());
        *self.install_cancel.lock().unwrap() = Some(cancel.clone());
        let result = self.do_install(cancel, on_progress);
        *self.install_cancel.lock().unwrap() = None;
        result
    }

    fn do_install(
        &self,
        token: &CancelToken,
        on_progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<()> {
        let (platform, arch) = self.platform;
        let Some(asset) = self.asset else {
            bail!("Workspace history search isn't available on {platform}-{arch}")
        };
        // A server running from the bin about to be replaced would keep the old files open.
        self.stop_server();

        let mut rec = self.read_record();
        if rec.as_ref().is_some_and(|r| r.platform != asset.key) {
            rec = None;
        }
        let need_binary = !self.binary_current(rec.as_ref(), asset);
        let need_model = !self.model_current(rec.as_ref());
        let total = if need_binary { asset.size } else { 0 }
            + if need_model { MODEL_ASSET.size } else { 0 };
        let mut downloaded = 0u64;
        let mut last_emit: Option<Instant> = None;
        let mut emit = |phase: InstallPhase,
                        downloaded: u64,
                        force: bool,
                        on_progress: &mut dyn FnMut(InstallProgress)| {
            if !force && last_emit.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
                return;
            }
            last_emit = Some(Instant::now());
            on_progress(InstallProgress {
                phase,
                downloaded_bytes: downloaded,
                total_bytes: total,
            });
        };

        let mut record = InstalledRecord {
            version: 1,
            platform: asset.key.to_string(),
            llama: rec.as_ref().and_then(|r| r.llama.clone()),
            model: rec.as_ref().and_then(|r| r.model.clone()),
        };
        let save = |record: &InstalledRecord| -> Result<()> {
            std::fs::create_dir_all(&self.root_dir)?;
            write_atomic(
                &self.record_path(),
                serde_json::to_string_pretty(record)?.as_bytes(),
                None,
            )?;
            Ok(())
        };

        emit(InstallPhase::Download, 0, true, on_progress);
        std::fs::create_dir_all(self.tmp_dir())?;
        let result = (|| -> Result<()> {
            if need_binary {
                // Invalidate first so a crash mid-swap can't leave a record vouching for a half-replaced bin/.
                record.llama = None;
                save(&record)?;
                let archive = self.tmp_dir().join(basename(asset.url));
                download_verified(asset.url, &archive, asset.sha256, token, &mut |n| {
                    downloaded += n;
                    emit(InstallPhase::Download, downloaded, false, on_progress);
                })?;
                if token.is_cancelled() {
                    return Err(Cancelled.into());
                }
                emit(InstallPhase::Extract, downloaded, true, on_progress);
                self.install_binary(asset, &archive)?;
                record.llama = Some(InstalledPin {
                    tag: Some(LLAMA_CPP_TAG.into()),
                    revision: None,
                    sha256: asset.sha256.into(),
                });
                save(&record)?;
            }
            if need_model {
                record.model = None;
                save(&record)?;
                emit(InstallPhase::Download, downloaded, true, on_progress);
                download_verified(
                    MODEL_ASSET.url,
                    &self.model_path(),
                    MODEL_ASSET.sha256,
                    token,
                    &mut |n| {
                        downloaded += n;
                        emit(InstallPhase::Download, downloaded, false, on_progress);
                    },
                )?;
                record.model = Some(InstalledPin {
                    tag: None,
                    revision: Some(MODEL_ASSET.revision.into()),
                    sha256: MODEL_ASSET.sha256.into(),
                });
                save(&record)?;
            }
            if !need_binary && !need_model {
                save(&record)?;
            }
            emit(InstallPhase::Done, downloaded, true, on_progress);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(self.tmp_dir());
        result
    }

    fn install_binary(&self, asset: &LlamaAsset, archive: &Path) -> Result<()> {
        let staging = self.tmp_dir().join("extract");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let (cmd, args) = extract_command(asset.archive, archive, &staging);
        run_tool(&cmd, &args)?;
        let archive_name = archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let server_src = asset
            .server_path
            .split('/')
            .fold(staging.clone(), |p, seg| p.join(seg));
        if !server_src.exists() {
            bail!("llama-server not found in {archive_name}");
        }
        let src_dir = server_src.parent().context("server dir")?.to_path_buf();
        for entry in std::fs::read_dir(&src_dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_kept_file(asset, &name) {
                continue;
            }
            let p = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
        for lib in asset.required_libs {
            if !src_dir.join(lib).exists() {
                bail!("{lib} missing from {archive_name}");
            }
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&server_src, std::fs::Permissions::from_mode(0o755))?;
        }
        if cfg!(target_os = "macos") {
            // Best effort: our own downloads carry no quarantine xattr anyway.
            let _ = run_tool(
                "/usr/bin/xattr",
                &[
                    "-dr".into(),
                    "com.apple.quarantine".into(),
                    path_str(&src_dir),
                ],
            );
        }
        let _ = std::fs::remove_dir_all(self.bin_dir());
        std::fs::rename(&src_dir, self.bin_dir())?;
        Ok(())
    }

    /// Stop the server, cancel any install, and delete everything under root_dir.
    pub fn uninstall(&self) -> Result<()> {
        if let Some(t) = self.install_cancel.lock().unwrap().as_ref() {
            t.cancel();
        }
        let _guard = self.install_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.stop();
        match std::fs::remove_dir_all(&self.root_dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    // ── Serving ──

    fn embed_batch(&self, raw: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<String> = raw.iter().map(|t| with_prefix(t, kind)).collect();
        match self.request(&inputs) {
            Ok(v) => Ok(v),
            Err(RequestError::TooLarge(msg)) => {
                // Find the offender one at a time, then halve it until it fits.
                if raw.len() > 1 {
                    let mut out = Vec::new();
                    for t in raw {
                        out.extend(self.embed_batch(std::slice::from_ref(t), kind)?);
                    }
                    return Ok(out);
                }
                let t = &raw[0];
                let len = crate::util::len16(t);
                if len <= MIN_RETRY_CHARS {
                    bail!("{msg}");
                }
                self.embed_batch(&[clamp_input(t, len / 2)], kind)
            }
            Err(e) => Err(anyhow!("{e}")),
        }
    }

    fn request(&self, inputs: &[String]) -> std::result::Result<Vec<Vec<f32>>, RequestError> {
        let mut attempt = 0;
        loop {
            let (port, key) = self
                .ensure_server()
                .map_err(|e| RequestError::Http(e.to_string()))?;
            match self.post(port, &key, inputs) {
                Ok(v) => return Ok(v),
                Err(RequestError::Transport(m)) if attempt == 0 => {
                    (self.log)(&format!(
                        "history runtime: request failed ({m}); restarting llama-server"
                    ));
                    self.stop_server();
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn post(
        &self,
        port: u16,
        key: &str,
        inputs: &[String],
    ) -> std::result::Result<Vec<Vec<f32>>, RequestError> {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(REQUEST_TIMEOUT))
                .build(),
        );
        let body = serde_json::json!({ "input": inputs, "encoding_format": "float" }).to_string();
        let res = agent
            .post(&format!("http://127.0.0.1:{port}/v1/embeddings"))
            .header("content-type", "application/json")
            .header("authorization", &format!("Bearer {key}"))
            .send(body.as_bytes())
            .map_err(|e| RequestError::Transport(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res
            .into_body()
            .with_config()
            .limit(256 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| RequestError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string))
                .unwrap_or_else(|| text.clone());
            if message.to_ascii_lowercase().contains("too large") {
                return Err(RequestError::TooLarge(message));
            }
            let short: String = message.chars().take(300).collect();
            return Err(RequestError::Http(format!(
                "llama-server HTTP {status}: {short}"
            )));
        }
        #[derive(Deserialize)]
        struct Item {
            index: usize,
            embedding: Vec<f64>,
        }
        #[derive(Deserialize)]
        struct Body {
            #[serde(default)]
            data: Vec<Item>,
        }
        let mut body: Body = serde_json::from_str(&text)
            .map_err(|e| RequestError::Http(format!("llama-server returned invalid JSON: {e}")))?;
        body.data.sort_by_key(|d| d.index);
        if body.data.len() != inputs.len() {
            return Err(RequestError::Http(format!(
                "llama-server returned {} embeddings for {} inputs",
                body.data.len(),
                inputs.len()
            )));
        }
        body.data
            .iter()
            .map(|d| {
                truncate_normalize(&d.embedding, HISTORY_EMBED_DIM)
                    .map_err(|e| RequestError::Http(e.to_string()))
            })
            .collect()
    }

    /// (port, api key) of a ready server, starting one if needed.
    fn ensure_server(&self) -> Result<(u16, String)> {
        let _start = self.start_lock.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut s = self.server.lock().unwrap();
            let alive = s
                .child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
            if alive && s.ready {
                return Ok((s.port, s.api_key.clone()));
            }
            if !alive && let Some(c) = s.child.take() {
                (self.log)(&format!(
                    "history runtime: llama-server exited (pid {})",
                    c.id()
                ));
                untrack_pid(c.id());
                s.ready = false;
            }
        }
        if !self.is_installed() {
            bail!("Embedding runtime is not installed");
        }
        let generation = self.server.lock().unwrap().generation;
        // One retry covers the free-port race.
        let mut attempt = 0;
        loop {
            if self.server.lock().unwrap().generation != generation {
                bail!("llama-server start cancelled");
            }
            match self.spawn_once(generation) {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 && self.server.lock().unwrap().generation == generation => {
                    (self.log)(&format!(
                        "history runtime: llama-server failed to start ({e}); retrying"
                    ));
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn spawn_once(&self, generation: u64) -> Result<(u16, String)> {
        let port = free_port()?;
        let api_key = random_hex(24);
        // Drop any LLAMA_ARG_* the user's shell exported: they'd override our flags.
        let mut env: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| !k.starts_with("LLAMA_ARG_"))
            .collect();
        env.push(("LLAMA_API_KEY".into(), api_key.clone()));
        let mut child = Command::new(self.server_path())
            .args(server_args(
                &path_str(&self.model_path()),
                port,
                self.threads,
            ))
            .current_dir(self.bin_dir())
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        track_pid(pid);
        lower_priority(pid);
        watch_parent(pid);
        let tail = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let tail = tail.clone();
            std::thread::spawn(move || {
                let mut r = err;
                let mut buf = [0u8; 4096];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > 4000 {
                        let cut = t.len() - 4000;
                        let cut = (cut..t.len())
                            .find(|&i| t.is_char_boundary(i))
                            .unwrap_or(t.len());
                        t.drain(..cut);
                    }
                }
            });
        }
        {
            let mut s = self.server.lock().unwrap();
            if s.generation != generation {
                drop(s);
                kill_child(child);
                bail!("llama-server start cancelled");
            }
            s.child = Some(child);
            s.ready = false;
            s.stderr_tail = Some(tail.clone());
        }
        let last_lines = || {
            let t = tail.lock().unwrap();
            let lines: Vec<&str> = t.trim().split('\n').collect();
            lines[lines.len().saturating_sub(5)..].join("\n")
        };
        let health = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(2)))
                .build(),
        );
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            {
                let mut s = self.server.lock().unwrap();
                if s.generation != generation {
                    let ours = s.child.as_ref().is_some_and(|c| c.id() == pid);
                    let c = if ours { s.child.take() } else { None };
                    drop(s);
                    if let Some(c) = c {
                        kill_child(c);
                    }
                    bail!("llama-server start cancelled");
                }
                let exited = match s.child.as_mut() {
                    Some(c) if c.id() == pid => !matches!(c.try_wait(), Ok(None)),
                    _ => true,
                };
                if exited {
                    if s.child.as_ref().is_some_and(|c| c.id() == pid) {
                        s.child = None;
                    }
                    untrack_pid(pid);
                    drop(s);
                    bail!("llama-server exited during startup: {}", last_lines());
                }
            }
            // 503 while the model loads, 200 once it can serve.
            if let Ok(res) = health
                .get(&format!("http://127.0.0.1:{port}/health"))
                .header("authorization", &format!("Bearer {api_key}"))
                .call()
                && res.status().is_success()
            {
                let mut s = self.server.lock().unwrap();
                if s.generation == generation && s.child.as_ref().is_some_and(|c| c.id() == pid) {
                    s.port = port;
                    s.api_key = api_key.clone();
                    s.ready = true;
                    drop(s);
                    (self.log)(&format!(
                        "history runtime: llama-server ready on 127.0.0.1:{port} (pid {pid})"
                    ));
                    return Ok((port, api_key));
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let c = {
            let mut s = self.server.lock().unwrap();
            if s.child.as_ref().is_some_and(|c| c.id() == pid) {
                s.child.take()
            } else {
                None
            }
        };
        if let Some(c) = c {
            kill_child(c);
        }
        bail!(
            "llama-server did not become healthy within {}s: {}",
            STARTUP_TIMEOUT.as_secs(),
            last_lines()
        )
    }

    fn stop_server(&self) {
        let child = {
            let mut s = self.server.lock().unwrap();
            s.generation += 1;
            s.ready = false;
            s.stderr_tail = None;
            s.child.take()
        };
        if let Some(c) = child {
            (self.log)(&format!(
                "history runtime: stopping llama-server (pid {})",
                c.id()
            ));
            kill_child(c);
        }
    }

    /// For quit / disable: stop the server and the idle timer. A later embed() starts it again.
    pub fn stop(&self) {
        self.idle.cancel();
        self.stop_server();
    }

    /// The pid of the running server (diagnostics/tests).
    pub fn server_pid(&self) -> Option<u32> {
        self.server.lock().unwrap().child.as_ref().map(Child::id)
    }
}

impl Embedder for EmbeddingRuntime {
    /// Starts the server on demand.
    fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        self.idle.busy();
        struct Release<'a>(&'a IdleTimer);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.release();
            }
        }
        let _release = Release(&self.idle);
        let clamped: Vec<String> = texts
            .iter()
            .map(|t| clamp_input(t, MAX_INPUT_CHARS))
            .collect();
        let mut out = Vec::with_capacity(texts.len());
        for batch in make_batches(&clamped, 32, 48_000) {
            out.extend(self.embed_batch(&batch, kind)?);
        }
        Ok(out)
    }
}

impl Drop for EmbeddingRuntime {
    fn drop(&mut self) {
        if let Some(c) = self.server.get_mut().ok().and_then(|s| s.child.take()) {
            kill_child(c);
        }
    }
}

/// What the service needs from the runtime (lets tests fake it).
pub trait RuntimeLike: Embedder {
    fn is_supported(&self) -> bool;
    fn is_installed(&self) -> bool;
    fn download_bytes(&self) -> u64;
    fn install(
        &self,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn stop(&self);
}

impl RuntimeLike for EmbeddingRuntime {
    fn is_supported(&self) -> bool {
        EmbeddingRuntime::is_supported(self)
    }
    fn is_installed(&self) -> bool {
        EmbeddingRuntime::is_installed(self)
    }
    fn download_bytes(&self) -> u64 {
        EmbeddingRuntime::download_bytes(self)
    }
    fn install(
        &self,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<()> {
        EmbeddingRuntime::install(self, cancel, on_progress)
    }
    fn uninstall(&self) -> Result<()> {
        EmbeddingRuntime::uninstall(self)
    }
    fn stop(&self) {
        EmbeddingRuntime::stop(self)
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
