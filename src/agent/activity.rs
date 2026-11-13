//! An agent's turn state from its lifecycle hooks (the Electron app's
//! agent-activity-machine.ts and agent-hook-detail.ts). The rule that keeps
//! "Done" honest: only a real end of turn (Stop) with nothing left to wake the
//! session is Done. An interrupt, a silent gap, a session starting or ending
//! goes to `Ready`: back at the prompt, nothing to announce.
//!
//! The hook input tells apart what the bare event names can't: `session_id`
//! (a nested `claude -p` inherits this terminal's env but is another session),
//! `agent_id` (subagents report their own tools), `notification_type` (only a
//! permission prompt or a question is a wait), and `background_tasks` on Stop
//! (a subagent still running will wake the session again).

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// At its prompt, nothing to show.
    Ready,
    Working,
    /// Blocked on the user: a permission prompt or a question.
    Waiting,
    /// Finished its turn.
    Idle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    pub tool_name: String,
    pub tool_input: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermActivity {
    pub state: State,
    /// ms since the epoch; 0 = never heard from.
    pub last_event_at: u64,
    pub session: Option<String>,
    pub pending: Option<Pending>,
    pub awaiting_background: bool,
}

impl Default for TermActivity {
    fn default() -> Self {
        Self { state: State::Ready, last_event_at: 0, session: None, pending: None, awaiting_background: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    Prompt,
    Tool,
    Stop,
    Notification,
    SessionStart,
    SessionEnd,
}

/// What the helpers may send (older helpers used several aliases).
pub fn hook_event(name: &str) -> Option<HookEvent> {
    Some(match name.trim().to_lowercase().as_str() {
        "prompt" | "prompt-submit" | "user-prompt" | "start" | "active" => HookEvent::Prompt,
        "tool" => HookEvent::Tool,
        "stop" | "idle" | "done" | "end" => HookEvent::Stop,
        "notification" | "notify" | "needs-input" | "waiting" | "permission" => HookEvent::Notification,
        "session-start" => HookEvent::SessionStart,
        "session-end" => HookEvent::SessionEnd,
        _ => return None,
    })
}

const WAIT_TYPES: [&str; 4] = ["permission_prompt", "elicitation_dialog", "elicitation_url_dialog", "agent_needs_input"];
/// Background work that reports back and wakes the session (shells may run forever).
const WAKING_TASKS: [&str; 4] = ["subagent", "workflow", "teammate", "cloud session"];

/// Silence after which a working agent without a running tool is presumed back at its prompt.
pub const SAFETY_READY_MS: u64 = 600_000;
/// The same while background agents were due to wake it.
pub const BACKGROUND_READY_MS: u64 = 3_600_000;

pub struct Step {
    pub next: TermActivity,
    /// Set when the state changed: the new state and its one-line detail.
    pub emit: Option<(State, Option<String>)>,
}

fn to(cur: &TermActivity, now: u64, state: State, update: impl FnOnce(&mut TermActivity), detail: Option<String>) -> Step {
    let mut next = cur.clone();
    update(&mut next);
    next.state = state;
    next.last_event_at = now;
    let changed = cur.state != state || cur.last_event_at == 0;
    Step { next, emit: changed.then_some((state, detail)) }
}

fn clear(a: &mut TermActivity) {
    a.pending = None;
    a.awaiting_background = false;
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Apply one hook event to a terminal's state (`prev` None = never heard from).
/// None for an event that is ignored outright.
pub fn apply(prev: Option<&TermActivity>, event: HookEvent, payload: &Value, now: u64) -> Option<Step> {
    let cur = prev.cloned().unwrap_or_default();
    let sid = s(payload, "session_id").map(str::to_owned);

    if event == HookEvent::SessionStart {
        // Another session starting while this one works is one it spawned
        // (`claude -p`); otherwise it replaces this one (/clear, a restart).
        if let (Some(sid), Some(mine)) = (&sid, &cur.session)
            && sid != mine
        {
            if cur.state == State::Working {
                return None;
            }
            let sid = sid.clone();
            return Some(to(&cur, now, State::Ready, |a| { a.session = Some(sid); clear(a) }, None));
        }
        if prev.is_some() && cur.session == sid {
            let mut next = cur;
            next.last_event_at = now;
            return Some(Step { next, emit: None });
        }
        let session = sid.or(cur.session.clone());
        return Some(to(&cur, now, State::Ready, |a| { a.session = session; clear(a) }, None));
    }

    if let (Some(sid), Some(mine)) = (&sid, &cur.session)
        && sid != mine
    {
        return None;
    }
    let mut owned = cur;
    if owned.session.is_none() {
        owned.session = sid;
    }

    if event == HookEvent::SessionEnd {
        return Some(to(&owned, now, State::Ready, |a| { a.session = None; clear(a) }, None));
    }

    // Subagents' own events: alive, but not the main thread's turn.
    if s(payload, "agent_id").is_some() {
        let mut next = owned;
        if next.state == State::Working {
            next.last_event_at = now;
        }
        return Some(Step { next, emit: None });
    }

    Some(match event {
        HookEvent::Prompt => to(&owned, now, State::Working, clear, None),
        HookEvent::Tool => match (s(payload, "hook_event_name"), s(payload, "tool_name")) {
            (Some("PreToolUse"), Some(tool)) => {
                let pending = Pending { tool_name: tool.to_owned(), tool_input: payload.get("tool_input").cloned().unwrap_or(Value::Null) };
                // A question to the user arrives as a tool call, but the agent now waits on them.
                if tool == "AskUserQuestion" {
                    let detail = waiting_detail(payload, None);
                    to(&owned, now, State::Waiting, |a| { a.pending = Some(pending); a.awaiting_background = false }, detail)
                } else {
                    to(&owned, now, State::Working, |a| { a.pending = Some(pending); a.awaiting_background = false }, None)
                }
            }
            (Some("PostToolUse"), _) => to(&owned, now, State::Working, clear, None),
            _ => to(&owned, now, State::Working, |a| a.awaiting_background = false, None),
        },
        HookEvent::Stop => {
            let waking = payload
                .get("background_tasks")
                .and_then(Value::as_array)
                .is_some_and(|tasks| tasks.iter().any(|t| s(t, "type").is_some_and(|ty| WAKING_TASKS.contains(&ty))));
            if waking {
                to(&owned, now, State::Working, |a| { a.pending = None; a.awaiting_background = true }, None)
            } else {
                let detail = s(payload, "last_assistant_message").map(|m| one_line(m, DETAIL_MAX)).filter(|d| !d.is_empty());
                to(&owned, now, State::Idle, clear, detail)
            }
        }
        HookEvent::Notification => {
            let kind = s(payload, "notification_type");
            if kind == Some("idle_prompt") {
                // The prompt sat idle: a "working" turn ended without a Stop (an interrupt).
                return Some(if owned.state == State::Working && !owned.awaiting_background {
                    to(&owned, now, State::Ready, |a| a.pending = None, None)
                } else {
                    Step { next: owned, emit: None }
                });
            }
            if kind.is_some_and(|k| !WAIT_TYPES.contains(&k)) || owned.state != State::Working {
                // Noise, or the same notification after a turn.
                return Some(Step { next: owned, emit: None });
            }
            let detail = waiting_detail(payload, owned.pending.as_ref());
            to(&owned, now, State::Waiting, |_| {}, detail)
        }
        HookEvent::SessionStart | HookEvent::SessionEnd => unreachable!(),
    })
}

/// The safety net for a turn that went silent; never claims Done.
pub fn sweep(cur: &TermActivity, now: u64) -> Option<Step> {
    if cur.state != State::Working || cur.pending.is_some() {
        return None;
    }
    let limit = if cur.awaiting_background { BACKGROUND_READY_MS } else { SAFETY_READY_MS };
    if now.saturating_sub(cur.last_event_at) <= limit {
        return None;
    }
    Some(to(cur, now, State::Ready, |a| a.awaiting_background = false, None))
}

// ── One notification-sized line ────────────────────────────────────────────

pub const DETAIL_MAX: usize = 180;

/// Markdown and line breaks out, whitespace collapsed, clipped at a word with "…".
pub fn one_line(text: &str, max: usize) -> String {
    use std::sync::LazyLock;
    static RULES: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
        [
            (r"```[\s\S]*?(```|$)", " "),
            (r"!?\[([^\]]*)\]\([^)]*\)", "$1"),
            (r"`([^`]*)`", "$1"),
            (r"(?m)^\s{0,3}(#{1,6}\s+|>\s?|[-*+]\s+|\d+[.)]\s+)", ""),
            (r"\*\*|__|~~", ""),
            (r"(?m)^\s*\|?[-:\s|]+\|?\s*$", " "),
            (r"\|", " "),
            (r"\s+", " "),
        ]
        .into_iter()
        .map(|(re, to)| (regex::Regex::new(re).expect("valid regex"), to))
        .collect()
    });
    let mut plain = text.to_owned();
    for (re, to) in RULES.iter() {
        plain = re.replace_all(&plain, *to).into_owned();
    }
    let plain = plain.trim();
    if plain.chars().count() <= max {
        return plain.to_owned();
    }
    let cut: String = plain.chars().take(max - 1).collect();
    let at_word = cut.rfind(' ').filter(|&i| cut[..i].chars().count() as f32 > max as f32 * 0.6);
    let head = at_word.map_or(cut.as_str(), |i| &cut[..i]);
    format!("{}…", head.trim_end_matches(|c: char| c.is_whitespace() || ",.;:–—-".contains(c)))
}

fn base_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(p)
}

