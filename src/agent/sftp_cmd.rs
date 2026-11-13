//! `sg-sftp` commands (the Electron app's agent-sftp-bridge +
//! useSftpAgentBridge + shared/agent-sftp-paths): an agent moves files between
//! this machine and the remote hosts of ITS OWN workspace through
//! `$SPLITGRID_SFTP_CLI`, with SplitGrid's saved credentials (which never reach
//! the shell).
//!
//! Gating is Electron's: nothing runs unless Agent integrations AND
//! `agentSftpControl` are on (`sftp_disabled`, checked before anything else);
//! commands that change a remote (`send push sync mkdir mv rm`) also need
//! `agentSftpWrite` (`write_not_allowed`). Both are re-read on every call.
//!
//! Targets are the workspace's sync targets (remote paths confined to their
//! remotePath) then the hosts of its SSH terminal panes (absolute paths only).
//! Local paths are confined to the working directory. Connections are made
//! unattended: a changed host key or a missing saved password fails with a
//! message, never a prompt (as in Electron).

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{AsyncApp, Context, WeakEntity};
use serde_json::{Value, json};

use super::terminal_cmd::{fail, reply};
use super::{AgentConfig, server};
use crate::app::AppView;
use crate::model::connections::SavedConnection;
use crate::model::workspace::Workspace;
use crate::sftp_sync::{self, Connections, Job, SyncConfig};

/// Electron's bridge budget; the helper waits 190s.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// Largest file `cat` returns (Electron's MAX_EDIT_SIZE).
pub const MAX_CAT_SIZE: u64 = 5 * 1024 * 1024;

/// Commands that change something on a remote host.
pub const WRITE_COMMANDS: [&str; 6] = ["send", "push", "sync", "mkdir", "mv", "rm"];

// ─── Gating + env ────────────────────────────────────────────────────────────

/// (SFTP access, writes allowed) from app-settings.json, as Electron arms them:
/// access needs the master switch, writes need access.
pub fn switches(cfg: &AgentConfig) -> (bool, bool) {
    let v: Value = std::fs::read(&cfg.settings_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let on = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let control = on("agentIntegrations") && on("agentSftpControl");
    (control, control && on("agentSftpWrite"))
}

/// The SPLITGRID_SFTP_* env of a new terminal (Electron's names).
pub fn add_env(cfg: &AgentConfig, env: &mut std::collections::HashMap<String, String>) {
    let (control, write) = switches(cfg);
    if !control {
        return;
    }
    env.insert("SPLITGRID_SFTP_ENDPOINT".into(), format!("http://127.0.0.1:{}/sftp", cfg.port));
    env.insert("SPLITGRID_SFTP_CLI".into(), cfg.helpers.join("splitgrid-sftp.sh").to_string_lossy().into_owned());
    env.insert("SPLITGRID_SFTP_TOKEN".into(), cfg.token.clone());
    if write {
        env.insert("SPLITGRID_SFTP_WRITE".into(), "1".into());
    }
}

/// The main-side gate (agent-sftp-bridge's processSftpCommand).
pub fn gate(control: bool, argv: &[String]) -> Result<(), Value> {
    if !control {
        return Err(fail("sftp_disabled", "SFTP agent access is disabled. Ask the user to enable it in Settings → Agent integrations."));
    }
    if argv.is_empty() {
        return Err(fail("empty_command", "usage: splitgrid-sftp <cmd> [args]"));
    }
    Ok(())
}

// ─── argv ────────────────────────────────────────────────────────────────────

/// The command and its args, with a few spellings accepted for Electron's names
/// (`hosts`→targets, `read`→cat, `download`→get, `upload`→send, `rename`→mv,
/// `delete`→rm, `sync now|push|pull|status`).
pub fn normalize(argv: &[String]) -> (String, Vec<String>) {
    let mut cmd = argv.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let mut rest = argv.get(1..).unwrap_or_default().to_vec();
    if cmd == "sync"
        && let Some(sub) = rest.first().map(|s| s.to_lowercase())
        && ["now", "push", "pull", "status"].contains(&sub.as_str())
    {
        rest.remove(0);
        cmd = if sub == "now" { "sync".into() } else { sub };
    }
    let cmd = match cmd.as_str() {
        "hosts" => "targets",
        "read" => "cat",
        "download" => "get",
        "upload" => "send",
        "rename" => "mv",
        "delete" => "rm",
        "--help" | "-h" | "" => "help",
        c => c,
    }
    .to_owned();
    (cmd, rest)
}

/// Pull `--target <name|n>` / `-t` / `--target=` out of the args.
pub fn take_target(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut rest = Vec::new();
    let mut target = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--target" || a == "-t" {
            target = it.next().cloned();
        } else if let Some(v) = a.strip_prefix("--target=").filter(|v| !v.is_empty()) {
            target = Some(v.to_owned());
        } else {
            rest.push(a.clone());
        }
    }
    (target, rest)
}

pub fn take_flag(args: &[String], flag: &str) -> (bool, Vec<String>) {
    let rest: Vec<String> = args.iter().filter(|a| *a != flag).cloned().collect();
    (rest.len() != args.len(), rest)
}

// ─── Paths (shared/agent-sftp-paths, POSIX) ──────────────────────────────────

