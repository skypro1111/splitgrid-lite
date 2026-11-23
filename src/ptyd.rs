//! Persistent local terminals (Settings → General → Persistent terminals):
//! with the option on, a local terminal's shell runs in its own
//! `splitgrid-ptyd` daemon (crates/sg-ptyd) instead of in this process, so it
//! survives quitting Lite and is re-attached by container id on the next start.
//!
//! - **Spawn/attach** ([`open`]): a pane whose container has a live session
//!   attaches to it (whatever the option says: turning it off only stops new
//!   sessions); otherwise, with the option on, a daemon is started from the
//!   installed binary. Any failure falls back to the in-process PTY.
//! - **Detach vs end**: dropping a pane's [`Session`] detaches (quit, window
//!   close, crash). Sessions end only when the user closes their pane,
//!   workspace or environment ([`end_session`]), or from Settings / the menu.
//! - **Install** ([`installed`]): the daemon and the menu helper are copied out
//!   of the app bundle to `<data dir>/ptyd/bin/<name>-<version>-<hash>`, so an
//!   app update installs new binaries for new sessions while running ones keep
//!   theirs.
//! - **Menu bar** ([`ensure_menu`]): `splitgrid-sessions` lists every live
//!   session while Lite is closed; Lite starts it when sessions exist.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener as _};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::Processor;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::Context;
use sg_ptyd::client::{self, Conn};
use sg_ptyd::daemon::SpawnSpec;
use sg_ptyd::proto::{self, Size, kind};
use sg_ptyd::{Meta, paths};

use crate::app::AppView;
use crate::terminal::{GridSize, Listener, TermEvent, Terminal};

/// The app-settings key (Lite's own; the Electron app keeps it as an unknown key).
pub const SETTING: &str = "persistentTerminals";
/// Show the menu-bar helper while sessions exist (default on).
pub const MENU_SETTING: &str = "ptydMenuBarIcon";
const TIMEOUT: Duration = Duration::from_secs(2);

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where sessions live: Lite's data dir (main.rs).
pub fn init(data_dir: &Path) {
    let _ = DATA_DIR.set(data_dir.to_path_buf());
}

fn data_dir() -> Option<&'static Path> {
    DATA_DIR.get().map(PathBuf::as_path)
}

pub fn sessions_dir() -> Option<PathBuf> {
    data_dir().map(paths::sessions_dir)
}

