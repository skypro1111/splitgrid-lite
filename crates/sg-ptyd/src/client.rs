//! A blocking client for one session daemon.

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::meta::{Meta, SessionInfo};
use crate::proto::{self, Decoder, Size, kind};

/// The daemon answered the hello with an error (an incompatible protocol).
pub fn is_incompatible(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
}

/// No session there: no socket file, or nobody listening on it (stale).
pub fn is_gone(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

pub struct Conn {
    stream: UnixStream,
    dec: Decoder,
    fds: Vec<OwnedFd>,
    /// The agreed protocol version.
    pub version: u16,
    /// The shell's pid.
    pub pid: i32,
}

/// What `attach` returns.
pub struct Attached {
    pub size: Size,
    pub pid: i32,
    /// A dup of the PTY master: for tcgetpgrp/tcgetsid only, never read.
    pub master: Option<OwnedFd>,
}

/// Open a raw connection to `socket` and check the daemon runs as us.
fn open(socket: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    let uid = crate::sys::peer_uid(stream.as_raw_fd())?;
    if uid != crate::sys::euid() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("session socket owned by uid {uid}")));
    }
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

impl Conn {
    /// Connect and say hello with this build's protocol range.
    pub fn connect(socket: &Path, timeout: Duration) -> io::Result<Self> {
        Self::connect_with(socket, timeout, (proto::MIN_PROTOCOL_VERSION, proto::PROTOCOL_VERSION))
    }

    /// Connect speaking an explicit protocol range (tests: a "future" client).
    pub fn connect_with(socket: &Path, timeout: Duration, versions: (u16, u16)) -> io::Result<Self> {
        let mut stream = open(socket, timeout)?;
        stream.write_all(&proto::hello(versions.0, versions.1))?;
        let mut conn = Self { stream, dec: Decoder::default(), fds: Vec::new(), version: 0, pid: 0 };
        match conn.recv()? {
            Some((kind::HELLO_OK, p)) => {
                let (version, pid) = proto::parse_hello_ok(&p)?;
                conn.version = version;
                conn.pid = pid;
                Ok(conn)
            }
            Some((kind::ERROR, p)) => Err(io::Error::new(io::ErrorKind::Unsupported, String::from_utf8_lossy(&p).into_owned())),
            Some((k, _)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected frame 0x{k:02x} after hello"))),
            None => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "daemon closed the connection")),
        }
    }

    /// Clear the timeouts (a long-lived attached reader blocks forever).
    /// macOS refuses this (EINVAL) once the peer has closed.
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)?;
        self.stream.set_write_timeout(timeout)
    }

    /// Another handle on the socket, for a writer thread.
    pub fn writer(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }

    pub fn send(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.stream.write_all(&proto::frame(kind, payload))
    }

    /// The next frame; `None` at end of stream.
    pub fn recv(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        loop {
            if let Some(f) = self.dec.next_frame()? {
                return Ok(Some(f));
            }
            let mut buf = vec![0u8; 64 * 1024];
            let (n, fds) = crate::sys::recv_with_fds(self.stream.as_raw_fd(), &mut buf)?;
            self.fds.extend(fds);
            if n == 0 {
                return Ok(None);
            }
            self.dec.push(&buf[..n]);
        }
    }

    /// Every frame already buffered, without blocking for more.
    pub fn try_next_buffered(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        self.dec.next_frame()
    }

    /// Wait for a reply of `want`, skipping output that streams meanwhile.
    fn expect(&mut self, want: u8) -> io::Result<Vec<u8>> {
        loop {
            match self.recv()? {
                Some((k, p)) if k == want => return Ok(p),
                Some((kind::ERROR, p)) => {
                    return Err(io::Error::other(String::from_utf8_lossy(&p).into_owned()));
                }
                Some((kind::EXIT, _)) => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "session exited"));
                }
                Some(_) => continue,
                None => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the connection"));
                }
            }
        }
    }

    /// Become the session's client: its size and pid now; the replay and
    /// live output follow as frames (`OUTPUT`… `REPLAY_DONE`, `OUTPUT`…).
    pub fn attach(&mut self) -> io::Result<Attached> {
        self.send(kind::ATTACH, &[])?;
        let p = self.expect(kind::ATTACHED)?;
        let (size, pid) = proto::parse_attached(&p)?;
        let master = self.fds.pop();
        self.fds.clear();
        Ok(Attached { size, pid, master })
    }

    pub fn info(&mut self) -> io::Result<SessionInfo> {
        self.send(kind::INFO, &[])?;
        let p = self.expect(kind::INFO_REPLY)?;
        serde_json::from_slice(&p).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn set_meta(&mut self, meta: &Meta) -> io::Result<()> {
        let json = serde_json::to_vec(meta).map_err(io::Error::other)?;
        self.send(kind::SET_META, &json)?;
        self.expect(kind::OK).map(|_| ())
    }

    /// The session's size and up to `max` bytes of its recent output.
    pub fn snapshot(&mut self, max: u32) -> io::Result<(Size, Vec<u8>)> {
        self.send(kind::SNAPSHOT, &max.to_le_bytes())?;
        let p = self.expect(kind::SNAPSHOT_REPLY)?;
        proto::parse_snapshot_reply(&p)
    }

    pub fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(kind::INPUT, bytes)
    }

    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        self.send(kind::RESIZE, &size.encode())
    }

    pub fn signal(&mut self, target: u8, signo: i32) -> io::Result<()> {
        let mut p = vec![target];
        p.extend_from_slice(&signo.to_le_bytes());
        self.send(kind::SIGNAL, &p)
    }

    /// End the session: SIGHUP to the shell, SIGKILL if it lingers. Returns
    /// once the daemon reported the exit (or closed).
    pub fn kill(&mut self) -> io::Result<i32> {
        self.send(kind::KILL, &[])?;
        loop {
            match self.recv() {
                Ok(Some((kind::EXIT, p))) => return Ok(proto::parse_exit(&p)),
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(-1),
                Err(e) => return Err(e),
            }
        }
    }

    pub fn detach(&mut self) -> io::Result<()> {
        self.send(kind::DETACH, &[])
    }
}