/// Collapse `.`, `..` and repeated slashes; leading `..` kept so escapes show.
fn normalize_segments<'a>(segs: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for s in segs {
        match s {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    out
}

/// An agent's LOCAL path resolved against the workspace root, or None when it
/// escapes it (or there is no root). Relative paths join the root; absolute
/// ones must already be under it.
pub fn resolve_local(root: Option<&str>, input: &str) -> Option<String> {
    let root = root.filter(|r| !r.is_empty())?;
    if input.is_empty() || input.contains('\0') {
        return None;
    }
    let root_segs = normalize_segments(root.split('/'));
    if root_segs.contains(&"..") {
        return None;
    }
    let segs = if input.starts_with('/') {
        normalize_segments(input.split('/'))
    } else {
        normalize_segments(root.split('/').chain(input.split('/')))
    };
    if segs.len() < root_segs.len() || segs[..root_segs.len()] != root_segs[..] || segs.contains(&"..") {
        return None;
    }
    Some(format!("/{}", segs.join("/")))
}

/// An agent's REMOTE path. With a root, relative paths join it and the result
/// must stay inside; without one (an SSH-pane host) only absolute (or `~`) paths.
pub fn resolve_remote(root: Option<&str>, input: &str) -> Option<String> {
    if input.is_empty() || input.contains('\0') {
        return None;
    }
    let Some(root) = root.filter(|r| !r.is_empty()) else {
        if input.starts_with('~') {
            return Some(input.to_owned());
        }
        if !input.starts_with('/') {
            return None;
        }
        return Some(format!("/{}", normalize_segments(input.split('/')).join("/")));
    };
    let root_segs = normalize_segments(root.split('/'));
    let segs = if input.starts_with('/') {
        normalize_segments(input.split('/'))
    } else {
        normalize_segments(root.split('/').chain(input.split('/')))
    };
    if segs.contains(&"..") || segs.len() < root_segs.len() || segs[..root_segs.len()] != root_segs[..] {
        return None;
    }
    Some(format!("/{}", segs.join("/")))
}

pub fn remote_basename(p: &str) -> &str {
    p.split('/').rfind(|s| !s.is_empty()).unwrap_or("")
}

// ─── Targets ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AgentTarget {
    pub name: String,
    /// "sync" | "ssh"
    pub kind: &'static str,
    pub connection_id: String,
    pub host: String,
    pub username: Option<String>,
    pub remote_root: Option<String>,
    pub enabled: bool,
}

/// Every host this workspace reaches: its sync targets, then its SSH panes' hosts.
pub fn targets_of(ws: &Workspace, sync: Option<&SyncConfig>, saved: &[SavedConnection]) -> Vec<AgentTarget> {
    let conn = |id: &str| saved.iter().find(|c| c.id == id);
    let mut out: Vec<AgentTarget> = Vec::new();
    for t in sync.map(|s| s.targets.as_slice()).unwrap_or_default() {
        let Some(cid) = t.connection_id.as_deref().filter(|c| !c.is_empty()) else { continue };
        let c = conn(cid);
        let name = [Some(t.name.clone()), c.map(|c| c.label.clone())].into_iter().flatten().find(|n| !n.is_empty()).unwrap_or_else(|| t.id.clone());
        out.push(AgentTarget {
            name,
            kind: "sync",
            connection_id: cid.to_owned(),
            host: c.map(|c| c.host.clone()).unwrap_or_else(|| "unknown".into()),
            username: c.map(|c| c.username.clone()),
            remote_root: Some(t.remote_path.clone()).filter(|p| !p.is_empty()),
            enabled: t.enabled && sync.is_some_and(|s| s.enabled),
        });
    }
    for c in &ws.containers {
        if !c.content.is_ssh_terminal() {
            continue;
        }
        let Some(cid) = c.content.connection_id.as_deref().filter(|c| !c.is_empty()) else { continue };
        if out.iter().any(|t| t.connection_id == cid) {
            continue;
        }
        let sc = conn(cid);
        let name = [sc.map(|s| s.label.clone()), c.content.label.clone()].into_iter().flatten().find(|n| !n.is_empty()).unwrap_or_else(|| cid.to_owned());
        out.push(AgentTarget {
            name,
            kind: "ssh",
            connection_id: cid.to_owned(),
            host: sc.map(|s| s.host.clone()).unwrap_or_else(|| "unknown".into()),
            username: sc.map(|s| s.username.clone()),
            remote_root: None,
            enabled: true,
        });
    }
    out
}

/// `--target` by 1-based number, exact name, host, or name substring.
pub fn pick_target(targets: &[AgentTarget], want: Option<&str>) -> Result<AgentTarget, Value> {
    let usable: Vec<&AgentTarget> = targets.iter().filter(|t| t.enabled).collect();
    let names = || usable.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    if usable.is_empty() {
        return Err(fail("no_targets", "this workspace has no remote host: configure a sync target or open an SSH terminal in it"));
    }
    let Some(want) = want else {
        if usable.len() == 1 {
            return Ok(usable[0].clone());
        }
        return Err(reply(false, Some("ambiguous_target"), json!({ "message": "several hosts available — pass --target <name>", "targets": names() })));
    };
    if let Ok(n) = want.parse::<usize>()
        && (1..=usable.len()).contains(&n)
    {
        return Ok(usable[n - 1].clone());
    }
    let q = want.to_lowercase();
    usable
        .iter()
        .find(|t| t.name.to_lowercase() == q)
        .or_else(|| usable.iter().find(|t| t.host.to_lowercase() == q))
        .or_else(|| usable.iter().find(|t| t.name.to_lowercase().contains(&q)))
        .map(|t| (*t).clone())
        .ok_or_else(|| reply(false, Some("unknown_target"), json!({ "message": "no such host in this workspace; run: targets", "query": want, "targets": names() })))
}

