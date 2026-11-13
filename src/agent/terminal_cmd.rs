//! `sg-terminal` commands (the Electron app's useTerminalAgentBridge): an
//! agent lists, reads and drives the terminals of ITS OWN workspace, and opens
//! or closes panes in its zone. Pane ids are container ids; the calling
//! terminal is `$SPLITGRID_TERMINAL`, the container its shell runs in.

use gpui_kit::{Context, Window};
use serde_json::{Map, Value, json};

use super::zones::{self, OpenArgs, ZoneOwner, ZoneSplit};
use crate::app::AppView;
use crate::model::workspace::{Container, ContainerContent, ContentType, Workspace, new_id};
use crate::terminal::view::TerminalView;

const DEFAULT_TAIL_LINES: usize = 200;

/// The wire shape agents read: `{ok, ...data, error?}`.
pub(super) fn reply(ok: bool, error: Option<&str>, data: Value) -> Value {
    let mut out = Map::new();
    out.insert("ok".into(), ok.into());
    if let Value::Object(m) = data {
        out.extend(m);
    }
    if let Some(e) = error {
        out.insert("error".into(), e.into());
    }
    Value::Object(out)
}

pub(super) fn ok(data: Value) -> Value {
    reply(true, None, data)
}

pub(super) fn fail(error: &str, message: impl Into<String>) -> Value {
    reply(false, Some(error), json!({ "message": message.into() }))
}

fn help() -> Value {
    ok(json!({
        "usage": "splitgrid-terminal <command> [args]   (acts on terminals in YOUR workspace only)",
        "commands": [
            "list                         — terminals in this workspace (id, label, status, cwd)",
            "read <id> [--tail N]         — recent output of a terminal (default last 200 lines)",
            "info <id>                    — a terminal's label, status, cwd, shell, type",
            "tree <id>                    — the terminal's live process tree (local only)",
            "send <id> <text...>          — type text + Enter (runs it as a command)",
            "type <id> <text...>          — type text WITHOUT Enter",
            "key  <id> <name>             — send a key: enter|tab|esc|up|down|left|right|",
            "                               home|end|pageup|pagedown|backspace|delete|space|",
            "                               ctrl-c|ctrl-d|ctrl-z|ctrl-l|ctrl-<letter>",
            "open [label]                 — open a new LOCAL terminal in your zone",
            "open ssh <connection>        — open an SSH terminal from a saved connection, in your zone",
            "     [--split right|down]    — where the new pane goes (default: alternates)",
            "     [--at <id>]             — which of your zone's panes to split (default: the last one)",
            "     [--background|--bg]     — a background process (dev server, watcher): goes in the",
            "                               thin tray along your zone's bottom, side by side",
            "zone                         — your zone's panes, in order",
            "connections                  — list saved SSH connections (name + host) for `open ssh`",
            "background <id>              — collapse a terminal of your zone into its tray (keeps running)",
            "foreground <id>              — bring a tray terminal of your zone back out as a regular pane",
            "close <id>                   — close a terminal in your zone (kills it + its pane)",
            "--zone <name>                — on open/close/background/foreground/list/zone: use your own zone when you are a",
            "                               subagent (they share the parent's terminal); default: main",
        ],
        "zones": "The first pane you open splits your own terminal and starts your zone; every later pane is placed inside it. Panes outside your zone can be read and typed into, but not closed.",
    }))
}

/// A key name → the bytes a PTY expects (named keys, ctrl-<letter>, ^x).
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let k = name.to_lowercase();
    let named: &[u8] = match k.as_str() {
        "enter" | "return" => b"\n",
        "tab" => b"\t",
        "esc" | "escape" => b"\x1b",
        "space" => b" ",
        "backspace" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        _ => {
            let letter = k.strip_prefix("ctrl-").or_else(|| k.strip_prefix("ctrl+")).or_else(|| k.strip_prefix('^'))?;
            let mut chars = letter.chars();
            return match (chars.next(), chars.next()) {
                (Some(c @ 'a'..='z'), None) => Some(vec![c as u8 - b'a' + 1]),
                _ => None,
            };
        }
    };
    Some(named.to_vec())
}

