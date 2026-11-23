//! Saved commands, the recent-command feed and saved prompts: the Electron
//! app's shapes (shared/types.ts: SavedCommand, RecentCommand, SavedPrompt)
//! under its JSON keys, so both apps read and edit the same lists.
//!
//! They live in the `extra` maps of `WorkspaceState` (envCommandFavorites,
//! recentCommands, envPrompts) and `Workspace` (commandFavorites) rather than
//! in typed fields: an entry this build can't parse is kept as it is instead
//! of failing the whole state file, and every item keeps the fields it
//! doesn't know in its own `extra`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::model::workspace::new_id;

/// `WorkspaceState.envCommandFavorites`: favorites for the whole environment.
pub const ENV_FAVORITES: &str = "envCommandFavorites";
/// `WorkspaceState.recentCommands`: the environment's recent feed, newest first.
pub const RECENT: &str = "recentCommands";
/// `WorkspaceState.envPrompts`: the environment's saved prompts.
pub const ENV_PROMPTS: &str = "envPrompts";
/// `Workspace.commandFavorites`: favorites bound to one workspace.
pub const WS_FAVORITES: &str = "commandFavorites";
/// useWorkspace's RECENT_COMMAND_LIMIT.
pub const RECENT_LIMIT: usize = 40;

/// Timestamps and counters are JS numbers: accept a float as well.
fn num<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(f64::deserialize(d)?.round() as i64)
}

fn opt_num<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|v| v.round() as i64))
}

/// A favorite command, at one of two levels (this workspace / the environment).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedCommand {
    pub id: String,
    pub command: String,
    /// A friendly label shown instead of the raw command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_num")]
    pub last_used_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_num")]
    pub use_count: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One line of the recent feed, deduplicated by its text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentCommand {
    pub command: String,
    #[serde(default, deserialize_with = "num")]
    pub last_used_at: i64,
    #[serde(default, deserialize_with = "num")]
    pub count: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A reusable, multi-line prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPrompt {
    pub id: String,
    /// Shown instead of the body's first line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub body: String,
    #[serde(default, deserialize_with = "num")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_num")]
    pub updated_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SavedPrompt {
    /// The first non-empty line of the body (PromptsButton's `firstLine`).
    pub fn first_line(&self) -> &str {
        self.body.split('\n').find(|l| !l.trim().is_empty()).unwrap_or("")
    }
}

/// The entries of `key` this build understands (others are skipped, not lost).
pub fn list<T: DeserializeOwned>(extra: &Map<String, Value>, key: &str) -> Vec<T> {
    extra.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| T::deserialize(v).ok()).collect()).unwrap_or_default()
}

/// Edit the list under `key`. Entries that don't parse stay in the file,
/// after the edited ones.
pub fn update<T: DeserializeOwned + Serialize>(extra: &mut Map<String, Value>, key: &str, edit: impl FnOnce(&mut Vec<T>)) {
    let raw = match extra.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut typed = Vec::new();
    let mut unknown = Vec::new();
    for v in raw {
        match T::deserialize(&v) {
            Ok(t) => typed.push(t),
            Err(_) => unknown.push(v),
        }
    }
    edit(&mut typed);
    let mut out: Vec<Value> = typed.iter().filter_map(|t| serde_json::to_value(t).ok()).collect();
    out.extend(unknown);
    extra.insert(key.to_owned(), Value::Array(out));
}

/// Into the recent feed: dedupe by text, move to the front, bump its count,
/// cap the list (useWorkspace's recordRecentCommand). False for a blank line.
pub fn record_recent(list: &mut Vec<RecentCommand>, command: &str, now: i64) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    let existing = list.iter().position(|r| r.command == cmd).map(|i| list.remove(i));
    let entry = match existing {
        Some(mut e) => {
            e.last_used_at = now;
            e.count += 1;
            e
        }
        None => RecentCommand { command: cmd.to_owned(), last_used_at: now, count: 1, extra: Map::new() },
    };
    list.retain(|r| r.command != cmd);
    list.insert(0, entry);
    list.truncate(RECENT_LIMIT);
    true
}