fn setting(dir: &Path, key: &str) -> Option<bool> {
    let v: serde_json::Value = std::fs::read(dir.join("app-settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
    v.get(key)?.as_bool()
}

/// New local terminals run in a daemon.
pub fn enabled() -> bool {
    data_dir().and_then(|d| setting(d, SETTING)).unwrap_or(false)
}

pub fn menu_icon_enabled() -> bool {
    data_dir().and_then(|d| setting(d, MENU_SETTING)).unwrap_or(true)
}

// ── The Lite side of one attached session ───────────────────────────────────

enum Cmd {
    Input(Vec<u8>),
    Resize(Size),
    Meta(Vec<u8>),
}

/// A terminal's link to its daemon: a writer thread sends input, size and
/// metadata; a reader thread parses output into the `Term`. Dropping it
/// detaches.
pub struct Session {
    tx: mpsc::Sender<Cmd>,
    detaching: Arc<AtomicBool>,
    master: Option<OwnedFd>,
    pid: i32,
    meta_sent: RefCell<Option<Meta>>,
}

type SharedTerm = Arc<FairMutex<Term<Listener>>>;

impl Session {
    /// Attach on `conn`: `make` builds the `Term` at the session's size, then
    /// the replay is fed into it (with `replaying` set until it ends).
    pub fn attach(
        mut conn: Conn,
        socket: PathBuf,
        listener: Listener,
        replaying: Arc<AtomicBool>,
        make: impl FnOnce(Size) -> (SharedTerm, GridSize),
    ) -> io::Result<(Self, (SharedTerm, GridSize))> {
        let attached = conn.attach()?;
        let (term, grid) = make(attached.size);
        // (Fails on macOS when the daemon already closed: the reader sees the end.)
        let _ = conn.set_timeout(None);
        let mut writer = conn.writer()?;
        let (tx, rx) = mpsc::channel::<Cmd>();
        let detaching = Arc::new(AtomicBool::new(false));

        std::thread::Builder::new().name("ptyd-writer".into()).spawn(move || {
            use std::io::Write as _;
            for cmd in rx {
                let frame = match cmd {
                    Cmd::Input(b) => proto::frame(kind::INPUT, &b),
                    Cmd::Resize(s) => proto::frame(kind::RESIZE, &s.encode()),
                    Cmd::Meta(json) => proto::frame(kind::SET_META, &json),
                };
                if writer.write_all(&frame).is_err() {
                    return;
                }
            }
            // The Session was dropped: detach and let the reader see the end.
            let _ = writer.write_all(&proto::frame(kind::DETACH, &[]));
            let _ = writer.shutdown(std::net::Shutdown::Both);
        })?;

        let reader_term = term.clone();
        let done = detaching.clone();
        std::thread::Builder::new().name("ptyd-reader".into()).spawn(move || read_loop(conn, &socket, reader_term, listener, replaying, done))?;

        let session = Self { tx, detaching, master: attached.master, pid: attached.pid, meta_sent: RefCell::new(None) };
        Ok((session, (term, grid)))
    }

    /// The PTY master the daemon passed: for tcgetpgrp/tcgetsid only.
    pub fn master(&self) -> Option<RawFd> {
        self.master.as_ref().map(|f| f.as_raw_fd())
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn input(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::Input(bytes));
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let _ = self.tx.send(Cmd::Resize(Size { cols, rows, cell_width, cell_height }));
    }

    /// Tell the daemon what this session is now (sent only when it changed).
    pub fn set_meta(&self, meta: Meta) {
        if self.meta_sent.borrow().as_ref() == Some(&meta) {
            return;
        }
        if let Ok(json) = serde_json::to_vec(&meta) {
            let _ = self.tx.send(Cmd::Meta(json));
        }
        *self.meta_sent.borrow_mut() = Some(meta);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The writer thread sees the channel close, detaches and shuts the socket.
        self.detaching.store(true, Ordering::Relaxed);
    }
}

fn read_loop(mut conn: Conn, socket: &Path, term: SharedTerm, listener: Listener, replaying: Arc<AtomicBool>, detaching: Arc<AtomicBool>) {
    let mut parser: Processor = Processor::new();
    loop {
        match conn.recv() {
            Ok(Some((kind::OUTPUT, p))) => {
                parser.advance(&mut *term.lock(), &p);
                if !replaying.load(Ordering::Relaxed) {
                    listener.send_event(Event::Wakeup);
                }
            }
            Ok(Some((kind::REPLAY_DONE, _))) => {
                replaying.store(false, Ordering::Relaxed);
                listener.send_event(Event::Wakeup);
            }
            Ok(Some((kind::EXIT, _))) => {
                replaying.store(false, Ordering::Relaxed);
                listener.send_event(Event::Exit);
                return;
            }
            Ok(Some((kind::DETACHED, _))) => {
                replaying.store(false, Ordering::Relaxed);
                parser.advance(&mut *term.lock(), b"\r\n\x1b[2m[This session is now shown in another window.]\x1b[0m\r\n");
                listener.send_event(Event::Wakeup);
                return;
            }
            Ok(Some((kind::ERROR, p))) => log::warn!("ptyd: {}", String::from_utf8_lossy(&p)),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                replaying.store(false, Ordering::Relaxed);
                if detaching.load(Ordering::Relaxed) {
                    return;
                }
                if Conn::connect(socket, TIMEOUT).is_ok() {
                    // The daemon dropped us (it does when a client falls far behind) but the
                    // session runs on: say so, and leave it be (closing the pane would end it).
                    parser.advance(
                        &mut *term.lock(),
                        b"\r\n\x1b[2m[Lost the connection to this terminal; it keeps running and reconnects when SplitGrid Lite restarts.]\x1b[0m\r\n",
                    );
                    listener.send_event(Event::Wakeup);
                } else {
                    // The daemon is gone (crashed or killed). Closing the pane drops our dup of
                    // the PTY master, which hangs up whatever was left on it.
                    listener.send_event(Event::Exit);
                }
                return;
            }
        }
    }
}

// ── Opening a local terminal ─────────────────────────────────────────────────

/// Attach `container_id`'s live session, or start one when the option is on.
/// `None`: use an in-process shell (option off, or the daemon failed).
pub fn open(container_id: &str, cwd: Option<PathBuf>, env: HashMap<String, String>, meta: Meta) -> Option<(Terminal, UnboundedReceiver<TermEvent>)> {
    let dir = sessions_dir()?;
    let socket = paths::socket_path(&dir, container_id);
    let cell = (8., 17.);
    if socket.exists() {
        match Conn::connect(&socket, TIMEOUT) {
            Ok(conn) => match Terminal::attach_daemon(conn, socket.clone(), cell) {
                Ok(t) => return Some(t),
                Err(e) => {
                    log::warn!("ptyd: attaching {container_id}: {e}; using an in-process shell");
                    return None;
                }
            },
            Err(e) if client::is_gone(&e) => {
                let _ = std::fs::remove_file(&socket);
            }
            Err(e) => {
                // An incompatible (older/newer) daemon: leave it running; Settings and the menu can end it.
                log::warn!("ptyd: the session of {container_id} can't be attached ({e}); using an in-process shell");
                return None;
            }
        }
    }
    if !enabled() {
        return None;
    }
    let started = spawn_daemon(&socket, cwd, env, meta)
        .and_then(|()| Conn::connect(&socket, TIMEOUT))
        .and_then(|conn| Terminal::attach_daemon(conn, socket.clone(), cell));
    match started {
        Ok(t) => {
            std::thread::spawn(ensure_menu);
            Some(t)
        }
        Err(e) => {
            log::error!("ptyd: starting a session for {container_id}: {e}; using an in-process shell");
            None
        }
    }
}

fn spawn_daemon(socket: &Path, cwd: Option<PathBuf>, mut env: HashMap<String, String>, meta: Meta) -> io::Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    if let Some(dir) = socket.parent() {
        paths::ensure_private_dir(dir)?;
    }
    paths::check_len(socket)?;
    let bin = installed()?.daemon;
    // This shell keeps this run's agent token; the next run must use it too.
    if let (Some(data), Some(token)) = (data_dir(), env.get("SPLITGRID_BROWSER_TOKEN")) {
        adopt_agent_token(data, token);
    }
    // The same additions Terminal::spawn makes for an in-process shell.
    env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    env.insert("TERM_PROGRAM".to_owned(), "SplitGrid".to_owned());
    let spec = SpawnSpec {
        socket: socket.to_path_buf(),
        cwd: cwd.filter(|p| p.is_dir()).or_else(std::env::home_dir),
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 17,
        env: env.into_iter().collect(),
        meta,
        ..Default::default()
    };
    // A launchd job of its own, so it outlives Lite (sg_ptyd::launchd). If
    // launchd refuses, a plain child still works while Lite runs from a shell.
    match spawn_job(&bin, socket, &spec) {
        Ok(()) => return Ok(()),
        Err(e) => log::warn!("ptyd: launchd job for {}: {e}; starting it as a child", socket.display()),
    }
    let mut child = Command::new(&bin)
        .arg("serve")
        .current_dir(std::env::home_dir().unwrap_or_else(|| "/".into()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // The env (agent tokens) goes over stdin, never argv.
    child.stdin.take().expect("piped").write_all(&serde_json::to_vec(&spec).map_err(io::Error::other)?)?;
    let out = child.wait_with_output()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if line.starts_with("ok ") { Ok(()) } else { Err(io::Error::other(format!("splitgrid-ptyd: {line}"))) }
}

/// Start the daemon as a launchd job: the spec (env with agent tokens) in a
/// 0600 file the daemon deletes at once, readiness through a file it writes.
fn spawn_job(bin: &Path, socket: &Path, spec: &SpawnSpec) -> io::Result<()> {
    use sg_ptyd::launchd;
    let data = data_dir().ok_or_else(|| io::Error::other("no data dir"))?;
    let label = launchd::session_label(socket);
    let spec_file = socket.with_extension("spec");
    let ready = socket.with_extension("ready");
    let _ = std::fs::remove_file(&ready);
    write_private(&spec_file, &serde_json::to_string(spec).map_err(io::Error::other)?)?;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let args = vec![s(bin), "job".into(), "--spec-file".into(), s(&spec_file), "--ready-file".into(), s(&ready), "--label".into(), label.clone()];
    let home = std::env::home_dir().unwrap_or_else(|| "/".into());
    if let Err(e) = launchd::submit(&launchd::jobs_dir(data), &label, &args, &home) {
        let _ = std::fs::remove_file(&spec_file);
        return Err(e);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(line) = std::fs::read_to_string(&ready)
            && line.ends_with('\n')
        {
            let _ = std::fs::remove_file(&ready);
            let line = line.trim();
            if line.starts_with("ok ") {
                return Ok(());
            }
            launchd::remove(&label);
            return Err(io::Error::other(format!("splitgrid-ptyd: {line}")));
        }
        if std::time::Instant::now() > deadline {
            let _ = std::fs::remove_file(&spec_file);
            launchd::remove(&label);
            return Err(io::Error::other("splitgrid-ptyd: no answer from the job"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// End a container's session, if it has one (its pane, workspace or
/// environment was closed). Off the UI thread.
pub fn end_session(container_id: &str) {
    let Some(dir) = sessions_dir() else { return };
    let socket = paths::socket_path(&dir, container_id);
    if socket.exists() {
        std::thread::spawn(move || {
            if let Err(e) = client::terminate(&socket, Duration::from_secs(3)) {
                log::warn!("ptyd: ending {}: {e}", socket.display());
            }
        });
    }
}

// ── Installing the binaries ─────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Installed {
    pub daemon: PathBuf,
    pub menu: Option<PathBuf>,
}

static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);

/// Where a helper binary ships: next to the main executable (the bundle's
/// Contents/MacOS), else — a dev build — cargo's target dir.
fn bundled(name: &str) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut candidates = vec![exe_dir.join(name)];
    for profile in ["debug", "release"] {
        candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join(profile).join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Copy `src` to `bin_dir/<name>-<version>-<hash>` (temp + rename; never over
/// a binary a running session uses), and remove older copies of `name`.
fn install_one(name: &str, src: &Path, bin_dir: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::fs::PermissionsExt as _;
    let bytes = std::fs::read(src)?;
    let hash: String = Sha256::digest(&bytes).iter().take(6).map(|b| format!("{b:02x}")).collect();
    let file = format!("{name}-{}-{hash}", sg_ptyd::VERSION);
    let dest = bin_dir.join(&file);
    if !dest.is_file() {
        let tmp = bin_dir.join(format!(".{file}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &dest)?;
    }
    // Older versions: unlinking is safe for processes still running them.
    if let Ok(entries) = std::fs::read_dir(bin_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n != file && n.starts_with(&format!("{name}-")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(dest)
}

/// The helper bundle inside Lite's (Contents/Helpers): both binaries, the app
/// icon and its own name, so macOS lists the sessions as "SplitGrid Sessions"
/// (background items, Activity Monitor) rather than as Lite.
pub const HELPER_APP: &str = "SplitGrid Sessions.app";

fn bundled_helper_app() -> Option<PathBuf> {
    let contents = std::env::current_exe().ok()?.parent()?.parent()?.to_path_buf();
    Some(contents.join("Helpers").join(HELPER_APP)).filter(|p| p.join("Contents/MacOS").join(sg_ptyd::DAEMON_BIN).is_file())
}

/// Copy the helper bundle to `bin_dir/SplitGrid Sessions-<version>-<hash>.app`
/// (ditto keeps the signature; temp + rename), and remove older copies —
/// running sessions keep their mapped binaries.
fn install_app(src: &Path, bin_dir: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    for part in ["Contents/Info.plist", "Contents/MacOS/splitgrid-ptyd", "Contents/MacOS/splitgrid-sessions"] {
        h.update(std::fs::read(src.join(part)).unwrap_or_default());
    }
    let hash: String = h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    let name = format!("SplitGrid Sessions-{}-{hash}.app", sg_ptyd::VERSION);
    let dest = bin_dir.join(&name);
    if !dest.is_dir() {
        let tmp = bin_dir.join(format!(".{name}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let ok = std::process::Command::new("/usr/bin/ditto").arg(src).arg(&tmp).status()?.success();
        if !ok {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(io::Error::other("copying the helper bundle failed"));
        }
        std::fs::rename(&tmp, &dest)?;
    }
    if let Ok(entries) = std::fs::read_dir(bin_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let old_app = n != name && n.starts_with("SplitGrid Sessions-") && n.ends_with(".app");
            // Flat copies from before the helper bundle.
            let old_flat = n.starts_with(&format!("{}-", sg_ptyd::DAEMON_BIN)) || n.starts_with(&format!("{}-", sg_ptyd::MENU_BIN));
            if old_app {
                let _ = std::fs::remove_dir_all(e.path());
            } else if old_flat {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(dest)
}

/// The installed binaries, installing them on first use in this run.
pub fn installed() -> io::Result<Installed> {
    if let Some(i) = INSTALLED.lock().unwrap().clone() {
        return Ok(i);
    }
    let data = data_dir().ok_or_else(|| io::Error::other("no data dir"))?;
    let bin_dir = paths::bin_dir(data);
    paths::ensure_private_dir(&paths::root(data))?;
    std::fs::create_dir_all(&bin_dir)?;
    let (daemon, menu) = if let Some(app) = bundled_helper_app() {
        let dest = install_app(&app, &bin_dir)?.join("Contents/MacOS");
        (dest.join(sg_ptyd::DAEMON_BIN), Some(dest.join(sg_ptyd::MENU_BIN)))
    } else {
        // A dev build: the bare binaries from cargo's target dir.
        let src = bundled(sg_ptyd::DAEMON_BIN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "splitgrid-ptyd is not in the app bundle (dev: cargo build -p sg-ptyd)"))?;
        let daemon = install_one(sg_ptyd::DAEMON_BIN, &src, &bin_dir)?;
        (daemon, bundled(sg_ptyd::MENU_BIN).and_then(|src| install_one(sg_ptyd::MENU_BIN, &src, &bin_dir).ok()))
    };
    let i = Installed { daemon, menu };
    *INSTALLED.lock().unwrap() = Some(i.clone());
    Ok(i)
}

/// Install in the background (the option was just turned on).
pub fn install_in_background() {
    std::thread::spawn(|| {
        if let Err(e) = installed() {
            log::error!("ptyd: installing: {e}");
        }
    });
}

/// Start the menu-bar helper when it should show (it keeps a single instance
/// itself and quits once no session is left).
pub fn ensure_menu() {
    if !menu_icon_enabled() {
        return;
    }
    let Some(data) = data_dir() else { return };
    if paths::list_sockets(&paths::sessions_dir(data)).is_empty() {
        return;
    }
    let Some(menu) = installed().ok().and_then(|i| i.menu) else {
        return;
    };
    // Its own launchd job, so it stays after Lite quits (sg_ptyd::launchd).
    use sg_ptyd::launchd;
    let label = launchd::MENU_LABEL;
    let jobs = launchd::jobs_dir(data);
    let args = vec![menu.to_string_lossy().into_owned(), "--data-dir".into(), data.to_string_lossy().into_owned()];
    let plist = launchd::plist(label, &args, Path::new("/"));
    let current = std::fs::read_to_string(jobs.join(format!("{label}.plist"))).is_ok_and(|p| p == plist);
    // Same helper already loaded: start it if it isn't running. Otherwise
    // (first time, or a new helper version) load it afresh.
    if current && launchd::loaded(label) && launchd::kickstart(label) {
        return;
    }
    if let Err(e) = launchd::submit(&jobs, label, &args, Path::new("/")) {
        log::warn!("ptyd: starting the menu helper: {e}");
    }
}

/// At launch: install when the option is on, and bring up the menu helper
/// when sessions are running. Off the UI thread.
pub fn startup() {
    if enabled()
        && let Err(e) = installed()
    {
        log::error!("ptyd: installing: {e}");
    }
    ensure_menu();
}

/// Lite's agent token, stable across runs while sessions can outlive one: a
/// re-attached shell keeps the `SPLITGRID_*_TOKEN` it was started with. In
/// `<data dir>/ptyd/agent-token`, 0600. `None`: use a fresh one.
pub fn stable_agent_token(data_dir: &Path, fresh: impl FnOnce() -> String) -> Option<String> {
    let file = paths::root(data_dir).join("agent-token");
    let sessions = !paths::list_sockets(&paths::sessions_dir(data_dir)).is_empty();
    if setting(data_dir, SETTING) != Some(true) && !(sessions && file.is_file()) {
        return None;
    }
    if let Ok(t) = std::fs::read_to_string(&file) {
        let t = t.trim().to_owned();
        if t.len() >= 16 {
            return Some(t);
        }
    }
    let token = fresh();
    write_private(&file, &token).ok()?;
    Some(token)
}

/// Make `token` the stable one (a session was just started with it).
fn adopt_agent_token(data_dir: &Path, token: &str) {
    let file = paths::root(data_dir).join("agent-token");
    if std::fs::read_to_string(&file).is_ok_and(|t| t.trim() == token) {
        return;
    }
    if let Err(e) = write_private(&file, token) {
        log::warn!("ptyd: saving the agent token: {e}");
    }
}

fn write_private(file: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = file.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, file)
}

// ── Status (Settings) ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The installed daemon's file name (version + hash).
    pub installed: Option<String>,
    pub live: usize,
    pub attached: usize,
    /// Sessions whose container is in no saved environment (sockets).
    pub orphans: Vec<PathBuf>,
    /// Sessions this Lite can't talk to (another protocol).
    pub incompatible: usize,
    pub checked: Option<Instant>,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);
static REFRESHING: AtomicBool = AtomicBool::new(false);

pub fn status() -> Status {
    STATUS.lock().unwrap().clone().unwrap_or_default()
}

/// Sessions whose container id isn't in `known` and that are older than a
/// grace period (a pane just made in another window may not be saved yet).
fn compute_status(known: Option<HashSet<String>>) -> Status {
    let mut st = Status { checked: Some(Instant::now()), ..Default::default() };
    st.installed = INSTALLED.lock().unwrap().as_ref().and_then(|i| i.daemon.file_name()).map(|n| n.to_string_lossy().into_owned());
    let Some(dir) = sessions_dir() else { return st };
    let now_ms = crate::model::workspace::now_ms().max(0) as u64;
    for found in client::scan(&dir, TIMEOUT) {
        st.live += 1;
        match found.info {
            Ok(info) => {
                if info.attached {
                    st.attached += 1;
                }
                let old = now_ms.saturating_sub(info.created_ms) > 30_000;
                if let Some(known) = &known
                    && old
                    && !info.meta.container_id.is_empty()
                    && !known.contains(&info.meta.container_id)
                {
                    st.orphans.push(found.socket);
                }
            }
            Err(_) => st.incompatible += 1,
        }
    }
    st
}

/// End every live session (Settings → End all sessions).
pub fn end_all() {
    let Some(dir) = sessions_dir() else { return };
    std::thread::spawn(move || {
        for socket in paths::list_sockets(&dir) {
            let _ = client::terminate(&socket, Duration::from_secs(3));
        }
    });
}

/// End the given sessions (Settings → End orphaned).
pub fn end_sockets(sockets: Vec<PathBuf>) {
    std::thread::spawn(move || {
        for socket in sockets {
            let _ = client::terminate(&socket, Duration::from_secs(3));
        }
    });
}

impl AppView {
    /// What the daemon should call container `id` (its env, workspace and title).
    pub(crate) fn ptyd_meta(&self, id: &str) -> Meta {
        self.ptyd_meta_known(id).unwrap_or_else(|| Meta { container_id: id.to_owned(), ..Default::default() })
    }

    /// The same, only for a container of this window's current environment.
    fn ptyd_meta_known(&self, id: &str) -> Option<Meta> {
        self.state.workspaces.iter().any(|w| w.container(id).is_some()).then(|| self.ptyd_meta_unchecked(id))
    }

    fn ptyd_meta_unchecked(&self, id: &str) -> Meta {
        let env_name = self.envs.iter().find(|e| e.id == self.env_id).map(|e| e.name.clone()).unwrap_or_else(|| self.env_id.clone());
        let mut meta = Meta { container_id: id.to_owned(), env_id: self.env_id.clone(), env_name, ..Default::default() };
        if let Some(ws) = self.state.workspaces.iter().find(|w| w.container(id).is_some()) {
            meta.workspace_id = ws.id.clone();
            meta.workspace_name = ws.name.clone();
            if let Some(c) = ws.container(id) {
                meta.title = c.content.custom_name.clone().or_else(|| c.content.label.clone()).unwrap_or_else(|| "Terminal".into());
            }
        }
        meta
    }

    /// Every second (the process poll): keep the daemons' metadata current
    /// (renames), and the Settings status fresh while Settings is open.
    pub(crate) fn ptyd_poll(&mut self, cx: &mut Context<Self>) {
        for (id, pane) in &self.panes {
            // Panes of an environment this window switched away from stay
            // alive; their names are only known while it is shown.
            if let Some(session) = pane.view.read(cx).daemon_session()
                && let Some(meta) = self.ptyd_meta_known(id)
            {
                session.set_meta(meta);
            }
        }
        if self.settings_tab.is_none() || REFRESHING.swap(true, Ordering::Relaxed) {
            return;
        }
        let fresh = STATUS.lock().unwrap().as_ref().and_then(|s| s.checked).is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        if fresh {
            REFRESHING.store(false, Ordering::Relaxed);
            return;
        }
        // Every container any environment has (saved), plus this window's unsaved ones.
        let store = self.store.clone();
        let mine: Vec<String> = self.state.workspaces.iter().flat_map(|w| w.containers.iter().map(|c| c.id.clone())).collect();
        cx.spawn(async move |this, cx| {
            let st = cx
                .background_executor()
                .spawn(async move {
                    let mut known: Option<HashSet<String>> = Some(mine.into_iter().collect());
                    for env in store.list_envs() {
                        match store.load(&env.id) {
                            Ok(state) => {
                                if let Some(k) = known.as_mut() {
                                    k.extend(state.workspaces.iter().flat_map(|w| w.containers.iter().map(|c| c.id.clone())));
                                }
                            }
                            // Unreadable state: don't call anything an orphan.
                            Err(_) => known = None,
                        }
                    }
                    compute_status(known)
                })
                .await;
            *STATUS.lock().unwrap() = Some(st);
            REFRESHING.store(false, Ordering::Relaxed);
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_versioned_atomic_and_prunes_old_copies() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src-bin");
        std::fs::write(&src, b"v1").unwrap();
        let bin = t.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let a = install_one("splitgrid-ptyd", &src, &bin).unwrap();
        assert!(a.file_name().unwrap().to_string_lossy().starts_with(&format!("splitgrid-ptyd-{}-", sg_ptyd::VERSION)));
        // Same content: same file.
        assert_eq!(install_one("splitgrid-ptyd", &src, &bin).unwrap(), a);
        // New content: a new file, the old one removed, nothing else touched.
        std::fs::write(bin.join("splitgrid-sessions-x"), b"menu").unwrap();
        std::fs::write(&src, b"v2").unwrap();
        let b = install_one("splitgrid-ptyd", &src, &bin).unwrap();
        assert_ne!(a, b);
        assert!(!a.exists() && b.exists());
        assert!(bin.join("splitgrid-sessions-x").exists());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn agent_token_is_stable_only_when_sessions_can_outlive_a_run() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        assert_eq!(stable_agent_token(dir, || "x".repeat(32)), None);
        std::fs::write(dir.join("app-settings.json"), br#"{"persistentTerminals":true}"#).unwrap();
        let a = stable_agent_token(dir, || "a".repeat(32)).unwrap();
        let b = stable_agent_token(dir, || "b".repeat(32)).unwrap();
        assert_eq!(a, b);
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(paths::root(dir).join("agent-token")).unwrap().permissions().mode() & 0o777, 0o600);
        // Turned on mid-run: the first session adopts that run's (random) token,
        // and the next run, finding it, keeps it.
        let t2 = tempfile::tempdir().unwrap();
        let dir2 = t2.path();
        assert_eq!(stable_agent_token(dir2, || "r".repeat(32)), None);
        std::fs::write(dir2.join("app-settings.json"), br#"{"persistentTerminals":true}"#).unwrap();
        adopt_agent_token(dir2, &"r".repeat(32));
        assert_eq!(stable_agent_token(dir2, || "n".repeat(32)), Some("r".repeat(32)));
        // Turned off with no sessions left: a fresh token per run again.
        std::fs::write(dir.join("app-settings.json"), br#"{"persistentTerminals":false}"#).unwrap();
        assert_eq!(stable_agent_token(dir, || "c".repeat(32)), None);
    }

    /// A fake daemon on a real socket: Lite's attach parses the replay at the
    /// session's size, answers no query from it, then streams, sends input,
    /// and detaches (not kills) when the terminal goes away.
    #[test]
    fn attach_replays_at_the_session_size_and_detaches_on_drop() {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        use std::io::{Read as _, Write as _};
        let t = tempfile::Builder::new().prefix("lite-ptyd").tempdir_in("/tmp").unwrap();
        let sock = t.path().join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut dec = proto::Decoder::default();
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                while let Some((k, p)) = dec.next_frame().unwrap() {
                    match k {
                        kind::HELLO => s.write_all(&proto::hello_ok(1, 4242)).unwrap(),
                        kind::ATTACH => {
                            let size = Size { cols: 100, rows: 30, cell_width: 8, cell_height: 17 };
                            s.write_all(&proto::attached(size, 4242)).unwrap();
                            // A DA query from long ago, then text.
                            s.write_all(&proto::frame(kind::OUTPUT, b"\x1b[chello replay\r\n")).unwrap();
                            s.write_all(&proto::frame(kind::REPLAY_DONE, b"")).unwrap();
                            s.write_all(&proto::frame(kind::OUTPUT, b"live\r\n")).unwrap();
                        }
                        _ => seen.push((k, p)),
                    }
                }
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => return seen,
                    Ok(n) => dec.push(&buf[..n]),
                }
            }
        });

        let conn = Conn::connect(&sock, TIMEOUT).unwrap();
        let (terminal, mut events) = Terminal::attach_daemon(conn, sock.clone(), (8., 17.)).unwrap();
        let text = || {
            let term = terminal.term.lock();
            (0..term.screen_lines() as i32)
                .map(|l| (0..term.columns()).map(|c| term.grid()[Line(l)][Column(c)].c).collect::<String>().trim_end().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !text().contains("live") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(text().starts_with("hello replay\nlive"), "{:?}", text());
        {
            let term = terminal.term.lock();
            assert_eq!((term.columns(), term.screen_lines()), (100, 30));
        }
        assert_eq!(terminal.session_leader(), Some(4242));
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let TermEvent::Term(e) = ev {
                kinds.push(format!("{e:?}"));
            }
        }
        assert!(!kinds.iter().any(|k| k.starts_with("PtyWrite")), "{kinds:?}");
        terminal.write(b"ls\r".to_vec());
        drop(terminal);
        let seen = server.join().unwrap();
        assert_eq!(seen.first(), Some(&(kind::INPUT, b"ls\r".to_vec())));
        assert_eq!(seen.last().map(|f| f.0), Some(kind::DETACH));
        assert!(!seen.iter().any(|f| f.0 == kind::KILL || f.0 == kind::TERMINATE));
    }

    #[test]
    fn replay_mutes_replies_and_side_effects() {
        use alacritty_terminal::event::EventListener as _;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let flag = Arc::new(AtomicBool::new(true));
        let l = crate::terminal::Listener::for_test(tx, Some(flag.clone()));
        l.send_event(Event::PtyWrite("\x1b[?6c".into()));
        l.send_event(Event::Bell);
        l.send_event(Event::Title("vim".into()));
        flag.store(false, Ordering::Relaxed);
        l.send_event(Event::PtyWrite("\x1b[?6c".into()));
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let TermEvent::Term(e) = ev {
                got.push(format!("{e:?}"));
            }
        }
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("Title"));
        assert!(got[1].starts_with("PtyWrite"));
    }
}
