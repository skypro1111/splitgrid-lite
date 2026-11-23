//! What the popover shows, as plain data: the session tree (environment →
//! workspace → session), which rows are visible given the collapsed groups,
//! what a Stop or Close on a row reaches, and when to ask first. No AppKit
//! here, so all of it is unit-tested.

use std::collections::HashSet;
use std::path::PathBuf;

use sg_ptyd::{SessionInfo, tree};

/// Row heights (points) and the gap the outline view adds between rows.
pub const ENV_ROW: f64 = 26.0;
pub const WORKSPACE_ROW: f64 = 24.0;
pub const SESSION_ROW: f64 = 38.0;
pub const ROW_GAP: f64 = 2.0;

/// The status dot of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// A foreground job other than the shell runs (green).
    Busy,
    /// The shell sits at its prompt (grey).
    Idle,
    /// Speaks another protocol: nothing known but that it runs (orange).
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub socket: PathBuf,
    /// The pane title.
    pub title: String,
    /// The secondary line: what runs, where, for how long.
    pub detail: String,
    pub status: Status,
    /// Lite shows it right now.
    pub attached: bool,
    /// The foreground job's short name ("claude"), when busy.
    pub job: Option<String>,
    /// Speaks this build's protocol (Stop and View need that; Close doesn't).
    pub reachable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Env,
    Workspace,
    Session(Session),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable across scans: the outline view's item identity, the collapsed set's key.
    pub key: String,
    pub name: String,
    pub kind: Kind,
    pub children: Vec<Node>,
}

/// A row action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Ctrl-C to the foreground job.
    Stop,
    /// End the session.
    Close,
}

/// The dot for a session: busy unless the foreground job is the shell itself.
pub fn classify(info: &SessionInfo) -> Status {
    match &info.foreground {
        None => Status::Idle,
        Some(fg) if fg.pid == info.pid || tree::is_shell(&fg.name) => Status::Idle,
        Some(_) => Status::Busy,
    }
}

/// "12m", "3h", "2d"; under a minute is "<1m" (so the text changes at most
/// once a minute and the list isn't reloaded every scan for it).
pub fn uptime(created_ms: u64, now_ms: u64) -> String {
    let mins = now_ms.saturating_sub(created_ms) / 60_000;
    match mins {
        0 => "<1m".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h", m / 60),
        m => format!("{}d", m / (24 * 60)),
    }
}

/// `/Users/me/src/x` → `~/src/x`.
pub fn short_path(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty() && *h != "/") {
        Some(h) if path == h => "~".into(),
        Some(h) if path.starts_with(h) && path[h.len()..].starts_with('/') => format!("~{}", &path[h.len()..]),
        _ => path.to_owned(),
    }
}

/// What the foreground job is, readable: the command line when it starts
/// with the job's own name ("npm run dev"), else the name ("claude" for
/// `node /…/claude/cli.js`); long ones cut.
pub fn job_text(name: &str, command: &str) -> String {
    let mut words = command.split_whitespace();
    let first = words.next().map(|w| w.trim_start_matches('-').rsplit('/').next().unwrap_or(w)).unwrap_or("");
    let text = if !first.is_empty() && first == name { std::iter::once(first).chain(words).collect::<Vec<_>>().join(" ") } else { name.to_owned() };
    const MAX: usize = 48;
    if text.chars().count() > MAX { format!("{}…", text.chars().take(MAX - 1).collect::<String>()) } else { text }
}

fn session(socket: PathBuf, info: &SessionInfo, now_ms: u64, home: Option<&str>) -> Session {
    let status = classify(info);
    let job = match (&info.foreground, status) {
        (Some(fg), Status::Busy) => Some(fg.name.clone()),
        _ => None,
    };
    let mut parts = vec![match (&info.foreground, status) {
        (Some(fg), Status::Busy) => job_text(&fg.name, &fg.command),
        _ => "idle".to_owned(),
    }];
    if let Some(cwd) = &info.cwd {
        parts.push(short_path(cwd, home));
    }
    if info.created_ms > 0 {
        parts.push(uptime(info.created_ms, now_ms));
    }
    let title = Some(info.meta.title.trim()).filter(|t| !t.is_empty()).unwrap_or("Terminal").to_owned();
    Session { socket, title, detail: parts.join("  ·  "), status, attached: info.attached, job, reachable: true }
}

