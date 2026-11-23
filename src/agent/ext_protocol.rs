//! The desktop ⇄ browser-extension wire protocol (the Electron app's
//! shared/ext-protocol.ts): an agent drives ONE tab the user hands over in
//! their real browser through a companion MV3 extension, as a second surface
//! beside the browser panes.
//!
//! Lite's extension (`extension/` in this repo) is a copy of the Electron one
//! that dials Lite's receiver (:19568) instead of :19558, so both can be
//! installed side by side. The message shapes, error codes and timeouts are the
//! same; `extension/src/ext-protocol.ts` is the TypeScript half of this file.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u64 = 1;

/// WebSocket upgrade path on the loopback receiver.
pub const WS_PATH: &str = "/ext";

/// Heartbeat period. Under 30s: an MV3 service worker is evicted after 30s
/// idle, and WebSocket traffic resets that timer.
pub const HEARTBEAT_MS: u64 = 20_000;

/// How long a command waits for the extension. Below the helper's 35s budget,
/// so the agent gets our error rather than a transport timeout.
pub const COMMAND_TIMEOUT_MS: u64 = 30_000;

/// Pairing codes are short-lived: the user types one out of Settings.
pub const PAIRING_TTL_MS: u64 = 3 * 60 * 1000;

/// A connected-but-unauthenticated socket gets this long to pair or say hello.
pub const AUTH_TIMEOUT_MS: u64 = 15_000;

/// Error codes both ends agree on (EXT_ERRORS).
pub mod errors {
    pub const NO_EXTENSION: &str = "no_extension";
    pub const UNKNOWN_TAB: &str = "unknown_tab";
    pub const WRITE_DENIED: &str = "write_denied";
    pub const TIMEOUT: &str = "ext_timeout";
    /// Desktop-only: agentChromeControl is off, so nothing may be sent.
    pub const DISABLED: &str = "chrome_control_disabled";
    /// Desktop-only: a bare `tab:<id>` while several browsers are connected.
    pub const AMBIGUOUS_TAB: &str = "ambiguous_tab";
}

/// Who is on the other end of the socket.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub extension_id: String,
    /// "Chrome 141", "Edge 141", … for display.
    pub browser: String,
    pub version: String,
}

/// A tab the user handed to the agent, as the extension pushes it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    pub tab_id: u64,
    pub title: String,
    pub url: String,
    /// The origin granted at activation; a cross-origin navigation revokes control.
    pub origin: String,
    /// The extension-side interaction switch (the user's half of the write gate).
    pub writable: bool,
    pub activated_at: f64,
}

/// A command for the extension (`{ op: … }` on the wire).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Command {
    Info,
    Text,
    Html,
    Eval {
        js: String,
    },
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    Screenshot {
        #[serde(rename = "fullPage", skip_serializing_if = "Option::is_none", default)]
        full_page: Option<bool>,
    },
    Release,
}

impl Command {
    pub fn op(&self) -> &'static str {
        match self {
            Command::Info => "info",
            Command::Text => "text",
            Command::Html => "html",
            Command::Eval { .. } => "eval",
            Command::Navigate { .. } => "navigate",
            Command::Back => "back",
            Command::Forward => "forward",
            Command::Reload => "reload",
            Command::Screenshot { .. } => "screenshot",
            Command::Release => "release",
        }
    }

    /// Ops that change the page or act as the user: allowed only with both the
    /// desktop's `agentChromeWrite` and the extension's per-tab switch on.
    /// `eval` counts (arbitrary JS in the user's session); `release` is
    /// de-escalation and always allowed.
    pub fn is_write(&self) -> bool {
        matches!(self, Command::Eval { .. } | Command::Navigate { .. } | Command::Back | Command::Forward | Command::Reload)
    }
}

// ─── Surface ids ─────────────────────────────────────────────────────────────
// `tab:<clientId>.<tabId>` (`tab:c1.842`); the bare `tab:<tabId>` is accepted
// while exactly one browser is connected.

pub const SURFACE_PREFIX: &str = "tab:";

pub fn format_surface(client_id: &str, tab_id: u64) -> String {
    format!("{SURFACE_PREFIX}{client_id}.{tab_id}")
}

pub fn is_surface(surface: &str) -> bool {
    surface.starts_with(SURFACE_PREFIX)
}

/// Is `id` a client id (`c<digits>`)?
pub fn is_client_id(id: &str) -> bool {
    id.strip_prefix('c').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `(client id, tab id)` of a tab surface; the client is None for the bare form.
pub fn parse_surface(surface: &str) -> Option<(Option<String>, u64)> {
    let rest = surface.strip_prefix(SURFACE_PREFIX)?;
    let (client, tab) = match rest.rfind('.') {
        Some(dot) => (Some(&rest[..dot]), &rest[dot + 1..]),
        None => (None, rest),
    };
    if tab.is_empty() || !tab.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if client.is_some_and(|c| !is_client_id(c)) {
        return None;
    }
    Some((client.map(str::to_owned), tab.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn surfaces() {
        assert_eq!(format_surface("c1", 842), "tab:c1.842");
        assert_eq!(parse_surface("tab:c1.842"), Some((Some("c1".into()), 842)));
        assert_eq!(parse_surface("tab:842"), Some((None, 842)));
        assert_eq!(parse_surface("tab:c12.0"), Some((Some("c12".into()), 0)));
        for bad in ["c1.842", "tab:", "tab:c1.", "tab:c1.x", "tab:x1.5", "tab:c.5", "tab:c1.-5", "tab:c1.5.6", "tab:1.5"] {
            assert_eq!(parse_surface(bad), None, "{bad}");
        }
        assert!(is_surface("tab:anything") && !is_surface("c-1"));
    }

    #[test]
    fn write_ops() {
        let writes: Vec<&str> = [
            Command::Info,
            Command::Text,
            Command::Html,
            Command::Eval { js: "1".into() },
            Command::Navigate { url: "x".into() },
            Command::Back,
            Command::Forward,
            Command::Reload,
            Command::Screenshot { full_page: None },
            Command::Release,
        ]
        .iter()
        .filter(|c| c.is_write())
        .map(Command::op)
        .collect();
        assert_eq!(writes, ["eval", "navigate", "back", "forward", "reload"]);
    }

    #[test]
    fn commands_on_the_wire() {
        assert_eq!(serde_json::to_value(Command::Info).unwrap(), json!({ "op": "info" }));
        assert_eq!(serde_json::to_value(Command::Eval { js: "1+1".into() }).unwrap(), json!({ "op": "eval", "js": "1+1" }));
        assert_eq!(serde_json::to_value(Command::Screenshot { full_page: None }).unwrap(), json!({ "op": "screenshot" }));
        assert_eq!(serde_json::to_value(Command::Screenshot { full_page: Some(true) }).unwrap(), json!({ "op": "screenshot", "fullPage": true }));
        assert_eq!(serde_json::to_value(Command::Navigate { url: "http://a".into() }).unwrap(), json!({ "op": "navigate", "url": "http://a" }));
    }
}
