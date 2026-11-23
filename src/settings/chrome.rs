//! Settings → Agent integrations → browser control (the Electron app's
//! AgentChromeSettings): the two gates (agentChromeControl, agentChromeWrite),
//! and the pairing block — a one-time code counting down to its expiry, the
//! connected browsers and their activated tabs, and the stored pairings with
//! Unpair.
//!
//! Unlike the other switches it owns live state: the extension connects and
//! activates tabs on its own schedule, so while Settings is open a 1s tick
//! redraws it (and moves the countdown). Lite saves at once, so there is no
//! "press Save" note. A code nobody can see any more is only a widened
//! guessing budget: closing Settings, turning the gate off or minting a new
//! code closes the old pairing window.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{SettingsTab, ghost_button, set, small_button, toggle};
use crate::agent::ext_channel::{Channel, now_ms};
use crate::agent::ext_protocol::PAIRING_TTL_MS;
use crate::agent::server::PREFERRED_PORT;
use crate::app::AppView;
use crate::theme::*;
use crate::ui::px;

#[derive(Default)]
pub(crate) struct ChromeUi {
    pairing: Option<LiveCode>,
    /// The last code ran out before the extension used it.
    expired: bool,
    /// Connected browsers when the code was minted: one more means it was used.
    clients_at_start: usize,
    /// The record the user pressed Unpair on (asks once more).
    confirm_unpair: Option<String>,
    poll: Option<Task<()>>,
}

/// A code on screen. Dropping it closes its pairing window.
struct LiveCode {
    code: String,
    expires_at: u64,
    channel: Arc<Channel>,
}

impl Drop for LiveCode {
    fn drop(&mut self) {
        // Only our own code: another window may have minted a newer one.
        self.channel.cancel_pairing_code(&self.code);
    }
}

/// m:ss for the countdown.
fn mmss(ms: u64) -> String {
    let total = ms.div_ceil(1000);
    format!("{}:{:02}", total / 60, total % 60)
}

fn badge(interactive: bool) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(999.))
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(if interactive { ACCENT } else { TEXT_MUTED })
        .bg(if interactive { hexa(0x228df2, 0.14) } else { bg_hover() })
        .child(if interactive { "INTERACTIVE" } else { "READ-ONLY" })
}

impl AppView {
    fn ext_channel(&self) -> Option<Arc<Channel>> {
        self.agent.as_ref().and_then(|a| a.ext().cloned())
    }