#[derive(Debug, Default, PartialEq)]
pub struct ZoneFlags {
    pub zone: Option<String>,
    pub at: Option<String>,
    pub split: Option<String>,
    pub background: bool,
}

/// Pull `--zone`, `--at`, `--split` (also `--flag=value`) and `--background` out of the args.
pub fn take_zone_flags(args: &[String]) -> (ZoneFlags, Vec<String>) {
    let mut flags = ZoneFlags::default();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--background" || a == "--bg" {
            flags.background = true;
            continue;
        }
        let (name, inline) = match a.strip_prefix("--").map(|f| f.split_once('=').map_or((f, None), |(n, v)| (n, Some(v.to_owned())))) {
            Some((n @ ("zone" | "at" | "split"), v)) => (n, v),
            _ => {
                rest.push(a.clone());
                continue;
            }
        };
        let value = inline.or_else(|| it.next().cloned());
        match name {
            "zone" => flags.zone = value,
            "at" => flags.at = value,
            _ => flags.split = value,
        }
    }
    (flags, rest)
}

/// `--tail N` / `-n N` / `--tail=N`.
fn take_tail(args: &[String]) -> Option<usize> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--tail" || a == "-n" {
            return it.next()?.parse().ok();
        }
        if let Some(v) = a.strip_prefix("--tail=") {
            return v.parse().ok();
        }
    }
    None
}

