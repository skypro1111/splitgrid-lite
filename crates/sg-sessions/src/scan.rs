//! The background scan: every couple of seconds (every second while the
//! popover is open) ask each session daemon for its info. The UI reads the
//! last result; it never blocks on a daemon.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sg_ptyd::SessionInfo;
use sg_ptyd::client;

const SCAN_EVERY: Duration = Duration::from_secs(2);
const SCAN_EVERY_OPEN: Duration = Duration::from_secs(1);

#[derive(Clone, Default)]
pub struct Scan {
    pub sessions: Vec<(PathBuf, SessionInfo)>,
    /// Sessions that answered but couldn't be read (another protocol).
    pub broken: Vec<(PathBuf, String)>,
    /// Scans done so far (0: nothing known yet).
    pub scans: u64,
}

impl Scan {
    pub fn count(&self) -> usize {
        self.sessions.len() + self.broken.len()
    }
}

static SCAN: Mutex<Scan> = Mutex::new(Scan { sessions: Vec::new(), broken: Vec::new(), scans: 0 });
static FAST: AtomicBool = AtomicBool::new(false);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Start scanning `sessions_dir` on a background thread.
pub fn start(sessions_dir: PathBuf) {
    let dir = DIR.get_or_init(|| sessions_dir).clone();
    std::thread::spawn(move || {
        loop {
            scan_once(&dir);
            std::thread::sleep(if FAST.load(Ordering::Relaxed) { SCAN_EVERY_OPEN } else { SCAN_EVERY });
        }
    });
}

/// Scan faster while someone is looking.
pub fn set_fast(on: bool) {
    FAST.store(on, Ordering::Relaxed);
}

/// The last scan.
pub fn last() -> Scan {
    SCAN.lock().unwrap().clone()
}

/// (sessions, scans so far), cheap.
pub fn count() -> (usize, u64) {
    let s = SCAN.lock().unwrap();
    (s.count(), s.scans)
}

/// Scan again now, off the main thread (after a Stop or Close).
pub fn rescan_soon() {
    if let Some(dir) = DIR.get() {
        let dir = dir.clone();
        std::thread::spawn(move || scan_once(&dir));
    }
}

fn scan_once(dir: &Path) {
    let mut sessions = Vec::new();
    let mut broken = Vec::new();
    for found in client::scan(dir, Duration::from_secs(1)) {
        match found.info {
            Ok(info) => sessions.push((found.socket, info)),
            Err(e) => broken.push((found.socket, e)),
        }
    }
    let mut s = SCAN.lock().unwrap();
    s.sessions = sessions;
    s.broken = broken;
    s.scans += 1;
}
