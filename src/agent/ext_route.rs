//! `--target tab:c1.842`: the sg-browser argv onto the extension's command
//! vocabulary (the Electron app's agent-browser-tab-route + the tab half of
//! agent-browser-bridge). A tab is just another target for the same CLI, so
//! this says NO loudly for everything the extension can't do yet, rather than
//! letting an agent build on a command that silently did nothing.

use std::sync::Arc;

use gpui_kit::{AsyncApp, Context, WeakEntity};
use serde_json::{Map, Value, json};

use super::browser_cmd::{Answer, parse_argv, save_screenshot, screenshot_dir};
use super::ext_channel::{Channel, ExtResult, now_ms};
use super::ext_protocol::{Command, errors};
use crate::app::AppView;

pub const NOT_SUPPORTED_ON_TAB: &str = "not_supported_on_tab";

#[derive(Debug, PartialEq)]
pub enum TabRoute {
    /// Means the same on any surface (list/help/clipboard): the pane path answers it.
    Pane,
    /// `info_field`: `get url|title` is one `info` round-trip, answering that field only.
    Command {
        command: Command,
        info_field: Option<&'static str>,
        extra: Option<Value>,
    },
    Error {
        error: &'static str,
        message: String,
    },
}

fn unsupported(name: &str) -> TabRoute {
    TabRoute::Error {
        error: NOT_SUPPORTED_ON_TAB,
        message: format!("\"{name}\" is not available on a browser tab yet (phase 1); it works on a SplitGrid browser pane."),
    }
}

fn command(command: Command) -> TabRoute {
    TabRoute::Command { command, info_field: None, extra: None }
}

pub fn route_tab_command(argv: &[String]) -> TabRoute {
    let p = parse_argv(argv);
    let cmd = p.positional.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let rest = p.positional.get(1..).unwrap_or_default();
    if matches!(cmd.as_str(), "" | "list" | "help" | "--help" | "-h" | "clipboard") {
        return TabRoute::Pane;
    }
    match cmd.as_str() {
        "open" | "navigate" | "goto" => match rest.first() {
            // Echo the url back the way the pane path does.
            Some(url) => TabRoute::Command { command: Command::Navigate { url: url.clone() }, info_field: None, extra: Some(json!({ "url": url })) },
            None => TabRoute::Error { error: "missing_url", message: format!("usage: {cmd} <url>") },
        },
        "back" => command(Command::Back),
        "forward" => command(Command::Forward),
        "reload" => command(Command::Reload),
        "get" => {
            let what = rest.first().map(|s| s.to_lowercase()).unwrap_or_default();
            // Element-scoped reads need the DOM automation script: pane-only for now.
            if rest.get(1).is_some() && (what == "text" || what == "html") {
                return unsupported(&format!("get {what} <selector>"));
            }
            match what.as_str() {
                "url" => TabRoute::Command { command: Command::Info, info_field: Some("url"), extra: None },
                "title" => TabRoute::Command { command: Command::Info, info_field: Some("title"), extra: None },
                "text" => command(Command::Text),
                "html" => command(Command::Html),
                "" => TabRoute::Error { error: "unknown_get", message: "on a browser tab: get url|title|text|html".into() },
                other => unsupported(&format!("get {other}")),
            }
        }
        "eval" => {
            let js = rest.join(" ");
            if js.is_empty() {
                return TabRoute::Error { error: "missing_js", message: "usage: eval <js>".into() };
            }
            command(Command::Eval { js })
        }
        "screenshot" => command(Command::Screenshot { full_page: p.full.then_some(true) }),
        // `close` on a tab hands it back to the user; it doesn't destroy it.
        "close" | "quit" => {
            if rest.first().is_some_and(|s| s.eq_ignore_ascii_case("all")) {
                return unsupported("close all");
            }
            TabRoute::Command { command: Command::Release, info_field: None, extra: Some(json!({ "closed": true })) }
        }
        other => unsupported(other),
    }
}

/// The reply when no browser is connected at all (distinct from a gone tab: the fix differs).
pub fn not_connected(surface: &str) -> Value {
    json!({
        "ok": false,
        "error": errors::NO_EXTENSION,
        "surface": surface,
        "message": "no browser extension is connected — pair one and activate a tab in the SplitGrid Lite extension, or drop --target to use a browser pane",
    })
}

/// A route error as the agent's reply.
pub fn route_error(surface: &str, error: &str, message: &str) -> Value {
    json!({ "ok": false, "error": error, "surface": surface, "message": message })
}