// ─── Planning (pure) ─────────────────────────────────────────────────────────

/// The caller's workspace, as a command sees it.
pub struct Ctx {
    pub workspace: Workspace,
    pub sync: Option<SyncConfig>,
    pub saved: Vec<SavedConnection>,
}

impl Ctx {
    fn root(&self) -> Option<String> {
        self.workspace.working_directory.as_deref().filter(|d| !d.is_empty()).map(crate::grid::expand_tilde)
    }
}

/// One host operation.
#[derive(Debug, PartialEq)]
pub enum HostOp {
    Ls(String),
    Stat(String),
    Cat(String),
    Get { remote: String, local_dir: String },
    Send { locals: Vec<String>, remote_dir: String },
    Mkdir(String),
    Mv(String, String),
    Rm(String),
}

#[derive(Debug, PartialEq)]
pub enum Op {
    /// Answered without a connection.
    Reply(Value),
    Sync,
    Push(Vec<String>),
    Pull(Vec<String>),
    Host { target: AgentTarget, op: HostOp },
}

fn help(write_allowed: bool) -> Value {
    json!({
        "ok": true,
        "usage": "splitgrid-sftp <command> [args]   (acts on the remote hosts of YOUR workspace)",
        "commands": [
            "targets                      — hosts you can transfer to/from + the default one",
            "status                       — workspace sync config + last sync per target",
            "ls <remote> [--target N]     — list a remote directory",
            "stat <remote>                — one remote entry (size, mtime, dir?)",
            "cat <remote>                 — read a remote text file",
            "get <remote> [localdir]      — download a file/dir into the workspace",
            "send <local...> [remotedir]  — upload paths to the remote  (write)",
            "push <path...>               — sync-push workspace paths to every target (write)",
            "pull <path...>               — sync-pull workspace paths from the target",
            "sync                         — run the full workspace sync now (write)",
            "mkdir <remote>               — create a remote directory (write)",
            "mv <old> <new>               — rename/move on the remote (write)",
            "rm <remote> --force          — delete a remote path (write, needs --force)",
        ],
        "notes": [
            "Local paths are relative to the workspace directory and cannot leave it.",
            "Remote paths on a sync target are relative to its remotePath; on an SSH-pane target give an absolute path.",
            "Pick a host with --target <name|number> when `targets` lists more than one.",
        ],
        "writeAllowed": write_allowed,
    })
}

fn local_out_of_scope(path: &str, root: Option<&str>, message: &str) -> Value {
    reply(false, Some("path_out_of_scope"), json!({ "message": message, "path": path, "workspaceDir": root }))
}

