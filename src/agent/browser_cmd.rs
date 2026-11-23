//! `sg-browser` commands (the Electron app's agent-browser-bridge +
//! useBrowserAgentBridge): an agent drives the browser panes of ITS OWN
//! workspace through `$SPLITGRID_BROWSER_CLI`.
//!
//! Ownership is Electron's: a pane belongs to the agent that opened it
//! (`browserOwnerTerminal` + `browserOwnerZone` on the container), commands
//! without `--target` act on the caller's own pane, a lone unowned pane is
//! claimed, and ambiguity is reported, never guessed. New panes open in the
//! caller's zone (`--zone/--at/--split`), and only zone panes can be closed.
//!
//! Page work is JavaScript (see `browser::bridge`): WKWebView has no CDP, so
//! network capture, key presses and cookies are in-page approximations, and
//! PDF export and viewport emulation aren't available.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::{AsyncApp, ClipboardItem, Context, Entity, Task, WeakEntity, Window};
use serde_json::{Map, Value, json};

use super::terminal_cmd::{fail, label_of, ok, reply, valid_zone_name, zone_panes};
use super::zones::{self, DEFAULT_ZONE, OpenArgs, ZoneOwner, ZoneSplit};
use crate::app::AppView;
use crate::browser::BrowserPane;
use crate::browser::bridge;
use crate::model::workspace::{Container, ContainerContent, ContentType, new_id};

/// Below the helper's 35s budget, so a wait's own timeout answers first.
pub const MAX_WAIT_MS: u64 = 30_000;
const DEFAULT_WAIT_MS: u64 = 10_000;
/// Any one page script (eval of a Promise that never settles…).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(33);
/// How long a command waits for a just-opened pane's view to exist.
const PANE_APPEAR_TIMEOUT: Duration = Duration::from_secs(5);
const SCREENSHOT_CAP: usize = 50;

pub enum Answer {
    Now(Value),
    Later(Task<Value>),
}

// ─── argv ────────────────────────────────────────────────────────────────────

/// Electron's shared/browser-argv: the known flags, anywhere in argv.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedArgv {
    pub positional: Vec<String>,
    pub target: Option<String>,
    pub timeout_ms: Option<u64>,
    pub viewport: bool,
    pub full: bool,
    pub zone: Option<String>,
    pub at: Option<String>,
    pub split: Option<String>,
}

pub fn parse_argv(argv: &[String]) -> ParsedArgv {
    let mut p = ParsedArgv::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" | "--surface" => p.target = it.next().cloned(),
            "--timeout" | "-t" => {
                if let Some(n) = it.next().and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite()) {
                    p.timeout_ms = Some(n.max(0.0).min(MAX_WAIT_MS as f64) as u64);
                }
            }
            "--viewport" => p.viewport = true,
            "--full" | "--fullpage" => p.full = true,
            "--zone" => p.zone = it.next().cloned(),
            "--at" => p.at = it.next().cloned(),
            "--split" => p.split = it.next().cloned(),
            _ => p.positional.push(a.clone()),
        }
    }
    p
}

// ─── Ownership ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum TargetDecision {
    /// `claim`: an unowned pane the caller now takes.
    Resolved { id: String, claim: bool },
    NotFound,
    Ambiguous { mine: Vec<String> },
    None { all: Vec<String> },
}

/// Electron's decideBrowserTarget. `list` is (pane id, owner key).
pub fn decide_target(list: &[(String, Option<String>)], me: &str, explicit: Option<&str>) -> TargetDecision {
    if let Some(t) = explicit {
        return match list.iter().find(|(id, _)| id == t) {
            Some((id, _)) => TargetDecision::Resolved { id: id.clone(), claim: false },
            None => TargetDecision::NotFound,
        };
    }
    let mine: Vec<String> = list.iter().filter(|(_, o)| o.as_deref() == Some(me)).map(|(id, _)| id.clone()).collect();
    match mine.len() {
        1 => return TargetDecision::Resolved { id: mine[0].clone(), claim: false },
        n if n > 1 => return TargetDecision::Ambiguous { mine },
        _ => {}
    }
    let unowned: Vec<&String> = list.iter().filter(|(_, o)| o.is_none()).map(|(id, _)| id).collect();
    if unowned.len() == 1 {
        return TargetDecision::Resolved { id: unowned[0].clone(), claim: true };
    }
    TargetDecision::None { all: list.iter().map(|(id, _)| id.clone()).collect() }
}

/// Who owns a pane, as one key: its agent's terminal plus zone name.
pub fn owner_key(content: &ContainerContent) -> Option<String> {
    let terminal = content.extra_str("browserOwnerTerminal")?;
    let zone = content.extra_str("browserOwnerZone").unwrap_or_else(|| DEFAULT_ZONE.into());
    Some(format!("{terminal}#{zone}"))
}

fn claim(content: &mut ContainerContent, owner: &ZoneOwner) {
    content.set_extra_str("browserOwnerTerminal", Some(owner.terminal.clone()));
    content.set_extra_str("browserOwnerZone", (owner.name != DEFAULT_ZONE).then(|| owner.name.clone()));
}

// ─── Command plans ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum NetOp {
    Start,
    Stop,
    Clear,
    List,
}

