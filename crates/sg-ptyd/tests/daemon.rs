//! End-to-end tests against the real `splitgrid-ptyd` binary, each in its own
//! temp sessions dir with a synthetic `/bin/sh -c …` child. Every test owns a
//! [`Session`] guard that ends the daemon and the child's process group on
//! drop, pass or fail.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sg_ptyd::client::{self, Conn};
use sg_ptyd::daemon::SpawnSpec;
use sg_ptyd::meta::Meta;
use sg_ptyd::proto::{Size, kind};

const T: Duration = Duration::from_secs(5);

struct Session {
    socket: PathBuf,
    shell_pid: i32,
    daemon_pid: i32,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = client::terminate(&self.socket, Duration::from_secs(3));
        // SAFETY: signals only to the processes this test spawned (the
        // daemon, and the shell's own session/process group).
        unsafe {
            if self.daemon_pid > 0 {
                libc::kill(self.daemon_pid, libc::SIGKILL);
            }
            if self.shell_pid > 0 {
                libc::kill(-self.shell_pid, libc::SIGKILL);
                libc::kill(self.shell_pid, libc::SIGKILL);
            }
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A short temp dir (sun_path is 104 bytes).
fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap()
}

fn spawn_spec(spec: &SpawnSpec) -> Result<Session, String> {
    spawn_spec_env(spec, &[])
}

fn spawn_spec_env(spec: &SpawnSpec, launcher_env: &[(&str, &Path)]) -> Result<Session, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_splitgrid-ptyd"));
    for (k, v) in launcher_env {
        cmd.env(k, v);
    }
    let mut child = cmd.arg("serve").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(spec).unwrap()).unwrap();
    let out = child.wait_with_output().unwrap();
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let mut parts = line.split_whitespace();
    if parts.next() != Some("ok") {
        return Err(line);
    }
    let shell_pid = parts.next().unwrap().parse().unwrap();
    let daemon_pid = parts.next().unwrap().parse().unwrap();
    Ok(Session { socket: spec.socket.clone(), shell_pid, daemon_pid })
}

fn spec(dir: &Path, id: &str, script: &str) -> SpawnSpec {
    SpawnSpec {
        socket: sg_ptyd::paths::socket_path(dir, id),
        cwd: Some(dir.to_path_buf()),
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
        shell: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
        meta: Meta { container_id: id.into(), ..Default::default() },
        ..Default::default()
    }
}

fn spawn(dir: &Path, id: &str, script: &str) -> Session {
    spawn_spec(&spec(dir, id, script)).expect("daemon starts")
}

fn connect(s: &Session) -> Conn {
    Conn::connect(&s.socket, T).expect("connect")
}

/// Read frames until the accumulated OUTPUT contains `needle`. Returns all
/// output seen and whether REPLAY_DONE came by.
fn read_until(c: &mut Conn, needle: &str, timeout: Duration) -> (String, bool) {
    let _ = c.set_timeout(Some(Duration::from_millis(100)));
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut replay_done = false;
    while Instant::now() < deadline {
        match c.recv() {
            Ok(Some((kind::OUTPUT, p))) => out.extend(p),
            Ok(Some((kind::REPLAY_DONE, _))) => replay_done = true,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
        if String::from_utf8_lossy(&out).contains(needle) {
            break;
        }
    }
    (String::from_utf8_lossy(&out).into_owned(), replay_done)
}

#[test]
fn output_while_detached_is_replayed_on_attach() {
    let d = tmp();
    let s = spawn(d.path(), "c-replay", "i=0; while [ $i -lt 5 ]; do echo tick$i; i=$((i+1)); sleep 0.1; done; echo all-done; exec sleep 30");
    // Nobody attached while it runs.
    std::thread::sleep(Duration::from_millis(1200));
    let mut c = connect(&s);
    let a = c.attach().unwrap();
    assert_eq!(a.pid, s.shell_pid);
    assert_eq!((a.size.cols, a.size.rows), (80, 24));
    let (out, done) = read_until(&mut c, "all-done", T);
    for i in 0..5 {
        assert!(out.contains(&format!("tick{i}")), "{out:?}");
    }
    assert!(out.contains("all-done"));
    let _ = done;
}

#[test]
fn input_round_trip_detach_and_reattach() {
    let d = tmp();
    let s = spawn(d.path(), "c-input", "echo ready; read x; sleep 0.3; echo got:$x; exec sleep 30");
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "ready", T).0.contains("ready"));
    c.input(b"hello\n").unwrap();
    c.detach().unwrap();
    drop(c);
    // "got:hello" is printed while nobody is attached.
    std::thread::sleep(Duration::from_millis(800));
    let mut c = connect(&s);
    c.attach().unwrap();
    let (out, _) = read_until(&mut c, "got:hello", T);
    assert!(out.contains("ready") && out.contains("got:hello"), "{out:?}");
}