/// Add a favorite unless that command is already one at this level.
pub fn add_favorite(list: &mut Vec<SavedCommand>, command: &str, label: Option<&str>, now: i64) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() || list.iter().any(|f| f.command == cmd) {
        return false;
    }
    list.push(SavedCommand {
        id: new_id("cmd"),
        command: cmd.to_owned(),
        label: label.map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned),
        created_at: now,
        last_used_at: None,
        use_count: None,
        extra: Map::new(),
    });
    true
}

/// Save the prompt editor: a new prompt when `id` is None, else that one
/// (body trimmed, empty title dropped, as PromptsButton does). False when
/// the body is blank.
pub fn save_prompt(list: &mut Vec<SavedPrompt>, id: Option<&str>, title: &str, body: &str, now: i64) -> bool {
    let body = body.trim();
    if body.is_empty() {
        return false;
    }
    let title = Some(title.trim()).filter(|t| !t.is_empty()).map(str::to_owned);
    match id {
        Some(id) => {
            if let Some(p) = list.iter_mut().find(|p| p.id == id) {
                p.title = title;
                p.body = body.to_owned();
                p.updated_at = Some(now);
            }
        }
        None => list.push(SavedPrompt { id: new_id("prompt"), title, body: body.to_owned(), created_at: now, updated_at: None, extra: Map::new() }),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::WorkspaceState;
    use serde_json::json;

    /// What the Electron app writes (synthetic values).
    const ELECTRON: &str = r#"{
      "version": 1, "savedAt": 1, "activeWorkspaceId": "w1",
      "envCommandFavorites": [
        {"id":"f1","command":"make test","createdAt":1700000000000,"pinnedBy":"other-build"},
        {"id":"f2","command":"npm run dev","label":"dev","createdAt":1700000000001.0,"lastUsedAt":1700000000500,"useCount":3}
      ],
      "recentCommands": [
        {"command":"git status","lastUsedAt":1700000001000,"count":2},
        {"command":"ls -la","lastUsedAt":1700000000900,"count":1},
        {"bogus":true}
      ],
      "envPrompts": [
        {"id":"p1","title":"Review","body":"Review the diff.\nBe brief.","createdAt":5},
        {"id":"p2","body":"one liner","createdAt":6,"updatedAt":7}
      ],
      "workspaces": [{
        "id":"w1","name":"Demo",
        "layoutTree":{"type":"leaf","containerId":"c1"},
        "containers":[{"id":"c1","content":{"type":"terminal","terminalType":"local","terminalId":"t1"}}],
        "commandFavorites":[{"id":"wf1","command":"cargo check","createdAt":9}],
        "createdAt":1,"updatedAt":2
      }]
    }"#;

    #[test]
    fn reads_the_electron_shapes() {
        let state: WorkspaceState = serde_json::from_str(ELECTRON).unwrap();
        let favs: Vec<SavedCommand> = list(&state.extra, ENV_FAVORITES);
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[1].label.as_deref(), Some("dev"));
        assert_eq!(favs[1].created_at, 1700000000001, "a float timestamp still reads");
        assert_eq!(favs[1].use_count, Some(3));
        let recent: Vec<RecentCommand> = list(&state.extra, RECENT);
        assert_eq!(recent.iter().map(|r| r.command.as_str()).collect::<Vec<_>>(), ["git status", "ls -la"]);
        let prompts: Vec<SavedPrompt> = list(&state.extra, ENV_PROMPTS);
        assert_eq!(prompts[0].first_line(), "Review the diff.");
        assert_eq!(prompts[1].title, None);
        let ws: Vec<SavedCommand> = list(&state.workspaces[0].extra, WS_FAVORITES);
        assert_eq!(ws[0].command, "cargo check");
    }

    #[test]
    fn edits_round_trip_with_the_same_keys_and_unknown_fields() {
        let mut state: WorkspaceState = serde_json::from_str(ELECTRON).unwrap();
        update::<RecentCommand>(&mut state.extra, RECENT, |l| {
            record_recent(l, "ls -la", 1700000002000);
        });
        update::<SavedCommand>(&mut state.extra, ENV_FAVORITES, |l| {
            add_favorite(l, "  cargo build ", None, 10);
        });
        update::<SavedPrompt>(&mut state.extra, ENV_PROMPTS, |l| {
            save_prompt(l, Some("p2"), "", "  edited\n", 11);
        });
        update::<SavedCommand>(&mut state.workspaces[0].extra, WS_FAVORITES, |l| l.retain(|f| f.id != "wf1"));

        let back = serde_json::to_value(&state).unwrap();
        // The exact keys the Electron app reads.
        let favs = &back["envCommandFavorites"];
        assert_eq!(favs[0]["pinnedBy"], "other-build", "an item's unknown field survives");
        assert_eq!(favs[0]["createdAt"], 1700000000000_i64);
        assert!(favs[0].get("label").is_none(), "absent optionals stay absent, not null");
        assert!(favs[0].get("lastUsedAt").is_none() && favs[0].get("useCount").is_none());
        assert_eq!(favs[1]["useCount"], 3);
        assert_eq!(favs[2]["command"], "cargo build");
        assert!(favs[2]["id"].is_string() && favs[2]["createdAt"] == 10);

        let recent = &back["recentCommands"];
        assert_eq!(recent[0], json!({"command":"ls -la","lastUsedAt":1700000002000_i64,"count":2}));
        assert_eq!(recent[1]["command"], "git status");
        assert_eq!(recent[2], json!({"bogus":true}), "an entry this build can't read is kept");

        let prompts = &back["envPrompts"];
        assert_eq!(prompts[1], json!({"id":"p2","body":"edited","createdAt":6,"updatedAt":11}));
        assert_eq!(prompts[0]["title"], "Review");

        assert_eq!(back["workspaces"][0]["commandFavorites"], json!([]));
        // And it reads back in again.
        let again: WorkspaceState = serde_json::from_value(back).unwrap();
        assert_eq!(list::<SavedPrompt>(&again.extra, ENV_PROMPTS).len(), 2);
    }

    #[test]
    fn recent_feed_dedupes_moves_to_front_and_caps() {
        let mut l = Vec::new();
        for i in 0..50 {
            record_recent(&mut l, &format!("cmd {i}"), i);
        }
        assert_eq!(l.len(), RECENT_LIMIT);
        assert_eq!(l[0].command, "cmd 49");
        assert!(!record_recent(&mut l, "   ", 99));
        record_recent(&mut l, " cmd 30 ", 100);
        assert_eq!(l.len(), RECENT_LIMIT);
        assert_eq!((l[0].command.as_str(), l[0].count, l[0].last_used_at), ("cmd 30", 2, 100));
        assert_eq!(l.iter().filter(|r| r.command == "cmd 30").count(), 1);
    }

    #[test]
    fn favorites_are_unique_per_level() {
        let mut l = Vec::new();
        assert!(add_favorite(&mut l, "make", Some("  "), 1));
        assert!(!add_favorite(&mut l, " make ", None, 2));
        assert!(!add_favorite(&mut l, "", None, 2));
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].label, None, "a blank label is dropped");
    }

    #[test]
    fn prompts_need_a_body() {
        let mut l = Vec::new();
        assert!(!save_prompt(&mut l, None, "t", " \n ", 1));
        assert!(save_prompt(&mut l, None, " t ", "body", 1));
        assert_eq!((l[0].title.as_deref(), l[0].updated_at), (Some("t"), None));
    }

    #[test]
    fn a_missing_list_is_created() {
        let mut extra = Map::new();
        assert!(list::<SavedPrompt>(&extra, ENV_PROMPTS).is_empty());
        update::<SavedPrompt>(&mut extra, ENV_PROMPTS, |l| {
            save_prompt(l, None, "", "hi", 3);
        });
        assert_eq!(extra[ENV_PROMPTS][0]["body"], "hi");
    }
}