/// What a command does to a resolved pane.
#[derive(Debug, PartialEq)]
pub enum Action {
    Back,
    Forward,
    Reload,
    Screenshot { full: bool },
    Url,
    Title,
    Console,
    Network(NetOp),
    /// A script answering with a reply object, spread into the reply.
    Script(String),
    /// A script answering with a string, put in the reply as `field`.
    Text { script: String, field: &'static str },
    /// The agent's JS: `{result}`.
    Eval(String),
    WaitLoad(u64),
    Sleep(u64),
    /// Focus `target` first (if given), then press.
    Press { script: String, focus: Option<String> },
}

fn usage(error: &str, message: &str) -> Value {
    fail(error, message)
}

/// The plan for a pane command, or the reply refusing it.
pub fn plan(cmd: &str, rest: &[String], p: &ParsedArgv) -> Result<Action, Value> {
    let wait_ms = p.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
    let arg = |i: usize| rest.get(i).map(String::as_str).filter(|s| !s.is_empty());
    let joined = |from: usize| rest.get(from..).map(|r| r.join(" ")).unwrap_or_default();
    let target = |name: &str| arg(0).map(str::to_owned).ok_or_else(|| usage("missing_target", &format!("usage: {name} <target>")));
    Ok(match cmd {
        "back" => Action::Back,
        "forward" => Action::Forward,
        "reload" => Action::Reload,
        "screenshot" => Action::Screenshot { full: p.full },
        "eval" | "js" => {
            let code = joined(0);
            if code.is_empty() {
                return Err(usage("missing_js", "usage: eval <js>"));
            }
            Action::Eval(code)
        }
        "get" => {
            let what = arg(0).unwrap_or_default().to_lowercase();
            let sel = arg(1);
            match (what.as_str(), sel) {
                ("url", _) => Action::Url,
                ("title", _) => Action::Title,
                ("console", _) => Action::Console,
                ("network", _) => Action::Network(NetOp::List),
                ("text", None) => Action::Text { script: bridge::page_text_script(), field: "text" },
                ("html", None) => Action::Text { script: bridge::page_html_script(), field: "html" },
                ("text" | "html", Some(s)) => Action::Script(bridge::get_element_script(&what, s, None)),
                ("value" | "count" | "box", None) => return Err(usage("missing_selector", &format!("usage: get {what} <selector>"))),
                ("value" | "count" | "box", Some(s)) => Action::Script(bridge::get_element_script(&what, s, None)),
                ("attr", Some(s)) if arg(2).is_some() => Action::Script(bridge::get_element_script("attr", s, arg(2))),
                ("attr", _) => return Err(usage("missing_args", "usage: get attr <selector> <attribute>")),
                _ => return Err(usage("unknown_get", "get url|title|text|html|value|attr|count|box|console|network")),
            }
        }
        "is" => {
            let what = arg(0).unwrap_or_default().to_lowercase();
            match (what.as_str(), arg(1)) {
                ("visible" | "enabled" | "checked", Some(s)) => Action::Script(bridge::is_state_script(&what, s)),
                _ => return Err(usage("usage", "is visible|enabled|checked <selector>")),
            }
        }
        "hover" => Action::Script(bridge::hover_script(&target("hover")?)),
        "focus" => Action::Script(bridge::focus_script(&target("focus")?)),
        "dblclick" | "doubleclick" => Action::Script(bridge::dblclick_script(&target("dblclick")?)),
        "check" | "uncheck" => Action::Script(bridge::check_script(&target(cmd)?, cmd == "check")),
        "scroll" => {
            let px = arg(1).and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite() && *n > 0.0).map_or(400, |n| n as u32);
            Action::Script(bridge::scroll_script(&arg(0).unwrap_or_default().to_lowercase(), px))
        }
        "scrollintoview" | "scrollinto" => Action::Script(bridge::scroll_into_view_script(&target("scrollintoview")?)),
        "press" => {
            let Some(combo) = arg(0) else {
                return Err(usage("missing_key", "usage: press <key> [target]  e.g. press Enter, press Control+a"));
            };
            let Some((key, mods)) = bridge::parse_key_combo(combo) else { return Err(usage("missing_key", "usage: press <key> [target]")) };
            Action::Press { script: bridge::press_script(&key, &mods), focus: arg(1).map(str::to_owned) }
        }
        "find" => {
            let strategy = arg(0).unwrap_or_default().to_lowercase();
            let valid = ["text", "role", "label", "placeholder", "testid", "selector"].contains(&strategy.as_str());
            let Some(query) = arg(1).filter(|_| valid) else {
                return Err(usage("usage", "find <text|role|label|placeholder|testid|selector> <query> <action> [value]"));
            };
            let action = arg(2).unwrap_or("click").to_lowercase();
            Action::Script(bridge::find_script(&strategy, query, &action, &joined(3)))
        }
        "pushstate" => Action::Script(bridge::push_state_script(arg(0).ok_or_else(|| usage("missing_url", "usage: pushstate <url>"))?)),
        "storage" => {
            let area = arg(0).unwrap_or_default().to_lowercase();
            if area != "local" && area != "session" {
                return Err(usage("usage", "storage local|session [get [key] | set <k> <v> | clear]"));
            }
            let op = arg(1).unwrap_or("get").to_lowercase();
            let key = arg(2).unwrap_or_default();
            if op == "set" && key.is_empty() {
                return Err(usage("missing_args", "usage: storage <area> set <key> <value>"));
            }
            let value = if op == "set" { joined(3) } else { String::new() };
            Action::Script(bridge::storage_script(&area, &op, key, &value))
        }
        "cookies" => {
            let sub = arg(0).unwrap_or("list").to_lowercase();
            if sub != "list" && sub != "clear" {
                return Err(usage("usage", "cookies [list|clear]"));
            }
            Action::Script(bridge::cookies_script(&sub))
        }
        "pdf" => return Err(usage("not_supported", "pdf export isn't available in SplitGrid Lite (WKWebView); use screenshot")),
        "set" => {
            if arg(0).map(str::to_lowercase).as_deref() != Some("viewport") {
                return Err(usage("unknown_set", "set viewport <w> <h> | set viewport reset"));
            }
            let reset = arg(1).is_some_and(|a| a.eq_ignore_ascii_case("reset"));
            let size = |i| arg(i).and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite() && *n > 0.0);
            if !reset && (size(1).is_none() || size(2).is_none()) {
                return Err(usage("bad_args", "usage: set viewport <width> <height>"));
            }
            return Err(usage(
                "not_supported",
                "viewport emulation isn't available in SplitGrid Lite (WKWebView has no device emulation); resize the pane, or check the size with: eval \"[innerWidth, innerHeight]\"",
            ));
        }
        "snapshot" => Action::Script(bridge::snapshot_script(p.viewport)),
        "click" => {
            let Some(t) = arg(0) else { return Err(usage("missing_target", "usage: click <target>  (a snapshot ref or a CSS selector)")) };
            Action::Script(bridge::click_script(t))
        }
        "fill" => {
            let Some(t) = arg(0) else { return Err(usage("missing_target", "usage: fill <target> <text>  (target = ref or CSS selector)")) };
            Action::Script(bridge::fill_script(t, &joined(1)))
        }
        "type" => {
            let text = joined(0);
            if text.is_empty() {
                return Err(usage("missing_text", "usage: type <text>  (types into the focused field)"));
            }
            Action::Script(bridge::type_script(&text))
        }
        "select" => {
            let value = joined(1);
            match arg(0) {
                Some(t) if !value.is_empty() => Action::Script(bridge::select_script(t, &value)),
                _ => return Err(usage("missing_args", "usage: select <target> <value>  (target = ref or CSS selector)")),
            }
        }
        "wait" => {
            let what = arg(0).unwrap_or_default().to_lowercase();
            let tail = joined(1);
            let need = |s: &str, e: &str, m: &str| if s.is_empty() { Err(usage(e, m)) } else { Ok(()) };
            match what.as_str() {
                "load" | "navigation" => Action::WaitLoad(wait_ms),
                "selector" => {
                    need(&tail, "missing_selector", "usage: wait selector <css> [--timeout ms]")?;
                    Action::Script(bridge::wait_selector_script(&tail, wait_ms))
                }
                "text" => {
                    need(&tail, "missing_text", "usage: wait text <substring> [--timeout ms]")?;
                    Action::Script(bridge::wait_text_script(&tail, wait_ms))
                }
                "url" => {
                    need(&tail, "missing_pattern", "usage: wait url <substring|regex> [--timeout ms]")?;
                    Action::Script(bridge::wait_url_script(&tail, wait_ms))
                }
                "fn" | "function" => {
                    need(&tail, "missing_fn", "usage: wait fn <js-expression> [--timeout ms]")?;
                    Action::Script(bridge::wait_fn_script(&tail, wait_ms))
                }
                n => match n.parse::<f64>() {
                    Ok(ms) if ms.is_finite() && ms >= 0.0 => Action::Sleep((ms as u64).min(MAX_WAIT_MS)),
                    _ => return Err(usage("unknown_wait", "wait load | wait selector <css> | wait text <substr> | wait <ms>")),
                },
            }
        }
        "network" => Action::Network(match arg(0).unwrap_or("list").to_lowercase().as_str() {
            "start" => NetOp::Start,
            "stop" => NetOp::Stop,
            "clear" => NetOp::Clear,
            "list" => NetOp::List,
            _ => return Err(usage("unknown_network", "network start|stop|clear|list")),
        }),
        _ => return Err(usage("unknown_command", &format!("unknown command: {cmd}"))),
    })
}