#[test]
fn resize_reaches_the_child() {
    let d = tmp();
    let s = spawn(d.path(), "c-resize", "trap 'stty size' WINCH; stty size; while :; do sleep 0.05; done");
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "24 80", T).0.contains("24 80"));
    c.resize(Size { cols: 100, rows: 30, cell_width: 8, cell_height: 16 }).unwrap();
    let (out, _) = read_until(&mut c, "30 100", T);
    assert!(out.contains("30 100"), "{out:?}");
    // The daemon reports the new size, and a re-attach starts at it.
    let info = connect(&s).info().unwrap();
    assert_eq!((info.cols, info.rows), (100, 30));
    let mut c2 = connect(&s);
    let a = c2.attach().unwrap();
    assert_eq!((a.size.cols, a.size.rows), (100, 30));
}

#[test]
fn attach_signals_a_repaint_even_without_a_size_change() {
    let d = tmp();
    let s = spawn(d.path(), "c-winch", "trap 'echo winch' WINCH; echo up; while :; do sleep 0.05; done");
    let mut c = connect(&s);
    c.attach().unwrap();
    // Once the trap is surely in place, attach again: same size, still a SIGWINCH.
    assert!(read_until(&mut c, "up", T).0.contains("up"));
    let mut c = connect(&s);
    c.attach().unwrap();
    let (out, _) = read_until(&mut c, "winch", T);
    assert!(out.contains("winch"), "{out:?}");
}

#[test]
fn kill_ends_the_shell_and_removes_the_socket() {
    let d = tmp();
    let s = spawn(d.path(), "c-kill", "echo up; exec sleep 30");
    let mut watcher = connect(&s);
    watcher.attach().unwrap();
    assert!(read_until(&mut watcher, "up", T).0.contains("up"));
    let code = connect(&s).kill().unwrap();
    assert_eq!(code, 128 + libc::SIGHUP);
    // The attached client hears about it too.
    let _ = watcher.set_timeout(Some(T)); // EINVAL on macOS once the peer closed
    let mut saw_exit = false;
    while let Ok(Some((k, _))) = watcher.recv() {
        if k == kind::EXIT {
            saw_exit = true;
            break;
        }
    }
    assert!(saw_exit);
    let deadline = Instant::now() + T;
    while (alive(s.shell_pid) || alive(s.daemon_pid) || s.socket.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(s.shell_pid), "shell still running");
    assert!(!s.socket.exists(), "socket left behind");
    assert!(client::is_gone(&Conn::connect(&s.socket, T).err().unwrap()));
}

#[test]
fn a_shell_ignoring_sighup_is_killed_after_the_grace_period() {
    let d = tmp();
    let s = spawn(d.path(), "c-stubborn", "trap '' HUP; echo up; while :; do sleep 0.05; done");
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "up", T).0.contains("up"));
    let started = Instant::now();
    let code = connect(&s).kill().unwrap();
    assert_eq!(code, 128 + libc::SIGKILL);
    assert!(started.elapsed() >= Duration::from_millis(1500));
}

#[test]
fn child_exit_is_reported_with_its_code() {
    let d = tmp();
    let s = spawn(d.path(), "c-exit", "echo up; read x; exit 3");
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "up", T).0.contains("up"));
    c.input(b"\n").unwrap();
    let _ = c.set_timeout(Some(T));
    let mut code = None;
    while let Ok(Some((k, p))) = c.recv() {
        if k == kind::EXIT {
            code = Some(sg_ptyd::proto::parse_exit(&p));
            break;
        }
    }
    assert_eq!(code, Some(3));
    let deadline = Instant::now() + T;
    while s.socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!s.socket.exists());
}

#[test]
fn version_mismatch_is_rejected_cleanly() {
    let d = tmp();
    let s = spawn(d.path(), "c-version", "exec sleep 30");
    let err = Conn::connect_with(&s.socket, T, (90, 99)).err().expect("rejected");
    assert!(client::is_incompatible(&err), "{err}");
    assert!(err.to_string().contains("incompatible protocol"), "{err}");
    // The daemon is unharmed and still serves compatible clients.
    let info = connect(&s).info().unwrap();
    assert_eq!(info.pid, s.shell_pid);
    assert_eq!(info.protocol, sg_ptyd::proto::PROTOCOL_VERSION);
    // An unknown op is an error, not a disconnect.
    let mut c = connect(&s);
    c.send(0x55, b"").unwrap();
    let _ = c.set_timeout(Some(T));
    assert_eq!(c.recv().unwrap().map(|f| f.0), Some(kind::ERROR));
    assert!(c.info().is_ok());
}