/// The question an AskUserQuestion call puts to the user, with its choices.
fn question_of(input: &Value) -> String {
    let questions = input.get("questions").and_then(Value::as_array).cloned().unwrap_or_default();
    let Some(first) = questions.first() else { return String::new() };
    let q = s(first, "question").unwrap_or("").trim();
    if q.is_empty() {
        return String::new();
    }
    let labels: Vec<&str> = first
        .get("options")
        .and_then(Value::as_array)
        .map(|o| o.iter().filter_map(|x| s(x, "label").map(str::trim).filter(|l| !l.is_empty())).collect())
        .unwrap_or_default();
    let more = if questions.len() > 1 { format!(" (+{} more)", questions.len() - 1) } else { String::new() };
    if labels.is_empty() { format!("{q}{more}") } else { format!("{q} {}{more}", labels.join(" / ")) }
}

/// "Bash: npm test", "Edit: app.ts": what a tool call is about.
pub fn tool_summary(name: &str, input: &Value) -> String {
    if name == "AskUserQuestion" {
        return question_of(input);
    }
    let what = s(input, "command")
        .map(str::to_owned)
        .or_else(|| s(input, "file_path").map(|p| base_name(p).to_owned()))
        .or_else(|| s(input, "notebook_path").map(|p| base_name(p).to_owned()))
        .or_else(|| ["url", "pattern", "description", "prompt"].iter().find_map(|k| s(input, k).map(str::to_owned)))
        .filter(|w| !w.is_empty());
    let tool = regex::Regex::new(r"^mcp__([^_]+)__").expect("valid regex").replace(name, "$1: ").into_owned();
    match what {
        Some(w) => format!("{tool}: {w}"),
        None => tool,
    }
}