/// `{ok, surface, ...inner}` where the inner reply's own `ok` (default true) wins, as in Electron.
pub fn merge_reply(surface: &str, inner: Value) -> Value {
    let mut out = Map::new();
    let inner = match inner {
        Value::Object(m) => m,
        Value::Null => Map::from_iter([("ok".to_owned(), false.into()), ("error".to_owned(), "no_result".into())]),
        other => Map::from_iter([("ok".to_owned(), false.into()), ("error".to_owned(), "bad_result".into()), ("raw".to_owned(), other)]),
    };
    out.insert("ok".into(), inner.get("ok").cloned().unwrap_or(Value::Bool(true)));
    out.insert("surface".into(), surface.into());
    out.extend(inner.into_iter().filter(|(k, _)| k != "ok"));
    Value::Object(out)
}

fn help() -> Value {
    ok(json!({
        "usage": "splitgrid-browser <command> [args]   (target = a snapshot ref \"e1\" OR a CSS selector)",
        "commands": [
            "list                         — browsers in this workspace (with ownership)",
            "open|navigate|goto <url>     — focus/create a browser and load url; a local file path",
            "                               (absolute) loads as file://, localhost:port as http://",
            "     [--split right|down] [--at <pane>]  — where a NEW pane goes in your zone",
            "close [all]                  — close your browser pane in your zone (or all of them)",
            "pushstate <url>              — SPA client-side navigation (history.pushState)",
            "back | forward | reload",
            "get url|title|text|html|console",
            "get text|html|value|attr|count|box <selector> [attr]  — read a specific element",
            "is visible|enabled|checked <selector>  — boolean state check",
            "screenshot                   — PNG path of the visible page (--full: not available, captures the viewport)",
            "eval <js>                    — run JS in the page, returns the result (Promises awaited)",
            "snapshot [--viewport]        — tag interactive elements (incl. same-origin iframes), returns refs",
            "click | dblclick | hover | focus <target>",
            "fill <target> <text>         — set an input/textarea value",
            "type <text>                  — type into the focused field",
            "press <key> [target]         — keypress (Enter, Tab, Control+a, …): synthetic DOM events + the default action",
            "select <target> <value>      — pick a <select> option",
            "check | uncheck <target>     — toggle a checkbox/radio",
            "scroll up|down|left|right [px] · scrollintoview <target>",
            "find <text|role|label|placeholder|testid|selector> <query> <action> [value]",
            "wait load|selector <css>|text <substr>|url <pattern>|fn <js>|<ms>",
            "storage local|session [get [key] | set <k> <v> | clear]",
            "cookies [list|clear]         — the page's non-HttpOnly cookies (document.cookie)",
            "clipboard read|write [text]",
            "network start|list|clear|stop  — capture the page's requests (fetch/XHR + resources)",
            "--target <surface>           — disambiguate when you own several browsers",
            "--target tab:<c>.<id>        — a tab activated in the SplitGrid Lite browser extension (see list):",
            "                               open, back, forward, reload, get url|title|text|html, eval, screenshot, close",
            "--zone <name>                — act as that zone (a subagent passes its own; default: main)",
            "--timeout <ms>               — cap for wait selector/text/url/fn/load (default 10s)",
        ],
        "notSupported": ["pdf", "set viewport"],
    }))
}