#[test]
fn peers_of_another_uid_are_refused() {
    let d = tmp();
    let mut sp = spec(d.path(), "c-uid", "exec sleep 30");
    // Debug builds let a test pretend only another uid may connect.
    sp.expect_uid = Some(sg_ptyd::sys::euid() + 1);
    let s = spawn_spec(&sp).unwrap();
    let err = Conn::connect(&s.socket, T).err().expect("refused");
    assert!(!client::is_incompatible(&err), "{err}");
    // TERMINATE is refused as well: the session survives.
    let _ = client::terminate(&s.socket, Duration::from_millis(500));
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(s.shell_pid));
}

#[test]
fn master_fd_comes_along_for_process_queries() {
    let d = tmp();
    let s = spawn(d.path(), "c-fd", "echo up; exec sleep 30");
    let mut c = connect(&s);
    let a = c.attach().unwrap();
    let master = a.master.expect("PTY master passed");
    use std::os::fd::AsRawFd as _;
    // SAFETY: plain queries on an fd we own.
    let (sid, pgrp) = unsafe { (libc::tcgetsid(master.as_raw_fd()), libc::tcgetpgrp(master.as_raw_fd())) };
    assert_eq!(sid, s.shell_pid);
    assert_eq!(pgrp, s.shell_pid);
}

#[test]
fn metadata_round_trips_and_merges() {
    let d = tmp();
    let mut sp = spec(d.path(), "c-meta", "exec sleep 30");
    sp.meta = Meta {
        container_id: "c-meta".into(),
        env_id: "default".into(),
        env_name: "Default".into(),
        workspace_id: "ws-1".into(),
        workspace_name: "api".into(),
        title: "server".into(),
    };
    let s = spawn_spec(&sp).unwrap();
    // Let `sh -c 'exec sleep'` reach the exec.
    std::thread::sleep(Duration::from_millis(300));
    let mut c = connect(&s);
    assert_eq!(c.info().unwrap().meta, sp.meta);
    // A full update…
    let renamed = Meta { workspace_name: "billing".into(), title: "dev".into(), ..sp.meta.clone() };
    c.set_meta(&renamed).unwrap();
    assert_eq!(connect(&s).info().unwrap().meta, renamed);
    // …and a partial one leaves the rest.
    c.send(kind::SET_META, br#"{"envName":"Work"}"#).unwrap();
    let info = c.info().unwrap();
    assert_eq!(info.meta.env_name, "Work");
    assert_eq!(info.meta.workspace_name, "billing");
    assert_eq!(info.meta.container_id, "c-meta");
    assert_eq!(info.foreground.as_ref().map(|f| f.name.as_str()), Some("sleep"));
    assert_eq!(info.cwd.as_deref().map(|p| std::fs::canonicalize(p).unwrap()), Some(std::fs::canonicalize(d.path()).unwrap()));
}

#[test]
fn snapshot_renders_the_screen_without_attaching() {
    let d = tmp();
    let s = spawn(d.path(), "c-snap", "printf 'one\\ntwo\\n\\033[31mred\\033[0m\\n'; exec sleep 30");
    std::thread::sleep(Duration::from_millis(500));
    let mut c = connect(&s);
    let (size, bytes) = c.snapshot(1 << 20).unwrap();
    let text = sg_ptyd::screen::render_text(&bytes, size.cols, size.rows, 1000);
    assert_eq!(text, "one\ntwo\nred");
    assert!(!c.info().unwrap().attached);
}

#[test]
fn a_stalled_client_never_blocks_the_shell() {
    let d = tmp();
    let s = spawn(d.path(), "c-flood", "head -c 40000000 /dev/zero | tr '\\0' x; echo; echo FLOOD-DONE; exec sleep 30");
    // Attached, and never reads.
    let mut stalled = connect(&s);
    stalled.attach().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut done = false;
    while Instant::now() < deadline {
        // The daemon keeps answering while it floods; done once the shell reached `exec sleep`.
        if connect(&s).info().unwrap().foreground.is_some_and(|f| f.name == "sleep") {
            let (_, bytes) = connect(&s).snapshot(4096).unwrap();
            done = String::from_utf8_lossy(&bytes).contains("FLOOD-DONE");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(done, "the shell's output stalled behind a client that doesn't read");
    drop(stalled);
}

#[test]
fn a_new_attach_displaces_the_old_one() {
    let d = tmp();
    let s = spawn(d.path(), "c-displace", "echo up; exec sleep 30");
    let mut first = connect(&s);
    first.attach().unwrap();
    let mut second = connect(&s);
    second.attach().unwrap();
    // (setsockopt fails on macOS once the peer has closed: ignore it.)
    let _ = first.set_timeout(Some(T));
    let mut displaced = false;
    while let Ok(Some((k, _))) = first.recv() {
        if k == kind::DETACHED {
            displaced = true;
            break;
        }
    }
    assert!(displaced);
}

#[test]
fn stale_sockets_are_replaced_and_live_ones_are_not() {
    let d = tmp();
    let sp = spec(d.path(), "c-stale", "exec sleep 30");
    // A socket file nobody listens on (a daemon that died with the machine).
    std::fs::create_dir_all(d.path()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&sp.socket).unwrap());
    assert!(sp.socket.exists());
    assert!(client::is_gone(&Conn::connect(&sp.socket, T).err().unwrap()));
    let s = spawn_spec(&sp).unwrap();
    // A second daemon on the same live socket refuses to start.
    let err = spawn_spec(&sp).err().expect("refused");
    assert!(err.contains("already listening"), "{err}");
    let found = client::scan(d.path(), T);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].info.as_ref().unwrap().pid, s.shell_pid);
}