/// The extension's answer as the agent's reply: the same shape as for a pane,
/// screenshots spilled to a PNG path via `save_png`.
pub fn finish(
    surface: &str,
    command: &Command,
    info_field: Option<&str>,
    extra: Option<Value>,
    res: ExtResult,
    save_png: impl FnOnce(&[u8]) -> std::io::Result<std::path::PathBuf>,
) -> Value {
    let mut out = Map::new();
    let mut data = res.data;
    let fail = |error: String, mut fields: Map<String, Value>| {
        let mut out = Map::new();
        out.insert("ok".into(), false.into());
        out.insert("surface".into(), surface.into());
        fields.remove("ok");
        fields.remove("surface");
        out.extend(fields);
        out.insert("error".into(), error.into());
        Value::Object(out)
    };
    if !res.ok {
        return fail(res.error.unwrap_or_else(|| "ext_failed".into()), data);
    }
    out.insert("ok".into(), true.into());
    out.insert("surface".into(), surface.into());
    if let Some(field) = info_field {
        out.insert(field.into(), data.get(field).cloned().unwrap_or_else(|| "".into()));
        return Value::Object(out);
    }
    if matches!(command, Command::Screenshot { .. }) {
        let b64 = data.remove("pngBase64");
        let png = b64.as_ref().and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.decode(s)
        });
        return match png {
            None => {
                data.insert("message".into(), "the extension returned no image".into());
                fail("screenshot_failed".into(), data)
            }
            Some(Err(e)) => fail("screenshot_failed".into(), Map::from_iter([("screenshotError".to_owned(), e.to_string().into())])),
            Some(Ok(bytes)) => match save_png(&bytes) {
                Ok(path) => {
                    data.remove("ok");
                    data.remove("surface");
                    out.extend(data);
                    out.insert("screenshot".into(), path.to_string_lossy().into_owned().into());
                    Value::Object(out)
                }
                Err(e) => fail("screenshot_failed".into(), Map::from_iter([("screenshotError".to_owned(), e.to_string().into())])),
            },
        };
    }
    data.remove("ok");
    data.remove("surface");
    out.extend(data);
    if let Some(Value::Object(extra)) = extra {
        out.extend(extra);
    }
    Value::Object(out)
}

/// Run a `--target tab:…` command through the extension; None for the commands
/// that mean the same on any surface (the pane path answers those).
pub fn run_tab(ext: Option<Arc<Channel>>, surface: &str, argv: &[String], cx: &mut Context<AppView>) -> Option<Answer> {
    let (command, info_field, extra) = match route_tab_command(argv) {
        TabRoute::Pane => return None,
        TabRoute::Error { error, message } => {
            return Some(Answer::Now(route_error(surface, error, &message)));
        }
        TabRoute::Command { command, info_field, extra } => (command, info_field, extra),
    };
    let Some(ext) = ext.filter(|e| e.is_connected()) else {
        return Some(Answer::Now(not_connected(surface)));
    };
    let surface = surface.to_owned();
    Some(match ext.run_command(&surface, &command, now_ms()) {
        Err(res) => Answer::Now(finish(&surface, &command, info_field, extra, res, |_| Err(std::io::ErrorKind::Unsupported.into()))),
        Ok(rx) => Answer::Later(cx.spawn(async move |_: WeakEntity<AppView>, _: &mut AsyncApp| {
            let res = rx.await.unwrap_or_else(|_| ExtResult::err(errors::NO_EXTENSION));
            finish(&surface, &command, info_field, extra, res, |png| save_screenshot(&screenshot_dir(), png))
        })),
    })
}

