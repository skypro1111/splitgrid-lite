//! What each terminal's process tree is doing: its children, CPU, memory and
//! the TCP ports it listens on — the data behind the sidebar's ports button,
//! the pane header's processes button and the Resources task manager
//! (the Electron app's local-shell-manager.ts collectProcessMetrics /
//! collectListenPortsByPid / collectProcessTree).
//!
//! One `ps` and one `lsof` per tick cover every terminal of every window: the
//! scan is shared through a short cache (Electron's METRICS_CACHE_MS), runs on
//! a background thread, and the pure parsing / aggregation below is what the
//! tests pin down.

pub mod table;
pub mod ui;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::terminal::procinfo::TreeEntry;

/// One `ps` row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub pid: i32,
    pub ppid: i32,
    /// Cumulative CPU time (user + system), from `time=`.
    pub cpu_seconds: f64,
    /// ps's own %cpu (a decaying average): only for per-process rows.
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub command: String,
}

/// A port some processes of a terminal LISTEN on (Electron's TerminalListenPort).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenPort {
    pub port: u16,
    /// Sorted ascending.
    pub pids: Vec<i32>,
}

/// One machine-wide snapshot: every process, and which pids listen on which ports.
#[derive(Debug)]
pub struct Scan {
    pub rows: Vec<Row>,
    pub ports_by_pid: HashMap<i32, BTreeSet<u16>>,
    /// False when `ps` failed: "Process metrics: unavailable".
    pub supported: bool,
    /// When it was taken (CPU% divides by the time between scans).
    pub at: Instant,
}

/// A terminal's whole tree, summed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeSummary {
    pub cpu_seconds: f64,
    pub rss_bytes: u64,
    /// The shell itself included.
    pub process_count: usize,
    pub ports: Vec<ListenPort>,
}

// ── Parsing ──────────────────────────────────────────────────────────────────

/// The `ps` columns `parse_ps` reads.
pub const PS_ARGS: [&str; 2] = ["-axo", "pid=,ppid=,rss=,time=,%cpu=,command="];

/// ps's `time` ("0:01.52", "12:03:04", "2-03:04:05") in seconds
/// (Electron's parseCpuTimeSeconds).
pub fn parse_cpu_time(value: &str) -> f64 {
    let t = value.trim();
    if t.is_empty() {
        return 0.;
    }
    let (days, rest) = match t.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().unwrap_or(0.), rest),
        None => (0., t),
    };
    let seconds = rest.split(':').fold(0., |acc, part| acc * 60. + part.parse::<f64>().unwrap_or(0.));
    days * 86400. + seconds
}

/// `ps -axo pid=,ppid=,rss=,time=,%cpu=,command=`.
pub fn parse_ps(output: &str) -> Vec<Row> {
    output
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let rss_bytes = it.next()?.parse::<u64>().ok()? * 1024;
            let cpu_seconds = parse_cpu_time(it.next()?);
            let cpu_percent = it.next()?.replace(',', ".").parse().unwrap_or(0.);
            let command = it.collect::<Vec<_>>().join(" ");
            (!command.is_empty()).then_some(Row { pid, ppid, cpu_seconds, cpu_percent, rss_bytes, command })
        })
        .collect()
}

/// `lsof -nP -iTCP -sTCP:LISTEN -Fpn`: a `p<pid>` line, then one `n<addr>:<port>`
/// line per socket of that pid ("*:3000", "127.0.0.1:5173", "[::1]:8080").
pub fn parse_lsof(output: &str) -> HashMap<i32, BTreeSet<u16>> {
    let mut by_pid: HashMap<i32, BTreeSet<u16>> = HashMap::new();
    let mut pid = 0;
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => pid = value.trim().parse().unwrap_or(0),
            'n' if pid > 0 => {
                let port = value.rsplit_once(':').map_or("", |(_, p)| p);
                if let Ok(port) = port.parse::<u16>()
                    && port > 0
                {
                    by_pid.entry(pid).or_default().insert(port);
                }
            }
            _ => {}
        }
    }
    by_pid
}