#[test]
fn scan_drops_stale_sockets() {
    let d = tmp();
    let stale = d.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let _s = spawn(d.path(), "c-scan", "exec sleep 30");
    let found = client::scan(d.path(), T);
    assert_eq!(found.len(), 1);
    assert!(!stale.exists());
}

#[test]
fn interrupt_reaches_the_foreground_job_like_ctrl_c() {
    let d = tmp();
    let s = spawn(d.path(), "c-int", "trap 'echo got-int; exec sleep 30' INT; echo ready; while :; do sleep 0.2; done");
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "ready", T).0.contains("ready"));
    client::interrupt(&s.socket, T).expect("interrupt");
    let (out, _) = read_until(&mut c, "got-int", T);
    assert!(out.contains("got-int"), "{out:?}");
    // Ctrl-C, not an end: the session lives on.
    assert!(alive(s.shell_pid));
}

#[test]
fn env_reaches_the_shell_and_spec_stays_off_argv() {
    let d = tmp();
    let mut sp = spec(d.path(), "c-env", "echo token=$SPLITGRID_TEST_TOKEN; exec sleep 30");
    sp.env.insert("SPLITGRID_TEST_TOKEN".into(), "s3cr3t-value".into());
    let s = spawn_spec(&sp).unwrap();
    let mut c = connect(&s);
    c.attach().unwrap();
    assert!(read_until(&mut c, "token=s3cr3t-value", T).0.contains("token=s3cr3t-value"));
    let args = sg_ptyd::sys::argv(s.daemon_pid).unwrap_or_default().join(" ");
    assert!(!args.contains("s3cr3t"), "{args}");
}

/// The path real terminals take: no `shell`, so alacritty's macOS default
/// (`/usr/bin/login -flp $USER /bin/zsh -fc 'exec -a -sh $SHELL'`), with
/// HOME, ZDOTDIR and SHELL pointed at a scratch dir and /bin/sh.
#[test]
fn the_default_login_shell_path_works() {
    let d = tmp();
    let home = d.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Quiet login (alacritty checks $HOME/.hushlogin itself).
    std::fs::write(home.join(".hushlogin"), b"").unwrap();
    let mut sp = spec(d.path(), "c-login", "");
    sp.shell = None;
    sp.cwd = Some(home.clone());
    sp.env.insert("ZDOTDIR".into(), home.to_string_lossy().into_owned());
    sp.env.insert("ENV".into(), "/dev/null".into());
    let s = spawn_spec_env(&sp, &[("HOME", home.as_path()), ("ZDOTDIR", home.as_path()), ("SHELL", Path::new("/bin/sh"))]).expect("daemon starts");
    let mut c = connect(&s);
    c.attach().unwrap();
    c.input(b"echo login-$((40+2)) in $PWD\n").unwrap();
    let (out, _) = read_until(&mut c, "login-42 in", T);
    assert!(out.contains("login-42 in"), "{out:?}");
    // The shell is a login shell ("-sh") under login(1), in its own session.
    let info = connect(&s).info().unwrap();
    assert!(info.foreground.as_ref().is_some_and(|f| f.command.starts_with("-sh")), "{:?}", info.foreground);
    c.input(b"exit\n").unwrap();
}
