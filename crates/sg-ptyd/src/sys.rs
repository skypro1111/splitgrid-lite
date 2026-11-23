//! The few syscalls the daemon and its clients need (macOS).

use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

/// The effective uid of the peer of a connected unix socket.
pub fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: plain syscall with out-params on the stack.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

pub fn euid() -> u32 {
    // SAFETY: no arguments, no failure.
    unsafe { libc::geteuid() }
}

/// Send `data` with `fd` attached (SCM_RIGHTS). Returns the bytes sent.
pub fn send_with_fd(sock: RawFd, data: &[u8], fd: RawFd) -> io::Result<usize> {
    // SAFETY: the control buffer is sized with CMSG_SPACE for one fd and
    // aligned for cmsghdr; every pointer stays inside the buffers below.
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        let n = libc::sendmsg(sock, &msg, 0);
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }
}

/// Receive into `buf`; any fds that came along are returned (owned).
/// `Ok((0, _))` is end of stream.
pub fn recv_with_fds(sock: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    // SAFETY: as in send_with_fd; received fds are taken into OwnedFd once each.
    unsafe {
        let space = libc::CMSG_SPACE((4 * std::mem::size_of::<RawFd>()) as u32) as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let n = libc::recvmsg(sock, &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let header = libc::CMSG_LEN(0) as usize;
                let count = ((*cmsg).cmsg_len as usize).saturating_sub(header) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok((n as usize, fds))
    }
}

/// poll(2)'s interface over select(2): poll(2) is documented as unsupported
/// on devices on macOS (the PTY master is one); select works on them. Fds of
/// -1 are skipped; fds past FD_SETSIZE get POLLNVAL.
pub fn select_poll(fds: &mut [libc::pollfd], timeout_ms: i32) {
    // SAFETY: fd_sets are zeroed and only touched with in-range fds.
    unsafe {
        let mut rd: libc::fd_set = std::mem::zeroed();
        let mut wr: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut rd);
        libc::FD_ZERO(&mut wr);
        let mut max = -1;
        for p in fds.iter_mut() {
            p.revents = 0;
            if p.fd < 0 {
                continue;
            }
            if p.fd as usize >= libc::FD_SETSIZE {
                p.revents = libc::POLLNVAL;
                continue;
            }
            if p.events & libc::POLLIN != 0 {
                libc::FD_SET(p.fd, &mut rd);
            }
            if p.events & libc::POLLOUT != 0 {
                libc::FD_SET(p.fd, &mut wr);
            }
            max = max.max(p.fd);
        }
        let mut tv = libc::timeval { tv_sec: (timeout_ms / 1000) as _, tv_usec: ((timeout_ms % 1000) * 1000) as _ };
        let n = libc::select(max + 1, &mut rd, &mut wr, std::ptr::null_mut(), &mut tv);
        if n <= 0 {
            return;
        }
        for p in fds.iter_mut() {
            if p.fd < 0 || p.fd as usize >= libc::FD_SETSIZE {
                continue;
            }
            if libc::FD_ISSET(p.fd, &rd) {
                p.revents |= libc::POLLIN;
            }
            if libc::FD_ISSET(p.fd, &wr) {
                p.revents |= libc::POLLOUT;
            }
        }
    }
}

pub fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: fcntl on an fd we hold.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        let flags = if on { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}

/// Close every fd from 3 up: a daemon must not keep whatever its launcher
/// leaked (sockets, pipes) open for the life of the session.
pub fn close_inherited_fds() {
    // SAFETY: called first thing in the daemon, before it opens anything.
    unsafe {
        let max = libc::getdtablesize().clamp(256, 65536);
        for fd in 3..max {
            libc::close(fd);
        }
    }
}

pub fn set_winsize(master: RawFd, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: cols.saturating_mul(cell_width), ws_ypixel: rows.saturating_mul(cell_height) };
    // SAFETY: TIOCSWINSZ with a valid winsize on the PTY master.
    if unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The PTY's foreground process group (0 when none).
pub fn foreground_pgrp(master: RawFd) -> i32 {
    // SAFETY: plain syscall.
    unsafe { libc::tcgetpgrp(master) }.max(0)
}