// ── Trees ────────────────────────────────────────────────────────────────────

fn children_of(rows: &[Row]) -> HashMap<i32, Vec<usize>> {
    let mut map: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if r.pid != r.ppid {
            map.entry(r.ppid).or_default().push(i);
        }
    }
    for kids in map.values_mut() {
        kids.sort_by_key(|&i| rows[i].pid);
    }
    map
}

/// Every pid of `root`'s tree, root first; empty when `root` is gone.
pub fn tree_pids(rows: &[Row], root: i32) -> Vec<i32> {
    tree_entries(rows, root).into_iter().map(|e| e.pid).collect()
}

/// `root` and everything below it, depth-first, children by pid
/// (Electron's collectProcessTree).
pub fn tree_entries(rows: &[Row], root: i32) -> Vec<TreeEntry> {
    let by_pid: HashMap<i32, &Row> = rows.iter().map(|r| (r.pid, r)).collect();
    if !by_pid.contains_key(&root) {
        return Vec::new();
    }
    let kids = children_of(rows);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((pid, depth)) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let Some(r) = by_pid.get(&pid) else { continue };
        out.push(TreeEntry { pid, ppid: r.ppid, depth, command: r.command.clone(), cpu_percent: r.cpu_percent, rss_bytes: r.rss_bytes });
        // Reversed, so the lowest pid is walked first.
        for &i in kids.get(&pid).map(Vec::as_slice).unwrap_or_default().iter().rev() {
            stack.push((rows[i].pid, depth + 1));
        }
    }
    out
}

/// Sum a terminal's tree and collect the ports its processes listen on.
pub fn summarize(scan: &Scan, root: i32) -> TreeSummary {
    let by_pid: HashMap<i32, &Row> = scan.rows.iter().map(|r| (r.pid, r)).collect();
    let mut s = TreeSummary::default();
    let mut port_pids: BTreeMap<u16, BTreeSet<i32>> = BTreeMap::new();
    for pid in tree_pids(&scan.rows, root) {
        let r = by_pid[&pid];
        s.cpu_seconds += r.cpu_seconds;
        s.rss_bytes += r.rss_bytes;
        s.process_count += 1;
        for &port in scan.ports_by_pid.get(&pid).into_iter().flatten() {
            port_pids.entry(port).or_default().insert(pid);
        }
    }
    s.ports = port_pids.into_iter().map(|(port, pids)| ListenPort { port, pids: pids.into_iter().collect() }).collect();
    s
}

/// Every port of these terminals, once each, ascending (the sidebar's "ports" row).
pub fn merged_ports<'a>(lists: impl IntoIterator<Item = &'a [ListenPort]>) -> Vec<u16> {
    let set: BTreeSet<u16> = lists.into_iter().flatten().map(|p| p.port).collect();
    set.into_iter().collect()
}

// ── CPU% ─────────────────────────────────────────────────────────────────────

/// A tree's CPU% over the last poll: the change in its cumulative CPU time over
/// wall time. ps %cpu is a long average, useless for "busy right now".
#[derive(Default)]
pub struct CpuTracker {
    prev: HashMap<i32, (f64, Instant, f32)>,
}

impl CpuTracker {
    /// Samples closer than this keep the previous figure (too noisy to divide).
    const MIN_DT: f64 = 0.5;

    pub fn sample(&mut self, root: i32, cpu_seconds: f64, now: Instant) -> f32 {
        let mut percent = 0.;
        if let Some(&(secs, at, pct)) = self.prev.get(&root) {
            percent = pct;
            let dt = now.saturating_duration_since(at).as_secs_f64();
            if dt >= Self::MIN_DT {
                percent = (((cpu_seconds - secs) / dt) * 100.).max(0.) as f32;
            } else {
                // Keep the older baseline so the next sample spans enough time.
                return percent;
            }
        }
        self.prev.insert(root, (cpu_seconds, now, percent));
        percent
    }