// ─── Screenshots ─────────────────────────────────────────────────────────────

static SHOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Our own scratch dir: the Electron app empties `splitgrid-browser` on launch.
pub(super) fn screenshot_dir() -> PathBuf {
    std::env::temp_dir().join("splitgrid-lite-browser")
}

pub(super) fn save_screenshot(dir: &Path, png: &[u8]) -> std::io::Result<PathBuf> {
    static RESET: std::sync::Once = std::sync::Once::new();
    // Fresh each run: these are throwaway verification shots.
    RESET.call_once(|| {
        let _ = std::fs::remove_dir_all(dir);
    });
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!("r{}.png", SHOT_COUNTER.fetch_add(1, Ordering::Relaxed) + 1));
    std::fs::write(&file, png)?;
    prune(dir, &file, SCREENSHOT_CAP);
    Ok(file)
}

/// Keep the newest `cap` (by number: a string sort puts r1000 before r950); never `keep`.
fn prune(dir: &Path, keep: &Path, cap: usize) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(u64, PathBuf)> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name.strip_prefix('r')?.strip_suffix(".png")?.parse().ok()?;
            Some((n, e.path()))
        })
        .filter(|(_, p)| p != keep)
        .collect();
    if files.len() < cap {
        return;
    }
    files.sort_by_key(|(n, _)| *n);
    for (_, p) in &files[..files.len() + 1 - cap] {
        let _ = std::fs::remove_file(p);
    }
}

// ─── Running ─────────────────────────────────────────────────────────────────

/// Run `script` in the pane: its value, or the error ("timeout" past [`SCRIPT_TIMEOUT`]).
async fn run_raw(pane: &Entity<BrowserPane>, script: &str, cx: &mut AsyncApp) -> Result<Value, String> {
    let task = pane.update(cx, |p, cx| p.eval(script, cx));
    let timer = cx.background_executor().timer(SCRIPT_TIMEOUT);
    match futures::future::select(task, timer).await {
        futures::future::Either::Left((Ok(v), _)) => Ok(v),
        futures::future::Either::Left((Err(e), _)) => Err(format!("{e:#}")),
        futures::future::Either::Right(_) => Err("timeout".into()),
    }
}

/// Run one of our scripts and decode its JSON-string answer.
async fn run_js(pane: &Entity<BrowserPane>, script: String, cx: &mut AsyncApp) -> Result<Value, String> {
    run_raw(pane, &script, cx).await.map(bridge::decode)
}

fn js_error(surface: &str, e: String) -> Value {
    merge_reply(surface, json!({ "ok": false, "error": e }))
}

async fn wait_load(pane: &Entity<BrowserPane>, surface: &str, timeout_ms: u64, cx: &mut AsyncApp) -> Value {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut first = true;
    loop {
        let id = pane.entity_id();
        let starting = cx.update(|cx| bridge::with_log(id, cx, |log| log.nav_pending(Instant::now())));
        if !starting
            && !pane.read_with(cx, |p, _| p.is_loading())
            && run_js(pane, bridge::ready_state_script(), cx).await.is_ok_and(|v| v == "complete")
        {
            let mut r = json!({ "ok": true, "surface": surface, "loaded": true });
            if first {
                r["alreadyIdle"] = true.into();
            }
            return r;
        }
        first = false;
        if Instant::now() >= deadline {
            return json!({ "ok": false, "surface": surface, "error": "timeout" });
        }
        cx.background_executor().timer(Duration::from_millis(100)).await;
    }
}

