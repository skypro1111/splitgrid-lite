//! The bridge that lets agents running in our terminals drive the app (the
//! Electron app's agent-* bridges), and the agent zones their panes go in.
//!
//! An agent finds us through its terminal's env: `$SPLITGRID_TERMINAL` (the
//! pane's container id), the receiver's endpoints and a per-run token, plus
//! the bundled helper scripts the skills call (`$SPLITGRID_TERMINAL_CLI`, …).
//! What gets injected follows the user's Agent integrations settings, which
//! live in `app-settings.json` shared with the Electron app.

pub mod activity;
pub mod attention;
pub mod browser_cmd;
pub mod install;
pub mod notify;
pub mod server;
pub mod sql_cmd;
pub mod sftp_cmd;
pub mod terminal_cmd;
pub mod zones;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
fn now_ms() -> u64 {
    crate::model::workspace::now_ms().max(0) as u64
}
use serde_json::Value;

pub struct AgentConfig {
    pub port: u16,
    /// The per-run secret every command (but hooks) must carry.
    pub token: String,
    helpers: PathBuf,
    settings_file: PathBuf,
}

/// The Agent integrations switches (Settings → Agent integrations).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    pub integrations: bool,
    pub terminal_control: bool,
}

impl AgentConfig {
    pub fn new(port: u16, data_dir: &Path) -> Self {
        let token: [u8; 24] = rand::random();
        Self {
            port,
            token: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token),
            helpers: helpers_dir(),
            settings_file: data_dir.join("app-settings.json"),
        }
    }

    /// Read fresh each time: the user may flip them in either app.
    pub fn switches(&self) -> Switches {
        let v: Value = std::fs::read(&self.settings_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let on = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let integrations = on("agentIntegrations");
        Switches { integrations, terminal_control: integrations && on("agentTerminalControl") }
    }

    /// (system notifications on, notify on Waiting, mute all), as the user set them.
    pub fn notify_prefs(&self) -> (bool, bool, bool) {
        let v: Value = std::fs::read(&self.settings_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let flag = |k: &str, default: bool| v.get(k).and_then(Value::as_bool).unwrap_or(default);
        (flag("systemNotifications", true), flag("notifyOnWaiting", true), flag("muteAll", false))
    }

    pub fn is_token(&self, value: &Value) -> bool {
        value.as_str().is_some_and(|t| constant_time_eq(t.as_bytes(), self.token.as_bytes()))
    }

    /// The env for a new terminal in pane `terminal`; empty when integrations are off.
    pub fn env_for(&self, terminal: &str) -> HashMap<String, String> {
        let sw = self.switches();
        let mut env = HashMap::new();
        if !sw.integrations {
            return env;
        }
        let base = format!("http://127.0.0.1:{}", self.port);
        let helper = |name: &str| self.helpers.join(name).to_string_lossy().into_owned();
        env.insert("SPLITGRID_TERMINAL".into(), terminal.to_owned());
        env.insert("SPLITGRID_HOOK_ENDPOINT".into(), format!("{base}/hook"));
        env.insert("SPLITGRID_HOOK_CLI".into(), helper("splitgrid-hook.sh"));
        // Browser control rides the master switch alone, as in Electron.
        env.insert("SPLITGRID_BROWSER_ENDPOINT".into(), format!("{base}/browser"));
        env.insert("SPLITGRID_BROWSER_CLI".into(), helper("splitgrid-browser.sh"));
        env.insert("SPLITGRID_BROWSER_TOKEN".into(), self.token.clone());
        if sw.terminal_control {
            env.insert("SPLITGRID_TERMINAL_ENDPOINT".into(), format!("{base}/terminal"));
            env.insert("SPLITGRID_TERMINAL_CLI".into(), helper("splitgrid-terminal.sh"));
            env.insert("SPLITGRID_TERMINAL_TOKEN".into(), self.token.clone());
        }
        sftp_cmd::add_env(self, &mut env);
        sql_cmd::add_env(self, &mut env);
        env
    }
}

/// The helper scripts: in the app bundle's Resources, else (a dev build) the
/// Electron app's sources next to this repo.
pub(crate) fn helpers_dir() -> PathBuf {
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/helpers")));
    match bundled {
        Some(dir) if dir.join("splitgrid-terminal.sh").is_file() => dir,
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../swapit/resources")),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Drop any SPLITGRID_* we inherited (launched from another SplitGrid's
/// terminal): our shells must only ever see our own.
pub fn scrub_inherited_env() {
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        // Our own development switches (SPLITGRID_DEBUG_*, SPLITGRID_HUD) stay.
        if k.starts_with("SPLITGRID_") && !k.starts_with("SPLITGRID_DEBUG") && k != "SPLITGRID_HUD" {
            // SAFETY: called first thing in main, before any other thread exists.
            unsafe { std::env::remove_var(&key) };
        }
    }
}

impl crate::app::AppView {
    /// The env for a shell in pane `id` (nothing when the bridge is off).
    pub(crate) fn agent_env(&self, id: &str) -> HashMap<String, String> {
        self.agent.as_ref().map(|a| a.env_for(id)).unwrap_or_default()
    }

    /// Recompute the agent terminals' status and raise what the user should hear about.
    pub(crate) fn refresh_attention(&mut self, window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<Self>) {
        use attention::{Alert, Term};
        let now = now_ms();
        self.attention.sweep(now);
        let focused = window.is_window_active();
        let viewing = focused.then(|| self.state.active_workspace_id.clone()).flatten();
        let terms: Vec<Term> = self
            .state
            .workspaces
            .iter()
            .flat_map(|ws| {
                ws.containers.iter().filter(|c| c.content.kind == crate::model::workspace::ContentType::Terminal).map(|c| Term {
                    workspace: &ws.id,
                    id: &c.id,
                    is_agent: self
                        .panes
                        .get(&c.id)
                        .and_then(|p| p.process.as_ref())
                        .is_some_and(|p| attention::AGENT_COMMANDS.contains(&p.name.as_str())),
                })
            })
            .collect();
        let before = (self.attention.kinds.clone(), self.attention.unviewed.clone());
        let alerts = self.attention.recompute(&terms, viewing.as_deref(), now);
        if before != (self.attention.kinds.clone(), self.attention.unviewed.clone()) {
            cx.notify();
        }

        let Some(cfg) = self.agent.clone() else { return };
        let (system, on_waiting, mute_all) = cfg.notify_prefs();
        for (alert, id, ws_id) in alerts {
            if alert == Alert::Waiting && !on_waiting {
                continue;
            }
            let Some(ws) = self.state.workspaces.iter().find(|w| w.id == ws_id) else { continue };
            // The sound plays focused or not (useAgentAttention); the banner only in the background.
            crate::app_settings::play_alert_sound(&self.store.app_settings(), &ws.extra);
            if mute_all || !system || focused || ws.extra.get("notifyMuted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let label = ws
                .container(&id)
                .map(|c| c.content.custom_name.clone().filter(|n| !n.trim().is_empty()).or(c.content.label.clone()).unwrap_or_else(|| "Terminal".into()))
                .unwrap_or_else(|| "Terminal".into());
            let (state, fallback) = match alert {
                Alert::Done => ("Done", "Finished its turn"),
                Alert::Waiting => ("waiting for you", "Needs your input"),
            };
            let body = self.attention.detail(&id, alert).unwrap_or(fallback).to_owned();
            notify::post(&format!("{} · {label} — {state}", ws.name), &body);
        }
    }

    /// A zone pane's tray button: collapse it into the tray or bring it back out.
    pub(crate) fn toggle_zone_tray(&mut self, id: &str, cx: &mut gpui_kit::Context<Self>) {
        let Some(ws) = self.state.workspaces.iter_mut().find(|w| w.container(id).is_some()) else { return };
        let tree = ws.layout_tree.as_ref();
        let Some(place) = zones::zone_places(tree).get(id).copied() else { return };
        let anchor = zones::zone_of_pane(tree, id).map(|t| t.terminal);
        if let Some(next) = zones::move_zone_pane(tree, id, place == zones::ZonePlace::Main, anchor.as_deref()) {
            ws.layout_tree = Some(next);
            self.changed(cx);
        }
    }

    /// One request from the receiver; answered before returning.
    pub(crate) fn handle_agent_request(&mut self, req: server::Request, window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<Self>) {
        use serde_json::json;
        let Some(cfg) = self.agent.clone() else {
            req.respond(json!({}));
            return;
        };
        let kind = req.kind.clone();
        let req_body = req.body.clone();
        match kind.as_str() {
            // Agent lifecycle hooks: no token (they only report state).
            "hook" => {
                let terminal = req.query("terminal").map(str::to_owned).or_else(|| req.body["terminal"].as_str().map(str::to_owned));
                let event = req.query("event").map(str::to_owned).or_else(|| req.body["event"].as_str().map(str::to_owned));
                req.respond(json!({}));
                if let (Some(terminal), Some(event)) = (terminal, event)
                    && self.state.workspaces.iter().any(|w| w.container(&terminal).is_some())
                    && self.attention.on_hook(&terminal, &event, &req_body, now_ms())
                {
                    self.refresh_attention(window, cx);
                }
            }
            "terminal" => {
                let reply = if !cfg.is_token(&req.body["token"]) {
                    json!({ "ok": false, "error": "unauthorized" })
                } else if !cfg.switches().terminal_control {
                    json!({ "ok": false, "error": "terminal_control_disabled",
                            "message": "Cross-terminal control is disabled. Enable it in Settings → Agent integrations." })
                } else {
                    let terminal = req.body["terminal"].as_str().unwrap_or_default().to_owned();
                    let argv: Vec<String> =
                        req.body["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                    self.run_terminal_command(&terminal, &argv, window, cx)
                };
                req.respond(reply);
            }
            "browser" => {
                if !cfg.is_token(&req.body["token"]) {
                    req.respond(json!({ "ok": false, "error": "unauthorized" }));
                    return;
                }
                if !cfg.switches().integrations {
                    req.respond(json!({ "ok": false, "error": "agent_integrations_disabled",
                                        "message": "Agent integrations are off. Enable them in Settings → Agent integrations." }));
                    return;
                }
                let terminal = req.body["terminal"].as_str().unwrap_or_default().to_owned();
                let argv: Vec<String> =
                    req.body["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                match self.run_browser_command(&terminal, &argv, window, cx) {
                    browser_cmd::Answer::Now(v) => req.respond(v),
                    browser_cmd::Answer::Later(task) => cx.spawn(async move |_, _| req.respond(task.await)).detach(),
                }
            }
            "sftp" => self.handle_sftp_request(req, &cfg, cx),
            "sql" => self.handle_sql_request(req, &cfg, window, cx),
            other => req.respond(json!({
                "ok": false,
                "error": "not_supported",
                "message": format!("SplitGrid Lite doesn't handle /{other} yet"),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_follows_the_switches() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::new(19568, dir.path());
        assert!(cfg.env_for("c-1").is_empty(), "no settings file: integrations off");

        std::fs::write(dir.path().join("app-settings.json"), r#"{"agentIntegrations":true}"#).unwrap();
        let env = cfg.env_for("c-1");
        assert_eq!(env["SPLITGRID_TERMINAL"], "c-1");
        assert_eq!(env["SPLITGRID_HOOK_ENDPOINT"], "http://127.0.0.1:19568/hook");
        assert!(!env.contains_key("SPLITGRID_TERMINAL_TOKEN"));
        assert_eq!(env["SPLITGRID_BROWSER_ENDPOINT"], "http://127.0.0.1:19568/browser");
        assert!(env["SPLITGRID_BROWSER_CLI"].ends_with("splitgrid-browser.sh"));
        assert_eq!(env["SPLITGRID_BROWSER_TOKEN"], cfg.token);

        std::fs::write(dir.path().join("app-settings.json"), r#"{"agentIntegrations":true,"agentTerminalControl":true}"#).unwrap();
        let env = cfg.env_for("c-1");
        assert_eq!(env["SPLITGRID_TERMINAL_TOKEN"], cfg.token);
        assert!(env["SPLITGRID_TERMINAL_CLI"].ends_with("splitgrid-terminal.sh"));
        assert!(cfg.is_token(&Value::String(cfg.token.clone())));
        assert!(!cfg.is_token(&Value::String("nope".into())));
    }
}