    /// Forget terminals that are gone.
    pub fn retain(&mut self, live: &HashSet<i32>) {
        self.prev.retain(|pid, _| live.contains(pid));
    }
}

// ── Scanning ─────────────────────────────────────────────────────────────────

/// How long one scan is shared between windows and callers.
const CACHE_FOR: Duration = Duration::from_millis(1000);
/// A stuck `lsof` (an unreachable mount) can't hold a poll up longer than this.
const LSOF_TIMEOUT: Duration = Duration::from_millis(1500);
const PS_TIMEOUT: Duration = Duration::from_millis(3000);

static CACHE: Mutex<Option<(Instant, Arc<Scan>)>> = Mutex::new(None);

/// Run `program` and return its stdout, or None when it fails or overruns.
fn run(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a helper thread so a full pipe can't block the child.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut buf).ok();
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let buf = reader.join().ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Every process and listening port now. Blocking: call off the UI thread.
/// `fresh` skips the cache (right after a kill).
pub fn scan(fresh: bool) -> Arc<Scan> {
    if !fresh
        && let Some((at, scan)) = CACHE.lock().unwrap().as_ref()
        && at.elapsed() < CACHE_FOR
    {
        return scan.clone();
    }
    let at = Instant::now();
    let scan = Arc::new(match run("/bin/ps", &PS_ARGS, PS_TIMEOUT) {
        Some(out) => Scan {
            rows: parse_ps(&out),
            // lsof exits 1 when nothing listens: no output, no ports.
            ports_by_pid: run("/usr/sbin/lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"], LSOF_TIMEOUT).map(|o| parse_lsof(&o)).unwrap_or_default(),
            supported: true,
            at,
        },
        None => Scan { rows: Vec::new(), ports_by_pid: HashMap::new(), supported: false, at },
    });
    *CACHE.lock().unwrap() = Some((Instant::now(), scan.clone()));
    scan
}

/// Which signal a kill sends: the ports popover's Kill is SIGKILL, the task
/// manager's is SIGTERM (as in the Electron app).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// Whether `pid` may be signalled on behalf of the terminal whose shell is `root`:
/// only a process of that terminal's tree, and never init.
pub fn check_kill_target(rows: &[Row], root: i32, pid: i32) -> Result<(), String> {
    if pid <= 1 || root <= 1 {
        return Err("Invalid PID".into());
    }
    if !tree_pids(rows, root).contains(&pid) {
        return Err(format!("Process {pid} is not part of this terminal"));
    }
    Ok(())
}

/// Signal one process of a terminal's tree, after re-checking it still belongs
/// there (a pid may have been reused since the list was drawn). Blocking.
pub fn kill_in_tree(root: i32, pid: i32, signal: Signal) -> Result<(), String> {
    let rows = run("/bin/ps", &PS_ARGS, PS_TIMEOUT).map(|o| parse_ps(&o)).ok_or("Couldn't list processes")?;
    check_kill_target(&rows, root, pid)?;
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: a plain syscall; the pid was just checked to be a descendant of the terminal's shell.
    if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
}

// ── Presentation helpers (TerminalProcessesButton / WorkspaceTaskManager) ────

/// "0 B", "512 B", "1.5 KB", "12 MB", "1.2 GB".
pub fn format_bytes(value: u64) -> String {
    if value == 0 {
        return "0 B".into();
    }
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024. && unit < UNITS.len() - 1 {
        size /= 1024.;
        unit += 1;
    }
    if size >= 10. || unit == 0 { format!("{size:.0} {}", UNITS[unit]) } else { format!("{size:.1} {}", UNITS[unit]) }
}

/// "0.4%", "12%".
pub fn format_percent(value: f32) -> String {
    if !value.is_finite() {
        return "0%".into();
    }
    if value >= 10. { format!("{value:.0}%") } else { format!("{value:.1}%") }
}

/// A usage bar's fill: green, amber past 65%, red past 85% (the Meter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Hot,
}