async fn execute(action: Action, pane: Entity<BrowserPane>, surface: String, cx: &mut AsyncApp) -> Value {
    let s = surface.as_str();
    match action {
        Action::Back => {
            pane.update(cx, |p, cx| p.back(cx));
            cx.update(|cx| bridge::mark_navigation(pane.entity_id(), cx));
            json!({ "ok": true, "surface": s })
        }
        Action::Forward => {
            pane.update(cx, |p, cx| p.forward(cx));
            cx.update(|cx| bridge::mark_navigation(pane.entity_id(), cx));
            json!({ "ok": true, "surface": s })
        }
        Action::Reload => {
            pane.update(cx, |p, cx| p.reload(cx));
            cx.update(|cx| bridge::mark_navigation(pane.entity_id(), cx));
            json!({ "ok": true, "surface": s })
        }
        Action::Url => json!({ "ok": true, "surface": s, "url": pane.read_with(cx, |p, _| p.url()) }),
        Action::Title => json!({ "ok": true, "surface": s, "title": pane.read_with(cx, |p, _| p.title()) }),
        Action::Console => {
            let id = pane.entity_id();
            let console = cx.update(|cx| bridge::with_log(id, cx, |log| serde_json::to_value(&log.console).unwrap_or_default()));
            json!({ "ok": true, "surface": s, "console": console })
        }
        Action::Network(op) => {
            let id = pane.entity_id();
            let data = cx.update(|cx| bridge::with_log(id, cx, |log| network_op(log, op)));
            merge_reply(s, data)
        }
        Action::Screenshot { full } => {
            let task = pane.update(cx, |p, cx| p.screenshot(cx));
            match task.await {
                Ok(png) => match save_screenshot(&screenshot_dir(), &png) {
                    Ok(path) => {
                        let mut r = json!({ "ok": true, "surface": s, "screenshot": path.to_string_lossy() });
                        if full {
                            r["captured"] = "viewport".into();
                            r["note"] = "full-page capture isn't available in SplitGrid Lite; scroll and take more screenshots".into();
                        }
                        r
                    }
                    Err(e) => json!({ "ok": false, "surface": s, "error": "screenshot_failed", "screenshotError": e.to_string() }),
                },
                Err(e) => json!({ "ok": false, "surface": s, "error": "screenshot_failed", "screenshotError": format!("{e:#}") }),
            }
        }
        // The pane's eval already takes a statement list or an expression, awaits a
        // Promise and JSON-encodes the value; a throw is data, as in Electron.
        Action::Eval(code) => match run_raw(&pane, &code, cx).await {
            Ok(result) => json!({ "ok": true, "surface": s, "result": result }),
            Err(e) if e == "timeout" => js_error(s, e),
            Err(e) => json!({ "ok": true, "surface": s, "result": { "__evalError": e } }),
        },
        Action::Script(script) => match run_js(&pane, script, cx).await {
            Ok(v) => merge_reply(s, v),
            Err(e) => js_error(s, e),
        },
        Action::Text { script, field } => match run_js(&pane, script, cx).await {
            Ok(v) => {
                let mut r = json!({ "ok": true, "surface": s });
                r[field] = Value::String(v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()));
                r
            }
            Err(e) => js_error(s, e),
        },
        Action::WaitLoad(ms) => wait_load(&pane, s, ms, cx).await,
        Action::Sleep(ms) => {
            cx.background_executor().timer(Duration::from_millis(ms)).await;
            json!({ "ok": true, "surface": s, "sleptMs": ms })
        }
        Action::Press { script, focus } => {
            if let Some(t) = focus {
                let _ = run_js(&pane, bridge::focus_script(&t), cx).await;
            }
            match run_js(&pane, script, cx).await {
                Ok(v) => merge_reply(s, v),
                Err(e) => js_error(s, e),
            }
        }
    }
}

/// Electron's handleNetworkOp, over the pane's log.
pub fn network_op(log: &mut bridge::PaneLog, op: NetOp) -> Value {
    match op {
        NetOp::Stop => {
            log.capturing = false;
            json!({ "ok": true, "capturing": false, "stopped": true })
        }
        NetOp::Clear => {
            log.requests.clear();
            json!({ "ok": true, "cleared": true, "capturing": log.capturing })
        }
        NetOp::Start => {
            log.requests.clear();
            log.capturing = true;
            json!({ "ok": true, "capturing": true, "started": true, "requests": [] })
        }
        NetOp::List => {
            let fresh = !log.capturing;
            log.capturing = true;
            let mut r = json!({ "ok": true, "capturing": true });
            if fresh {
                r["started"] = true.into();
                r["note"] = "capture just started — re-run after the page makes requests".into();
            }
            r["count"] = log.requests.len().into();
            r["requests"] = serde_json::to_value(&log.requests).unwrap_or_default();
            r
        }
    }
}

impl AppView {
    /// Run one `sg-browser` command for the agent in pane `terminal`.
    pub(crate) fn run_browser_command(&mut self, terminal: &str, argv: &[String], window: &mut Window, cx: &mut Context<Self>) -> Answer {
        if argv.is_empty() {
            return Answer::Now(fail("empty_command", "usage: splitgrid-browser <cmd> [args]"));
        }
        let parsed = parse_argv(argv);
        let cmd = parsed.positional.first().map(|s| s.to_lowercase()).unwrap_or_default();
        let rest = parsed.positional.get(1..).unwrap_or_default().to_vec();

        if matches!(cmd.as_str(), "help" | "--help" | "-h" | "") {
            return Answer::Now(help());
        }
        // `--target tab:…` names a tab in the user's real browser (the companion
        // extension), not a pane: those commands never reach a pane.
        if let Some(t) = parsed
            .target
            .as_deref()
            .filter(|t| super::ext_protocol::is_surface(t))
        {
            let ext = self.agent.as_ref().and_then(|a| a.ext().cloned());
            if let Some(answer) = super::ext_route::run_tab(ext, t, argv, cx) {
                return answer;
            }
        }
        if cmd == "clipboard" {
            let sub = rest.first().map(|s| s.to_lowercase()).unwrap_or_else(|| "read".into());
            return Answer::Now(match sub.as_str() {
                "write" => {
                    cx.write_to_clipboard(ClipboardItem::new_string(rest[1..].join(" ")));
                    ok(json!({ "wrote": true }))
                }
                "read" => ok(json!({ "text": cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default() })),
                _ => fail("unknown_clipboard", "clipboard read|write [text]"),
            });
        }

        let Some(ws_ix) = self.state.workspaces.iter().position(|w| w.container(terminal).is_some()) else {
            return Answer::Now(fail("unknown_terminal", "cannot locate the calling terminal's workspace"));
        };
        let owner = ZoneOwner::new(terminal, parsed.zone.as_deref());
        let me = format!("{}#{}", owner.terminal, owner.name);

        match cmd.as_str() {
            "list" => Answer::Now(self.browser_list(ws_ix, &owner, &me, cx)),
            "open" | "navigate" | "goto" => Answer::Now(self.browser_open(ws_ix, &cmd, &rest, &parsed, &owner, &me, window, cx)),
            "close" | "quit" => Answer::Now(self.browser_close(ws_ix, &rest, &parsed, &owner, &me, cx)),
            _ => {
                let action = match plan(&cmd, &rest, &parsed) {
                    Ok(a) => a,
                    Err(e) => return Answer::Now(e),
                };
                let surface = match self.resolve_browser(ws_ix, &me, parsed.target.as_deref(), cx) {
                    Ok((id, claimed)) => {
                        if claimed {
                            self.claim_browser(&id, &owner, cx);
                        }
                        id
                    }
                    Err(e) => return Answer::Now(e),
                };
                Answer::Later(cx.spawn(async move |this: WeakEntity<AppView>, cx: &mut AsyncApp| {
                    match wait_for_pane(&this, &surface, cx).await {
                        Some(pane) => execute(action, pane, surface, cx).await,
                        None => json!({ "ok": false, "surface": surface, "error": "browser_not_ready" }),
                    }
                }))
            }
        }
    }

