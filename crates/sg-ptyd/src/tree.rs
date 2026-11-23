//! Sessions grouped for the menu: environment → workspace → session.

use crate::meta::SessionInfo;

#[derive(Clone, Debug, PartialEq)]
pub struct EnvGroup {
    pub id: String,
    pub name: String,
    pub workspaces: Vec<WorkspaceGroup>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceGroup {
    pub id: String,
    pub name: String,
    pub sessions: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The session's socket (how to reach it).
    pub socket: std::path::PathBuf,
    /// "Terminal — claude" style label.
    pub label: String,
    pub info: SessionInfo,
}

impl EnvGroup {
    pub fn count(&self) -> usize {
        self.workspaces.iter().map(|w| w.sessions.len()).sum()
    }
}

/// A foreground job with this name is the shell at its prompt (or `login`
/// starting it), not a job running in it.
pub fn is_shell(name: &str) -> bool {
    ["zsh", "bash", "sh", "fish", "login", "tcsh", "csh", "ksh", "dash", "nu"].contains(&name.trim_start_matches('-'))
}

/// The pane title and what runs in it: "api — npm run dev", "Terminal — claude".
pub fn session_label(info: &SessionInfo) -> String {
    let title = Some(info.meta.title.trim()).filter(|t| !t.is_empty()).unwrap_or("Terminal");
    match &info.foreground {
        Some(fg) => {
            // A shell at its prompt is just the title; a job shows its command.
            if is_shell(&fg.name) {
                title.to_owned()
            } else {
                let what = if fg.command.is_empty() || fg.command.len() > 60 { fg.name.clone() } else { fg.command.clone() };
                format!("{title} — {what}")
            }
        }
        None => title.to_owned(),
    }
}

/// Group sessions by environment, then workspace; names sorted (case-blind),
/// sessions without metadata under "Unknown".
pub fn group(sessions: Vec<(std::path::PathBuf, SessionInfo)>) -> Vec<EnvGroup> {
    let mut envs: Vec<EnvGroup> = Vec::new();
    for (socket, info) in sessions {
        let m = &info.meta;
        let env_name =
            Some(m.env_name.trim()).filter(|s| !s.is_empty()).unwrap_or(if m.env_id.is_empty() { "Unknown environment" } else { &m.env_id }).to_owned();
        let ws_name = Some(m.workspace_name.trim()).filter(|s| !s.is_empty()).unwrap_or("Unknown workspace").to_owned();
        let env_key = if m.env_id.is_empty() { env_name.clone() } else { m.env_id.clone() };
        let ws_key = if m.workspace_id.is_empty() { ws_name.clone() } else { m.workspace_id.clone() };
        let env = match envs.iter().position(|e| e.id == env_key) {
            Some(i) => &mut envs[i],
            None => {
                envs.push(EnvGroup { id: env_key, name: env_name, workspaces: Vec::new() });
                envs.last_mut().unwrap()
            }
        };
        let ws = match env.workspaces.iter().position(|w| w.id == ws_key) {
            Some(i) => &mut env.workspaces[i],
            None => {
                env.workspaces.push(WorkspaceGroup { id: ws_key, name: ws_name, sessions: Vec::new() });
                env.workspaces.last_mut().unwrap()
            }
        };
        ws.sessions.push(Item { socket, label: session_label(&info), info });
    }
    for env in &mut envs {
        for ws in &mut env.workspaces {
            ws.sessions.sort_by_key(|i| (i.label.to_lowercase(), i.info.created_ms));
        }
        env.workspaces.sort_by_key(|w| w.name.to_lowercase());
    }
    envs.sort_by_key(|e| e.name.to_lowercase());
    envs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Foreground, Meta};

    fn s(env: (&str, &str), ws: (&str, &str), title: &str, fg: Option<(&str, &str)>) -> (std::path::PathBuf, SessionInfo) {
        let info = SessionInfo {
            meta: Meta {
                container_id: format!("c-{title}"),
                env_id: env.0.into(),
                env_name: env.1.into(),
                workspace_id: ws.0.into(),
                workspace_name: ws.1.into(),
                title: title.into(),
            },
            foreground: fg.map(|(name, command)| Foreground { pid: 1, name: name.into(), command: command.into() }),
            ..Default::default()
        };
        (format!("/s/{title}.sock").into(), info)
    }

    #[test]
    fn groups_env_then_workspace_sorted() {
        let tree = group(vec![
            s(("e2", "Work"), ("w1", "api"), "server", Some(("npm", "npm run dev"))),
            s(("e1", "default"), ("w9", "Notes"), "b", None),
            s(("e2", "Work"), ("w1", "api"), "agent", Some(("claude", "node /x/claude"))),
            s(("e2", "Work"), ("w0", "Billing"), "shell", Some(("zsh", "-zsh"))),
            s(("", ""), ("", ""), "", None),
        ]);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["default", "Unknown environment", "Work"]);
        let work = &tree[2];
        assert_eq!(work.count(), 3);
        let ws: Vec<&str> = work.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(ws, ["api", "Billing"]);
        let labels: Vec<&str> = work.workspaces[0].sessions.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["agent — node /x/claude", "server — npm run dev"]);
        assert_eq!(work.workspaces[1].sessions[0].label, "shell");
        assert_eq!(tree[1].workspaces[0].name, "Unknown workspace");
        assert_eq!(tree[1].workspaces[0].sessions[0].label, "Terminal");
    }

    #[test]
    fn shells_are_told_from_jobs() {
        for n in ["zsh", "-zsh", "bash", "login", "fish"] {
            assert!(is_shell(n), "{n}");
        }
        for n in ["claude", "npm", "vim", "node", "shell-script"] {
            assert!(!is_shell(n), "{n}");
        }
    }

    #[test]
    fn same_name_different_ids_stay_apart() {
        let tree = group(vec![s(("e1", "A"), ("w1", "x"), "1", None), s(("e2", "A"), ("w1", "x"), "2", None)]);
        assert_eq!(tree.len(), 2);
    }
}
