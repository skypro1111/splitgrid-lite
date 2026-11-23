//! The session daemon: `splitgrid-ptyd serve` with a [`SpawnSpec`] (JSON) on
//! stdin. The env (it carries Lite's agent tokens) never goes on argv.
//!
//! Start-up: close inherited fds, read the spec, fork. The child calls
//! setsid, binds the socket, spawns the shell on a new PTY (alacritty's
//! `tty::new`, exactly as Lite's in-process terminals do), reports
//! "ok <shell pid> <daemon pid>" through a pipe, and serves. The parent prints that line and exits, so the
//! launcher's wait doubles as the readiness check and the daemon (reparented
//! to launchd) never becomes the launcher's zombie.
//!
//! Serving is one nonblocking select loop over the listener, the PTY master and
//! the connections. The PTY is read whenever it has output, whoever is
//! connected: output goes to the ring and is queued to the attached client;
//! a client that stops reading is dropped (it can re-attach and get the
//! replay), never allowed to stall the shell.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{OnResize as _, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty as _, Pty, Shell};
use serde::{Deserialize, Serialize};

use crate::meta::{Meta, SessionInfo};
use crate::proto::{self, Decoder, Size, kind};
use crate::ring::Ring;
use crate::sys;

/// What Lite asks a new daemon for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpawnSpec {
    pub socket: PathBuf,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    /// Added to the inherited environment (as `tty::Options::env`).
    pub env: BTreeMap<String, String>,
    /// argv of the program to run instead of the login shell (tests).
    pub shell: Option<Vec<String>>,
    pub meta: Meta,
    /// Ring size; `DEFAULT_RING_BYTES` when unset.
    pub ring_bytes: Option<usize>,
    /// Debug builds only: accept peers of this uid instead of our own (tests
    /// of the peer check).
    pub expect_uid: Option<u32>,
}

/// Pending keyboard input beyond this stops reading clients until the shell catches up.
const PTY_IN_CAP: usize = 1024 * 1024;
/// A client with more unsent output than this is dropped.
const CLIENT_OUT_CAP: usize = 32 * 1024 * 1024;
/// How long a killed shell has between SIGHUP and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

pub fn main() -> ! {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("splitgrid-ptyd {} protocol {}", crate::VERSION, proto::PROTOCOL_VERSION);
            std::process::exit(0);
        }
        Some("serve") => {}
        Some("job") => job_main(&args[2..]),
        _ => {
            eprintln!("usage: splitgrid-ptyd serve < spec.json   |   splitgrid-ptyd --version");
            std::process::exit(2);
        }
    }
    sys::close_inherited_fds();
    let mut input = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut input) {
        eprintln!("err reading the spec: {e}");
        std::process::exit(2);
    }
    let spec: SpawnSpec = match serde_json::from_slice(&input) {
        Ok(s) => s,
        Err(e) => {
            println!("err bad spec: {e}");
            std::process::exit(2);
        }
    };
    launch(spec)
}

/// `splitgrid-ptyd job --spec-file F --ready-file R --label L`: run as a
/// launchd job (see crate::launchd), in the foreground. The spec comes from a
/// 0600 file Lite wrote (deleted at once); "ok …" / "err …" goes to the ready
/// file, which Lite polls. No fork: the job's process is the daemon.
fn job_main(args: &[String]) -> ! {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let (Some(spec_file), Some(ready), Some(label)) = (flag("--spec-file"), flag("--ready-file"), flag("--label")) else {
        eprintln!("usage: splitgrid-ptyd job --spec-file F --ready-file R --label L");
        std::process::exit(2);
    };
    let label = label.to_string_lossy().into_owned();
    sys::close_inherited_fds();
    let report = |line: String| {
        let tmp = ready.with_extension("tmp");
        if std::fs::write(&tmp, line).is_ok() {
            let _ = std::fs::rename(&tmp, &ready);
        }
    };
    let spec = std::fs::read(&spec_file).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<SpawnSpec>(&b).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(&spec_file);
    let started = spec.and_then(|s| Daemon::start(s).map_err(|e| e.to_string()));
    match started {
        Ok(mut d) => {
            report(format!("ok {} {}\n", d.child_pid, std::process::id()));
            d.job_label = Some(label);
            d.run()
        }
        Err(e) => {
            report(format!("err {e}\n"));
            crate::launchd::remove_detached(&label);
            std::process::exit(1);
        }
    }
}