/// The tree from a scan. Sessions that answered in another protocol go in a
/// last group of their own: they count, and can be closed, but not stopped.
pub fn build(sessions: Vec<(PathBuf, SessionInfo)>, broken: &[(PathBuf, String)], now_ms: u64, home: Option<&str>) -> Vec<Node> {
    let mut out: Vec<Node> = tree::group(sessions)
        .into_iter()
        .map(|env| {
            let env_key = format!("e:{}", env.id);
            let workspaces = env
                .workspaces
                .into_iter()
                .map(|ws| Node {
                    key: format!("w:{}\u{1f}{}", env.id, ws.id),
                    name: ws.name,
                    kind: Kind::Workspace,
                    children: ws
                        .sessions
                        .into_iter()
                        .map(|item| {
                            let s = session(item.socket, &item.info, now_ms, home);
                            Node { key: format!("s:{}", s.socket.display()), name: s.title.clone(), kind: Kind::Session(s), children: Vec::new() }
                        })
                        .collect(),
                })
                .collect();
            Node { key: env_key, name: env.name, kind: Kind::Env, children: workspaces }
        })
        .collect();
    if !broken.is_empty() {
        let children = broken
            .iter()
            .map(|(socket, why)| {
                let title = socket.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Terminal".into());
                let s = Session { socket: socket.clone(), title: title.clone(), detail: why.clone(), status: Status::Unknown, attached: false, job: None, reachable: false };
                Node { key: format!("s:{}", socket.display()), name: title, kind: Kind::Session(s), children: Vec::new() }
            })
            .collect();
        out.push(Node { key: "x:other".into(), name: "From another SplitGrid version".into(), kind: Kind::Env, children });
    }
    out
}

impl Node {
    pub fn session(&self) -> Option<&Session> {
        match &self.kind {
            Kind::Session(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_group(&self) -> bool {
        !matches!(self.kind, Kind::Session(_))
    }

    /// Every session at or under this node.
    pub fn sessions(&self) -> Vec<&Session> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Session>) {
        match &self.kind {
            Kind::Session(s) => out.push(s),
            _ => self.children.iter().for_each(|c| c.collect(out)),
        }
    }

    pub fn count(&self) -> usize {
        self.sessions().len()
    }

    pub fn busy(&self) -> usize {
        self.sessions().iter().filter(|s| s.status == Status::Busy).count()
    }

    pub fn row_height(&self) -> f64 {
        match self.kind {
            Kind::Env => ENV_ROW,
            Kind::Workspace => WORKSPACE_ROW,
            Kind::Session(_) => SESSION_ROW,
        }
    }
}

/// The sockets an `op` on these nodes reaches: Stop only sessions running a
/// job (and speaking our protocol: SIGNAL needs a hello); Close every session.
pub fn targets<'a>(nodes: impl IntoIterator<Item = &'a Node>, op: Op) -> Vec<PathBuf> {
    nodes
        .into_iter()
        .flat_map(|n| n.sessions())
        .filter(|s| match op {
            Op::Stop => s.reachable && s.status == Status::Busy,
            Op::Close => true,
        })
        .map(|s| s.socket.clone())
        .collect()
}

pub fn find<'a>(nodes: &'a [Node], key: &str) -> Option<&'a Node> {
    nodes.iter().find_map(|n| if n.key == key { Some(n) } else { find(&n.children, key) })
}

pub fn total(nodes: &[Node]) -> usize {
    nodes.iter().map(Node::count).sum()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "terminal" } else { "terminals" }
}

/// A confirmation to ask before `op` on `node`: closing a group, a session
/// with a job still running, or one whose state is unknown (another protocol,
/// or it didn't answer). Stop (it's a Ctrl-C) and closing an idle shell go
/// ahead.
pub fn confirm(node: &Node, op: Op) -> Option<(String, String)> {
    if op != Op::Close {
        return None;
    }
    let end = "The shells and everything running in them are ended. This can't be undone.";
    match &node.kind {
        Kind::Session(s) => match (s.status, &s.job) {
            (Status::Idle, _) => None,
            (_, Some(job)) => Some((
                format!("End “{}”?", s.title),
                format!("{job} is still running in it and will be ended. This can't be undone."),
            )),
            (_, None) => Some((
                format!("End “{}”?", s.title),
                "Its state can't be read; whatever runs in it is ended. This can't be undone."
                    .into(),
            )),
        },
        Kind::Workspace | Kind::Env => {
            let n = node.count();
            let what = if matches!(node.kind, Kind::Env) { "environment" } else { "workspace" };
            Some((format!("End all {n} {} of {what} “{}”?", plural(n), node.name), end.into()))
        }
    }
}