/// Decide what a command does — target, confinement, write gate — without any I/O.
pub fn prepare(ctx: &Ctx, argv: &[String], write_allowed: bool) -> Result<Op, Value> {
    let (cmd, raw) = normalize(argv);
    if cmd == "help" {
        return Ok(Op::Reply(help(write_allowed)));
    }
    if WRITE_COMMANDS.contains(&cmd.as_str()) && !write_allowed {
        return Err(reply(
            false,
            Some("write_not_allowed"),
            json!({ "message": format!("\"{cmd}\" writes to a remote host; SFTP access is read-only. Ask the user to enable writes in Settings → Agent integrations.") }),
        ));
    }
    let ws = &ctx.workspace;
    let root = ctx.root();
    let targets = targets_of(ws, ctx.sync.as_ref(), &ctx.saved);

    match cmd.as_str() {
        "targets" => {
            let enabled: Vec<&AgentTarget> = targets.iter().filter(|t| t.enabled).collect();
            let list: Vec<Value> = targets
                .iter()
                .enumerate()
                .map(|(i, t)| json!({ "n": i + 1, "name": t.name, "kind": t.kind, "host": t.host, "user": t.username, "remoteRoot": t.remote_root, "enabled": t.enabled }))
                .collect();
            return Ok(Op::Reply(json!({
                "ok": true, "workspace": ws.name, "workspaceDir": root, "writeAllowed": write_allowed,
                "targets": list, "default": (enabled.len() == 1).then(|| enabled[0].name.clone()),
            })));
        }
        "status" => {
            let sync = ctx.sync.clone().unwrap_or_default();
            let st: Vec<Value> = sync
                .targets
                .iter()
                .map(|t| json!({ "name": t.name, "enabled": t.enabled, "remotePath": t.remote_path,
                                 "lastSyncAt": t.last_sync_at, "lastSyncStatus": t.last_sync_status, "lastSyncError": t.last_sync_error }))
                .collect();
            return Ok(Op::Reply(json!({
                "ok": true, "workspace": ws.name, "workspaceDir": root, "writeAllowed": write_allowed,
                "syncEnabled": sync.enabled, "useGitIgnore": sync.use_git_ignore, "syncTargets": st,
                "autoSync": sync.watchable() && root.is_some(),
                "syncRunning": sftp_sync::is_running(&ws.id),
                "sshTargets": targets.iter().filter(|t| t.kind == "ssh").map(|t| t.name.clone()).collect::<Vec<_>>(),
            })));
        }
        "push" | "pull" | "sync" => {
            if !ctx.sync.as_ref().is_some_and(SyncConfig::configured) {
                return Err(fail("sync_not_configured", "workspace sync is not configured/enabled; use `send`/`get` with --target instead"));
            }
            if root.is_none() {
                return Err(fail("no_workspace_dir", "this workspace has no working directory, so local paths cannot be resolved"));
            }
            if cmd == "sync" {
                return Ok(Op::Sync);
            }
            if raw.is_empty() {
                return Err(fail("missing_path", format!("usage: {cmd} <path...>  (paths are relative to the workspace directory)")));
            }
            let mut paths = Vec::new();
            for p in &raw {
                paths.push(resolve_local(root.as_deref(), p).ok_or_else(|| local_out_of_scope(p, root.as_deref(), "local paths must stay inside the workspace directory"))?);
            }
            return Ok(if cmd == "push" { Op::Push(paths) } else { Op::Pull(paths) });
        }
        "ls" | "stat" | "cat" | "get" | "send" | "mkdir" | "mv" | "rm" => {}
        _ => {
            let mut v = help(write_allowed);
            v["ok"] = false.into();
            v["error"] = "unknown_command".into();
            v["command"] = cmd.clone().into();
            return Err(v);
        }
    }

    let (want, args) = take_target(&raw);
    let target = pick_target(&targets, want.as_deref())?;
    let rroot = target.remote_root.clone();
    let out_of_scope = |p: &str| {
        let data = match &rroot {
            Some(r) => json!({ "message": "remote paths must stay inside this target's remote root", "path": p, "remoteRoot": r }),
            None => json!({ "message": "give an absolute remote path for an SSH-pane target (it has no configured remote root)", "path": p }),
        };
        reply(false, Some("remote_out_of_scope"), data)
    };
    let remote = |p: &str| resolve_remote(rroot.as_deref(), p).ok_or_else(|| out_of_scope(p));
    let need = |n: usize, usage: &str| if args.len() < n { Err(fail("missing_path", usage)) } else { Ok(()) };

    let op = match cmd.as_str() {
        "ls" => HostOp::Ls(remote(args.first().map(String::as_str).unwrap_or(rroot.as_deref().unwrap_or("")))?),
        "stat" => {
            need(1, "usage: stat <remote>")?;
            HostOp::Stat(remote(&args[0])?)
        }
        "cat" => {
            need(1, "usage: cat <remote>")?;
            HostOp::Cat(remote(&args[0])?)
        }
        "get" => {
            need(1, "usage: get <remote> [local-dir]")?;
            let r = remote(&args[0])?;
            let dir = args.get(1).map(String::as_str).unwrap_or(".");
            let local_dir = resolve_local(root.as_deref(), dir)
                .ok_or_else(|| local_out_of_scope(dir, root.as_deref(), "the download directory must be inside the workspace directory"))?;
            HostOp::Get { remote: r, local_dir }
        }
        "send" => {
            need(1, "usage: send <local...> [remote-dir]")?;
            let (locals, dir) = if args.len() > 1 { (&args[..args.len() - 1], Some(args[args.len() - 1].as_str())) } else { (&args[..], None) };
            let remote_dir = remote(dir.unwrap_or(rroot.as_deref().unwrap_or("")))?;
            let mut out = Vec::new();
            for p in locals {
                out.push(resolve_local(root.as_deref(), p).ok_or_else(|| local_out_of_scope(p, root.as_deref(), "local paths must stay inside the workspace directory"))?);
            }
            HostOp::Send { locals: out, remote_dir }
        }
        "mkdir" => {
            need(1, "usage: mkdir <remote>")?;
            HostOp::Mkdir(remote(&args[0])?)
        }
        "mv" => {
            need(2, "usage: mv <old-remote> <new-remote>")?;
            HostOp::Mv(remote(&args[0])?, remote(&args[1])?)
        }
        _ => {
            let (force, rest) = take_flag(&args, "--force");
            let Some(p) = rest.first() else { return Err(fail("missing_path", "usage: rm <remote> --force")) };
            if !force {
                return Err(reply(
                    false,
                    Some("force_required"),
                    json!({ "message": "deleting on a remote host is irreversible — repeat with --force if you mean it", "path": p }),
                ));
            }
            HostOp::Rm(remote(p)?)
        }
    };
    Ok(Op::Host { target, op })
}

// ─── Execution ───────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": e.to_string() })
}

