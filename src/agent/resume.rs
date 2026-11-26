//! The last agent session a terminal ran, kept in its container
//! (`content.agentResume`) so that after the shell is gone (a reboot, a closed
//! session) the pane's header offers to resume it: `claude --resume <id>` in
//! the directory it ran in. Learnt from the agent's lifecycle hooks, which
//! carry `session_id` and `cwd`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The key in the container's content.
pub const KEY: &str = "agentResume";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    /// "claude" or "codex".
    pub agent: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl Resume {
    /// From a hook's payload, when it is about `tracked`: the session the
    /// terminal's activity follows (a nested `claude -p` reports another one).
    pub fn from_hook(payload: &Value, tracked: Option<&str>) -> Option<Self> {
        let sid = payload.get("session_id").and_then(Value::as_str).filter(|s| is_session_id(s))?;
        if tracked.is_some_and(|t| t != sid) {
            return None;
        }
        let transcript = payload.get("transcript_path").and_then(Value::as_str).unwrap_or_default();
        let agent = if transcript.contains("/.codex/") { "codex" } else { "claude" };
        let cwd = payload.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned);
        Some(Resume { agent: agent.into(), session_id: sid.into(), cwd })
    }

    pub fn load(content_extra: &serde_json::Map<String, Value>) -> Option<Self> {
        serde_json::from_value(content_extra.get(KEY)?.clone()).ok()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The line to type at the shell's prompt: back into the session's
    /// directory when the shell is elsewhere, then the agent's resume.
    pub fn command(&self, shell_cwd: Option<&str>) -> String {
        let resume = match self.agent.as_str() {
            "codex" => format!("codex resume {}", self.session_id),
            _ => format!("claude --resume {}", self.session_id),
        };
        match self.cwd.as_deref() {
            Some(cwd) if shell_cwd != Some(cwd) => format!("cd {} && {resume}", shell_quote(cwd)),
            _ => resume,
        }
    }
}

/// Session ids are UUIDs: anything else is not typed into a shell.
fn is_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,@%:".contains(c)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "d9bd3e43-295a-41b3-9e9b-632354bd0df6";

    #[test]
    fn learns_claude_sessions_from_hooks_and_skips_nested_ones() {
        let p = json!({ "session_id": SID, "cwd": "/Users/me/my repo", "transcript_path": "/Users/me/.claude/projects/x/y.jsonl" });
        let r = Resume::from_hook(&p, Some(SID)).unwrap();
        assert_eq!(r, Resume { agent: "claude".into(), session_id: SID.into(), cwd: Some("/Users/me/my repo".into()) });
        assert_eq!(Resume::from_hook(&p, Some("another")), None, "a nested claude -p");
        assert!(Resume::from_hook(&p, None).is_some());
        assert_eq!(Resume::from_hook(&json!({ "session_id": "x; rm -rf ~" }), None), None);
        assert_eq!(Resume::from_hook(&json!({}), None), None);
    }

    #[test]
    fn the_command_goes_back_to_the_sessions_directory() {
        let r = Resume { agent: "claude".into(), session_id: SID.into(), cwd: Some("/Users/me/my repo".into()) };
        assert_eq!(r.command(Some("/Users/me/my repo")), format!("claude --resume {SID}"));
        assert_eq!(r.command(Some("/tmp")), format!("cd '/Users/me/my repo' && claude --resume {SID}"));
        let codex = Resume { agent: "codex".into(), session_id: "abc".into(), cwd: Some("/srv/app".into()) };
        assert_eq!(codex.command(None), "cd /srv/app && codex resume abc");
        let p = json!({ "session_id": "abc", "transcript_path": "/Users/me/.codex/sessions/a.jsonl" });
        assert_eq!(Resume::from_hook(&p, None).unwrap().agent, "codex");
    }

    #[test]
    fn round_trips_through_the_container() {
        let r = Resume { agent: "claude".into(), session_id: SID.into(), cwd: None };
        let mut extra = serde_json::Map::new();
        extra.insert(KEY.into(), r.to_value());
        assert_eq!(Resume::load(&extra), Some(r));
    }
}