    /// The 1s tick while Settings is open (dropped with ManagerState on close).
    pub(super) fn ensure_chrome_poll(&mut self, cx: &mut Context<Self>) {
        if self.manager.chrome.poll.is_some() {
            return;
        }
        self.manager.chrome.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |this, cx| this.chrome_tick(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn chrome_tick(&mut self, cx: &mut Context<Self>) {
        if self.settings_tab != Some(SettingsTab::General) {
            // The code is off screen now: close its window.
            self.manager.chrome.pairing = None;
            return;
        }
        let all = self.store.app_settings();
        let on = |k: &str| all.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
        if !(on("agentIntegrations") && on("agentChromeControl")) {
            self.manager.chrome.pairing = None;
            return;
        }
        let ui = &mut self.manager.chrome;
        if let Some(live) = &ui.pairing {
            // Single use: a newly connected browser means the code did its job.
            if live.channel.clients().len() > ui.clients_at_start {
                ui.pairing = None;
            } else if now_ms() >= live.expires_at {
                ui.pairing = None;
                ui.expired = true;
            }
        }
        cx.notify();
    }

    fn begin_chrome_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.ext_channel() else {
            return;
        };
        let ui = &mut self.manager.chrome;
        // Close the old window first: dropping it after minting would close the new one.
        ui.pairing = None;
        ui.expired = false;
        ui.clients_at_start = channel.clients().len();
        let (code, expires_at) = channel.begin_pairing(now_ms());
        ui.pairing = Some(LiveCode { code, expires_at, channel });
        cx.notify();
    }

    fn set_chrome_control(&mut self, v: bool, cx: &mut Context<Self>) {
        if v {
            set(self, &[("agentChromeControl", true)], cx);
        } else {
            // Turning the gate off also closes the pairing window it opened.
            self.manager.chrome.pairing = None;
            set(self, &[("agentChromeControl", false), ("agentChromeWrite", false)], cx);
        }
    }

    /// The browser-control switches and the pairing block, nested under Agent integrations.
    pub(super) fn render_chrome(&self, integrations: bool, cx: &mut Context<Self>) -> Div {
        let all = self.store.app_settings();
        let flag = |k: &str| all.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let on = integrations && flag("agentChromeControl");
        let write = flag("agentChromeWrite");
        let nested = |d: Div| div().ml(px(22.)).mt(px(14.)).child(d);

        div()
            .child(toggle(
                "chrome",
                "Let agents inspect a tab in your real browser (read-only)",
                Some("Lets an agent read a tab you hand over from the SplitGrid Lite browser extension — its text, HTML and screenshots. Unlike the browser pane, which has its own isolated session, this is your real profile with your real logins. Off by default."),
                on,
                !integrations,
                |t, v, cx| t.set_chrome_control(v, cx),
                cx,
            ))
            .child(nested(toggle(
                "chrome-write",
                "Allow agents to interact with pages (write)",
                Some("Also permit eval, navigation and reloads in that tab. Arbitrary JS runs inside your logged-in session, so it can act as you. Off by default."),
                on && write,
                !on,
                |t, v, cx| set(t, &[("agentChromeWrite", v)], cx),
                cx,
            )))
            .when(on, |d| d.child(self.render_chrome_panel(write, cx)))
    }

    fn render_chrome_panel(&self, write: bool, cx: &mut Context<Self>) -> Div {
        let muted = |text: SharedString| div().text_size(px(11.)).line_height(px(16.5)).text_color(TEXT_MUTED).child(text);
        let channel = self.ext_channel();
        let ui = &self.manager.chrome;
        let clients = channel.as_ref().map(|c| c.clients()).unwrap_or_default();
        let surfaces = channel.as_ref().map(|c| c.surfaces()).unwrap_or_default();
        let paired = channel.as_ref().map(|c| c.paired()).unwrap_or_default();
        let now = now_ms();

        let pair_row = match ui.pairing.as_ref().filter(|p| p.expires_at > now) {
            Some(live) => div().child(muted("Type this code into the extension's popup:".into())).child(
                div()
                    .mt(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(div().font_family("Menlo").text_size(px(22.)).font_weight(FontWeight::BOLD).text_color(ACCENT).child(live.code.clone()))
                    .child(muted(format!("expires in {}", mmss(live.expires_at - now)).into()))
                    .child(div().flex_1())
                    .child(ghost_button("chrome-pair-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                        this.manager.chrome.pairing = None;
                        cx.notify();
                    }))),
            ),
            None => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .flex_wrap()
                .when(channel.is_some(), |d| {
                    d.child(
                        ghost_button("chrome-pair", if clients.is_empty() { "Pair browser extension" } else { "Pair another browser" })
                            .on_click(cx.listener(|this, _, _, cx| this.begin_chrome_pairing(cx))),
                    )
                })
                .child(muted(if ui.expired {
                    "That code expired before the extension used it.".into()
                } else {
                    format!("Shows a one-time code, valid for {} minutes.", PAIRING_TTL_MS / 60_000).into()
                })),
        };

        // "No extension paired" and "paired but nothing activated" have different fixes.
        let status = if channel.is_none() {
            div().text_color(ACCENT_RED).child("The agent bridge isn't running, so no browser can connect.")
        } else if clients.is_empty() {
            div().child("No browser connected. Install the SplitGrid Lite extension, then pair it here.")
        } else {
            let names = clients.iter().map(|c| format!("{} ({})", c.identity.browser, c.client_id)).collect::<Vec<_>>().join(", ");
            div().child(div().text_color(ACCENT_GREEN).font_weight(FontWeight::SEMIBOLD).child(format!("Connected: {names}"))).map(|d| {
                if surfaces.is_empty() {
                    d.child(
                        div()
                            .mt(px(6.))
                            .child("Paired, but no tab is activated. Click the extension's toolbar icon and activate the tab you want an agent to see."),
                    )
                } else {
                    d.child(div().mt(px(8.)).flex().flex_col().gap(px(6.)).children(surfaces.iter().map(|s| {
                        // Interaction needs both: the extension's per-tab switch and agentChromeWrite.
                        let interactive = s.writable && write;
                        div()
                            .px(px(9.))
                            .py(px(7.))
                            .rounded(px(8.))
                            .min_w_0()
                            .border_1()
                            .border_color(border())
                            .bg(bg_surface())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(TEXT_PRIMARY)
                                            .child(if s.title.is_empty() { "Untitled tab".to_owned() } else { s.title.clone() }),
                                    )
                                    .child(badge(interactive)),
                            )
                            .child(div().mt(px(2.)).truncate().child(s.url.clone()))
                            .child(div().mt(px(2.)).truncate().font_family("Menlo").opacity(0.75).child(s.surface.clone()))
                    })))
                }
            })
        };

        let pairings = (!paired.is_empty()).then(|| {
            div().mt(px(12.)).flex().flex_col().gap(px(6.)).child(muted("Paired browsers".into())).children(paired.iter().map(|p| {
                let confirming = ui.confirm_unpair.as_deref() == Some(p.client_id.as_str());
                let id = p.client_id.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(div().flex_1().min_w_0().truncate().text_color(TEXT_PRIMARY).child(format!("{} ({})", p.browser, p.client_id)))
                    .child(muted(if p.connected { "connected".into() } else { "not connected".into() }))
                    .child(
                        small_button(SharedString::from(format!("chrome-unpair-{}", p.client_id)), if confirming { "Confirm unpair" } else { "Unpair" })
                            .when(confirming, |d| d.text_color(ACCENT_RED))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.manager.chrome.confirm_unpair.as_deref() == Some(id.as_str()) {
                                    this.manager.chrome.confirm_unpair = None;
                                    if let Some(ch) = this.ext_channel() {
                                        ch.unpair(&id);
                                    }
                                } else {
                                    this.manager.chrome.confirm_unpair = Some(id.clone());
                                }
                                cx.notify();
                            })),
                    )
            }))
        });

        // The extension dials :19568 only; the receiver falls back to any port when it's taken.
        let port = self.agent.as_ref().map(|a| a.port).unwrap_or(PREFERRED_PORT);

        div()
            .ml(px(22.))
            .mt(px(14.))
            .p(px(12.))
            .rounded(px(10.))
            .border_1()
            .border_color(border())
            .bg(bg_primary())
            .child(pair_row)
            .child(div().mt(px(12.)).text_size(px(11.)).line_height(px(16.5)).text_color(TEXT_MUTED).child(status))
            .children(pairings)
            .when(port != PREFERRED_PORT, |d| {
                d.child(div().mt(px(10.)).text_size(px(11.)).line_height(px(16.5)).text_color(ACCENT_RED).child(format!(
                    "SplitGrid Lite is listening on port {port}, not {PREFERRED_PORT} (something else had it), so the extension can't reach it. Free {PREFERRED_PORT} and restart Lite."
                )))
            })
            .child(div().mt(px(10.)).child(muted(
                "Don't have the extension yet? extension/README.md in the SplitGrid Lite sources has the build and Load unpacked steps. It is a separate extension from the SplitGrid app's, so both can be installed.".into(),
            )))
    }
}

#[cfg(test)]
mod tests {
    use super::mmss;

    #[test]
    fn countdown() {
        assert_eq!(mmss(180_000), "3:00");
        assert_eq!(mmss(61_001), "1:02");
        assert_eq!(mmss(999), "0:01");
        assert_eq!(mmss(0), "0:00");
    }
}
