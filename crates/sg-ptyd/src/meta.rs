//! What a session is (for humans): which pane of which workspace of which
//! environment it belongs to. Lite sets it at spawn and keeps it current
//! (renames) with `SET_META`; the daemon holds it, so the menu-bar helper can
//! name sessions while Lite is closed.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    /// Lite's container (pane) id: the session's identity across restarts.
    pub container_id: String,
    pub env_id: String,
    pub env_name: String,
    pub workspace_id: String,
    pub workspace_name: String,
    /// The pane's title (its label, else the terminal's title).
    pub title: String,
}

impl Meta {
    /// Merge a partial update: fields present in `patch` win.
    pub fn merge(&mut self, patch: &serde_json::Value) {
        let Some(obj) = patch.as_object() else { return };
        let set = |field: &mut String, key: &str| {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                *field = v.to_owned();
            }
        };
        set(&mut self.container_id, "containerId");
        set(&mut self.env_id, "envId");
        set(&mut self.env_name, "envName");
        set(&mut self.workspace_id, "workspaceId");
        set(&mut self.workspace_name, "workspaceName");
        set(&mut self.title, "title");
    }
}

/// The foreground job of a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Foreground {
    pub pid: i32,
    /// Short name ("claude", "vim", "zsh").
    pub name: String,
    /// The command line, as `ps` would show it.
    pub command: String,
}

/// `INFO`'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionInfo {
    pub protocol: u16,
    pub daemon_version: String,
    /// The daemon's own pid.
    pub daemon_pid: i32,
    /// The shell's pid.
    pub pid: i32,
    pub cols: u16,
    pub rows: u16,
    /// A client is attached (Lite shows it).
    pub attached: bool,
    pub created_ms: u64,
    pub cwd: Option<String>,
    pub foreground: Option<Foreground>,
    pub meta: Meta,
}
