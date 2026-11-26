//! What the sidebar shows for agent terminals, and when to tell the user (the
//! Electron app's useTerminalViewState + useAgentAttention).
//!
//! Only terminals whose foreground process is an agent CLI get a status:
//! Working, Waiting, or Done. A Done is "unviewed" when it finished while its
//! workspace wasn't on screen (or the window was in the background); it stays
//! highlighted until the user looks. New unviewed Dones and new Waitings the
//! user isn't looking at are the edges a notification fires on.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::activity::{self, State, TermActivity};

/// The agent CLIs that get a status (SplitGrid's AGENT_COMMANDS).
pub const AGENT_COMMANDS: [&str; 4] = ["claude", "codex", "cursor", "opencode"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Working,
    Waiting,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    Done,
    Waiting,
}

/// One terminal pane, as `recompute` sees it.
pub struct Term<'a> {
    pub workspace: &'a str,
    pub id: &'a str,
    /// Its foreground process is an agent CLI.
    pub is_agent: bool,
}

#[derive(Default)]
pub struct Attention {
    activity: HashMap<String, TermActivity>,
    /// The last state each terminal's hooks reported, with its one-line detail.
    detail: HashMap<String, (State, Option<String>)>,
    last_kind: HashMap<String, Kind>,
    done_at: HashMap<String, u64>,
    viewed_at: HashMap<String, u64>,
    prev_unviewed: HashSet<String>,
    primed: bool,
    pub kinds: HashMap<String, Kind>,
    pub unviewed: HashSet<String>,
    pub unviewed_workspaces: HashSet<String>,
}

impl Attention {
    /// The agent session terminal `id`'s hooks follow, while one runs.
    pub fn session(&self, id: &str) -> Option<&str> {
        self.activity.get(id)?.session.as_deref()
    }

    /// A hook event from terminal `id`. True when its state changed.
    pub fn on_hook(&mut self, id: &str, event: &str, payload: &Value, now: u64) -> bool {
        let Some(event) = activity::hook_event(event) else { return false };
        let Some(step) = activity::apply(self.activity.get(id), event, payload, now) else { return false };
        self.activity.insert(id.to_owned(), step.next);
        match step.emit {
            Some(emit) => {
                log::info!("[activity] term={id} -> {:?}", emit.0);
                self.detail.insert(id.to_owned(), emit);
                true
            }
            None => false,
        }
    }

    /// The silent-turn safety net; true when anything changed.
    pub fn sweep(&mut self, now: u64) -> bool {
        let mut changed = false;
        for (id, cur) in self.activity.iter_mut() {
            if let Some(step) = activity::sweep(cur, now) {
                *cur = step.next;
                if let Some(emit) = step.emit {
                    self.detail.insert(id.clone(), emit);
                }
                changed = true;
            }
        }
        changed
    }

    /// Forget a closed terminal.
    pub fn forget(&mut self, id: &str) {
        self.activity.remove(id);
        self.detail.remove(id);
        self.last_kind.remove(id);
        self.done_at.remove(id);
        self.viewed_at.remove(id);
    }

    /// The one-line detail of the state an alert is about (the final reply, the question…).
    pub fn detail(&self, id: &str, alert: Alert) -> Option<&str> {
        let want = match alert {
            Alert::Done => State::Idle,
            Alert::Waiting => State::Waiting,
        };
        self.detail.get(id).filter(|(s, _)| *s == want).and_then(|(_, d)| d.as_deref())
    }

    fn kind_for(&self, t: &Term) -> Option<Kind> {
        if !t.is_agent {
            return None;
        }
        match self.activity.get(t.id)?.state {
            State::Working => Some(Kind::Working),
            State::Waiting => Some(Kind::Waiting),
            State::Idle => Some(Kind::Done),
            State::Ready => None,
        }
    }