    /// The workspace's browser panes: (id, owner key).
    fn browser_owners(&self, ws_ix: usize) -> Vec<(String, Option<String>)> {
        self.state.workspaces[ws_ix]
            .containers
            .iter()
            .filter(|c| c.content.kind == ContentType::Browser)
            .map(|c| (c.id.clone(), owner_key(&c.content)))
            .collect()
    }

    fn browser_info(&self, c: &Container, cx: &Context<Self>) -> Value {
        let live = self.browser_pane(&c.id);
        let url = live.as_ref().map(|p| p.read(cx).url()).filter(|u| !u.is_empty()).or_else(|| c.content.extra_str("browserUrl")).unwrap_or_default();
        let title = live.as_ref().map(|p| p.read(cx).title()).filter(|t| !t.is_empty()).or_else(|| c.content.label.clone()).unwrap_or_else(|| "Browser".into());
        let mut info = json!({ "id": c.id, "url": url, "title": title });
        if let Some(o) = c.content.extra_str("browserOwnerTerminal") {
            info["owner"] = o.into();
        }
        info
    }

    fn browser_infos(&self, ws_ix: usize, ids: &[String], cx: &Context<Self>) -> Vec<Value> {
        let ws = &self.state.workspaces[ws_ix];
        ids.iter().filter_map(|id| ws.container(id)).map(|c| self.browser_info(c, cx)).collect()
    }

    /// The existing pane a command acts on (and whether it was just claimed), or the refusal.
    fn resolve_browser(&self, ws_ix: usize, me: &str, target: Option<&str>, cx: &Context<Self>) -> Result<(String, bool), Value> {
        match decide_target(&self.browser_owners(ws_ix), me, target) {
            TargetDecision::Resolved { id, claim } => Ok((id, claim)),
            TargetDecision::NotFound => Err(reply(false, Some("target_not_found"), json!({ "target": target }))),
            TargetDecision::Ambiguous { mine } => Err(reply(
                false,
                Some("ambiguous_target"),
                json!({ "message": "you own multiple browsers here — pass --target <surface>", "browsers": self.browser_infos(ws_ix, &mine, cx) }),
            )),
            TargetDecision::None { all } => Err(reply(
                false,
                Some("no_owned_target"),
                json!({ "message": "no browser you own here — pass --target <surface> or run: open <url>", "browsers": self.browser_infos(ws_ix, &all, cx) }),
            )),
        }
    }

    fn claim_browser(&mut self, id: &str, owner: &ZoneOwner, cx: &mut Context<Self>) {
        if let Some(c) = self.state.workspaces.iter_mut().find_map(|w| w.containers.iter_mut().find(|c| c.id == id)) {
            claim(&mut c.content, owner);
            self.changed(cx);
        }
    }