/// The process's argv, from `sysctl(KERN_PROCARGS2)`.
pub fn argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: first call asks for the size; the second fills a buffer of that size.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(size);
    parse_procargs(&buf)
}

fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    let mut i = rest.iter().position(|&b| b == 0)?;
    while rest.get(i) == Some(&0) {
        i += 1;
    }
    let args: Vec<String> = rest[i..].split(|&b| b == 0).take(argc).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    (!args.is_empty()).then_some(args)
}

pub fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buffer sized as passed.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

pub fn proc_cwd(pid: i32) -> Option<String> {
    // SAFETY: proc_pidinfo fills a struct of the size we pass.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) };
    if n != size {
        return None;
    }
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(path.to_string_lossy().into_owned()).filter(|p| !p.is_empty())
}

/// A short name for a command line: the agent CLI, the interpreter's script,
/// else the executable's basename ("-zsh" → "zsh"), as Lite labels panes.
pub fn label(argv: &[String]) -> String {
    const AGENTS: [&str; 4] = ["claude", "codex", "cursor", "opencode"];
    const INTERPRETERS: [&str; 14] = ["node", "nodejs", "bun", "deno", "python", "python3", "ruby", "php", "npx", "pnpm", "yarn", "sh", "bash", "zsh"];
    let joined = argv.join(" ").to_lowercase();
    if let Some(agent) = AGENTS.iter().find(|a| joined.contains(*a)) {
        return (*agent).to_owned();
    }
    let base = |p: &str| -> String {
        let p = p.trim_start_matches('-');
        p.rsplit('/').next().unwrap_or(p).trim_end_matches(':').to_owned()
    };
    let Some(first) = argv.first() else {
        return String::new();
    };
    let exe = base(first);
    if INTERPRETERS.iter().any(|i| i.eq_ignore_ascii_case(&exe))
        && let Some(script) = argv[1..].iter().find(|a| !a.starts_with('-'))
    {
        let b = script.rsplit('/').next().unwrap_or(script);
        let b = [".js", ".mjs", ".cjs", ".ts", ".py", ".rb"].iter().fold(b, |b, ext| b.strip_suffix(ext).unwrap_or(b));
        if !b.is_empty() {
            return b.to_owned();
        }
    }
    exe
}

/// The foreground job of a PTY, named.
pub fn foreground(master: RawFd) -> Option<crate::meta::Foreground> {
    let pid = foreground_pgrp(master);
    if pid <= 0 {
        return None;
    }
    let args = argv(pid).unwrap_or_default();
    let name = Some(label(&args)).filter(|n| !n.is_empty()).or_else(|| proc_name(pid))?;
    let mut command = args.join(" ");
    if command.len() > 200 {
        let mut end = 200;
        while !command.is_char_boundary(end) {
            end -= 1;
        }
        command.truncate(end);
    }
    Some(crate::meta::Foreground { pid, name, command })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::net::UnixStream;

    #[test]
    fn peer_uid_is_ours() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(a.as_raw_fd()).unwrap(), euid());
    }

    #[test]
    fn passes_an_fd() {
        let (a, b) = UnixStream::pair().unwrap();
        let (p, q) = UnixStream::pair().unwrap();
        assert_eq!(send_with_fd(a.as_raw_fd(), b"x", p.as_raw_fd()).unwrap(), 1);
        let mut buf = [0u8; 8];
        let (n, fds) = recv_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!((n, fds.len()), (1, 1));
        // The received fd is the same socket: writing to it reaches q.
        let got = unsafe { UnixStream::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(fds.into_iter().next().unwrap())) };
        use std::io::{Read as _, Write as _};
        (&got).write_all(b"hi").unwrap();
        let mut two = [0u8; 2];
        (&q).read_exact(&mut two).unwrap();
        assert_eq!(&two, b"hi");
    }

    #[test]
    fn labels() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(label(&v(&["-zsh"])), "zsh");
        assert_eq!(label(&v(&["node", "/x/claude/cli.js"])), "claude");
        assert_eq!(label(&v(&["npm", "run", "dev"])), "npm");
    }
}