fn launch(spec: SpawnSpec) -> ! {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe/fork/setsid/dup2 at start-up, single-threaded.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            println!("err pipe: {}", io::Error::last_os_error());
            std::process::exit(1);
        }
        match libc::fork() {
            -1 => {
                println!("err fork: {}", io::Error::last_os_error());
                std::process::exit(1);
            }
            0 => {
                libc::close(fds[0]);
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    for fd in 0..3 {
                        libc::dup2(null, fd);
                    }
                    if null > 2 {
                        libc::close(null);
                    }
                }
                libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
                let mut ready = std::fs::File::from_raw_fd(fds[1]);
                match Daemon::start(spec) {
                    Ok(d) => {
                        let _ = writeln!(ready, "ok {} {}", d.child_pid, std::process::id());
                        drop(ready);
                        d.run()
                    }
                    Err(e) => {
                        let _ = writeln!(ready, "err {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                libc::close(fds[1]);
                let mut line = String::new();
                let _ = std::fs::File::from_raw_fd(fds[0]).read_to_string(&mut line);
                let line = if line.trim().is_empty() { "err the daemon exited before it was ready\n".to_owned() } else { line };
                print!("{line}");
                let _ = io::stdout().flush();
                std::process::exit(if line.starts_with("ok ") { 0 } else { 1 });
            }
        }
    }
}

struct Conn {
    s: UnixStream,
    dec: Decoder,
    out: VecDeque<u8>,
    /// Bytes written / queued, ever (absolute stream offsets).
    written: u64,
    queued: u64,
    /// The PTY master rides with the byte at this offset.
    fd_at: Option<u64>,
    version: Option<u16>,
    attached: bool,
    /// Close once `out` is flushed.
    closing: bool,
    dead: bool,
}

impl Conn {
    fn new(s: UnixStream) -> Self {
        Self {
            s,
            dec: Decoder::default(),
            out: VecDeque::new(),
            written: 0,
            queued: 0,
            fd_at: None,
            version: None,
            attached: false,
            closing: false,
            dead: false,
        }
    }

    fn queue(&mut self, bytes: &[u8]) {
        self.out.extend(bytes);
        self.queued += bytes.len() as u64;
        if self.out.len() > CLIENT_OUT_CAP {
            self.dead = true;
        }
    }

    fn queue_with_fd(&mut self, bytes: &[u8]) {
        self.fd_at = Some(self.queued);
        self.queue(bytes);
    }

    fn flush(&mut self, master: RawFd) {
        while !self.out.is_empty() && !self.dead {
            let buf = self.out.make_contiguous();
            let res = match self.fd_at {
                Some(at) if at == self.written => sys::send_with_fd(self.s.as_raw_fd(), buf, master).inspect(|_| self.fd_at = None),
                Some(at) if at > self.written => (&self.s).write(&buf[..((at - self.written) as usize).min(buf.len())]),
                _ => (&self.s).write(buf),
            };
            match res {
                Ok(0) => self.dead = true,
                Ok(n) => {
                    self.out.drain(..n);
                    self.written += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => self.dead = true,
            }
        }
        if self.out.is_empty() && self.closing {
            self.dead = true;
        }
    }
}

pub struct Daemon {
    socket: PathBuf,
    socket_ino: u64,
    listener: UnixListener,
    pty: Pty,
    master: RawFd,
    child_pid: i32,
    ring: Ring,
    conns: Vec<Conn>,
    to_pty: Vec<u8>,
    size: Size,
    meta: Meta,
    created_ms: u64,
    peer_uid: u32,
    pty_eof: bool,
    kill_deadline: Option<Instant>,
    killed_hard: bool,
    /// Set when running as a launchd job: unloaded as the daemon exits.
    job_label: Option<String>,
}

impl Daemon {
    pub fn start(spec: SpawnSpec) -> io::Result<Self> {
        let socket = spec.socket.clone();
        crate::paths::check_len(&socket)?;
        if let Some(dir) = socket.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        if socket.exists() {
            if UnixStream::connect(&socket).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "a session is already listening on this socket"));
            }
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let socket_ino = std::fs::metadata(&socket)?.ino();