/// Leave out idle shells (nothing runs in them: a prompt, launched and
/// forgotten) and the groups they leave empty; returns their sockets, which
/// "Close all" still ends. Unreachable sessions always stay.
pub fn hide_idle(nodes: Vec<Node>) -> (Vec<Node>, Vec<PathBuf>) {
    fn prune(node: Node, hidden: &mut Vec<PathBuf>) -> Option<Node> {
        match &node.kind {
            Kind::Session(s) if s.reachable && s.status == Status::Idle => {
                hidden.push(s.socket.clone());
                None
            }
            Kind::Session(_) => Some(node),
            Kind::Workspace | Kind::Env => {
                let Node {
                    key,
                    name,
                    kind,
                    children,
                } = node;
                let children: Vec<Node> = children
                    .into_iter()
                    .filter_map(|c| prune(c, hidden))
                    .collect();
                (!children.is_empty()).then_some(Node {
                    key,
                    name,
                    kind,
                    children,
                })
            }
        }
    }
    let mut hidden = Vec::new();
    let nodes = nodes
        .into_iter()
        .filter_map(|n| prune(n, &mut hidden))
        .collect();
    (nodes, hidden)
}

/// The footer's "Close all sessions": always asked; `idle` hidden idle shells end too.
pub fn confirm_all(nodes: &[Node], idle: usize) -> (String, String) {
    let n = total(nodes) + idle;
    let extra = match idle {
        0 => String::new(),
        1 => " (1 of them an idle shell)".into(),
        i => format!(" ({i} of them idle shells)"),
    };
    (
        format!("End all {n} running {}{extra}?", plural(n)),
        "The shells and everything running in them are ended. This can't be undone.".into(),
    )
}

