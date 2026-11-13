//! Which program runs in a terminal right now, and where (macOS).
//!
//! The PTY's foreground process group is the job the user sees (`claude`,
//! `vim`, or the shell itself at its prompt); its leader's name and working
//! directory label the pane, as the Electron app's pane headers do.

use std::ffi::CStr;
use std::os::fd::RawFd;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub name: String,
    pub cwd: Option<String>,
}

/// One process under a terminal's shell (the Electron app's TerminalProcessInfo).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
    pub pid: i32,
    pub ppid: i32,
    /// Nesting under the shell (the shell is 0).
    pub depth: usize,
    pub command: String,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
}

/// The shell `root` and everything below it, depth-first, from `ps`.
pub fn tree(root: i32) -> Vec<TreeEntry> {
    let Ok(out) = std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,%cpu=,rss=,command="]).output() else {
        return Vec::new();
    };
    tree_from_ps(&String::from_utf8_lossy(&out.stdout), root)
}

fn tree_from_ps(ps: &str, root: i32) -> Vec<TreeEntry> {
    let rows: Vec<TreeEntry> = ps
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu_percent = it.next()?.parse().unwrap_or(0.);
            let rss_bytes = it.next()?.parse::<u64>().unwrap_or(0) * 1024;
            let command = it.collect::<Vec<_>>().join(" ");
            Some(TreeEntry { pid, ppid, depth: 0, command, cpu_percent, rss_bytes })
        })
        .collect();
    fn walk(rows: &[TreeEntry], pid: i32, depth: usize, out: &mut Vec<TreeEntry>) {
        let Some(me) = rows.iter().find(|r| r.pid == pid) else { return };
        out.push(TreeEntry { depth, ..me.clone() });
        for child in rows.iter().filter(|r| r.ppid == pid && r.pid != pid) {
            walk(rows, child.pid, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(&rows, root, 0, &mut out);
    out
}

pub fn foreground(master: RawFd) -> Option<Process> {
    // SAFETY: plain syscalls on an fd we own; buffers sized as the API asks.
    let pgid = unsafe { libc::tcgetpgrp(master) };
    if pgid <= 0 {
        return None;
    }
    let name = argv(pgid).map(|a| label(&a)).filter(|n| !n.is_empty()).or_else(|| name(pgid))?;
    Some(Process { pid: pgid, name, cwd: cwd(pgid) })
}

/// Agent CLIs recognised anywhere in the command line (SplitGrid's AGENT_COMMANDS).
const KNOWN_AGENTS: [&str; 4] = ["claude", "codex", "cursor", "opencode"];
const INTERPRETERS: [&str; 14] = ["node", "nodejs", "bun", "deno", "python", "python3", "ruby", "php", "npx", "pnpm", "yarn", "sh", "bash", "zsh"];

/// A short name for a command line, as the Electron app derives it
/// (local-shell-manager.ts deriveProcessLabel): an agent CLI by name, an
/// interpreter by its script, else the executable's basename ("-zsh" → "zsh").
pub fn label(argv: &[String]) -> String {
    let joined = argv.join(" ").to_lowercase();
    if let Some(agent) = KNOWN_AGENTS.iter().find(|a| joined.contains(*a)) {
        return (*agent).to_owned();
    }
    let base = |p: &str| -> String {
        let p = p.trim_start_matches('-');
        p.rsplit('/').next().unwrap_or(p).trim_end_matches(':').to_owned()
    };
    let Some(first) = argv.first() else { return String::new() };
    let exe = base(first);
    if INTERPRETERS.iter().any(|i| i.eq_ignore_ascii_case(&exe))
        && let Some(script) = argv[1..].iter().find(|a| !a.starts_with('-')) {
            let b = script.rsplit('/').next().unwrap_or(script);
            let b = [".js", ".mjs", ".cjs", ".ts", ".py", ".rb"].iter().fold(b, |b, ext| b.strip_suffix(ext).unwrap_or(b));
            if !b.is_empty() {
                return b.to_owned();
            }
        }
    exe
}

/// The process's argv, from `sysctl(KERN_PROCARGS2)`: argc, the exec path,
/// padding, then the arguments, NUL-separated.
fn argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: first call only asks for the size; the second fills a buffer of that size.
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
    // Skip the exec path, then the NUL padding after it.
    let mut i = rest.iter().position(|&b| b == 0)?;
    while rest.get(i) == Some(&0) {
        i += 1;
    }
    let args: Vec<String> = rest[i..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

fn name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn cwd(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) };
    if n != size {
        return None;
    }
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(path.to_string_lossy().into_owned()).filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_from_ps_output() {
        let ps = "  1     0  0.0  100 /sbin/launchd\n 10     1  0.0  2000 -zsh\n 11    10  12.5 4000 node server.js --port 3\n 12    11  0.0  10 sleep 1\n 13     1  0.0  10 other\n";
        let t = tree_from_ps(ps, 10);
        let got: Vec<(i32, usize)> = t.iter().map(|e| (e.pid, e.depth)).collect();
        assert_eq!(got, [(10, 0), (11, 1), (12, 2)]);
        assert_eq!(t[1].command, "node server.js --port 3");
        assert_eq!(t[1].rss_bytes, 4000 * 1024);
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn labels_like_splitgrid() {
        assert_eq!(label(&v(&["-zsh"])), "zsh");
        assert_eq!(label(&v(&["/bin/zsh", "-l"])), "zsh");
        assert_eq!(label(&v(&["node", "/opt/lib/claude/cli.js", "--foo"])), "claude");
        assert_eq!(label(&v(&["python3", "-u", "/x/serve.py"])), "serve");
        assert_eq!(label(&v(&["/usr/bin/vim", "a.rs"])), "vim");
        assert_eq!(label(&v(&["node"])), "node");
    }

    #[test]
    fn parses_procargs_layout() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/zsh\0\0\0-zsh\0-l\0HOME=/x\0");
        assert_eq!(parse_procargs(&buf), Some(v(&["-zsh", "-l"])));
    }

    #[test]
    fn reads_our_own_process() {
        let me = std::process::id() as i32;
        assert!(argv(me).is_some_and(|a| !a.is_empty()));
        let me = std::process::id() as i32;
        assert!(name(me).is_some_and(|n| !n.is_empty()));
        let here = std::env::current_dir().unwrap().to_string_lossy().into_owned();
        assert_eq!(cwd(me).as_deref(), Some(here.as_str()));
    }
}