/// The line for a waiting agent: its question, or the tool call it wants allowed.
fn waiting_detail(payload: &Value, pending: Option<&Pending>) -> Option<String> {
    let call = match s(payload, "tool_name") {
        Some(name) => Some((name.to_owned(), payload.get("tool_input").cloned().unwrap_or(Value::Null))),
        None => pending.map(|p| (p.tool_name.clone(), p.tool_input.clone())),
    };
    let line = match call {
        Some((name, input)) => {
            let summary = tool_summary(&name, &input);
            one_line(&if name == "AskUserQuestion" { summary } else { format!("Allow {summary}?") }, DETAIL_MAX)
        }
        None => one_line(s(payload, "message").unwrap_or(""), DETAIL_MAX),
    };
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(events: &[(&str, Value)]) -> (TermActivity, Vec<(State, Option<String>)>) {
        let mut cur: Option<TermActivity> = None;
        let mut emitted = Vec::new();
        for (i, (name, payload)) in events.iter().enumerate() {
            if let Some(step) = apply(cur.as_ref(), hook_event(name).unwrap(), payload, 1000 + i as u64) {
                emitted.extend(step.emit);
                cur = Some(step.next);
            }
        }
        (cur.unwrap(), emitted)
    }

    #[test]
    fn a_turn_ends_done_with_the_final_reply() {
        let (cur, emitted) = run(&[
            ("session-start", json!({ "session_id": "s1" })),
            ("prompt", json!({ "session_id": "s1" })),
            ("tool", json!({ "session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": { "command": "npm test" } })),
            ("notification", json!({ "session_id": "s1", "notification_type": "permission_prompt" })),
            ("tool", json!({ "session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": "Bash" })),
            ("stop", json!({ "session_id": "s1", "last_assistant_message": "## All **green**\n\nTests pass." })),
        ]);
        assert_eq!(cur.state, State::Idle);
        let states: Vec<State> = emitted.iter().map(|e| e.0).collect();
        assert_eq!(states, [State::Ready, State::Working, State::Waiting, State::Working, State::Idle]);
        assert_eq!(emitted[2].1.as_deref(), Some("Allow Bash: npm test?"));
        assert_eq!(emitted[4].1.as_deref(), Some("All green Tests pass."));
    }

    #[test]
    fn nested_sessions_subagents_and_background_work_dont_end_the_turn() {
        let (cur, _) = run(&[
            ("prompt", json!({ "session_id": "s1" })),
            // A `claude -p` the agent ran: another session.
            ("session-start", json!({ "session_id": "s2" })),
            ("stop", json!({ "session_id": "s2" })),
            // A subagent's own tool calls.
            ("tool", json!({ "session_id": "s1", "agent_id": "a1", "hook_event_name": "PreToolUse", "tool_name": "Read" })),
            // Stop with a subagent still running: still working.
            ("stop", json!({ "session_id": "s1", "background_tasks": [{ "type": "subagent" }] })),
        ]);
        assert_eq!(cur.state, State::Working);
        assert!(cur.awaiting_background);
        assert_eq!(cur.session.as_deref(), Some("s1"));
    }

    #[test]
    fn interrupts_and_silence_go_back_to_ready_never_done() {
        let (cur, _) = run(&[("prompt", json!({})), ("notification", json!({ "notification_type": "idle_prompt" }))]);
        assert_eq!(cur.state, State::Ready);
        let working = TermActivity { state: State::Working, last_event_at: 1, ..Default::default() };
        assert!(sweep(&working, 1 + SAFETY_READY_MS).is_none());
        assert_eq!(sweep(&working, 2 + SAFETY_READY_MS).unwrap().next.state, State::Ready);
    }

    #[test]
    fn questions_and_lines() {
        let (_, emitted) = run(&[
            ("prompt", json!({})),
            ("tool", json!({ "hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
                "tool_input": { "questions": [{ "question": "Which DB?", "options": [{ "label": "Postgres" }, { "label": "SQLite" }] }] } })),
        ]);
        assert_eq!(emitted.last().unwrap(), &(State::Waiting, Some("Which DB? Postgres / SQLite".into())));
        assert_eq!(tool_summary("mcp__github__create_issue", &json!({ "description": "x" })), "github: create_issue: x");
        assert_eq!(tool_summary("Edit", &json!({ "file_path": "/a/b/app.ts" })), "Edit: app.ts");
        let long = "word ".repeat(60);
        let clipped = one_line(&long, 40);
        assert!(clipped.ends_with('…') && clipped.chars().count() <= 40, "{clipped}");
    }
}
