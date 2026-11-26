//! Workspaces, their containers and todos — SplitGrid's JSON shapes
//! (shared/types.ts: Workspace, Container, WorkspaceTodo, WorkspaceState).
//!
//! Every struct keeps the fields this build doesn't use in `extra`, so a state
//! file round-trips without losing what the Electron app (or a later phase of
//! this one) stored there: browser URLs, SFTP paths, favourites, sync config…

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::layout::{Direction, LayoutNode};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Short unique id: time + counter, base 36, like SplitGrid's `Date.now().toString(36)` ids.
pub fn new_id(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    format!("{prefix}-{}{}", base36(nanos / 1000), base36(n))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentType {
    Empty,
    Terminal,
    Ide,
    Browser,
    Sql,
    Sftp,
    SshConnect,
    Agent,
    Git,
    #[serde(other)]
    Unknown,
}

impl ContentType {
    /// The name in the JSON (and in the agent CLIs' replies).
    pub fn wire_name(self) -> &'static str {
        match self {
            ContentType::Empty => "empty",
            ContentType::Terminal => "terminal",
            ContentType::Ide => "ide",
            ContentType::Browser => "browser",
            ContentType::Sql => "sql",
            ContentType::Sftp => "sftp",
            ContentType::SshConnect => "sshConnect",
            ContentType::Agent => "agent",
            ContentType::Git => "git",
            ContentType::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ContentType::Empty => "Empty",
            ContentType::Terminal => "Terminal",
            ContentType::Ide => "Editor",
            ContentType::Browser => "Browser",
            ContentType::Sql => "SQL",
            ContentType::Sftp => "SFTP",
            ContentType::SshConnect => "SSH",
            ContentType::Agent => "Claude Code",
            ContentType::Git => "Git",
            ContentType::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerContent {
    #[serde(rename = "type")]
    pub kind: ContentType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    /// "local" | "ssh" | "wsl"…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ContainerContent {
    pub fn local_terminal(cwd: Option<String>) -> Self {
        ContainerContent {
            kind: ContentType::Terminal,
            terminal_id: Some(new_id("term")),
            terminal_type: Some("local".into()),
            connection_id: None,
            label: None,
            custom_name: None,
            cwd,
            shell: None,
            extra: Map::new(),
        }
    }

    /// A container waiting for its type to be picked (what a split creates).
    pub fn empty() -> Self {
        ContainerContent {
            kind: ContentType::Empty,
            terminal_id: None,
            terminal_type: None,
            connection_id: None,
            label: None,
            custom_name: None,
            cwd: None,
            shell: None,
            extra: Map::new(),
        }
    }

    /// An SSH terminal for a saved connection.
    pub fn ssh_terminal(connection_id: &str, label: &str) -> Self {
        ContainerContent {
            kind: ContentType::Terminal,
            terminal_id: Some(new_id("term")),
            terminal_type: Some("ssh".into()),
            connection_id: Some(connection_id.to_owned()),
            label: Some(label.to_owned()),
            ..Self::empty()
        }
    }

    /// A string field this build keeps in `extra` (SFTP pane state: sftpConnectionId, sftpRemotePath, sftpLocalPath).
    pub fn extra_str(&self, key: &str) -> Option<String> {
        self.extra.get(key).and_then(Value::as_str).map(str::to_owned)
    }

    pub fn set_extra_str(&mut self, key: &str, value: Option<String>) {
        match value {
            Some(v) => {
                self.extra.insert(key.to_owned(), Value::from(v));
            }
            None => {
                self.extra.remove(key);
            }
        }
    }

    pub fn is_ssh_terminal(&self) -> bool {
        self.kind == ContentType::Terminal && self.terminal_type.as_deref() == Some("ssh")
    }

    pub fn is_local_terminal(&self) -> bool {
        self.kind == ContentType::Terminal && self.terminal_type.as_deref().is_none_or(|t| t == "local")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub content: ContainerContent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Todo,
    InProgress,
    Done,
}

impl TodoStatus {
    /// Click order: todo → in progress → done → todo.
    pub fn next(self) -> Self {
        match self {
            TodoStatus::Todo => TodoStatus::InProgress,
            TodoStatus::InProgress => TodoStatus::Done,
            TodoStatus::Done => TodoStatus::Todo,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Todo {
    pub id: String,
    pub text: String,
    /// Missing in items saved before statuses existed: `done` decides (see `normalize`).
    #[serde(default)]
    pub status: Option<TodoStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Todo {
    pub fn new(text: impl Into<String>) -> Self {
        Todo {
            id: new_id("todo"),
            text: text.into(),
            status: Some(TodoStatus::Todo),
            done: None,
            created_at: now_ms(),
            extra: Map::new(),
        }
    }

    pub fn status(&self) -> TodoStatus {
        self.status.unwrap_or(if self.done == Some(true) { TodoStatus::Done } else { TodoStatus::Todo })
    }

    pub fn set_status(&mut self, status: TodoStatus) {
        self.status = Some(status);
        self.done = None;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub layout_tree: Option<LayoutNode>,
    #[serde(default)]
    pub containers: Vec<Container>,
    #[serde(default)]
    pub focused_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Workspace {
    /// A new workspace with one local terminal.
    pub fn new(name: impl Into<String>, working_directory: Option<String>) -> Self {
        let container = Container { id: new_id("c"), content: ContainerContent::local_terminal(working_directory.clone()) };
        let now = now_ms();
        let mut extra = Map::new();
        extra.insert("containerZooms".into(), Value::Object(Map::new()));
        Workspace {
            id: new_id("ws"),
            name: name.into(),
            working_directory,
            layout_tree: Some(LayoutNode::leaf(&container.id)),
            focused_container_id: Some(container.id.clone()),
            containers: vec![container],
            notes: None,
            todos: Vec::new(),
            created_at: now,
            updated_at: now,
            extra,
        }
    }

    pub fn container(&self, id: &str) -> Option<&Container> {
        self.containers.iter().find(|c| c.id == id)
    }

    /// A pane's terminal font size (`containerZooms`), when it was zoomed.
    pub fn zoom(&self, container_id: &str) -> Option<f32> {
        self.extra.get("containerZooms")?.get(container_id)?.as_f64().map(|z| z as f32)
    }

    pub fn set_zoom(&mut self, container_id: &str, size: f32) {
        let zooms = self.extra.entry("containerZooms").or_insert_with(|| Value::Object(Map::new()));
        if !zooms.is_object() {
            *zooms = Value::Object(Map::new());
        }
        if let Some(map) = zooms.as_object_mut() {
            map.insert(container_id.to_owned(), Value::from(size as f64));
        }
    }

    /// Back to the default size (Settings → Appearance): the pane keeps no zoom of its own.
    pub fn clear_zoom(&mut self, container_id: &str) {
        if let Some(map) = self.extra.get_mut("containerZooms").and_then(Value::as_object_mut) {
            map.remove(container_id);
        }
    }

    pub fn container_mut(&mut self, id: &str) -> Option<&mut Container> {
        self.containers.iter_mut().find(|c| c.id == id)
    }

    /// Saved connections used here, most recent first (`recentConnectionIds`).
    pub fn recent_connections(&self) -> Vec<String> {
        self.extra
            .get("recentConnectionIds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }

    /// Move a connection to the front of `recentConnectionIds` (max 8, as SplitGrid keeps).
    pub fn record_connection_use(&mut self, connection_id: &str) {
        let mut ids = self.recent_connections();
        ids.retain(|i| i != connection_id);
        ids.insert(0, connection_id.to_owned());
        ids.truncate(8);
        self.extra.insert("recentConnectionIds".into(), Value::from(ids));
    }

    pub fn open_todos(&self) -> usize {
        self.todos.iter().filter(|t| t.status() != TodoStatus::Done).count()
    }

    /// Make the layout and containers agree: drop leaves without a container,
    /// containers without a leaf, and fix the focus. Imported state can be off.
    pub fn normalize(&mut self) {
        let ids: Vec<String> = self.containers.iter().map(|c| c.id.clone()).collect();
        self.layout_tree = self.layout_tree.take().and_then(|t| t.prune(&|id| ids.iter().any(|c| c == id)));
        let leaves = self.leaves();
        // Collapsed panes have no leaf but are kept (model::collapse).
        self.containers.retain(|c| leaves.contains(&c.id) || c.content.extra_str("collapsedInto").is_some());
        self.rehome_collapsed(None);
        let leaves = self.leaves();
        if self.focused_container_id.as_ref().is_none_or(|f| !leaves.contains(f)) {
            self.focused_container_id = leaves.first().cloned();
        }
    }

    /// Split `target` (or the focused pane) with a new empty container, whose
    /// type is picked in it (as in the Electron app); returns its id.
    pub fn split(&mut self, target: Option<&str>, direction: Direction) -> String {
        let container = Container { id: new_id("c"), content: ContainerContent::empty() };
        let id = container.id.clone();
        let target = target.map(str::to_owned).or_else(|| self.focused_container_id.clone());
        let placed = match (&mut self.layout_tree, target) {
            (Some(tree), Some(t)) => tree.split_leaf(&t, &id, direction),
            _ => false,
        };
        if !placed {
            self.layout_tree = Some(LayoutNode::leaf(&id));
        }
        self.containers.push(container);
        self.focused_container_id = Some(id.clone());
        self.updated_at = now_ms();
        id
    }

    /// Close a pane; the focus moves to the first remaining one.
    pub fn close(&mut self, container_id: &str) {
        // Panes collapsed into it move to the neighbour that takes its space.
        let heir = self.layout_tree.as_ref().and_then(|t| t.absorber(container_id));
        self.layout_tree = self.layout_tree.take().and_then(|t| t.remove_leaf(container_id));
        self.containers.retain(|c| c.id != container_id);
        if let Some(map) = self.extra.get_mut("containerZooms").and_then(Value::as_object_mut) {
            map.remove(container_id);
        }
        self.rehome_collapsed(heir.as_deref());
        if self.focused_container_id.as_deref().is_none_or(|f| f == container_id || !self.leaves().iter().any(|l| l == f)) {
            self.focused_container_id = self.layout_tree.as_ref().and_then(|t| t.leaf_ids().into_iter().next());
        }
        self.updated_at = now_ms();
    }
}

/// One environment: a set of workspaces plus environment-wide todos.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    #[serde(default)]
    pub active_workspace_id: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_todos: Vec<Todo>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl WorkspaceState {
    pub fn active(&self) -> Option<&Workspace> {
        let id = self.active_workspace_id.as_deref()?;
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn active_mut(&mut self) -> Option<&mut Workspace> {
        let id = self.active_workspace_id.clone()?;
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    pub fn normalize(&mut self) {
        for ws in &mut self.workspaces {
            ws.normalize();
        }
        if self.active().is_none() {
            self.active_workspace_id = self.workspaces.first().map(|w| w.id.clone());
        }
    }

    /// Move workspace `id` to the place of workspace `onto` (dropped on it in the
    /// sidebar): dragged down it lands after `onto`, dragged up before it.
    pub fn move_workspace(&mut self, id: &str, onto: &str) -> bool {
        let from = self.workspaces.iter().position(|w| w.id == id);
        let to = self.workspaces.iter().position(|w| w.id == onto);
        let (Some(from), Some(to)) = (from, to) else { return false };
        if from == to {
            return false;
        }
        let ws = self.workspaces.remove(from);
        self.workspaces.insert(to, ws);
        true
    }

    /// Move pane `id` (a leaf of its workspace's layout) into workspace `to`,
    /// beside that workspace's focused pane; it keeps its container (and so its
    /// running terminal) and its zoom, and becomes the focused pane there.
    pub fn move_pane(&mut self, id: &str, to: &str) -> bool {
        let Some(from) = self.workspaces.iter().position(|w| w.leaves().iter().any(|l| l == id)) else { return false };
        let Some(dest) = self.workspaces.iter().position(|w| w.id == to) else { return false };
        if from == dest {
            return false;
        }
        let source = &mut self.workspaces[from];
        let Some(container) = source.container(id).cloned() else { return false };
        let zoom = source.zoom(id);
        source.close(id);

        let target = &mut self.workspaces[dest];
        let beside = target.focused_container_id.clone().filter(|f| target.leaves().contains(f));
        let placed = match (&mut target.layout_tree, beside) {
            (Some(tree), Some(f)) => tree.split_leaf(&f, id, Direction::Horizontal),
            _ => false,
        };
        if !placed {
            target.layout_tree = Some(match target.layout_tree.take() {
                Some(tree) => LayoutNode::split(Direction::Horizontal, 0.5, tree, LayoutNode::leaf(id)),
                None => LayoutNode::leaf(id),
            });
        }
        target.containers.push(container);
        if let Some(z) = zoom {
            target.set_zoom(id, z);
        }
        target.focused_container_id = Some(id.to_owned());
        target.updated_at = now_ms();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "version": 1, "savedAt": 1, "activeWorkspaceId": "w1",
      "envTodos": [{"id":"e1","text":"env task","done":true,"createdAt":5}],
      "envPrompts": [{"id":"p","text":"keep me"}],
      "workspaces": [{
        "id": "w1", "name": "Demo", "workingDirectory": null,
        "layoutTree": {"type":"split","direction":"vertical","ratio":0.5,
          "first":{"type":"leaf","containerId":"c1"},
          "second":{"type":"leaf","containerId":"gone"}},
        "containers": [
          {"id":"c1","content":{"type":"terminal","terminalType":"local","cwd":"/tmp","terminalId":"t1"}},
          {"id":"c2","content":{"type":"browser","browserUrl":"https://example.com"}}
        ],
        "containerZooms": {}, "focusedContainerId": "gone",
        "todos": [{"id":"t","text":"a","status":"in_progress","createdAt":1}],
        "createdAt": 1, "updatedAt": 2
      }]
    }"#;

    #[test]
    fn imports_splitgrid_state_and_keeps_unknown_fields() {
        let mut state: WorkspaceState = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(state.env_todos[0].status(), TodoStatus::Done, "legacy `done` migrates");
        let ws = &state.workspaces[0];
        assert_eq!(ws.todos[0].status(), TodoStatus::InProgress);
        assert_eq!(ws.containers[1].content.kind, ContentType::Browser);

        state.normalize();
        let ws = &state.workspaces[0];
        assert_eq!(ws.layout_tree, Some(LayoutNode::leaf("c1")), "leaf without a container is pruned");
        assert_eq!(ws.containers.len(), 1, "container without a leaf is dropped");
        assert_eq!(ws.focused_container_id.as_deref(), Some("c1"));

        let back = serde_json::to_value(&state).unwrap();
        assert_eq!(back["envPrompts"][0]["text"], "keep me");
        assert_eq!(back["workspaces"][0]["containerZooms"], serde_json::json!({}));
        assert_eq!(back["workspaces"][0]["containers"][0]["content"]["terminalId"], "t1");
    }

    #[test]
    fn unknown_container_types_still_load() {
        let c: ContainerContent = serde_json::from_str(r#"{"type":"hologram"}"#).unwrap();
        assert_eq!(c.kind, ContentType::Unknown);
    }

    #[test]
    fn split_adds_an_empty_container_and_close_refocuses() {
        let mut ws = Workspace::new("W", Some("/home".into()));
        let first = ws.focused_container_id.clone().unwrap();
        let second = ws.split(None, Direction::Horizontal);
        assert_eq!(ws.container(&second).unwrap().content.kind, ContentType::Empty);
        assert_eq!(ws.focused_container_id.as_deref(), Some(second.as_str()));
        assert_eq!(ws.layout_tree.as_ref().unwrap().leaf_ids(), [first.clone(), second.clone()]);

        ws.close(&second);
        assert_eq!(ws.focused_container_id.as_deref(), Some(first.as_str()));
        ws.close(&first);
        assert!(ws.layout_tree.is_none() && ws.containers.is_empty());
        // Splitting an empty workspace starts a fresh tree.
        let only = ws.split(None, Direction::Vertical);
        assert_eq!(ws.layout_tree, Some(LayoutNode::leaf(&only)));
    }

    #[test]
    fn zoom_is_kept_per_pane_and_dropped_on_close() {
        let mut ws = Workspace::new("W", None);
        let id = ws.focused_container_id.clone().unwrap();
        assert_eq!(ws.zoom(&id), None);
        ws.set_zoom(&id, 16.);
        assert_eq!(ws.zoom(&id), Some(16.));
        let json = serde_json::to_value(&ws).unwrap();
        assert_eq!(json["containerZooms"][&id], 16.0, "stored where SplitGrid keeps it");
        ws.close(&id);
        assert_eq!(ws.zoom(&id), None);
    }

    #[test]
    fn recent_connections_are_capped_and_deduplicated() {
        let mut ws = Workspace::new("W", None);
        for i in 0..10 {
            ws.record_connection_use(&format!("c{i}"));
        }
        ws.record_connection_use("c5");
        let recent = ws.recent_connections();
        assert_eq!(recent.len(), 8);
        assert_eq!(recent[0], "c5");
        assert_eq!(recent.iter().filter(|r| *r == "c5").count(), 1);
    }

    #[test]
    fn todo_status_cycles() {
        let mut t = Todo::new("x");
        t.set_status(t.status().next());
        assert_eq!(t.status(), TodoStatus::InProgress);
        t.set_status(t.status().next().next());
        assert_eq!(t.status(), TodoStatus::Todo);
    }

    fn two_workspaces() -> WorkspaceState {
        // A: two panes (the second zoomed), B: one, C: none.
        let mut a = Workspace::new("A", None);
        let second = a.split(None, Direction::Horizontal);
        a.set_zoom(&second, 16.);
        let b = Workspace::new("B", None);
        let mut c = Workspace::new("C", None);
        c.layout_tree = None;
        c.containers.clear();
        c.focused_container_id = None;
        WorkspaceState { active_workspace_id: Some(a.id.clone()), workspaces: vec![a, b, c], ..Default::default() }
    }

    #[test]
    fn a_workspace_moves_to_the_place_it_is_dropped_on() {
        let mut s = two_workspaces();
        let ids = |s: &WorkspaceState| s.workspaces.iter().map(|w| w.name.clone()).collect::<Vec<_>>();
        let (a, c) = (s.workspaces[0].id.clone(), s.workspaces[2].id.clone());
        assert!(s.move_workspace(&a, &c));
        assert_eq!(ids(&s), ["B", "C", "A"]);
        assert!(s.move_workspace(&a, &s.workspaces[0].id.clone()));
        assert_eq!(ids(&s), ["A", "B", "C"]);
        assert!(!s.move_workspace(&a, &a));
        assert!(!s.move_workspace("nope", &c));
    }

    #[test]
    fn a_pane_moves_to_another_workspace_with_its_container_and_zoom() {
        let mut s = two_workspaces();
        let moved = s.workspaces[0].leaves()[1].clone();
        let (b, c) = (s.workspaces[1].id.clone(), s.workspaces[2].id.clone());
        assert!(s.move_pane(&moved, &b));
        assert_eq!(s.workspaces[0].leaves().len(), 1);
        assert!(s.workspaces[0].container(&moved).is_none());
        let dest = &s.workspaces[1];
        assert_eq!(dest.leaves().len(), 2);
        assert!(dest.leaves().contains(&moved) && dest.container(&moved).is_some());
        assert_eq!(dest.focused_container_id.as_deref(), Some(moved.as_str()));
        assert_eq!(dest.zoom(&moved), Some(16.));
        // Into an empty workspace it is the whole layout; not into its own.
        assert!(s.move_pane(&moved, &c));
        assert_eq!(s.workspaces[2].leaves(), [moved.clone()]);
        assert!(!s.move_pane(&moved, &c));
    }

    #[test]
    fn ids_are_unique() {
        let a = new_id("c");
        let b = new_id("c");
        assert_ne!(a, b);
        assert!(a.starts_with("c-"));
    }
}