/// How full a bar is (0–100) and its level, for `value` out of `max`.
pub fn meter(value: f64, max: f64) -> (f32, Level) {
    let pct = if max > 0. { (value / max * 100.).clamp(0., 100.) } else { 0. } as f32;
    let level = if pct > 85. {
        Level::Hot
    } else if pct > 65. {
        Level::Warn
    } else {
        Level::Ok
    };
    (pct, level)
}

/// A CPU figure turns red past this (the task manager's and popover's 40%).
pub const CPU_HOT: f32 = 40.;

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
    1     0   9000  12:34.56   0.0 /sbin/launchd
  100     1   2048   0:00.10   0.0 -zsh
  101   100  40960   1:02.50  55.0 node /app/server.js --port 3000
  102   101   1024   0:00.01   0.0 /bin/sh -c sleep 100
  103   102    512   0:00.00   0.0 sleep 100
  104   100   4096   0:03.00   1.5 python3 -m http.server 8080
  200     1   2048 1-00:00:01  0.0 -bash
  201   200    100   0:00.00   0.0 vim notes.md
";

    const LSOF: &str = "p101\nf23\nn*:3000\nf24\nn[::1]:3000\np104\nn127.0.0.1:8080\np999\nn*:5432\np105\nn*:*\n";

    fn fixture() -> Scan {
        Scan { rows: parse_ps(PS), ports_by_pid: parse_lsof(LSOF), supported: true, at: Instant::now() }
    }

    #[test]
    fn cpu_time_formats() {
        assert_eq!(parse_cpu_time("0:01.50"), 1.5);
        assert_eq!(parse_cpu_time("12:03"), 723.);
        assert_eq!(parse_cpu_time("1:00:00"), 3600.);
        assert_eq!(parse_cpu_time("2-00:00:01"), 2. * 86400. + 1.);
        assert_eq!(parse_cpu_time(""), 0.);
    }

    #[test]
    fn parses_ps_rows() {
        let rows = parse_ps(PS);
        assert_eq!(rows.len(), 8);
        let node = rows.iter().find(|r| r.pid == 101).unwrap();
        assert_eq!(node.ppid, 100);
        assert_eq!(node.rss_bytes, 40960 * 1024);
        assert_eq!(node.cpu_seconds, 62.5);
        assert_eq!(node.cpu_percent, 55.);
        assert_eq!(node.command, "node /app/server.js --port 3000");
        assert_eq!(rows.iter().find(|r| r.pid == 200).unwrap().cpu_seconds, 86401.);
        // Garbage and blank lines are skipped.
        assert!(parse_ps("PID PPID\n\n  x y z\n").is_empty());
    }

    #[test]
    fn parses_lsof_listeners() {
        let ports = parse_lsof(LSOF);
        assert_eq!(ports[&101], BTreeSet::from([3000]));
        assert_eq!(ports[&104], BTreeSet::from([8080]));
        assert_eq!(ports[&999], BTreeSet::from([5432]));
        // "*:*" has no port.
        assert!(!ports.contains_key(&105));
        assert!(parse_lsof("").is_empty());
    }

    #[test]
    fn tree_is_depth_first_by_pid() {
        let t = tree_entries(&parse_ps(PS), 100);
        let got: Vec<(i32, usize)> = t.iter().map(|e| (e.pid, e.depth)).collect();
        assert_eq!(got, [(100, 0), (101, 1), (102, 2), (103, 3), (104, 1)]);
        assert!(tree_entries(&parse_ps(PS), 4242).is_empty());
    }

    #[test]
    fn tree_survives_a_cycle() {
        let rows = parse_ps("10 11 1 0:00 0 a\n11 10 1 0:00 0 b\n");
        assert_eq!(tree_pids(&rows, 10), [10, 11]);
    }

    #[test]
    fn summarizes_a_terminal() {
        let s = summarize(&fixture(), 100);
        assert_eq!(s.process_count, 5);
        assert_eq!(s.rss_bytes, (2048 + 40960 + 1024 + 512 + 4096) * 1024);
        assert!((s.cpu_seconds - (0.1 + 62.5 + 0.01 + 3.)).abs() < 1e-9);
        assert_eq!(s.ports, [ListenPort { port: 3000, pids: vec![101] }, ListenPort { port: 8080, pids: vec![104] }]);
        // A port held by a process outside the tree isn't the terminal's.
        assert!(summarize(&fixture(), 200).ports.is_empty());
        assert_eq!(summarize(&fixture(), 200).process_count, 2);
    }

    #[test]
    fn a_port_held_by_several_pids() {
        let scan = Scan { rows: parse_ps(PS), ports_by_pid: parse_lsof("p103\nn*:9000\np101\nn*:9000\n"), supported: true, at: Instant::now() };
        assert_eq!(summarize(&scan, 100).ports, [ListenPort { port: 9000, pids: vec![101, 103] }]);
    }

    #[test]
    fn merges_workspace_ports() {
        let a = [ListenPort { port: 8080, pids: vec![1] }, ListenPort { port: 3000, pids: vec![2] }];
        let b = [ListenPort { port: 3000, pids: vec![3] }, ListenPort { port: 5173, pids: vec![4] }];
        assert_eq!(merged_ports([&a[..], &b[..]]), [3000, 5173, 8080]);
    }

    #[test]
    fn cpu_percent_is_a_rate_over_the_poll() {
        let mut t = CpuTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.sample(100, 10., t0), 0.);
        // 1 CPU-second over 2 wall seconds.
        assert_eq!(t.sample(100, 11., t0 + Duration::from_secs(2)), 50.);
        // Too soon after: the last figure stands.
        assert_eq!(t.sample(100, 30., t0 + Duration::from_millis(2100)), 50.);
        // …and the baseline didn't move, so the next sample spans the whole gap.
        assert_eq!(t.sample(100, 12., t0 + Duration::from_secs(4)), 50.);
        // A tree that lost CPU time (a child exited) never goes negative.
        assert_eq!(t.sample(100, 1., t0 + Duration::from_secs(6)), 0.);
        t.retain(&HashSet::new());
        assert_eq!(t.sample(100, 50., t0 + Duration::from_secs(8)), 0.);
    }

    #[test]
    fn kill_only_within_the_tree() {
        let rows = parse_ps(PS);
        assert!(check_kill_target(&rows, 100, 103).is_ok());
        assert!(check_kill_target(&rows, 100, 100).is_ok());
        assert!(check_kill_target(&rows, 100, 201).is_err());
        assert!(check_kill_target(&rows, 100, 1).is_err());
        assert!(check_kill_target(&rows, 100, 0).is_err());
        assert!(check_kill_target(&rows, 100, -5).is_err());
        assert!(check_kill_target(&rows, 1, 100).is_err());
    }

    #[test]
    fn formats_like_the_electron_app() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(12 * 1024 * 1024), "12 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_percent(0.42), "0.4%");
        assert_eq!(format_percent(12.6), "13%");
        assert_eq!(format_percent(f32::NAN), "0%");
    }

    #[test]
    fn meter_thresholds() {
        assert_eq!(meter(10., 100.), (10., Level::Ok));
        assert_eq!(meter(65., 100.).1, Level::Ok);
        assert_eq!(meter(66., 100.).1, Level::Warn);
        assert_eq!(meter(85., 100.).1, Level::Warn);
        assert_eq!(meter(86., 100.).1, Level::Hot);
        assert_eq!(meter(500., 100.), (100., Level::Hot));
        assert_eq!(meter(5., 0.), (0., Level::Ok));
    }

    #[test]
    fn finds_a_port_this_process_listens_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        let s = summarize(&scan(true), me);
        assert!(s.ports.iter().any(|p| p.port == port && p.pids == [me]), "{port} not in {:?}", s.ports);
        assert!(s.cpu_seconds > 0.);
    }

    #[test]
    fn scans_this_machine() {
        let scan = scan(true);
        assert!(scan.supported);
        let me = std::process::id() as i32;
        let s = summarize(&scan, me);
        assert!(s.process_count >= 1 && s.rss_bytes > 0);
    }
}