/// The `list` entries for the activated tabs (kind "tab"; `id` is what --target takes).
pub fn list_entries(surfaces: &[super::ext_channel::SurfaceInfo]) -> Vec<Value> {
    surfaces
        .iter()
        .map(|t| {
            json!({
                "kind": "tab",
                "id": t.surface,
                "surface": t.surface,
                "url": t.url,
                "title": t.title,
                "browser": t.browser,
                // The extension-side interaction switch: read-only tabs refuse eval/open.
                "writable": t.writable,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn cmd(c: Command) -> TabRoute {
        TabRoute::Command { command: c, info_field: None, extra: None }
    }

    #[test]
    fn supported_ops() {
        for c in ["open", "navigate", "goto"] {
            assert_eq!(
                route_tab_command(&s(&[c, "http://localhost:3000"])),
                TabRoute::Command {
                    command: Command::Navigate { url: "http://localhost:3000".into() },
                    info_field: None,
                    extra: Some(json!({ "url": "http://localhost:3000" })),
                }
            );
        }
        assert_eq!(route_tab_command(&s(&["back"])), cmd(Command::Back));
        assert_eq!(route_tab_command(&s(&["forward"])), cmd(Command::Forward));
        assert_eq!(route_tab_command(&s(&["reload"])), cmd(Command::Reload));
        assert_eq!(route_tab_command(&s(&["get", "url"])), TabRoute::Command { command: Command::Info, info_field: Some("url"), extra: None });
        assert_eq!(route_tab_command(&s(&["get", "title"])), TabRoute::Command { command: Command::Info, info_field: Some("title"), extra: None });
        assert_eq!(route_tab_command(&s(&["get", "text"])), cmd(Command::Text));
        assert_eq!(route_tab_command(&s(&["get", "html"])), cmd(Command::Html));
        assert_eq!(route_tab_command(&s(&["eval", "document.title", "+", "\"!\""])), cmd(Command::Eval { js: "document.title + \"!\"".into() }));
        assert_eq!(route_tab_command(&s(&["screenshot"])), cmd(Command::Screenshot { full_page: None }));
        assert_eq!(route_tab_command(&s(&["screenshot", "--full"])), cmd(Command::Screenshot { full_page: Some(true) }));
        assert_eq!(route_tab_command(&s(&["screenshot", "--fullpage"])), cmd(Command::Screenshot { full_page: Some(true) }));
        for c in ["close", "quit"] {
            assert_eq!(route_tab_command(&s(&[c])), TabRoute::Command { command: Command::Release, info_field: None, extra: Some(json!({ "closed": true })) });
        }
        // Flags it doesn't need (--target itself, --timeout) are ignored.
        assert_eq!(
            route_tab_command(&s(&["get", "title", "--target", "tab:c1.842", "--timeout", "2000"])),
            TabRoute::Command { command: Command::Info, info_field: Some("title"), extra: None }
        );
    }

    #[test]
    fn surface_independent_commands_stay_with_the_panes() {
        for c in ["list", "help", "--help", "-h", "clipboard", ""] {
            assert_eq!(route_tab_command(&s(&[c])), TabRoute::Pane, "{c}");
        }
    }

    #[test]
    fn unsupported_commands_fail_loudly() {
        for c in [
            "snapshot",
            "click",
            "fill",
            "type",
            "select",
            "check",
            "uncheck",
            "hover",
            "focus",
            "dblclick",
            "press",
            "find",
            "wait",
            "network",
            "cookies",
            "storage",
            "pdf",
            "is",
            "set",
            "scroll",
            "scrollintoview",
            "pushstate",
        ] {
            match route_tab_command(&s(&[c, "e1"])) {
                TabRoute::Error { error, message } => {
                    assert_eq!(error, NOT_SUPPORTED_ON_TAB);
                    assert!(message.contains(&format!("\"{c}\"")) && message.contains("browser pane"), "{message}");
                }
                other => panic!("{c}: {other:?}"),
            }
        }
        for argv in
            [&["get", "text", "#app"][..], &["get", "html", "#app"], &["get", "console"], &["get", "value", "#email"], &["get", "network"], &["close", "all"]]
        {
            assert!(matches!(route_tab_command(&s(argv)), TabRoute::Error { error: NOT_SUPPORTED_ON_TAB, .. }), "{argv:?}");
        }
        assert!(matches!(route_tab_command(&s(&["open"])), TabRoute::Error { error: "missing_url", .. }));
        assert!(matches!(route_tab_command(&s(&["eval"])), TabRoute::Error { error: "missing_js", .. }));
        assert!(matches!(route_tab_command(&s(&["get"])), TabRoute::Error { error: "unknown_get", .. }));
    }

    fn ok_result(data: Value) -> ExtResult {
        ExtResult { ok: true, data: data.as_object().cloned().unwrap_or_default(), error: None }
    }

    #[test]
    fn replies() {
        let never = |_: &[u8]| -> std::io::Result<std::path::PathBuf> { panic!("no screenshot here") };
        let info = ok_result(json!({ "url": "http://a/", "title": "A" }));
        assert_eq!(finish("tab:c1.5", &Command::Info, Some("title"), None, info, never), json!({ "ok": true, "surface": "tab:c1.5", "title": "A" }));
        let nav = ok_result(json!({ "title": "B" }));
        assert_eq!(
            finish("tab:c1.5", &Command::Navigate { url: "http://b".into() }, None, Some(json!({ "url": "http://b" })), nav, never),
            json!({ "ok": true, "surface": "tab:c1.5", "title": "B", "url": "http://b" })
        );
        let denied = ExtResult { ok: false, data: Map::from_iter([("message".to_owned(), "m".into())]), error: Some("write_denied".into()) };
        assert_eq!(
            finish("tab:c1.5", &Command::Reload, None, None, denied, never),
            json!({ "ok": false, "surface": "tab:c1.5", "message": "m", "error": "write_denied" })
        );
    }

    #[test]
    fn screenshots_are_spilled_to_a_path() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let png = b"\x89PNG fake";
        let res = ok_result(json!({ "pngBase64": base64::engine::general_purpose::STANDARD.encode(png), "width": 10 }));
        let path = dir.path().join("r1.png");
        let p2 = path.clone();
        let v = finish("tab:c1.5", &Command::Screenshot { full_page: None }, None, None, res, move |b| {
            std::fs::write(&p2, b)?;
            Ok(p2)
        });
        assert_eq!(v, json!({ "ok": true, "surface": "tab:c1.5", "width": 10, "screenshot": path.to_string_lossy() }));
        assert_eq!(std::fs::read(&path).unwrap(), png);

        let empty = ok_result(json!({}));
        let v = finish("tab:c1.5", &Command::Screenshot { full_page: None }, None, None, empty, |_| unreachable!());
        assert_eq!(v["error"], "screenshot_failed");
    }
}