    fn browser_list(&self, ws_ix: usize, owner: &ZoneOwner, me: &str, cx: &Context<Self>) -> Value {
        let ws = &self.state.workspaces[ws_ix];
        let in_zone = zones::zone_pane_ids(ws.layout_tree.as_ref(), owner);
        let browsers: Vec<Value> = ws
            .containers
            .iter()
            .filter(|c| c.content.kind == ContentType::Browser)
            .map(|c| {
                let mut info = json!({ "kind": "pane" });
                if let (Value::Object(dst), Value::Object(src)) = (&mut info, self.browser_info(c, cx)) {
                    dst.extend(src);
                }
                info["mine"] = (owner_key(&c.content).as_deref() == Some(me)).into();
                info["inZone"] = in_zone.contains(&c.id).into();
                info
            })
            .collect();
        // Plus the tabs activated in the browser extension: one call finds every surface.
        let ext = self.agent.as_ref().and_then(|a| a.ext());
        let mut browsers = browsers;
        browsers.extend(
            ext.map(|e| super::ext_route::list_entries(&e.surfaces()))
                .unwrap_or_default(),
        );
        ok(
            json!({ "browsers": browsers, "extensionConnected": ext.is_some_and(|e| e.is_connected()) }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn browser_open(
        &mut self,
        ws_ix: usize,
        cmd: &str,
        rest: &[String],
        parsed: &ParsedArgv,
        owner: &ZoneOwner,
        me: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Value {
        let url = crate::browser::url::normalize_input(&rest.join(" "));
        if url.is_empty() {
            return fail("missing_url", format!("usage: {cmd} <url>"));
        }
        let decision = decide_target(&self.browser_owners(ws_ix), me, parsed.target.as_deref());
        match decision {
            TargetDecision::Resolved { id, claim: claimed } => {
                if let Some(pane) = self.browser_pane(&id) {
                    pane.update(cx, |p, cx| p.navigate(&url, cx));
                    bridge::mark_navigation(pane.entity_id(), cx);
                }
                if let Some(c) = self.state.workspaces[ws_ix].containers.iter_mut().find(|c| c.id == id) {
                    c.content.set_extra_str("browserUrl", Some(url.clone()));
                    if claimed {
                        claim(&mut c.content, owner);
                    }
                }
                self.changed(cx);
                return ok(json!({ "surface": id, "url": url, "reused": true }));
            }
            TargetDecision::Ambiguous { .. } if cmd != "open" => {
                return self.resolve_browser(ws_ix, me, parsed.target.as_deref(), cx).err().unwrap_or_default();
            }
            _ => {}
        }

        // A new pane, owned by this agent, in its zone.
        if let Some(z) = &parsed.zone
            && !valid_zone_name(z)
        {
            return fail("bad_zone", "--zone takes a short name: letters, digits, \"-\", \"_\" or \".\" (up to 40)");
        }
        let split = match ZoneSplit::parse(parsed.split.as_deref()) {
            None => None,
            Some(Some(s)) => Some(s),
            Some(None) => return fail("bad_split", "--split takes right or down"),
        };
        let terminal = owner.terminal.as_str();
        let ws = &self.state.workspaces[ws_ix];
        if let Some(at) = &parsed.at
            && !zones::zone_layout(ws.layout_tree.as_ref(), owner).main.contains(at)
        {
            let panes = zone_panes(ws, owner);
            let message = if panes.as_array().is_some_and(|a| !a.is_empty()) {
                "--at must be one of your zone's regular panes"
            } else {
                "your zone has no panes yet; open one without --at"
            };
            return reply(false, Some("not_in_zone"), json!({ "message": message, "zone": owner.name, "panes": panes }));
        }
        let caller_label = ws.container(terminal).map(|c| label_of(c, self.process_name(terminal))).unwrap_or_else(|| "Agent".into());
        let zone_label = if owner.name == DEFAULT_ZONE { caller_label } else { format!("{caller_label} · {}", owner.name) };

        let mut content = ContainerContent { kind: ContentType::Browser, label: Some("Browser".into()), ..ContainerContent::empty() };
        content.set_extra_str("browserUrl", Some(url.clone()));
        content.set_extra_str("browserPartition", Some(crate::browser::DEFAULT_PARTITION.into()));
        claim(&mut content, owner);

        let id = new_id("c");
        let ws = &mut self.state.workspaces[ws_ix];
        let args = OpenArgs {
            owner,
            label: &zone_label,
            new_id: &id,
            anchor_id: Some(terminal),
            at: parsed.at.as_deref(),
            split,
            background: false,
        };
        match zones::open_in_zone(ws.layout_tree.take(), &args) {
            Ok(tree) => ws.layout_tree = Some(tree),
            Err(message) => return fail("not_in_zone", message),
        }
        ws.containers.push(Container { id: id.clone(), content });
        ws.updated_at = crate::model::workspace::now_ms();
        if let Some(pane) = self.open_browser_pane(&id, window, cx) {
            bridge::mark_navigation(pane.entity_id(), cx);
        }
        self.changed(cx);
        ok(json!({ "surface": id, "url": url, "created": true, "zone": owner.name }))
    }

    fn browser_close(&mut self, ws_ix: usize, rest: &[String], parsed: &ParsedArgv, owner: &ZoneOwner, me: &str, cx: &mut Context<Self>) -> Value {
        let ws = &self.state.workspaces[ws_ix];
        let in_zone = zones::zone_pane_ids(ws.layout_tree.as_ref(), owner);
        if rest.first().is_some_and(|a| a.eq_ignore_ascii_case("all")) {
            let mine: Vec<String> = self.browser_owners(ws_ix).into_iter().map(|(id, _)| id).filter(|id| in_zone.contains(id)).collect();
            for id in &mine {
                self.close_pane(id, cx);
            }
            return ok(json!({ "closed": mine, "count": mine.len() }));
        }
        let id = match self.resolve_browser(ws_ix, me, parsed.target.as_deref(), cx) {
            Ok((id, _)) => id,
            Err(e) => return e,
        };
        if !in_zone.contains(&id) {
            return reply(
                false,
                Some("not_in_zone"),
                json!({ "message": "only browser panes in your zone can be closed; ask the user to close this one",
                        "surface": id, "zone": owner.name, "panes": zone_panes(ws, owner) }),
            );
        }
        self.close_pane(&id, cx);
        ok(json!({ "surface": id, "closed": true }))
    }
}

/// The pane's view, waiting briefly for one that was just opened.
async fn wait_for_pane(this: &WeakEntity<AppView>, id: &str, cx: &mut AsyncApp) -> Option<Entity<BrowserPane>> {
    let deadline = Instant::now() + PANE_APPEAR_TIMEOUT;
    loop {
        let found = this.read_with(cx, |v, _| (v.browser_pane(id), v.state.workspaces.iter().any(|w| w.container(id).is_some()))).ok()?;
        match found {
            (Some(pane), _) => return Some(pane),
            (None, false) => return None,
            (None, true) if Instant::now() >= deadline => return None,
            _ => cx.background_executor().timer(Duration::from_millis(50)).await,
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
    fn argv_flags_anywhere() {
        let p = parse_argv(&s(&["wait", "--timeout", "99999", "selector", "#a", "--target", "c-2", "--zone", "rev", "--full", "--viewport"]));
        assert_eq!(p.positional, s(&["wait", "selector", "#a"]));
        assert_eq!(p.timeout_ms, Some(MAX_WAIT_MS));
        assert_eq!(p.target.as_deref(), Some("c-2"));
        assert_eq!(p.zone.as_deref(), Some("rev"));
        assert!(p.full && p.viewport);
        let p = parse_argv(&s(&["open", "x", "-t", "nope", "--split", "down", "--at", "c-1"]));
        assert_eq!(p.timeout_ms, None);
        assert_eq!((p.split.as_deref(), p.at.as_deref()), (Some("down"), Some("c-1")));
        assert_eq!(parse_argv(&s(&["wait", "-t", "-5"])).timeout_ms, Some(0));
    }

    #[test]
    fn ownership_matrix() {
        let me = "c-1#main";
        let list = |v: &[(&str, Option<&str>)]| v.iter().map(|(i, o)| (i.to_string(), o.map(str::to_owned))).collect::<Vec<_>>();
        let l = list(&[("b1", Some("c-9#main")), ("b2", Some(me))]);
        assert_eq!(decide_target(&l, me, None), TargetDecision::Resolved { id: "b2".into(), claim: false });
        assert_eq!(decide_target(&l, me, Some("b1")), TargetDecision::Resolved { id: "b1".into(), claim: false });
        assert_eq!(decide_target(&l, me, Some("zz")), TargetDecision::NotFound);
        let l = list(&[("b1", Some(me)), ("b2", Some(me))]);
        assert_eq!(decide_target(&l, me, None), TargetDecision::Ambiguous { mine: vec!["b1".into(), "b2".into()] });
        let l = list(&[("b1", Some("c-9#main")), ("b2", None)]);
        assert_eq!(decide_target(&l, me, None), TargetDecision::Resolved { id: "b2".into(), claim: true });
        // Same terminal, other zone: a subagent doesn't take its parent's pane.
        let l = list(&[("b1", Some("c-1#rev")), ("b2", None), ("b3", None)]);
        assert_eq!(decide_target(&l, me, None), TargetDecision::None { all: vec!["b1".into(), "b2".into(), "b3".into()] });
    }

    #[test]
    fn owner_keys_and_claims() {
        let mut c = ContainerContent { kind: ContentType::Browser, ..ContainerContent::empty() };
        assert_eq!(owner_key(&c), None);
        claim(&mut c, &ZoneOwner::new("c-1", None));
        assert_eq!(owner_key(&c).as_deref(), Some("c-1#main"));
        assert!(!c.extra.contains_key("browserOwnerZone"), "main zone is stored as absent, like Electron");
        claim(&mut c, &ZoneOwner::new("c-1", Some("review")));
        assert_eq!(owner_key(&c).as_deref(), Some("c-1#review"));
        assert_eq!(c.extra["browserOwnerZone"], "review");
    }

    #[test]
    fn plans() {
        let p = ParsedArgv::default();
        let run = |argv: &[&str]| {
            let a = s(argv);
            plan(&a[0], &a[1..], &p)
        };
        assert_eq!(run(&["get", "url"]), Ok(Action::Url));
        assert_eq!(run(&["get", "network"]), Ok(Action::Network(NetOp::List)));
        assert_eq!(run(&["network"]), Ok(Action::Network(NetOp::List)));
        assert_eq!(run(&["wait", "250"]), Ok(Action::Sleep(250)));
        assert_eq!(run(&["wait", "999999"]), Ok(Action::Sleep(MAX_WAIT_MS)));
        assert_eq!(run(&["wait", "load"]), Ok(Action::WaitLoad(DEFAULT_WAIT_MS)));
        assert_eq!(run(&["eval", "1", "+", "1"]), Ok(Action::Eval("1 + 1".into())));
        assert!(matches!(run(&["get", "text"]), Ok(Action::Text { field: "text", .. })));
        assert!(matches!(run(&["get", "text", "#a"]), Ok(Action::Script(_))));
        assert!(matches!(run(&["press", "Enter", "#q"]), Ok(Action::Press { focus: Some(ref f), .. }) if f == "#q"));
        let err = |argv: &[&str]| run(argv).unwrap_err()["error"].as_str().unwrap().to_owned();
        assert_eq!(err(&["eval"]), "missing_js");
        assert_eq!(err(&["click"]), "missing_target");
        assert_eq!(err(&["get", "attr", "a"]), "missing_args");
        assert_eq!(err(&["wait", "nope"]), "unknown_wait");
        assert_eq!(err(&["network", "pause"]), "unknown_network");
        assert_eq!(err(&["pdf"]), "not_supported");
        assert_eq!(err(&["set", "viewport", "x"]), "bad_args");
        assert_eq!(err(&["set", "viewport", "390", "844"]), "not_supported");
        assert_eq!(err(&["frobnicate"]), "unknown_command");
    }

    #[test]
    fn reply_shapes() {
        assert_eq!(merge_reply("b1", json!({ "ok": false, "error": "not_found", "target": "e9" })), json!({ "ok": false, "surface": "b1", "error": "not_found", "target": "e9" }));
        assert_eq!(merge_reply("b1", json!({ "count": 2 })), json!({ "ok": true, "surface": "b1", "count": 2 }));
        assert_eq!(merge_reply("b1", Value::Null), json!({ "ok": false, "surface": "b1", "error": "no_result" }));
        // Key order: ok, surface, then the script's fields.
        let keys: Vec<String> = merge_reply("b1", json!({ "x": 1, "ok": true })).as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["ok", "surface", "x"]);
    }

    #[test]
    fn network_ops() {
        let mut log = bridge::PaneLog::default();
        let r = network_op(&mut log, NetOp::List);
        assert_eq!(r["started"], true);
        assert_eq!(r["count"], 0);
        assert!(log.capturing);
        assert!(network_op(&mut log, NetOp::List).get("started").is_none());
        log.requests.push(bridge::NetEntry { request_id: "1".into(), ..Default::default() });
        assert_eq!(network_op(&mut log, NetOp::Clear), json!({ "ok": true, "cleared": true, "capturing": true }));
        assert!(log.requests.is_empty());
        assert_eq!(network_op(&mut log, NetOp::Stop)["capturing"], false);
        assert_eq!(network_op(&mut log, NetOp::Start), json!({ "ok": true, "capturing": true, "started": true, "requests": [] }));
    }

    #[test]
    fn screenshots_are_pruned_by_number() {
        let dir = tempfile::tempdir().unwrap();
        for n in [1, 2, 950, 1000] {
            std::fs::write(dir.path().join(format!("r{n}.png")), b"x").unwrap();
        }
        let keep = dir.path().join("r1001.png");
        std::fs::write(&keep, b"x").unwrap();
        prune(dir.path(), &keep, 3);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["r1000.png", "r1001.png", "r950.png"]);
    }
}