/// Run one host operation over the workspace's agent session to that host.
async fn run_host(ws_id: String, target: AgentTarget, op: HostOp, connections: Connections) -> Value {
    let spec = match sftp_sync::conn_spec(&connections, &target.connection_id) {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let key = format!("{ws_id}:agent-sftp:{}", target.connection_id);
    let t = target.name.clone();
    // `retry!(c, [a, b] => body)`: run `body` on the session with `a`, `b` as references.
    macro_rules! retry {
        ($c:ident, [$($v:ident),*] => $body:expr) => {{
            $(let $v = &$v;)*
            sftp_sync::with_retry(&key, &spec, |$c| async move { $body }).await
        }};
    }
    match op {
        HostOp::Ls(p) => match retry!(c, [p] => c.list(p).await) {
            Ok(entries) => json!({ "ok": true, "target": t, "path": p,
                "entries": entries.iter().map(|e| json!({ "name": e.name, "dir": e.is_dir, "size": e.size, "mtime": e.modified.unwrap_or(0) })).collect::<Vec<_>>() }),
            Err(e) => err(format!("{e:#}")),
        },
        HostOp::Stat(p) => {
            let parent = crate::sftp::parent(&p);
            let base = remote_basename(&p).to_owned();
            match retry!(c, [parent] => c.list(parent).await) {
                Ok(entries) => match entries.into_iter().find(|e| e.name == base) {
                    Some(e) => json!({ "ok": true, "target": t, "path": p, "dir": e.is_dir, "size": e.size, "mtime": e.modified.unwrap_or(0), "mode": e.mode.unwrap_or(0) }),
                    None => json!({ "ok": false, "error": "not_found", "target": t, "path": p }),
                },
                Err(e) => err(format!("{e:#}")),
            }
        }
        HostOp::Cat(p) => {
            let res = retry!(c, [p] => {
                let st = c.stat(p).await?;
                if st.size > MAX_CAT_SIZE {
                    return Ok((String::new(), st.size, true, false));
                }
                let bytes = c.read(p).await?;
                if bytes.contains(&0) {
                    return Ok((String::new(), st.size, false, true));
                }
                Ok((String::from_utf8_lossy(&bytes).into_owned(), st.size, false, false))
            });
            match res {
                Ok((content, size, truncated, binary)) => {
                    json!({ "ok": true, "target": t, "path": p, "content": content, "size": size, "truncated": truncated, "isBinary": binary })
                }
                Err(e) => err(format!("{e:#}")),
            }
        }
        HostOp::Get { remote, local_dir } => {
            // Probe whether it's a folder: listing a file fails.
            let is_dir = retry!(c, [remote] => c.list(remote).await.map(|_| ())).is_ok();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let res = retry!(c, [remote, local_dir, cancel] => Ok(c.download_many(vec![(remote.clone(), is_dir)], PathBuf::from(local_dir), &|_| {}, cancel).await));
            match res {
                Ok(r) => json!({ "ok": !r.cancelled, "command": "get", "target": t, "remote": remote, "localDir": local_dir,
                    "local": format!("{local_dir}/{}", remote_basename(&remote)),
                    "transferred": r.transferred, "total": r.total, "errors": r.errors }),
                Err(e) => err(format!("{e:#}")),
            }
        }
        HostOp::Send { locals, remote_dir } => {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let res = retry!(c, [locals, remote_dir, cancel] => {
                // Missing locals are reported per path, as Electron's stat failure is.
                let (present, missing): (Vec<PathBuf>, Vec<PathBuf>) = locals.iter().map(PathBuf::from).partition(|p| p.exists());
                let mut r = crate::sftp::TransferResult::default();
                if !present.is_empty() {
                    // Electron's upload makes the destination folder (mkdir -p) first.
                    c.ensure_dir_all(remote_dir).await;
                    r = c.upload_many(present, remote_dir.clone(), &|_| {}, cancel).await;
                }
                r.errors.extend(missing.iter().map(|p| format!("{}: no such file or directory", p.display())));
                Ok(r)
            });
            match res {
                Ok(r) => json!({ "ok": !r.cancelled, "command": "send", "target": t, "remoteDir": remote_dir, "locals": locals,
                    "transferred": r.transferred, "total": r.total, "errors": r.errors }),
                Err(e) => err(format!("{e:#}")),
            }
        }
        HostOp::Mkdir(p) => match retry!(c, [p] => c.mkdir(p).await) {
            Ok(()) => json!({ "ok": true, "command": "mkdir", "target": t, "path": p }),
            Err(e) => err(format!("{e:#}")),
        },
        HostOp::Mv(from, to) => match retry!(c, [from, to] => c.rename(from, to).await) {
            Ok(()) => json!({ "ok": true, "command": "mv", "target": t, "from": from, "to": to }),
            Err(e) => err(format!("{e:#}")),
        },
        HostOp::Rm(p) => match retry!(c, [p] => c.remove_if_exists(p).await) {
            Ok(()) => json!({ "ok": true, "command": "rm", "target": t, "path": p, "deleted": true }),
            Err(e) => err(format!("{e:#}")),
        },
    }
}

/// Flatten a sync result into the agent's reply (`{ ok, command, ...result }`).
fn sync_reply(cmd: &str, paths: Option<&[String]>, result: Value) -> Value {
    let mut out = json!({ "ok": true, "command": cmd });
    if let Some(p) = paths {
        out["paths"] = json!(p);
    }
    if let (Value::Object(o), Value::Object(r)) = (&mut out, result) {
        o.extend(r);
    }
    out
}

impl AppView {
    /// A `POST /sftp` from an agent: authorised, gated, then planned here and run off the UI thread.
    pub(crate) fn handle_sftp_request(&mut self, req: server::Request, cfg: &AgentConfig, cx: &mut Context<Self>) {
        if !cfg.is_token(&req.body["token"]) {
            req.respond(json!({ "ok": false, "error": "unauthorized" }));
            return;
        }
        let (control, write) = switches(cfg);
        let argv: Vec<String> = req.body["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        if let Err(v) = gate(control, &argv) {
            req.respond(v);
            return;
        }
        let terminal = req.body["terminal"].as_str().unwrap_or_default();
        let ws = self.state.workspaces.iter().find(|w| w.container(terminal).is_some()).or(match self.state.workspaces.as_slice() {
            [only] => Some(only),
            _ => None,
        });
        let Some(ws) = ws else {
            req.respond(fail("unknown_terminal", "cannot locate the calling terminal's workspace"));
            return;
        };
        let saved = {
            let mut store = self.connections.lock().unwrap();
            store.sync_from_disk();
            store.connections.clone()
        };
        let ctx = Ctx { workspace: ws.clone(), sync: SyncConfig::of(ws), saved };
        let op = match prepare(&ctx, &argv, write) {
            Ok(Op::Reply(v)) | Err(v) => {
                req.respond(v);
                return;
            }
            Ok(op) => op,
        };
        let connections = self.connections.clone();
        let ws_id = ctx.workspace.id.clone();
        let job = ctx.sync.clone().and_then(|s| Job::new(&ctx.workspace, s, connections.clone()));
        cx.spawn(async move |this: WeakEntity<AppView>, cx: &mut AsyncApp| {
            let is_sync = matches!(op, Op::Sync);
            let ran = ws_id.clone();
            let work = async move {
                match op {
                    Op::Sync => {
                        let job = job.expect("checked by prepare");
                        let Some(_running) = sftp_sync::begin(&ws_id, job.cancel.clone()) else {
                            return fail("sync_running", "a sync of this workspace is already running");
                        };
                        sync_reply("sync", None, sftp_sync::sync_now(job).await)
                    }
                    Op::Push(paths) => {
                        let job = job.expect("checked by prepare");
                        let r = sftp_sync::push_paths(job, paths.iter().map(PathBuf::from).collect()).await;
                        sync_reply("push", Some(&paths), r)
                    }
                    Op::Pull(paths) => {
                        let job = job.expect("checked by prepare");
                        let r = sftp_sync::pull_paths(job, paths.iter().map(PathBuf::from).collect()).await;
                        sync_reply("pull", Some(&paths), r)
                    }
                    Op::Host { target, op } => run_host(ws_id, target, op, connections).await,
                    Op::Reply(v) => v,
                }
            };
            let value = crate::sftp::call(async move {
                Ok(tokio::time::timeout(REQUEST_TIMEOUT, work).await.unwrap_or_else(|_| json!({ "ok": false, "error": "timeout" })))
            })
            .await
            .unwrap_or_else(|e| err(format!("{e:#}")));
            if is_sync && value["ok"].as_bool() == Some(true) {
                let v = value.clone();
                let _ = this.update(cx, |this, cx| this.record_sync_now(&ran, &v, cx));
            }
            req.respond(value);
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::{Container, ContainerContent};
    use crate::sftp_sync::SyncTarget;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn conn(id: &str, label: &str, host: &str) -> SavedConnection {
        serde_json::from_value(json!({ "id": id, "label": label, "host": host, "port": 22, "username": "me", "authMethod": "password" })).unwrap()
    }

    fn ctx(sync: Option<SyncConfig>, ssh: &[&str]) -> Ctx {
        let mut ws = Workspace::new("proj", Some("/work/proj".into()));
        for id in ssh {
            ws.containers.push(Container { id: format!("c-{id}"), content: ContainerContent::ssh_terminal(id, "pane") });
        }
        Ctx { workspace: ws, sync, saved: vec![conn("c1", "prod", "prod.example"), conn("c2", "stage", "stage.example")] }
    }

    fn sync_cfg() -> SyncConfig {
        SyncConfig {
            enabled: true,
            targets: vec![SyncTarget { id: "t1".into(), name: "".into(), enabled: true, connection_id: Some("c1".into()), remote_path: "/srv/app".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn gate_refuses_everything_when_disabled() {
        for argv in [s(&["help"]), s(&["ls", "/srv"]), s(&["targets"])] {
            assert_eq!(gate(false, &argv).unwrap_err()["error"], "sftp_disabled");
        }
        assert_eq!(gate(true, &[]).unwrap_err()["error"], "empty_command");
        assert!(gate(true, &s(&["targets"])).is_ok());
    }

    #[test]
    fn switches_need_the_master_and_access_switches() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::new(19568, dir.path());
        let set = |v: &str| std::fs::write(dir.path().join("app-settings.json"), v).unwrap();
        assert_eq!(switches(&cfg), (false, false));
        set(r#"{"agentSftpControl":true,"agentSftpWrite":true}"#);
        assert_eq!(switches(&cfg), (false, false), "master switch off");
        set(r#"{"agentIntegrations":true,"agentSftpWrite":true}"#);
        assert_eq!(switches(&cfg), (false, false), "write without access");
        set(r#"{"agentIntegrations":true,"agentSftpControl":true}"#);
        assert_eq!(switches(&cfg), (true, false));
        let mut env = Default::default();
        add_env(&cfg, &mut env);
        assert_eq!(env["SPLITGRID_SFTP_ENDPOINT"], "http://127.0.0.1:19568/sftp");
        assert!(env["SPLITGRID_SFTP_CLI"].ends_with("splitgrid-sftp.sh"));
        assert_eq!(env["SPLITGRID_SFTP_TOKEN"], cfg.token);
        assert!(!env.contains_key("SPLITGRID_SFTP_WRITE"));
        set(r#"{"agentIntegrations":true,"agentSftpControl":true,"agentSftpWrite":true}"#);
        let mut env = Default::default();
        add_env(&cfg, &mut env);
        assert_eq!(env["SPLITGRID_SFTP_WRITE"], "1");
    }

    #[test]
    fn write_commands_need_write() {
        let c = ctx(Some(sync_cfg()), &[]);
        for argv in [s(&["send", "a"]), s(&["push", "a"]), s(&["sync"]), s(&["mkdir", "x"]), s(&["mv", "a", "b"]), s(&["rm", "x", "--force"]), s(&["upload", "a"]), s(&["sync", "now"])] {
            assert_eq!(prepare(&c, &argv, false).unwrap_err()["error"], "write_not_allowed", "{argv:?}");
        }
        for argv in [s(&["ls"]), s(&["stat", "x"]), s(&["cat", "x"]), s(&["get", "x"]), s(&["pull", "a"]), s(&["targets"]), s(&["status"]), s(&["sync", "status"])] {
            assert!(prepare(&c, &argv, false).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn argv_aliases_and_flags() {
        assert_eq!(normalize(&s(&["sync", "now"])), ("sync".into(), vec![]));
        assert_eq!(normalize(&s(&["sync", "push", "a"])), ("push".into(), s(&["a"])));
        assert_eq!(normalize(&s(&["HOSTS"])).0, "targets");
        assert_eq!(normalize(&s(&["download", "x"])).0, "get");
        assert_eq!(normalize(&s(&[])).0, "help");
        assert_eq!(take_target(&s(&["ls", "--target", "prod", "/x"])), (Some("prod".into()), s(&["ls", "/x"])));
        assert_eq!(take_target(&s(&["-t", "2", "/x"])), (Some("2".into()), s(&["/x"])));
        assert_eq!(take_target(&s(&["--target=web", "/x"])), (Some("web".into()), s(&["/x"])));
        assert_eq!(take_flag(&s(&["x", "--force"]), "--force"), (true, s(&["x"])));
    }

    #[test]
    fn local_paths_are_confined() {
        let r = Some("/work/proj");
        assert_eq!(resolve_local(r, "dist/app.js").as_deref(), Some("/work/proj/dist/app.js"));
        assert_eq!(resolve_local(r, ".").as_deref(), Some("/work/proj"));
        assert_eq!(resolve_local(r, "/work/proj/a").as_deref(), Some("/work/proj/a"));
        assert_eq!(resolve_local(r, "a/../../proj/b").as_deref(), Some("/work/proj/b"));
        for bad in ["../x", "/etc/passwd", "/work/projx/a", "a/../../..", "", "a\0b"] {
            assert_eq!(resolve_local(r, bad), None, "{bad}");
        }
        assert_eq!(resolve_local(None, "a"), None);
        // `~` is not expanded: it is a folder name under the root.
        assert_eq!(resolve_local(r, "~/.ssh/id_rsa").as_deref(), Some("/work/proj/~/.ssh/id_rsa"));
    }

    #[test]
    fn remote_paths_are_confined_to_a_sync_root() {
        let r = Some("/srv/app");
        assert_eq!(resolve_remote(r, "x/y").as_deref(), Some("/srv/app/x/y"));
        assert_eq!(resolve_remote(r, "/srv/app").as_deref(), Some("/srv/app"));
        assert_eq!(resolve_remote(r, "/srv/app/./a//b").as_deref(), Some("/srv/app/a/b"));
        assert_eq!(resolve_remote(r, "../etc"), None);
        assert_eq!(resolve_remote(r, "/etc/passwd"), None);
        assert_eq!(resolve_remote(r, "/srv/application"), None);
        // SSH-pane hosts: absolute (or ~) only.
        assert_eq!(resolve_remote(None, "/var/log/x").as_deref(), Some("/var/log/x"));
        assert_eq!(resolve_remote(None, "~/x").as_deref(), Some("~/x"));
        assert_eq!(resolve_remote(None, "rel"), None);
        assert_eq!(remote_basename("/a/b/"), "b");
        assert_eq!(remote_basename("/"), "");
    }

    #[test]
    fn targets_are_sync_targets_then_ssh_panes() {
        let c = ctx(Some(sync_cfg()), &["c1", "c2"]);
        let t = targets_of(&c.workspace, c.sync.as_ref(), &c.saved);
        assert_eq!(t.len(), 2, "c1's pane is covered by its sync target");
        assert_eq!((t[0].name.as_str(), t[0].kind, t[0].remote_root.as_deref()), ("prod", "sync", Some("/srv/app")));
        assert_eq!((t[1].name.as_str(), t[1].kind, t[1].remote_root.as_deref()), ("stage", "ssh", None));
        assert_eq!(pick_target(&t, None).unwrap_err()["error"], "ambiguous_target");
        assert_eq!(pick_target(&t, Some("2")).unwrap().name, "stage");
        assert_eq!(pick_target(&t, Some("PROD")).unwrap().name, "prod");
        assert_eq!(pick_target(&t, Some("stage.example")).unwrap().name, "stage");
        assert_eq!(pick_target(&t, Some("sta")).unwrap().name, "stage");
        assert_eq!(pick_target(&t, Some("nope")).unwrap_err()["error"], "unknown_target");
        assert_eq!(pick_target(&[], None).unwrap_err()["error"], "no_targets");

        // A disabled workspace sync leaves its targets listed but unusable.
        let mut off = sync_cfg();
        off.enabled = false;
        let c = ctx(Some(off), &[]);
        let t = targets_of(&c.workspace, c.sync.as_ref(), &c.saved);
        assert!(!t[0].enabled);
        assert_eq!(pick_target(&t, None).unwrap_err()["error"], "no_targets");
    }

    #[test]
    fn host_commands_resolve_their_paths() {
        let c = ctx(Some(sync_cfg()), &[]);
        let host = |argv: &[&str]| match prepare(&c, &s(argv), true) {
            Ok(Op::Host { op, .. }) => Ok(op),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e["error"].as_str().unwrap().to_owned()),
        };
        assert_eq!(host(&["ls"]), Ok(HostOp::Ls("/srv/app".into())));
        assert_eq!(host(&["cat", "conf.yml"]), Ok(HostOp::Cat("/srv/app/conf.yml".into())));
        assert_eq!(host(&["get", "logs/a.log"]), Ok(HostOp::Get { remote: "/srv/app/logs/a.log".into(), local_dir: "/work/proj".into() }));
        assert_eq!(host(&["get", "x", "../.."]), Err("path_out_of_scope".into()));
        assert_eq!(host(&["send", "dist/a.js"]), Ok(HostOp::Send { locals: vec!["/work/proj/dist/a.js".into()], remote_dir: "/srv/app".into() }));
        assert_eq!(
            host(&["send", "a", "b", "releases"]),
            Ok(HostOp::Send { locals: vec!["/work/proj/a".into(), "/work/proj/b".into()], remote_dir: "/srv/app/releases".into() })
        );
        assert_eq!(host(&["send", "/etc/hosts", "x"]), Err("path_out_of_scope".into()));
        assert_eq!(host(&["mv", "a", "/tmp/b"]), Err("remote_out_of_scope".into()));
        assert_eq!(host(&["rm", "a"]), Err("force_required".into()));
        assert_eq!(host(&["rm", "a", "--force"]), Ok(HostOp::Rm("/srv/app/a".into())));
        assert_eq!(host(&["stat"]), Err("missing_path".into()));
        assert_eq!(host(&["frobnicate"]), Err("unknown_command".into()));
    }

    #[test]
    fn sync_commands_need_a_configured_sync() {
        assert_eq!(prepare(&ctx(None, &["c2"]), &s(&["push", "a"]), true).unwrap_err()["error"], "sync_not_configured");
        let c = ctx(Some(sync_cfg()), &[]);
        assert_eq!(prepare(&c, &s(&["sync"]), true), Ok(Op::Sync));
        assert_eq!(prepare(&c, &s(&["push", "src", "README.md"]), true), Ok(Op::Push(s(&["/work/proj/src", "/work/proj/README.md"]))));
        assert_eq!(prepare(&c, &s(&["pull"]), true).unwrap_err()["error"], "missing_path");
        assert_eq!(prepare(&c, &s(&["pull", "../x"]), true).unwrap_err()["error"], "path_out_of_scope");
        let mut no_dir = ctx(Some(sync_cfg()), &[]);
        no_dir.workspace.working_directory = None;
        assert_eq!(prepare(&no_dir, &s(&["sync"]), true).unwrap_err()["error"], "no_workspace_dir");
    }

    #[test]
    fn targets_and_status_replies() {
        let c = ctx(Some(sync_cfg()), &["c2"]);
        let Ok(Op::Reply(v)) = prepare(&c, &s(&["targets"]), false) else { panic!() };
        assert_eq!(v["targets"][1]["kind"], "ssh");
        assert_eq!(v["default"], Value::Null);
        let Ok(Op::Reply(v)) = prepare(&ctx(Some(sync_cfg()), &[]), &s(&["hosts"]), false) else { panic!() };
        assert_eq!(v["default"], "prod");
        let Ok(Op::Reply(v)) = prepare(&c, &s(&["status"]), true) else { panic!() };
        assert_eq!((v["syncEnabled"].as_bool(), v["writeAllowed"].as_bool()), (Some(true), Some(true)));
        assert_eq!(v["syncTargets"][0]["remotePath"], "/srv/app");
        assert_eq!(v["sshTargets"], json!(["stage"]));
    }
}
