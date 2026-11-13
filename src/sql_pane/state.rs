//! The pane's persisted state: the container's `sqlState` (SQLContainerState in
//! the Electron app). This build edits only the fields it uses — the saved
//! connection, the console text, the split sizes — and leaves everything else
//! (other tabs, history, favourites) as the other app wrote it.
//!
//! The Electron app keeps the console in a `query` tab; the text here is that
//! of the active query tab, or the first one, created when there is none.

use serde_json::{Map, Value, json};

use super::store::SavedSql;

pub type SqlState = Map<String, Value>;

/// The query tab the console is: the active one if it is a query tab, else the first.
fn console_index(state: &SqlState) -> Option<usize> {
    let tabs = state.get("tabs")?.as_array()?;
    let is_query = |t: &Value| t.get("type").and_then(Value::as_str).is_none_or(|ty| ty == "query") && t.get("readOnly") != Some(&Value::Bool(true));
    let active = state.get("activeTabId").and_then(Value::as_str);
    tabs.iter()
        .position(|t| active.is_some() && t.get("id").and_then(Value::as_str) == active && is_query(t))
        .or_else(|| tabs.iter().position(is_query))
}

pub fn console_text(state: &SqlState) -> String {
    console_index(state)
        .and_then(|i| state["tabs"][i].get("query").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

pub fn set_console_text(state: &mut SqlState, text: &str) {
    match console_index(state) {
        Some(i) => state["tabs"][i]["query"] = Value::from(text),
        None => {
            let id = crate::model::workspace::new_id("sqltab");
            let tab = json!({ "id": id, "type": "query", "title": "Console", "query": text });
            match state.get_mut("tabs").and_then(Value::as_array_mut) {
                Some(tabs) => tabs.push(tab),
                None => {
                    state.insert("tabs".into(), Value::Array(vec![tab]));
                }
            }
            state.insert("activeTabId".into(), Value::from(id));
        }
    }
}

pub fn saved_connection_id(state: &SqlState) -> Option<String> {
    state.get("savedConnectionId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The pane is on `conn` now (None: no active connection). The live connection id
/// is the other app's, only meaningful while it runs, so it is cleared.
pub fn set_connection(state: &mut SqlState, conn: Option<&SavedSql>) {
    state.insert("connectionId".into(), Value::Null);
    match conn {
        Some(c) => {
            state.insert("savedConnectionId".into(), Value::from(c.id.clone()));
            state.insert("connectionName".into(), Value::from(c.label.clone()));
            state.insert("host".into(), Value::from(c.host.clone()));
            state.insert("port".into(), Value::from(c.port));
            state.insert("user".into(), Value::from(c.user.clone()));
            state.insert("database".into(), Value::from(c.database.clone()));
            state.insert("ssl".into(), Value::from(c.ssl));
        }
        None => {
            state.insert("savedConnectionId".into(), Value::Null);
        }
    }
}

/// The fields SQLContainerState requires, for a state this build creates.
pub fn ensure_shape(state: &mut SqlState) {
    for (k, v) in [
        ("connectionId", Value::Null),
        ("savedConnectionId", Value::Null),
        ("connectionName", Value::from("")),
        ("host", Value::from("")),
        ("port", Value::from(0)),
        ("user", Value::from("")),
        ("database", Value::from("")),
        ("ssl", Value::from(false)),
        ("tabs", Value::Array(vec![])),
        ("activeTabId", Value::Null),
        ("history", Value::Array(vec![])),
    ] {
        state.entry(k).or_insert(v);
    }
}

/// The explorer's expanded nodes (this build's own field; the other app ignores it).
pub fn tree_expanded(state: &SqlState) -> Vec<String> {
    state.get("treeExpanded").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

pub fn set_tree_expanded<'a>(state: &mut SqlState, keys: impl IntoIterator<Item = &'a String>) {
    let mut keys: Vec<&String> = keys.into_iter().collect();
    keys.sort();
    state.insert("treeExpanded".into(), Value::Array(keys.into_iter().map(|k| Value::from(k.as_str())).collect()));
}

pub fn size(state: &SqlState, key: &str) -> Option<f32> {
    state.get(key).and_then(Value::as_f64).map(|v| v as f32).filter(|v| v.is_finite() && *v > 0.)
}

pub fn set_size(state: &mut SqlState, key: &str, v: f32) {
    state.insert(key.into(), Value::from(v.round() as i64));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_the_console_and_keeps_the_rest() {
        let mut state: SqlState = serde_json::from_str(
            r#"{"connectionId":"live-1","savedConnectionId":"c1","connectionName":"Prod","host":"h","port":5432,"user":"u",
            "database":"d","ssl":false,"activeTabId":"t2","history":[{"id":"h1","query":"select 1","executedAt":1,"ok":true}],
            "favorites":[{"id":"f","name":"n","query":"q","createdAt":1}],"treeWidth":240,"futureField":true,
            "tabs":[{"id":"t1","type":"query","title":"Q1","query":"select 1"},
                    {"id":"t2","type":"table","title":"users","schema":"public","objectName":"users"},
                    {"id":"t3","type":"query","title":"Q2","query":"select 3"}]}"#,
        )
        .unwrap();
        assert_eq!(console_text(&state), "select 1", "the active tab is a table view: the first query tab");
        set_console_text(&mut state, "select 42");
        assert_eq!(state["tabs"][0]["query"], "select 42");
        assert_eq!(state["tabs"][2]["query"], "select 3");
        assert_eq!(state["favorites"][0]["name"], "n");
        assert_eq!(state["futureField"], true);
        assert_eq!(size(&state, "treeWidth"), Some(240.));
        assert_eq!(saved_connection_id(&state).as_deref(), Some("c1"));

        state.insert("activeTabId".into(), Value::from("t3"));
        assert_eq!(console_text(&state), "select 3");

        let conn = SavedSql { id: "c2".into(), label: "Lite".into(), dialect: "sqlite".into(), ..Default::default() };
        set_connection(&mut state, Some(&conn));
        assert_eq!(state["connectionId"], Value::Null);
        assert_eq!(state["connectionName"], "Lite");
        set_connection(&mut state, None);
        assert_eq!(saved_connection_id(&state), None);

        assert!(tree_expanded(&state).is_empty());
        let keys = ["b".to_owned(), "a\u{1f}x".to_owned()];
        set_tree_expanded(&mut state, &keys);
        let back: SqlState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(tree_expanded(&back), ["a\u{1f}x", "b"]);
        assert_eq!(back["futureField"], true);
    }

    #[test]
    fn a_fresh_state_gets_a_console_tab() {
        let mut state = SqlState::new();
        ensure_shape(&mut state);
        assert_eq!(console_text(&state), "");
        set_console_text(&mut state, "select 1");
        assert_eq!(state["tabs"][0]["type"], "query");
        assert_eq!(state["activeTabId"], state["tabs"][0]["id"]);
        assert_eq!(console_text(&state), "select 1");
        // A read-only DDL tab is never the console.
        let mut ro: SqlState = serde_json::from_str(r#"{"tabs":[{"id":"d","type":"query","title":"DDL","query":"create","readOnly":true}]}"#).unwrap();
        set_console_text(&mut ro, "x");
        assert_eq!(ro["tabs"][0]["query"], "create");
        assert_eq!(ro["tabs"][1]["query"], "x");
    }
}