pub(super) fn valid_zone_name(name: &str) -> bool {
    (1..=40).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_terminal(c: &Container) -> bool {
    c.content.kind == ContentType::Terminal
}

/// Custom name, else the pane's label, else what runs in it (zsh, claude…).
pub(super) fn label_of(c: &Container, process: Option<&str>) -> String {
    c.content
        .custom_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(c.content.label.as_deref())
        .or(process)
        .unwrap_or("Terminal")
        .to_owned()
}

pub(super) fn zone_panes(ws: &Workspace, owner: &ZoneOwner) -> Value {
    let layout = zones::zone_layout(ws.layout_tree.as_ref(), owner);
    let entry = |id: &String, background: bool| {
        let kind = ws.container(id).map(|c| c.content.kind.wire_name()).unwrap_or("empty");
        if background { json!({ "id": id, "type": kind, "background": true }) } else { json!({ "id": id, "type": kind }) }
    };
    Value::Array(layout.main.iter().map(|id| entry(id, false)).chain(layout.tray.iter().map(|id| entry(id, true))).collect())
}

impl AppView {
    /// Run one command for the agent in pane `terminal`.
    pub(crate) fn run_terminal_command(&mut self, terminal: &str, argv: &[String], window: &mut Window, cx: &mut Context<Self>) -> Value {
        let cmd = argv.first().map(|s| s.to_lowercase()).unwrap_or_default();
        if matches!(cmd.as_str(), "" | "help" | "--help" | "-h") {
            return help();
        }
        // Zone flags only on the commands that place or remove panes; `send`/`type` text passes untouched.
        let (flags, rest) = if matches!(cmd.as_str(), "open" | "close" | "background" | "foreground" | "list" | "zone") {
            take_zone_flags(&argv[1..])
        } else {
            (ZoneFlags::default(), argv[1..].to_vec())
        };

        // The caller's workspace: the one holding its terminal.
        let Some(ws_ix) = self.state.workspaces.iter().position(|w| w.container(terminal).is_some()) else {
            return fail("unknown_terminal", "cannot locate the calling terminal's workspace");
        };
        let owner = ZoneOwner::new(terminal, flags.zone.as_deref());

        match cmd.as_str() {
            "zone" => {
                let ws = &self.state.workspaces[ws_ix];
                ok(json!({ "zone": owner.name, "panes": zone_panes(ws, &owner) }))
            }
            "list" => {
                let ws = &self.state.workspaces[ws_ix];
                let in_zone = zones::zone_pane_ids(ws.layout_tree.as_ref(), &owner);
                let terminals: Vec<Value> = ws
                    .containers
                    .iter()
                    .filter(|c| is_terminal(c))
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "label": label_of(c, self.process_name(&c.id)),
                            "type": c.content.terminal_type.as_deref().unwrap_or("local"),
                            "status": self.terminal_status(&c.id, cx),
                            "cwd": self.terminal_cwd(c),
                            "mine": c.id == terminal,
                            "inZone": in_zone.contains(&c.id),
                        })
                    })
                    .collect();
                ok(json!({ "workspace": ws.name, "zone": owner.name, "terminals": terminals }))
            }
            "connections" | "conns" => {
                let store = self.connections.lock().unwrap();
                let list: Vec<Value> = store
                    .connections
                    .iter()
                    .map(|c| json!({ "id": c.id, "label": c.label, "host": c.host, "username": c.username, "port": c.port }))
                    .collect();
                ok(json!({ "connections": list }))
            }
            "open" => self.agent_open(ws_ix, terminal, &owner, &flags, &rest, window, cx),
            _ => self.agent_on_target(ws_ix, terminal, &owner, &cmd, &rest, cx),
        }
    }

    fn terminal_status(&self, id: &str, cx: &Context<Self>) -> &'static str {
        use crate::terminal::RemoteStatus;
        let Some(pane) = self.panes.get(id) else {
            return if self.ssh_errors.contains_key(id) { "error" } else { "disconnected" };
        };
        match pane.view.read(cx).remote_status() {
            None | Some(RemoteStatus::Connected) => "connected",
            Some(RemoteStatus::Connecting) => "connecting",
            Some(RemoteStatus::Failed(_)) => "error",
            Some(RemoteStatus::Closed { .. }) => "disconnected",
        }
    }

    pub(super) fn process_name(&self, id: &str) -> Option<&str> {
        self.panes.get(id).and_then(|p| p.process.as_ref()).map(|p| p.name.as_str())
    }

    fn terminal_cwd(&self, c: &Container) -> Option<String> {
        self.panes.get(&c.id).and_then(|p| p.process.as_ref()?.cwd.clone()).or_else(|| c.content.cwd.clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn agent_open(
        &mut self,
        ws_ix: usize,
        terminal: &str,
        owner: &ZoneOwner,
        flags: &ZoneFlags,
        rest: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Value {
        if let Some(z) = &flags.zone
            && !valid_zone_name(z)
        {
            return fail("bad_zone", "--zone takes a short name: letters, digits, \"-\", \"_\" or \".\" (up to 40)");
        }
        let split = match ZoneSplit::parse(flags.split.as_deref()) {
            None => None,
            Some(Some(s)) => Some(s),
            Some(None) => return fail("bad_split", "--split takes right or down"),
        };
        let ws = &self.state.workspaces[ws_ix];
        if let Some(at) = &flags.at {
            let layout = zones::zone_layout(ws.layout_tree.as_ref(), owner);
            let pool = if flags.background { &layout.tray } else { &layout.main };
            if !pool.contains(at) {
                let panes = zone_panes(ws, owner);
                let message = if panes.as_array().is_some_and(|a| !a.is_empty()) {
                    format!("--at must be one of your zone's {} panes", if flags.background { "tray" } else { "regular" })
                } else {
                    "your zone has no panes yet; open one without --at".to_owned()
                };
                return reply(false, Some("not_in_zone"), json!({ "message": message, "zone": owner.name, "panes": panes }));
            }
        }
        let caller_label = ws.container(terminal).map(|c| label_of(c, self.process_name(terminal))).unwrap_or_else(|| "Agent".into());
        let zone_label = if owner.name == zones::DEFAULT_ZONE { caller_label } else { format!("{caller_label} · {}", owner.name) };

        // What to open: `ssh <connection>`, else a local shell with an optional label.
        let ssh = rest.first().is_some_and(|w| w.eq_ignore_ascii_case("ssh"));
        let (content, conn) = if ssh {
            let query = rest[1..].join(" ").trim().to_owned();
            if query.is_empty() {
                return fail("missing_connection", "usage: open ssh <connection name|id>  (run: connections)");
            }
            let store = self.connections.lock().unwrap();
            let q = query.to_lowercase();
            let found = store
                .connections
                .iter()
                .find(|c| c.id == query)
                .or_else(|| store.connections.iter().find(|c| c.label.to_lowercase() == q))
                .or_else(|| store.connections.iter().find(|c| c.label.to_lowercase().contains(&q)))
                .cloned();
            let Some(conn) = found else {
                let available: Vec<&str> = store.connections.iter().map(|c| c.label.as_str()).collect();
                return reply(
                    false,
                    Some("unknown_connection"),
                    json!({ "message": "no saved SSH connection matches; run: connections", "query": query, "available": available }),
                );
            };
            (ContainerContent::ssh_terminal(&conn.id, &conn.label), Some(conn))
        } else {
            let words: Vec<&String> = rest.iter().skip_while(|w| w.eq_ignore_ascii_case("local")).collect();
            let label = words.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ").trim().to_owned();
            let mut content = ContainerContent::local_terminal(ws.working_directory.clone());
            content.label = (!label.is_empty()).then_some(label);
            (content, None)
        };

        let id = new_id("c");
        let ws = &mut self.state.workspaces[ws_ix];
        let args = OpenArgs {
            owner,
            label: &zone_label,
            new_id: &id,
            anchor_id: Some(terminal),
            at: flags.at.as_deref(),
            split,
            background: flags.background,
        };
        match zones::open_in_zone(ws.layout_tree.take(), &args) {
            Ok(tree) => ws.layout_tree = Some(tree),
            Err(message) => return fail("not_in_zone", message),
        }
        let label = content.label.clone();
        let cwd = content.cwd.clone();
        ws.containers.push(Container { id: id.clone(), content });
        ws.updated_at = crate::model::workspace::now_ms();

        // Start it now, even in a workspace that isn't on screen.
        match &conn {
            Some(c) => self.connect_saved(id.clone(), c.id.clone(), window, cx),
            None => {
                let env = self.agent_env(&id);
                match TerminalView::spawn(cwd.map(Into::into), env) {
                    Ok(spawned) => self.add_pane(id.clone(), spawned, None, window, cx),
                    Err(e) => return fail("open_failed", format!("{e:#}")),
                }
            }
        }
        self.changed(cx);

        let mut data = json!({
            "id": id,
            "label": label.unwrap_or_else(|| "Terminal".into()),
            "type": if conn.is_some() { "ssh" } else { "local" },
            "opened": true,
            "zone": owner.name,
        });
        if let Some(c) = &conn {
            data["connection"] = c.label.clone().into();
        }
        if flags.background {
            data["background"] = true.into();
        }
        ok(data)
    }

    fn agent_on_target(&mut self, ws_ix: usize, terminal: &str, owner: &ZoneOwner, cmd: &str, rest: &[String], cx: &mut Context<Self>) -> Value {
        let Some(target) = rest.first().cloned() else {
            return fail("missing_id", format!("usage: {cmd} <id> …  (run: list)"));
        };
        let ws = &self.state.workspaces[ws_ix];
        let Some(container) = ws.container(&target).filter(|c| is_terminal(c)).cloned() else {
            return reply(false, Some("out_of_scope"), json!({ "message": "no such terminal in your workspace; run: list", "id": target }));
        };
        let args = &rest[1..];
        let in_zone = zones::zone_pane_ids(ws.layout_tree.as_ref(), owner).contains(&target);
        let not_in_zone = |what: &str| {
            reply(false, Some("not_in_zone"), json!({ "message": what, "id": target, "zone": owner.name, "panes": zone_panes(ws, owner) }))
        };

        match cmd {
            "info" => ok(json!({
                "id": target,
                "label": label_of(&container, self.process_name(&target)),
                "type": container.content.terminal_type.as_deref().unwrap_or("local"),
                "status": self.terminal_status(&target, cx),
                "cwd": self.terminal_cwd(&container),
                "shell": container.content.shell.clone().or_else(|| std::env::var("SHELL").ok()),
                "mine": target == terminal,
            })),
            "read" => {
                let text = self.panes.get(&target).map(|p| p.view.read(cx).text()).unwrap_or_default();
                let lines: Vec<&str> = text.split('\n').collect();
                let n = take_tail(args).filter(|n| *n > 0).unwrap_or(DEFAULT_TAIL_LINES);
                let slice = &lines[lines.len().saturating_sub(n)..];
                ok(json!({ "id": target, "lines": slice.len(), "truncated": lines.len() > slice.len(), "output": slice.join("\n") }))
            }
            "tree" => {
                let pid = self.panes.get(&target).and_then(|p| p.view.read(cx).shell_pid());
                let processes = pid.map(crate::terminal::procinfo::tree).unwrap_or_default();
                ok(json!({ "id": target, "processes": processes }))
            }
            "send" | "type" | "key" => {
                let Some(pane) = self.panes.get(&target) else {
                    return reply(false, Some("not_running"), json!({ "message": "that terminal isn't running", "id": target }));
                };
                let text = args.join(" ");
                let (bytes, data) = match cmd {
                    "send" => (format!("{text}\n").into_bytes(), json!({ "id": target, "sent": text })),
                    "type" if text.is_empty() => return fail("missing_text", "usage: type <id> <text...>"),
                    "type" => (text.clone().into_bytes(), json!({ "id": target, "typed": text })),
                    _ => {
                        let Some(name) = args.first() else {
                            return fail("missing_key", "usage: key <id> <name>  e.g. key <id> ctrl-c");
                        };
                        let Some(bytes) = key_bytes(name) else { return fail("unknown_key", format!("unknown key: {name}")) };
                        (bytes, json!({ "id": target, "key": name.to_lowercase() }))
                    }
                };
                pane.view.read(cx).write_input(bytes);
                ok(data)
            }
            "close" => {
                if !in_zone {
                    return not_in_zone("only panes in your zone can be closed; ask the user to close this one");
                }
                self.close_pane(&target, cx);
                ok(json!({ "id": target, "closed": true, "mine": target == terminal }))
            }
            "background" | "foreground" => {
                if !in_zone {
                    return not_in_zone("only panes in your zone can be moved to or from its tray");
                }
                let to_tray = cmd == "background";
                let moved = zones::move_zone_pane(ws.layout_tree.as_ref(), &target, to_tray, Some(terminal));
                let did_move = moved.is_some();
                if let Some(tree) = moved {
                    self.state.workspaces[ws_ix].layout_tree = Some(tree);
                    self.changed(cx);
                }
                ok(json!({ "id": target, "background": to_tray, "moved": did_move, "zone": owner.name }))
            }
            _ => fail("unknown_command", format!("unknown command: {cmd}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn keys() {
        assert_eq!(key_bytes("ctrl-c"), Some(vec![3]));
        assert_eq!(key_bytes("^A"), Some(vec![1]));
        assert_eq!(key_bytes("Enter"), Some(b"\n".to_vec()));
        assert_eq!(key_bytes("ctrl-1"), None);
        assert_eq!(key_bytes("hyper"), None);
    }

    #[test]
    fn flags() {
        let (f, rest) = take_zone_flags(&s(&["ssh", "prod", "--zone", "review", "--split=down", "--bg", "--at", "c-1"]));
        assert_eq!(rest, s(&["ssh", "prod"]));
        assert_eq!(f, ZoneFlags { zone: Some("review".into()), at: Some("c-1".into()), split: Some("down".into()), background: true });
        assert_eq!(take_tail(&s(&["--tail", "40"])), Some(40));
        assert_eq!(take_tail(&s(&["--tail=5"])), Some(5));
        assert!(valid_zone_name("e2e-login") && !valid_zone_name("a b") && !valid_zone_name(""));
    }

    #[test]
    fn reply_shape() {
        assert_eq!(fail("x", "m"), json!({ "ok": false, "error": "x", "message": "m" }));
        assert_eq!(ok(json!({ "id": "a" })), json!({ "ok": true, "id": "a" }));
    }
}