        let size = Size { cols: spec.cols.max(2), rows: spec.rows.max(1), cell_width: spec.cell_width, cell_height: spec.cell_height };
        let options = tty::Options {
            shell: spec.shell.as_ref().and_then(|argv| Some(Shell::new(argv.first()?.clone(), argv[1..].to_vec()))),
            working_directory: spec.cwd.clone().filter(|p| p.is_dir()).or_else(std::env::home_dir),
            drain_on_exit: true,
            env: spec.env.clone().into_iter().collect::<HashMap<_, _>>(),
        };
        let ws = WindowSize { num_lines: size.rows, num_cols: size.cols, cell_width: size.cell_width, cell_height: size.cell_height };
        let pty = match tty::new(&options, ws, 0) {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_file(&socket);
                return Err(e);
            }
        };
        let master = pty.file().as_raw_fd();
        let child_pid = pty.child().id() as i32;
        #[cfg(debug_assertions)]
        let peer_uid = spec.expect_uid.unwrap_or_else(sys::euid);
        #[cfg(not(debug_assertions))]
        let peer_uid = sys::euid();
        Ok(Self {
            socket,
            socket_ino,
            listener,
            pty,
            master,
            child_pid,
            ring: Ring::new(spec.ring_bytes.unwrap_or(crate::DEFAULT_RING_BYTES)),
            conns: Vec::new(),
            to_pty: Vec::new(),
            size,
            meta: spec.meta,
            created_ms: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            peer_uid,
            pty_eof: false,
            kill_deadline: None,
            killed_hard: false,
            job_label: None,
        })
    }

    pub fn run(mut self) -> ! {
        loop {
            let mut pfds = vec![
                libc::pollfd { fd: self.listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd {
                    fd: if self.pty_eof { -1 } else { self.master },
                    events: libc::POLLIN | if self.to_pty.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
            ];
            let reading = self.to_pty.len() < PTY_IN_CAP;
            for c in &self.conns {
                let mut ev = 0;
                if reading && !c.closing {
                    ev |= libc::POLLIN;
                }
                if !c.out.is_empty() {
                    ev |= libc::POLLOUT;
                }
                pfds.push(libc::pollfd { fd: c.s.as_raw_fd(), events: ev, revents: 0 });
            }
            let timeout = if self.kill_deadline.is_some() || self.pty_eof { 50 } else { 250 };
            // select(2) rather than poll(2): macOS has long documented poll as
            // unsupported on devices, and the PTY master is one.
            sys::select_poll(&mut pfds, timeout);

            if pfds[0].revents != 0 {
                self.accept();
            }
            if pfds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                self.read_pty();
            }
            if pfds[1].revents & libc::POLLOUT != 0 || !self.to_pty.is_empty() {
                self.write_pty();
            }
            let n = self.conns.len().min(pfds.len() - 2);
            for i in 0..n {
                let re = pfds[i + 2].revents;
                if re & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read_conn(i);
                }
            }
            for c in &mut self.conns {
                c.flush(self.master);
            }
            self.conns.retain(|c| !c.dead);

            // The shell exited?
            // (One SIGCHLD per call; a pending one is picked up on the next pass.)
            if let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                let code = status.map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))).unwrap_or(-1);
                self.finish(code);
            }
            if let Some(deadline) = self.kill_deadline
                && Instant::now() >= deadline
                && !self.killed_hard
            {
                self.killed_hard = true;
                // SAFETY: our own unreaped child.
                unsafe { libc::kill(self.child_pid, libc::SIGKILL) };
            }
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    let ok = sys::peer_uid(s.as_raw_fd()).is_ok_and(|u| u == self.peer_uid);
                    if !ok {
                        continue; // dropped: closes the connection
                    }
                    let _ = s.set_nonblocking(true);
                    self.conns.push(Conn::new(s));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    fn read_pty(&mut self) {
        let mut buf = vec![0u8; 64 * 1024];
        // Bounded per wakeup so clients get served in between.
        for _ in 0..16 {
            match self.pty.file().read(&mut buf) {
                Ok(0) => {
                    self.pty_eof = true;
                    break;
                }
                Ok(n) => self.on_output(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // EIO: the slave side is gone (the shell exited).
                Err(_) => {
                    self.pty_eof = true;
                    break;
                }
            }
        }
    }

    fn on_output(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
        for c in self.conns.iter_mut().filter(|c| c.attached && !c.closing) {
            for chunk in bytes.chunks(proto::CHUNK) {
                c.queue(&proto::frame(kind::OUTPUT, chunk));
            }
        }
    }

    fn write_pty(&mut self) {
        while !self.to_pty.is_empty() {
            match self.pty.file().write(&self.to_pty) {
                Ok(0) => break,
                Ok(n) => {
                    self.to_pty.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.to_pty.clear();
                    break;
                }
            }
        }
    }

    fn read_conn(&mut self, i: usize) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let c = &mut self.conns[i];
            if c.dead || c.closing {
                return;
            }
            match (&c.s).read(&mut buf) {
                Ok(0) => {
                    c.dead = true;
                    return;
                }
                Ok(n) => {
                    c.dec.push(&buf[..n]);
                    loop {
                        let next = self.conns[i].dec.next_frame();
                        match next {
                            Ok(Some((k, p))) => self.handle(i, k, &p),
                            Ok(None) => break,
                            Err(_) => {
                                self.conns[i].dead = true;
                                return;
                            }
                        }
                    }
                    if self.to_pty.len() >= PTY_IN_CAP {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.conns[i].dead = true;
                    return;
                }
            }
        }
    }

    fn handle(&mut self, i: usize, k: u8, p: &[u8]) {
        if k == kind::TERMINATE {
            self.begin_kill();
            return;
        }
        if self.conns[i].version.is_none() {
            let c = &mut self.conns[i];
            if k != kind::HELLO {
                c.queue(&proto::error("say hello first"));
                c.closing = true;
                return;
            }
            let theirs = match proto::parse_hello(p) {
                Ok(v) => v,
                Err(e) => {
                    c.queue(&proto::error(&e.to_string()));
                    c.closing = true;
                    return;
                }
            };
            match proto::negotiate((proto::MIN_PROTOCOL_VERSION, proto::PROTOCOL_VERSION), theirs) {
                Some(v) => {
                    c.version = Some(v);
                    c.queue(&proto::hello_ok(v, self.child_pid));
                }
                None => {
                    let msg = format!(
                        "incompatible protocol: splitgrid-ptyd {} speaks {}..={}, the client {}..={}",
                        crate::VERSION,
                        proto::MIN_PROTOCOL_VERSION,
                        proto::PROTOCOL_VERSION,
                        theirs.0,
                        theirs.1
                    );
                    c.queue(&proto::error(&msg));
                    c.closing = true;
                }
            }
            return;
        }
        match k {
            kind::ATTACH => self.attach(i),
            kind::INPUT => self.to_pty.extend_from_slice(p),
            kind::RESIZE => {
                if let Ok(size) = Size::decode(p)
                    && size.cols > 0
                    && size.rows > 0
                {
                    self.resize(size);
                }
            }
            kind::SIGNAL => {
                if p.len() >= 5 {
                    let signo = i32::from_le_bytes(p[1..5].try_into().unwrap());
                    let target = if p[0] == proto::SIGNAL_FOREGROUND { -sys::foreground_pgrp(self.master) } else { self.child_pid };
                    if target != 0 && (1..32).contains(&signo) {
                        // SAFETY: a signal to our shell or its foreground group.
                        unsafe { libc::kill(target, signo) };
                    }
                }
            }
            kind::INFO => {
                let info = self.info(self.conns[i].version.unwrap_or(0));
                let json = serde_json::to_vec(&info).unwrap_or_default();
                self.conns[i].queue(&proto::frame(kind::INFO_REPLY, &json));
            }
            kind::DETACH => {
                let c = &mut self.conns[i];
                c.attached = false;
                c.closing = true;
            }
            kind::KILL => self.begin_kill(),
            kind::SET_META => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(p) {
                    self.meta.merge(&v);
                    self.conns[i].queue(&proto::frame(kind::OK, &[]));
                } else {
                    self.conns[i].queue(&proto::error("SET_META: not JSON"));
                }
            }
            kind::SNAPSHOT => {
                let max = p.get(..4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).unwrap_or(u32::MAX);
                let bytes = self.ring.replay(Some(max as usize));
                let frame = proto::snapshot_reply(self.size, &bytes);
                self.conns[i].queue(&frame);
            }
            other => self.conns[i].queue(&proto::error(&format!("unknown op 0x{other:02x}"))),
        }
    }

    fn attach(&mut self, i: usize) {
        for (j, c) in self.conns.iter_mut().enumerate() {
            if j != i && c.attached {
                c.attached = false;
                c.queue(&proto::frame(kind::DETACHED, b"attached elsewhere"));
                c.closing = true;
            }
        }
        let replay = self.ring.replay(None);
        let c = &mut self.conns[i];
        c.queue_with_fd(&proto::attached(self.size, self.child_pid));
        for chunk in replay.chunks(proto::CHUNK) {
            c.queue(&proto::frame(kind::OUTPUT, chunk));
        }
        c.queue(&proto::frame(kind::REPLAY_DONE, &[]));
        c.attached = true;
        // A full-screen program repaints on SIGWINCH; TIOCSWINSZ with an
        // unchanged size sends none, so ask for it directly.
        let pg = sys::foreground_pgrp(self.master);
        if pg > 0 {
            // SAFETY: the PTY's own foreground group.
            unsafe { libc::kill(-pg, libc::SIGWINCH) };
        }
    }

    fn resize(&mut self, size: Size) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.pty.on_resize(WindowSize { num_lines: size.rows, num_cols: size.cols, cell_width: size.cell_width, cell_height: size.cell_height });
    }

    fn info(&self, protocol: u16) -> SessionInfo {
        let foreground = sys::foreground(self.master);
        let cwd = foreground.as_ref().and_then(|f| sys::proc_cwd(f.pid)).or_else(|| sys::proc_cwd(self.child_pid));
        SessionInfo {
            protocol,
            daemon_version: crate::VERSION.to_owned(),
            daemon_pid: std::process::id() as i32,
            pid: self.child_pid,
            cols: self.size.cols,
            rows: self.size.rows,
            attached: self.conns.iter().any(|c| c.attached && !c.closing),
            created_ms: self.created_ms,
            cwd,
            foreground,
            meta: self.meta.clone(),
        }
    }

    fn begin_kill(&mut self) {
        if self.kill_deadline.is_none() {
            self.kill_deadline = Some(Instant::now() + KILL_GRACE);
            // SAFETY: our own unreaped child (as alacritty's Pty drop does).
            unsafe { libc::kill(self.child_pid, libc::SIGHUP) };
        }
    }

    /// The shell exited: drain its last output, tell every client, go.
    fn finish(&mut self, code: i32) -> ! {
        if !self.pty_eof {
            self.read_pty();
        }
        for c in self.conns.iter_mut().filter(|c| c.version.is_some()) {
            c.queue(&proto::exit(code));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        for c in &mut self.conns {
            let _ = c.s.set_nonblocking(false);
            let _ = c.s.set_write_timeout(Some(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(10))));
            c.flush(self.master);
        }
        // Remove the socket unless another daemon has replaced it since.
        if std::fs::metadata(&self.socket).is_ok_and(|m| m.ino() == self.socket_ino) {
            let _ = std::fs::remove_file(&self.socket);
        }
        // A launchd job unloads itself (and its plist goes) so no label lingers.
        if let Some(label) = &self.job_label {
            if let Some(root) = self.socket.parent().and_then(Path::parent) {
                let _ = std::fs::remove_file(root.join("jobs").join(format!("{label}.plist")));
            }
            crate::launchd::remove_detached(label);
        }
        // Exit without dropping the Pty: its Drop would SIGHUP the (reaped) pid.
        std::process::exit(0);
    }
}