    /// Recompute what the sidebar shows; returns the alerts to raise now
    /// (terminal id, workspace id). `viewing` is the workspace on screen in a
    /// focused window, if any.
    pub fn recompute(&mut self, terms: &[Term], viewing: Option<&str>, now: u64) -> Vec<(Alert, String, String)> {
        let mut kinds = HashMap::new();
        let mut unviewed = HashSet::new();
        let mut unviewed_workspaces = HashSet::new();
        let mut alerts = Vec::new();

        for t in terms {
            let viewed = viewing == Some(t.workspace);
            let kind = self.kind_for(t);
            let last = self.last_kind.get(t.id).copied();
            if kind == Some(Kind::Done) && last != Some(Kind::Done) {
                self.done_at.insert(t.id.to_owned(), now);
            }
            if kind == Some(Kind::Waiting) && last != Some(Kind::Waiting) && !viewed && self.primed {
                alerts.push((Alert::Waiting, t.id.to_owned(), t.workspace.to_owned()));
            }
            if let Some(k) = kind {
                self.last_kind.insert(t.id.to_owned(), k);
                kinds.insert(t.id.to_owned(), k);
            }
            if viewed {
                self.viewed_at.insert(t.id.to_owned(), now);
            }
            let done_at = self.done_at.get(t.id).copied().unwrap_or(0);
            if kind == Some(Kind::Done) && done_at > self.viewed_at.get(t.id).copied().unwrap_or(0) {
                unviewed.insert(t.id.to_owned());
                unviewed_workspaces.insert(t.workspace.to_owned());
                if self.primed && !self.prev_unviewed.contains(t.id) {
                    alerts.push((Alert::Done, t.id.to_owned(), t.workspace.to_owned()));
                }
            }
        }
        self.primed = true;
        self.prev_unviewed = unviewed.clone();
        self.kinds = kinds;
        self.unviewed = unviewed;
        self.unviewed_workspaces = unviewed_workspaces;
        alerts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn done_in_the_background_stays_unviewed_until_looked_at() {
        let mut a = Attention::default();
        let terms = [Term { workspace: "w2", id: "t", is_agent: true }];
        assert!(a.recompute(&terms, Some("w1"), 1).is_empty(), "priming pass");

        a.on_hook("t", "prompt", &json!({}), 2);
        a.recompute(&terms, Some("w1"), 2);
        assert_eq!(a.kinds.get("t"), Some(&Kind::Working));

        a.on_hook("t", "stop", &json!({ "last_assistant_message": "Shipped." }), 3);
        let alerts = a.recompute(&terms, Some("w1"), 3);
        assert_eq!(alerts, [(Alert::Done, "t".to_owned(), "w2".to_owned())]);
        assert_eq!(a.detail("t", Alert::Done), Some("Shipped."));
        assert!(a.unviewed_workspaces.contains("w2"));
        assert!(a.recompute(&terms, Some("w1"), 4).is_empty(), "one alert per Done");

        // The user opens w2: seen.
        a.recompute(&terms, Some("w2"), 5);
        assert!(a.unviewed.is_empty());
        assert_eq!(a.kinds.get("t"), Some(&Kind::Done), "still Done, just not highlighted");
    }

    #[test]
    fn waiting_alerts_only_when_not_looking_and_shells_get_nothing() {
        let mut a = Attention::default();
        let terms = [Term { workspace: "w1", id: "t", is_agent: true }, Term { workspace: "w1", id: "sh", is_agent: false }];
        a.recompute(&terms, None, 1);
        a.on_hook("t", "prompt", &json!({}), 2);
        a.on_hook("t", "notification", &json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission" }), 3);
        a.on_hook("sh", "prompt", &json!({}), 3);
        let alerts = a.recompute(&terms, None, 3);
        assert_eq!(alerts, [(Alert::Waiting, "t".to_owned(), "w1".to_owned())]);
        assert!(!a.kinds.contains_key("sh"));
        // Finishing while the user watches: Done, but nothing to alert.
        a.on_hook("t", "stop", &json!({}), 4);
        assert!(a.recompute(&terms, Some("w1"), 4).is_empty());
        assert!(a.unviewed.is_empty());
    }
}