/// The visible rows, depth-first, with their depth: a collapsed group shows,
/// its children don't. Groups are expanded unless their key is in `collapsed`.
pub fn visible<'a>(nodes: &'a [Node], collapsed: &HashSet<String>) -> Vec<(usize, &'a Node)> {
    fn walk<'a>(nodes: &'a [Node], depth: usize, collapsed: &HashSet<String>, out: &mut Vec<(usize, &'a Node)>) {
        for n in nodes {
            out.push((depth, n));
            if n.is_group() && !collapsed.contains(&n.key) {
                walk(&n.children, depth + 1, collapsed, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, collapsed, &mut out);
    out
}

/// Groups in depth-first order, each with whether it should be expanded:
/// what to replay into the outline view after a reload (a parent before its
/// children; children of a collapsed group are skipped, they can't show).
pub fn expansion(nodes: &[Node], collapsed: &HashSet<String>) -> Vec<(String, bool)> {
    fn walk(nodes: &[Node], collapsed: &HashSet<String>, out: &mut Vec<(String, bool)>) {
        for n in nodes.iter().filter(|n| n.is_group()) {
            let open = !collapsed.contains(&n.key);
            out.push((n.key.clone(), open));
            if open {
                walk(&n.children, collapsed, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, collapsed, &mut out);
    out
}

/// Forget collapsed keys of groups that are gone, so a group coming back
/// later starts expanded again.
pub fn prune_collapsed(nodes: &[Node], collapsed: &mut HashSet<String>) {
    fn keys(nodes: &[Node], out: &mut HashSet<String>) {
        for n in nodes.iter().filter(|n| n.is_group()) {
            out.insert(n.key.clone());
            keys(&n.children, out);
        }
    }
    let mut live = HashSet::new();
    keys(nodes, &mut live);
    collapsed.retain(|k| live.contains(k));
}

/// Height of the list's content: the visible rows and their gaps.
pub fn list_height(nodes: &[Node], collapsed: &HashSet<String>) -> f64 {
    visible(nodes, collapsed).iter().map(|(_, n)| n.row_height() + ROW_GAP).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sg_ptyd::meta::{Foreground, Meta};

    const NOW: u64 = 10 * 24 * 3600 * 1000;

    fn info(env: (&str, &str), ws: (&str, &str), title: &str, fg: Option<(i32, &str, &str)>) -> SessionInfo {
        SessionInfo {
            pid: 100,
            created_ms: NOW - 5 * 60_000,
            cwd: Some("/Users/me/src/app".into()),
            meta: Meta { env_id: env.0.into(), env_name: env.1.into(), workspace_id: ws.0.into(), workspace_name: ws.1.into(), title: title.into(), ..Default::default() },
            foreground: fg.map(|(pid, name, command)| Foreground { pid, name: name.into(), command: command.into() }),
            ..Default::default()
        }
    }

    fn sock(n: &str) -> PathBuf {
        format!("/s/{n}.sock").into()
    }

    /// Work/api: server (npm, busy), shell (idle); Work/billing: agent (claude,
    /// busy); Home/notes: notes (idle); plus one of another protocol.
    fn sample() -> Vec<Node> {
        build(
            vec![
                (sock("server"), info(("e2", "Work"), ("w1", "api"), "server", Some((200, "npm", "npm run dev")))),
                (sock("shell"), info(("e2", "Work"), ("w1", "api"), "shell", Some((101, "zsh", "-zsh")))),
                (sock("agent"), info(("e2", "Work"), ("w2", "billing"), "agent", Some((300, "claude", "node /x/claude/cli.js --resume")))),
                (sock("notes"), info(("e1", "Home"), ("w3", "notes"), "notes", None)),
            ],
            &[(sock("old"), "incompatible protocol".into())],
            NOW,
            Some("/Users/me"),
        )
    }

    #[test]
    fn status_dot() {
        let idle_shell = info(("e", "E"), ("w", "W"), "t", Some((101, "zsh", "-zsh")));
        assert_eq!(classify(&idle_shell), Status::Idle);
        // The shell's own pid in the foreground is idle whatever it's called.
        let own = info(("e", "E"), ("w", "W"), "t", Some((100, "weird", "weird")));
        assert_eq!(classify(&own), Status::Idle);
        assert_eq!(classify(&info(("e", "E"), ("w", "W"), "t", None)), Status::Idle);
        let job = info(("e", "E"), ("w", "W"), "t", Some((200, "vim", "vim x")));
        assert_eq!(classify(&job), Status::Busy);
        let login = info(("e", "E"), ("w", "W"), "t", Some((90, "login", "login -pfl me")));
        assert_eq!(classify(&login), Status::Idle);
    }

    #[test]
    fn tree_shape_counts_and_details() {
        let t = sample();
        let names: Vec<&str> = t.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Home", "Work", "From another SplitGrid version"]);
        assert_eq!(total(&t), 5);
        let work = &t[1];
        assert_eq!((work.count(), work.busy()), (3, 2));
        assert_eq!(work.children.iter().map(Node::count).collect::<Vec<_>>(), [2, 1]);
        let api = &work.children[0];
        let server = api.children.iter().find_map(|n| n.session().filter(|s| s.title == "server")).unwrap();
        assert_eq!(server.detail, "npm run dev  ·  ~/src/app  ·  5m");
        assert_eq!(server.status, Status::Busy);
        assert_eq!(server.job.as_deref(), Some("npm"));
        let shell = api.children.iter().find_map(|n| n.session().filter(|s| s.title == "shell")).unwrap();
        assert!(shell.detail.starts_with("idle  ·  "));
        assert_eq!(shell.job, None);
        let agent = work.children[1].children[0].session().unwrap();
        assert!(agent.detail.starts_with("claude  ·  "), "{}", agent.detail);
        let other = &t[2];
        assert_eq!(other.count(), 1);
        let old = other.children[0].session().unwrap();
        assert_eq!((old.status, old.reachable, old.title.as_str()), (Status::Unknown, false, "old"));
    }

    #[test]
    fn idle_shells_are_hidden_with_their_empty_groups() {
        let (t, idle) = hide_idle(sample());
        let names: Vec<&str> = t.iter().map(|n| n.name.as_str()).collect();
        // Home had only an idle shell; the other-protocol one stays (unreachable).
        assert_eq!(names, ["Work", "From another SplitGrid version"]);
        assert_eq!(idle, [sock("notes"), sock("shell")]);
        assert_eq!(total(&t), 3);
        assert_eq!(t[0].children[0].children.len(), 1);
        assert_eq!(
            confirm_all(&t, idle.len()).0,
            "End all 5 running terminals (2 of them idle shells)?"
        );
    }

    #[test]
    fn keys_are_unique_and_stable() {
        let a = sample();
        let b = sample();
        let mut keys = HashSet::new();
        for (_, n) in visible(&a, &HashSet::new()) {
            assert!(keys.insert(n.key.clone()), "duplicate {}", n.key);
        }
        assert_eq!(a, b);
        assert!(find(&a, "s:/s/agent.sock").is_some());
        assert!(find(&a, "w:e2\u{1f}w2").is_some());
        assert!(find(&a, "nope").is_none());
    }

    #[test]
    fn flattening_and_expansion() {
        let t = sample();
        let mut collapsed = HashSet::new();
        // All expanded: 3 envs + 3 workspaces + 5 sessions.
        let rows = visible(&t, &collapsed);
        assert_eq!(rows.len(), 11);
        let depths: Vec<usize> = rows.iter().map(|(d, _)| *d).collect();
        assert_eq!(depths, [0, 1, 2, 0, 1, 2, 2, 1, 2, 0, 1]);
        collapsed.insert("w:e2\u{1f}w1".to_owned());
        assert_eq!(visible(&t, &collapsed).len(), 9);
        collapsed.insert("e:e2".to_owned());
        let rows = visible(&t, &collapsed);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|(_, n)| !n.key.starts_with("w:e2")));
        // Replay order: parents first; children of a collapsed group skipped.
        let exp = expansion(&t, &collapsed);
        assert_eq!(exp, [("e:e1".into(), true), ("w:e1\u{1f}w3".into(), true), ("e:e2".into(), false), ("x:other".into(), true)]);
        // A group gone from the scan is forgotten.
        collapsed.insert("e:gone".into());
        prune_collapsed(&t, &mut collapsed);
        assert_eq!(collapsed.len(), 2);
    }

    #[test]
    fn list_height_follows_visible_rows() {
        let t = sample();
        let mut collapsed = HashSet::new();
        let full = 3.0 * (ENV_ROW + ROW_GAP) + 3.0 * (WORKSPACE_ROW + ROW_GAP) + 5.0 * (SESSION_ROW + ROW_GAP);
        assert_eq!(list_height(&t, &collapsed), full);
        collapsed.insert("x:other".into());
        assert_eq!(list_height(&t, &collapsed), full - SESSION_ROW - ROW_GAP);
        assert_eq!(list_height(&[], &collapsed), 0.0);
    }

    #[test]
    fn group_targets() {
        let t = sample();
        let work = find(&t, "e:e2").unwrap();
        let mut stop = targets([work], Op::Stop);
        stop.sort();
        assert_eq!(stop, [sock("agent"), sock("server")]);
        assert_eq!(targets([work], Op::Close).len(), 3);
        // An idle session has nothing to stop.
        assert!(targets([find(&t, "s:/s/shell.sock").unwrap()], Op::Stop).is_empty());
        // Another protocol: closable, never stoppable.
        let other = find(&t, "x:other").unwrap();
        assert!(targets([other], Op::Stop).is_empty());
        assert_eq!(targets([other], Op::Close), [sock("old")]);
        // Everything.
        assert_eq!(targets(&t, Op::Close).len(), 5);
        assert_eq!(targets(&t, Op::Stop).len(), 2);
    }

    #[test]
    fn confirmations() {
        let t = sample();
        let get = |k: &str| find(&t, k).unwrap();
        assert!(confirm(get("e:e2"), Op::Stop).is_none());
        assert!(confirm(get("s:/s/shell.sock"), Op::Close).is_none(), "an idle shell closes right away");
        let (msg, why) = confirm(get("s:/s/agent.sock"), Op::Close).unwrap();
        assert_eq!(msg, "End “agent”?");
        assert!(why.starts_with("claude is still running"));
        assert_eq!(
            confirm(get("s:/s/old.sock"), Op::Close).unwrap().0,
            "End “old”?",
            "unknown state asks"
        );
        assert_eq!(
            confirm(get("w:e2\u{1f}w1"), Op::Close).unwrap().0,
            "End all 2 terminals of workspace “api”?"
        );
        assert_eq!(
            confirm(get("e:e1"), Op::Close).unwrap().0,
            "End all 1 terminal of environment “Home”?"
        );
        assert_eq!(confirm_all(&t, 0).0, "End all 5 running terminals?");
    }

    #[test]
    fn small_formatters() {
        assert_eq!(uptime(NOW, NOW), "<1m");
        assert_eq!(uptime(NOW - 59 * 60_000, NOW), "59m");
        assert_eq!(uptime(NOW - 3 * 3_600_000, NOW), "3h");
        assert_eq!(uptime(NOW - 3 * 24 * 3_600_000, NOW), "3d");
        assert_eq!(uptime(NOW + 5, NOW), "<1m");
        assert_eq!(short_path("/Users/me", Some("/Users/me")), "~");
        assert_eq!(short_path("/Users/me/x", Some("/Users/me")), "~/x");
        assert_eq!(short_path("/Users/meow", Some("/Users/me")), "/Users/meow");
        assert_eq!(short_path("/tmp", None), "/tmp");
        assert_eq!(job_text("npm", "npm run dev"), "npm run dev");
        assert_eq!(job_text("npm", "/usr/local/bin/npm run dev"), "npm run dev");
        assert_eq!(job_text("claude", "node /x/claude/cli.js"), "claude");
        assert_eq!(job_text("vim", ""), "vim");
        let long = job_text("cargo", &format!("cargo {}", "x".repeat(100)));
        assert_eq!(long.chars().count(), 48);
        assert!(long.ends_with('…'));
    }
}