/// End a session without a hello, whatever protocol it speaks (the frozen
/// TERMINATE frame). Gone sessions are fine.
pub fn terminate(socket: &Path, timeout: Duration) -> io::Result<()> {
    let mut stream = match open(socket, timeout) {
        Ok(s) => s,
        Err(e) if is_gone(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    stream.write_all(&proto::frame(kind::TERMINATE, &[]))?;
    // Wait (bounded by the timeout) for the daemon to finish and close.
    let mut sink = [0u8; 4096];
    loop {
        match io::Read::read(&mut stream, &mut sink) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => continue,
        }
    }
}

/// Interrupt the session's foreground job like Ctrl-C: SIGINT to the PTY's
/// foreground process group. Returns once the daemon has handled it (the
/// `INFO` round trip after the reply-less `SIGNAL` proves that).
pub fn interrupt(socket: &Path, timeout: Duration) -> io::Result<()> {
    let mut c = Conn::connect(socket, timeout)?;
    c.signal(proto::SIGNAL_FOREGROUND, libc::SIGINT)?;
    c.info().map(|_| ())
}

/// One session found in a sessions dir.
pub struct Found {
    pub socket: PathBuf,
    /// Its info, or why it couldn't be read (an incompatible daemon says so).
    pub info: Result<SessionInfo, String>,
}

/// Every live session in `sessions_dir`. Stale sockets (nobody listening)
/// are removed.
pub fn scan(sessions_dir: &Path, timeout: Duration) -> Vec<Found> {
    let mut out = Vec::new();
    for socket in crate::paths::list_sockets(sessions_dir) {
        match Conn::connect(&socket, timeout).and_then(|mut c| c.info()) {
            Ok(info) => out.push(Found { socket, info: Ok(info) }),
            Err(e) if is_gone(&e) => {
                let _ = std::fs::remove_file(&socket);
            }
            Err(e) => out.push(Found { socket, info: Err(e.to_string()) }),
        }
    }
    out
}
