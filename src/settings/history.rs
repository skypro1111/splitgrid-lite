//! Settings → Workspace history (the Electron app's HistorySettings +
//! HistoryDistillSettings): opt-in local search over past Claude Code sessions,
//! and under it the decision notes that run Claude Haiku.
//!
//! Unlike the switches around it, this acts through the history service:
//! turning it on is consent to a download, so it goes through a confirm step
//! that first shows what it costs, and `historyEnabled` is saved only once the
//! install succeeded. Turning it off asks whether to also delete what it put on
//! disk — deleting is the suggested choice. Decision notes likewise: the cost
//! estimate is on screen before the switch can do anything, and the switch only
//! opens a confirm step; deleting notes is offered only while they are off.
//!
//! [`HistoryUi`] is the section's state machine, pure so it is unit-tested; a
//! transition that needs the service returns an [`Effect`], which
//! [`AppView::history_run`] performs off the UI thread.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;
use sg_history::{
    DistillState, HistoryDistillEstimate, HistoryDistillStatus, HistoryEstimate, HistoryProgress, HistoryStatus, HistoryWorkspaceStats, ProgressState,
};

use super::{ghost_button, row, toggle};
use crate::app::AppView;
use crate::theme::*;
use crate::ui::px;

// ── Formatting (pure) ────────────────────────────────────────────────────────

/// JS `n.toFixed(d)`: ties away from zero, unlike Rust's `{:.d}`.
fn fixed(n: f64, d: i32) -> String {
    let p = 10f64.powi(d);
    format!("{:.*}", d as usize, (n * p).round() / p)
}

/// 1.2 GB / 310 MB / 48 KB. Binary units, one decimal below 10.
pub fn format_bytes(n: f64) -> String {
    if !n.is_finite() || n <= 0. {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (n, 0);
    while v >= 1024. && i < units.len() - 1 {
        v /= 1024.;
        i += 1;
    }
    if i == 0 {
        return format!("{} B", v.round());
    }
    if v < 10. { format!("{} {}", fixed(v, 1), units[i]) } else { format!("{} {}", v.round(), units[i]) }
}

/// "under a minute" / "about 25 minutes" / "about 2 h 10 min".
pub fn format_minutes(min: f64) -> String {
    if !min.is_finite() || min < 1. {
        return "under a minute".into();
    }
    let total = min.round() as u64;
    if total < 60 {
        return format!("about {total} minute{}", if total == 1 { "" } else { "s" });
    }
    let (h, m) = (total / 60, total % 60);
    if m > 0 { format!("about {h} h {m} min") } else { format!("about {h} h") }
}

/// 1,234 (en-US grouping).
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// $0 / $0.0042 / $0.37 / $12. Sub-cent amounts keep four decimals so a real spend never reads as $0.00.
pub fn format_usd(n: f64) -> String {
    if !n.is_finite() || n <= 0. {
        return "$0".into();
    }
    if n < 0.01 {
        return format!("${}", fixed(n, 4));
    }
    if n < 100. {
        return format!("${}", fixed(n, 2));
    }
    format!("${}", count(n.round() as u64))
}

/// 950 / 12k / 3.4M tokens.
pub fn format_tokens(n: f64) -> String {
    if !n.is_finite() || n <= 0. {
        return "0".into();
    }
    if n < 1_000. {
        return format!("{}", n.round());
    }
    if n < 1_000_000. {
        return format!("{}k", (n / 1_000.).round());
    }
    format!("{}M", fixed(n / 1_000_000., if n < 10_000_000. { 1 } else { 0 }))
}

/// Download bytes while installing, embedded/total chunks while indexing. A
/// paused state carries whichever counters the paused phase was reporting.
pub fn progress_line(p: &HistoryProgress) -> (String, Option<f64>) {
    let paused = p.state == ProgressState::Paused;
    if p.state == ProgressState::Indexing || (paused && p.total_chunks.is_some()) {
        let done = p.embedded_chunks.unwrap_or(0);
        let total = p.total_chunks.unwrap_or(0);
        let of = if total > 0 { format!(" of {}", count(total)) } else { String::new() };
        let text = format!("{} {}{of} chunks embedded", if paused { "Indexing paused —" } else { "Indexing sessions…" }, count(done));
        return (text, (total > 0).then(|| done as f64 / total as f64));
    }
    let total = p.total_bytes.unwrap_or(0);
    // A resumed download that had to start over counts some bytes twice.
    let done = p.downloaded_bytes.unwrap_or(0).min(if total > 0 { total } else { u64::MAX });
    let of = if total > 0 { format!(" of {}", format_bytes(total as f64)) } else { String::new() };
    let text = format!("{} {}{of}", if paused { "Download paused —" } else { "Downloading the runtime and model…" }, format_bytes(done as f64));
    (text, (total > 0).then(|| done as f64 / total as f64))
}

pub fn estimate_headline(e: &HistoryEstimate) -> String {
    if e.transcript_files > 0 {
        format!(
            "Found {} transcript file{} ({}) from Claude Code sessions in your workspace folders.",
            count(e.transcript_files),
            plural(e.transcript_files, "", "s"),
            format_bytes(e.transcript_bytes as f64)
        )
    } else {
        "No Claude Code transcripts found in your workspace folders yet — new sessions are indexed as they happen.".into()
    }
}

pub fn workspace_label(names: &[(String, String)], id: &str) -> String {
    match names.iter().find(|(i, n)| i == id && !n.is_empty()) {
        Some((_, n)) => n.clone(),
        None => format!("Other workspace ({})", id.chars().take(8).collect::<String>()),
    }
}

pub fn workspace_line(w: &HistoryWorkspaceStats) -> String {
    let embedded = if w.embedded_chunks < w.chunks { format!(" ({} embedded)", count(w.embedded_chunks)) } else { String::new() };
    format!("{} session{} · {} chunks{embedded} · {}", count(w.sessions), plural(w.sessions, "", "s"), count(w.chunks), format_bytes(w.db_bytes as f64))
}

pub fn confirm_off_text(index_bytes: u64) -> String {
    let indexes = if index_bytes > 0 { format!(" (indexes: {})", format_bytes(index_bytes as f64)) } else { String::new() };
    format!(
        "Indexing stops and agents can no longer search past sessions. Deleting also removes the llama.cpp runtime, the embedding model and every workspace index{indexes}. Keeping them lets you turn it back on without downloading again; nothing runs while it is off."
    )
}

pub fn distill_state_line(s: &HistoryDistillStatus) -> &'static str {
    match s.state {
        DistillState::Running => "Distilling a session with Claude Haiku…",
        DistillState::Paused => "Paused with workspace history.",
        DistillState::Waiting => "Waiting to retry.",
        DistillState::Error => "Stopped with an error.",
        DistillState::Idle if s.groups_pending > 0 => "Waiting for the next pass.",
        DistillState::Idle => "Up to date.",
        DistillState::Off => "",
    }
}

/// Text with one bold span (the amount), for StyledText.
pub type Rich = (String, Option<Range<usize>>);

fn with_bold(before: String, bold: String, after: String) -> Rich {
    let start = before.len();
    let end = start + bold.len();
    (before + &bold + &after, Some(start..end))
}

pub fn backlog_text(e: &HistoryDistillEstimate) -> Rich {
    if e.pending_groups == 0 {
        return ("Nothing to distill yet".into(), None);
    }
    with_bold(
        format!(
            "{} group{} of settled sessions, ~{} input + {} output tokens ≈ ",
            count(e.pending_groups),
            plural(e.pending_groups, "", "s"),
            format_tokens(e.pending_input_tokens as f64),
            format_tokens(e.pending_output_tokens as f64)
        ),
        format_usd(e.backlog_usd),
        " once".into(),
    )
}

pub fn ongoing_text(e: &HistoryDistillEstimate) -> Rich {
    with_bold(
        "≈ ".into(),
        format_usd(e.monthly_usd),
        format!(
            " / month at your last 30 days' pace ({} group{}, ~{} input tokens)",
            count(e.monthly_groups),
            plural(e.monthly_groups, "", "s"),
            format_tokens(e.monthly_input_tokens as f64)
        ),
    )
}

pub fn price_note(e: &HistoryDistillEstimate) -> String {
    let mut s = format!(
        "{} list price: ${} per million input tokens, ${} per million output tokens. Claude Code writes each prompt to its 1-hour prompt cache, billed at twice the input price, and the estimate includes that. Tokens are estimated at about 3 characters each, so the real figure can differ.",
        e.model,
        fixed(e.price_input_per_m_tok, 2),
        fixed(e.price_output_per_m_tok, 2)
    );
    if e.unsettled_groups > 0 {
        s += &format!(
            " {} more group{} from sessions active in the last 30 minutes and will be counted once they settle.",
            count(e.unsettled_groups),
            plural(e.unsettled_groups, " is", "s are")
        );
    }
    s
}

pub fn distill_confirm_headline(e: Option<&HistoryDistillEstimate>) -> String {
    match e {
        Some(e) => format!(
            "Run Claude Haiku on {} group{} (≈ {} at API rates)?",
            count(e.pending_groups),
            plural(e.pending_groups, "", "s"),
            format_usd(e.backlog_usd)
        ),
        None => "Run Claude Haiku on your settled sessions?".into(),
    }
}

pub fn distill_spent_note(spent: f64, s: &HistoryDistillStatus) -> String {
    let mut out = format!("Spent so far: {}, as reported by Claude Code (on a subscription this is plan usage, not a charge).", format_usd(spent));
    if s.groups_unsettled > 0 {
        out += &format!(" {} group{} wait for their session to settle.", count(s.groups_unsettled), plural(s.groups_unsettled, "", "s"));
    }
    if s.groups_failed > 0 {
        out += &format!(" {} gave unusable output and were skipped.", count(s.groups_failed));
    }
    out
}

// ── State machine (pure) ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Estimating,
    Confirm,
    Enabling,
    ConfirmOff,
    Disabling,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DistillPhase {
    #[default]
    Idle,
    Confirm,
    Enabling,
    ConfirmDelete,
    Deleting,
}

/// A service call a transition asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Estimate,
    Enable,
    Disable { delete_data: bool },
    Pause,
    Resume,
    DistillEstimate,
    DistillEnable,
    DistillDisable,
    DeleteNotes,
}

/// What the section is drawn from: the persisted consents and the service's last status.
#[derive(Clone, Debug, Default)]
pub struct Props {
    /// AppSettings.historyEnabled.
    pub enabled: bool,
    /// AppSettings.historyDistill.
    pub distill: bool,
    pub status: Option<HistoryStatus>,
}

impl Props {
    pub fn progress_state(&self) -> Option<ProgressState> {
        self.status.as_ref().map(|s| s.progress.state)
    }
    pub fn paused(&self) -> bool {
        self.progress_state() == Some(ProgressState::Paused)
    }
    /// An install/index still running counts as on even though consent is persisted only once it completes.
    pub fn active(&self) -> bool {
        matches!(self.progress_state(), Some(ProgressState::Installing | ProgressState::Indexing | ProgressState::Paused))
    }
}

static SESSIONS: AtomicU64 = AtomicU64::new(1);

/// The section's own state; made fresh each time Settings opens (React's mount).
#[derive(Debug)]
pub struct HistoryUi {
    /// Which opening of Settings this is: a reply to an earlier one is dropped.
    pub session: u64,
    pub phase: Phase,
    pub estimate: Option<HistoryEstimate>,
    pub error: String,
    pub d_phase: DistillPhase,
    pub d_estimate: Option<HistoryDistillEstimate>,
    pub d_estimating: bool,
    /// The cost estimate was asked for while notes are off (asked again once they were on).
    pub d_asked: bool,
    pub d_error: String,
}

impl Default for HistoryUi {
    fn default() -> Self {
        Self {
            session: SESSIONS.fetch_add(1, Ordering::Relaxed),
            phase: Phase::Idle,
            estimate: None,
            error: String::new(),
            d_phase: DistillPhase::Idle,
            d_estimate: None,
            d_estimating: false,
            d_asked: false,
            d_error: String::new(),
        }
    }
}

impl HistoryUi {
    pub fn on(&self, p: &Props) -> bool {
        if self.phase == Phase::Disabling {
            return false;
        }
        p.enabled || p.status.as_ref().is_some_and(|s| s.enabled) || self.phase == Phase::Enabling || p.active()
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Enabling | Phase::Disabling)
    }

    /// The failure to show: ours, else the indexer's.
    pub fn shown_error(&self, p: &Props) -> String {
        if !self.error.is_empty() {
            return self.error.clone();
        }
        match p.status.as_ref().map(|s| &s.progress) {
            Some(pr) if pr.state == ProgressState::Error => {
                pr.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "Workspace history stopped with an error.".into())
            }
            _ => String::new(),
        }
    }

    /// Stats, and decision notes, show while consent stands and no turn-off is under way.
    pub fn shows_stats(&self, p: &Props) -> bool {
        p.enabled && !matches!(self.phase, Phase::ConfirmOff | Phase::Disabling)
    }

    pub fn toggle(&mut self, p: &Props) -> Option<Effect> {
        if self.on(p) {
            self.phase = if self.phase == Phase::ConfirmOff { Phase::Idle } else { Phase::ConfirmOff };
            return None;
        }
        if matches!(self.phase, Phase::Estimating | Phase::Confirm) {
            self.phase = Phase::Idle;
            return None;
        }
        Some(self.start_enable())
    }

    /// Check the transcripts first (also "Try again").
    pub fn start_enable(&mut self) -> Effect {
        self.error.clear();
        self.phase = Phase::Estimating;
        Effect::Estimate
    }

    pub fn estimated(&mut self, r: Result<HistoryEstimate, String>) {
        match r {
            Ok(e) => {
                self.estimate = Some(e);
                self.phase = Phase::Confirm;
            }
            Err(e) => {
                self.error = format!("Could not check your transcripts: {e}");
                self.phase = Phase::Idle;
            }
        }
    }

    pub fn confirm_enable(&mut self) -> Effect {
        self.error.clear();
        self.phase = Phase::Enabling;
        Effect::Enable
    }

    pub fn enabled(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.error = e;
        }
        self.phase = Phase::Idle;
    }

    pub fn confirm_disable(&mut self, delete_data: bool) -> Effect {
        self.error.clear();
        self.phase = Phase::Disabling;
        Effect::Disable { delete_data }
    }

    pub fn disabled(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.error = e;
        }
        self.phase = Phase::Idle;
    }

    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
    }

    pub fn pause_or_resume(&self, p: &Props) -> Effect {
        if p.paused() { Effect::Resume } else { Effect::Pause }
    }

    // Decision notes.

    pub fn d_on(&self, p: &Props) -> bool {
        p.distill || self.d_phase == DistillPhase::Enabling
    }

    /// The cost is shown before the switch can do anything: ask for it as soon
    /// as the section is visible with notes off.
    pub fn take_distill_estimate(&mut self, p: &Props) -> Option<Effect> {
        if p.distill {
            self.d_asked = false;
            return None;
        }
        if !self.shows_stats(p) || self.d_asked {
            return None;
        }
        self.d_asked = true;
        self.d_estimating = true;
        Some(Effect::DistillEstimate)
    }

    pub fn wants_distill_estimate(&self, p: &Props) -> bool {
        (p.distill && self.d_asked) || (!p.distill && self.shows_stats(p) && !self.d_asked)
    }

    pub fn distill_estimated(&mut self, r: Result<HistoryDistillEstimate, String>) {
        self.d_estimating = false;
        match r {
            Ok(e) => self.d_estimate = Some(e),
            Err(e) => self.d_error = format!("Could not estimate the cost: {e}"),
        }
    }

    /// The switch: off immediately (notes kept); on only opens the confirm step.
    pub fn d_toggle(&mut self, p: &Props) -> Option<Effect> {
        if self.d_on(p) {
            self.d_error.clear();
            return Some(Effect::DistillDisable);
        }
        self.d_phase = if self.d_phase == DistillPhase::Confirm { DistillPhase::Idle } else { DistillPhase::Confirm };
        None
    }

    pub fn d_confirm_enable(&mut self) -> Effect {
        self.d_error.clear();
        self.d_phase = DistillPhase::Enabling;
        Effect::DistillEnable
    }

    pub fn distill_enabled(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.d_error = e;
        }
        self.d_phase = DistillPhase::Idle;
    }

    pub fn distill_disabled(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.d_error = e;
        }
    }

    pub fn d_ask_delete(&mut self) {
        self.d_phase = DistillPhase::ConfirmDelete;
    }

    pub fn d_confirm_delete(&mut self) -> Effect {
        self.d_error.clear();
        self.d_phase = DistillPhase::Deleting;
        Effect::DeleteNotes
    }

    pub fn notes_deleted(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.d_error = e;
        }
        self.d_phase = DistillPhase::Idle;
    }

    pub fn d_cancel(&mut self) {
        self.d_phase = DistillPhase::Idle;
    }

    /// The decision-notes failure to show: ours, else the distiller's back-off or error.
    pub fn d_shown_error(&self, status: Option<&HistoryDistillStatus>) -> String {
        if !self.d_error.is_empty() {
            return self.d_error.clone();
        }
        match status {
            Some(s) if matches!(s.state, DistillState::Error | DistillState::Waiting) => s.error.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

// ── Rendering + effects ──────────────────────────────────────────────────────

const STOPPED: &str = "Workspace history stopped unexpectedly.";

/// The panel inside the card (HistorySettings' panelStyle).
fn panel(mt: f32) -> Div {
    div().mt(px(mt)).p(px(12.)).rounded(px(10.)).border_1().border_color(BORDER).bg(BG_PRIMARY)
}

fn note(text: impl Into<SharedString>) -> Div {
    div().text_size(px(11.)).line_height(px(17.)).text_color(TEXT_MUTED).child(text.into())
}

fn value(text: impl Into<SharedString>) -> Div {
    div().text_size(px(12.)).line_height(px(18.)).text_color(TEXT_PRIMARY).child(text.into())
}

fn rich((text, bold): Rich) -> Div {
    let styled = StyledText::new(text).with_highlights(bold.map(|r| (r, HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() })));
    div().flex_1().min_w_0().text_size(px(12.)).line_height(px(18.)).text_color(TEXT_PRIMARY).child(styled)
}

fn error_line(text: String) -> Div {
    div().mt(px(10.)).text_size(px(11.)).line_height(px(16.5)).text_color(ACCENT_RED).child(text)
}

/// settings-ui's primaryBtnStyle(true).
fn primary_button(id: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(20.))
        .py(px(9.))
        .rounded(px(9.))
        .bg(ACCENT)
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .text_color(BG_PRIMARY)
        .cursor_pointer()
        .hover(|d| d.opacity(0.9))
        .child(label.into())
}

fn bar(fraction: f64, paused: bool) -> Div {
    let f = fraction.clamp(0., 1.) as f32;
    div().mt(px(8.)).h(px(4.)).rounded(px(2.)).bg(BORDER).overflow_hidden().child(div().h_full().w(relative(f)).bg(if paused { TEXT_MUTED } else { ACCENT }))
}

fn buttons() -> Div {
    div().flex().flex_wrap().gap(px(8.))
}

fn err_text(e: anyhow::Error) -> String {
    let s = format!("{e:#}");
    if s.is_empty() { "Unknown error".into() } else { s }
}

impl AppView {
    pub(crate) fn history_props(&self, cx: &App) -> Props {
        let (enabled, distill) = crate::history::flags(&self.store.app_settings());
        Props { enabled, distill, status: crate::history::status(cx).cloned() }
    }

    /// Save one consent flag once the service call it stands for succeeded.
    /// Turning history on or off re-applies the sg-history skill, but only
    /// while Agent integrations are on (with them off nothing agent-facing exists).
    fn persist_history_flag(&mut self, key: &str, on: bool) {
        let all = self.store.app_settings();
        if all.get(key).and_then(Value::as_bool).unwrap_or(false) == on {
            return;
        }
        if let Err(e) = self.store.patch_app_settings(&[(key, on.into())]) {
            log::error!("saving {key}: {e:#}");
        }
        if key == "historyEnabled" && all.get("agentIntegrations").and_then(Value::as_bool) == Some(true) {
            let installer = crate::agent::install::Installer::new(self.store.dir());
            std::thread::spawn(move || installer.apply());
        }
    }

    /// Run `work` on a thread of its own, then `done` back here. `done` learns
    /// whether the Settings opening that asked is still the one on screen.
    fn history_call<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut AppView, Result<T, String>, bool) + 'static,
        cx: &mut Context<Self>,
    ) {
        let session = self.manager.history.session;
        cx.spawn(async move |this, cx| {
            let r = crate::history::blocking(work).await.unwrap_or_else(|| Err(STOPPED.into()));
            let _ = this.update(cx, |this, cx| {
                let alive = this.settings_tab.is_some() && this.manager.history.session == session;
                done(this, r, alive);
                cx.notify();
            });
        })
        .detach();
    }

    /// Perform what a transition asked for.
    pub(crate) fn history_run(&mut self, effect: Option<Effect>, cx: &mut Context<Self>) {
        cx.notify();
        let Some(effect) = effect else { return };
        let Some(svc) = crate::history::service(cx) else {
            let ui = &mut self.manager.history;
            ui.error = "Workspace history is not available in this run.".into();
            ui.phase = Phase::Idle;
            ui.d_phase = DistillPhase::Idle;
            ui.d_estimating = false;
            return;
        };
        match effect {
            Effect::Estimate => self.history_call(
                move || svc.estimate().map_err(err_text),
                |this, r, alive| {
                    if alive {
                        this.manager.history.estimated(r);
                    }
                },
                cx,
            ),
            Effect::Enable => {
                let epoch = crate::history::epoch();
                self.history_call(
                    move || svc.enable().map_err(err_text),
                    move |this, r, alive| {
                        // Turned off while it was installing: consent must not land afterwards.
                        let r = r.and_then(|()| {
                            if crate::history::epoch() == epoch {
                                Ok(())
                            } else {
                                Err("Workspace history was turned off while it was being set up.".to_owned())
                            }
                        });
                        if r.is_ok() {
                            this.persist_history_flag("historyEnabled", true);
                        }
                        if alive {
                            this.manager.history.enabled(r);
                        }
                    },
                    cx,
                )
            }
            Effect::Disable { delete_data } => {
                // Consent is withdrawn first, so even a failing cleanup leaves the
                // feature off; withdrawing history withdraws decision notes too.
                crate::history::bump_epoch();
                self.persist_history_flag("historyEnabled", false);
                self.persist_history_flag("historyDistill", false);
                self.history_call(
                    move || svc.disable(delete_data).map_err(err_text),
                    |this, r, alive| {
                        if alive {
                            this.manager.history.disabled(r);
                        }
                    },
                    cx,
                )
            }
            Effect::Pause => self.history_call(
                move || {
                    svc.pause();
                    Ok(())
                },
                |_, _, _| {},
                cx,
            ),
            Effect::Resume => self.history_call(
                move || {
                    svc.resume();
                    Ok(())
                },
                |_, _, _| {},
                cx,
            ),
            Effect::DistillEstimate => self.history_call(
                move || svc.distill_estimate().map_err(err_text),
                |this, r, alive| {
                    if alive {
                        this.manager.history.distill_estimated(r);
                    }
                },
                cx,
            ),
            Effect::DistillEnable => {
                let consent = crate::history::flags(&self.store.app_settings()).0;
                self.history_call(
                    move || {
                        if !consent || !svc.status().enabled {
                            return Err("Turn on workspace history first.".to_owned());
                        }
                        svc.set_distill_enabled(true);
                        Ok(())
                    },
                    |this, r, alive| {
                        if r.is_ok() {
                            this.persist_history_flag("historyDistill", true);
                        }
                        if alive {
                            this.manager.history.distill_enabled(r);
                        }
                    },
                    cx,
                )
            }
            Effect::DistillDisable => {
                self.persist_history_flag("historyDistill", false);
                self.history_call(
                    move || {
                        svc.set_distill_enabled(false);
                        Ok(())
                    },
                    |this, r, alive| {
                        if alive {
                            this.manager.history.distill_disabled(r);
                        }
                    },
                    cx,
                )
            }
            Effect::DeleteNotes => self.history_call(
                move || svc.delete_notes().map_err(err_text),
                |this, r, alive| {
                    if alive {
                        this.manager.history.notes_deleted(r);
                    }
                },
                cx,
            ),
        }
    }

    /// A transition on the section's state, made with fresh props, then its effect.
    fn history_do(&mut self, f: impl FnOnce(&mut HistoryUi, &Props) -> Option<Effect>, cx: &mut Context<Self>) {
        let props = self.history_props(cx);
        let effect = f(&mut self.manager.history, &props);
        self.history_run(effect, cx);
    }

    /// The Workspace history card (SettingsModal's `<Card title="Workspace history">`).
    pub(super) fn render_history(&self, integrations: bool, cx: &mut Context<Self>) -> Div {
        let p = self.history_props(cx);
        let ui = &self.manager.history;
        // Visible with notes off: fetch their cost (once per opening).
        if ui.wants_distill_estimate(&p) {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, |this, cx| this.history_do(|ui, p| ui.take_distill_estimate(p), cx));
            });
        }
        let on = ui.on(&p);
        let progress = p.status.as_ref().map(|s| s.progress.clone()).unwrap_or_default();
        let paused = p.paused();
        let line = p.active().then(|| progress_line(&progress));
        let workspaces = p.status.as_ref().map(|s| s.workspaces.clone()).unwrap_or_default();
        let index_bytes: u64 = workspaces.iter().map(|w| w.db_bytes).sum();
        let error = ui.shown_error(&p);
        let names: Vec<(String, String)> = self.state.workspaces.iter().map(|w| (w.id.clone(), w.name.clone())).collect();

        let mut card = super::card("Workspace history", None).child(div().mt(px(14.)).child(toggle(
            "history",
            "Enable workspace history",
            Some("Indexes the Claude Code sessions that ran in each workspace's folder so agents can search past decisions through the sg-history skill (requires Agent integrations). It is local and private: a local embedding model does the work and nothing is uploaded. Off by default — turning it on downloads the llama.cpp runtime and a ~310 MB model, after you confirm. Applies immediately."),
            on,
            ui.busy(),
            |this, _, cx| this.history_do(|ui, p| ui.toggle(p), cx),
            cx,
        )));

        if !integrations {
            card = card.child(note("Agent integrations is off, so agents cannot use the sg-history skill yet — indexing still works. Turn on Agent integrations above to let agents search it.").mt(px(10.)));
        }
        if ui.phase == Phase::Estimating {
            card = card.child(note("Checking your Claude Code transcripts…").mt(px(10.)));
        }
        if ui.phase == Phase::Confirm
            && let Some(e) = &ui.estimate
        {
            card = card.child(
                panel(14.)
                    .child(value(estimate_headline(e)).font_weight(FontWeight::SEMIBOLD).mb(px(10.)))
                    .child(row("Download").child(value(format!("{} once — the llama.cpp runtime plus the EmbeddingGemma model (~310 MB)", format_bytes(e.download_bytes as f64))).flex_1().min_w_0()))
                    .child(row("Disk use").child(value(format!("about {} — runtime, model and the workspace indexes", format_bytes(e.disk_bytes as f64))).flex_1().min_w_0()))
                    .child(row("First import").child(value(format!("{} in the background; you can pause it", format_minutes(e.first_import_minutes))).flex_1().min_w_0()))
                    .child(note("Everything stays on this machine: transcripts are read and embedded locally and the indexes never leave it. The only network access is this download."))
                    .child(
                        buttons()
                            .mt(px(12.))
                            .child(primary_button("history-enable", "Download and enable").on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.confirm_enable()), cx))))
                            .child(ghost_button("history-enable-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| {
                                ui.cancel();
                                None
                            }, cx)))),
                    ),
            );
        }
        if ui.phase == Phase::Enabling && line.is_none() {
            card = card.child(note("Setting up workspace history…").mt(px(10.)));
        }
        if let Some((text, fraction)) = line.filter(|_| ui.phase != Phase::Disabling) {
            card = card.child(
                panel(14.)
                    .child(
                        div().flex().items_center().gap(px(10.)).child(value(text).flex_1().min_w_0()).child(
                            ghost_button("history-pause", if paused { "Resume" } else { "Pause" })
                                .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, p| Some(ui.pause_or_resume(p)), cx))),
                        ),
                    )
                    .when_some(fraction, |d, f| d.child(bar(f, paused))),
            );
        }
        if ui.shows_stats(&p) {
            let stats = if workspaces.is_empty() {
                panel(14.).child(note("No workspace sessions indexed yet."))
            } else {
                panel(14.)
                    .child(div().flex().flex_col().gap(px(6.)).children(workspaces.iter().map(|w| {
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(10.))
                            .min_w_0()
                            .child(value(workspace_label(&names, &w.workspace_id)).font_weight(FontWeight::SEMIBOLD).flex_1().min_w_0().truncate())
                            .child(note(workspace_line(w)).flex_none())
                    })))
                    .child(note(format!("Indexes use {} in total.", format_bytes(index_bytes as f64))).mt(px(8.)))
            };
            card = card.child(stats).child(self.render_distill(&p, cx));
        }
        if ui.phase == Phase::ConfirmOff {
            card = card.child(
                panel(14.)
                    .child(value("Turn off workspace history?").font_weight(FontWeight::SEMIBOLD))
                    .child(note(confirm_off_text(index_bytes)).mt(px(6.)))
                    .child(
                        buttons()
                            .mt(px(12.))
                            // Deleting is the suggested choice: no resources left behind.
                            .child(
                                primary_button("history-off-delete", "Turn off and delete data")
                                    .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.confirm_disable(true)), cx))),
                            )
                            .child(
                                ghost_button("history-off-keep", "Turn off, keep data")
                                    .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.confirm_disable(false)), cx))),
                            )
                            .child(ghost_button("history-off-cancel", "Cancel").text_color(TEXT_MUTED).on_click(cx.listener(|this, _, _, cx| {
                                this.history_do(
                                    |ui, _| {
                                        ui.cancel();
                                        None
                                    },
                                    cx,
                                )
                            }))),
                    ),
            );
        }
        if ui.phase == Phase::Disabling {
            card = card.child(note("Turning off workspace history…").mt(px(10.)));
        }
        if !error.is_empty() {
            // A failed enable may leave a partial download behind; let the user clear it.
            let offer = !on && ui.phase == Phase::Idle;
            card = card.child(error_line(error)).when(offer, |d| {
                d.child(
                    buttons()
                        .mt(px(8.))
                        .child(
                            ghost_button("history-retry", "Try again")
                                .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.start_enable()), cx))),
                        )
                        .child(
                            ghost_button("history-remove", "Remove downloaded files")
                                .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.confirm_disable(true)), cx))),
                        ),
                )
            });
        }
        card
    }

    /// Decision notes: a second, separately confirmed opt-in under history.
    fn render_distill(&self, p: &Props, cx: &mut Context<Self>) -> Div {
        let ui = &self.manager.history;
        let status = p.status.as_ref().and_then(|s| s.distill.clone());
        let estimate = ui.d_estimate.as_ref();
        let on = ui.d_on(p);
        let paused = p.paused();
        let spent = status.as_ref().map(|s| s.spent_usd).or(estimate.map(|e| e.spent_usd)).unwrap_or(0.);
        let notes = status.as_ref().map(|s| s.notes).or(estimate.map(|e| e.notes)).unwrap_or(0);
        let error = ui.d_shown_error(status.as_ref());

        let mut d = panel(12.).child(toggle(
            "history-distill",
            "Decision notes (uses Claude Haiku)",
            Some("Distills finished sessions into short notes — what was decided and why, alternatives that were rejected, gotchas, conventions — which agents find through sg-history next to the turns they came from. Runs Claude Haiku through your Claude Code login: on a subscription that uses your plan limits rather than money; with an API key it is billed at the rates below. A session is distilled once it has been idle for 30 minutes, and never twice for the same content. Off by default."),
            on,
            matches!(ui.d_phase, DistillPhase::Enabling | DistillPhase::Deleting),
            |this, _, cx| this.history_do(|ui, p| ui.d_toggle(p), cx),
            cx,
        ));

        if !on {
            let mut block = div().mt(px(10.));
            if ui.d_estimating && estimate.is_none() {
                block = block.child(note("Estimating the cost from your index…"));
            }
            if let Some(e) = estimate {
                block = block
                    .child(row("Backlog").child(rich(backlog_text(e))))
                    .child(row("Ongoing").child(rich(ongoing_text(e))))
                    .when(e.spent_usd > 0., |b| b.child(row("Spent so far").child(value(format!("{} ({} notes)", format_usd(e.spent_usd), count(e.notes))))))
                    .child(note(price_note(e)));
            }
            d = d.child(block);
        }

        if ui.d_phase == DistillPhase::Confirm {
            d = d.child(
                div()
                    .mt(px(12.))
                    .child(value(distill_confirm_headline(estimate)).font_weight(FontWeight::SEMIBOLD))
                    .child(note("Excerpts of your past sessions (secrets already redacted) are sent to Anthropic through Claude Code, one group at a time in the background. You can pause or turn it off at any time; notes already written are kept.").mt(px(4.)))
                    .child(
                        buttons()
                            .mt(px(10.))
                            .child(primary_button("distill-enable", "Enable decision notes").on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.d_confirm_enable()), cx))))
                            .child(ghost_button("distill-enable-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| {
                                ui.d_cancel();
                                None
                            }, cx)))),
                    ),
            );
        }

        if on && let Some(s) = &status {
            let done = s.groups_distilled;
            let total = done + s.groups_pending;
            d = d.child(
                div()
                    .mt(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                value(format!("{} of {} groups distilled · {} notes · {}", count(done), count(total), count(notes), distill_state_line(s)))
                                    .flex_1()
                                    .min_w_0(),
                            )
                            .child(
                                ghost_button("distill-pause", if paused { "Resume" } else { "Pause" })
                                    .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, p| Some(ui.pause_or_resume(p)), cx))),
                            ),
                    )
                    .when(total > 0, |b| b.child(bar(done as f64 / total as f64, paused)))
                    .child(note(distill_spent_note(spent, s)).mt(px(6.))),
            );
        }

        if !on && notes > 0 && !matches!(ui.d_phase, DistillPhase::ConfirmDelete | DistillPhase::Deleting) {
            d = d.child(
                div()
                    .mt(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(note(format!("{} decision notes are kept and still searchable.", count(notes))).flex_1())
                    .child(ghost_button("distill-delete", "Delete notes").on_click(cx.listener(|this, _, _, cx| {
                        this.history_do(
                            |ui, _| {
                                ui.d_ask_delete();
                                None
                            },
                            cx,
                        )
                    }))),
            );
        }

        if ui.d_phase == DistillPhase::ConfirmDelete {
            d = d.child(
                div()
                    .mt(px(10.))
                    .child(note(format!(
                        "Delete all {} decision notes? Your sessions stay indexed; turning notes on again would distill (and pay for) them again.",
                        count(notes)
                    )))
                    .child(
                        buttons()
                            .mt(px(8.))
                            .child(
                                primary_button("distill-delete-confirm", format!("Delete {} notes", count(notes)))
                                    .on_click(cx.listener(|this, _, _, cx| this.history_do(|ui, _| Some(ui.d_confirm_delete()), cx))),
                            )
                            .child(ghost_button("distill-delete-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                                this.history_do(
                                    |ui, _| {
                                        ui.d_cancel();
                                        None
                                    },
                                    cx,
                                )
                            }))),
                    ),
            );
        }

        if !error.is_empty() {
            d = d.child(error_line(error));
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Not gpui's #[test] (glob-imported through gpui_kit::*).
    use core::prelude::v1::test;

    const MB: f64 = 1024. * 1024.;
    const GB: f64 = 1024. * MB;

    fn estimate() -> HistoryEstimate {
        HistoryEstimate {
            transcript_files: 1234,
            transcript_bytes: (2. * GB) as u64,
            download_bytes: (420. * MB) as u64,
            disk_bytes: (1.5 * GB) as u64,
            first_import_minutes: 25.,
        }
    }

    fn status(state: ProgressState) -> HistoryStatus {
        HistoryStatus { enabled: state != ProgressState::Off, progress: HistoryProgress { state, ..Default::default() }, ..Default::default() }
    }

    fn off() -> Props {
        Props { enabled: false, distill: false, status: Some(status(ProgressState::Off)) }
    }

    fn on() -> Props {
        Props { enabled: true, distill: false, status: Some(status(ProgressState::Idle)) }
    }

    #[test]
    fn formats_bytes_and_minutes_for_people() {
        assert_eq!(format_bytes(0.), "0 B");
        assert_eq!(format_bytes(512.), "512 B");
        assert_eq!(format_bytes(310. * MB), "310 MB");
        assert_eq!(format_bytes(1.25 * GB), "1.3 GB");
        assert_eq!(format_bytes(2. * GB), "2.0 GB");
        assert_eq!(format_minutes(0.4), "under a minute");
        assert_eq!(format_minutes(1.), "about 1 minute");
        assert_eq!(format_minutes(25.4), "about 25 minutes");
        assert_eq!(format_minutes(130.), "about 2 h 10 min");
        assert_eq!(format_minutes(120.), "about 2 h");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(999), "999");
    }

    #[test]
    fn formats_money_so_a_real_spend_never_reads_as_zero() {
        assert_eq!(format_usd(0.), "$0");
        assert_eq!(format_usd(0.00042), "$0.0004");
        assert_eq!(format_usd(0.2715), "$0.27");
        assert_eq!(format_usd(1234.4), "$1,234");
    }

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_tokens(950.), "950");
        assert_eq!(format_tokens(12_400.), "12k");
        assert_eq!(format_tokens(3_400_000.), "3.4M");
        assert_eq!(format_tokens(25_000_000.), "25M");
    }

    #[test]
    fn shows_what_enabling_costs_before_anything_is_enabled() {
        let mut ui = HistoryUi::default();
        let p = off();
        assert_eq!(ui.toggle(&p), Some(Effect::Estimate));
        assert_eq!(ui.phase, Phase::Estimating);
        ui.estimated(Ok(estimate()));
        assert_eq!(ui.phase, Phase::Confirm);
        assert_eq!(
            estimate_headline(ui.estimate.as_ref().unwrap()),
            "Found 1,234 transcript files (2.0 GB) from Claude Code sessions in your workspace folders."
        );
        assert_eq!(format_bytes(estimate().download_bytes as f64), "420 MB");
        assert_eq!(format_bytes(estimate().disk_bytes as f64), "1.5 GB");
        assert_eq!(format_minutes(estimate().first_import_minutes), "about 25 minutes");
        // Showing the estimate is not consent: the switch is still off.
        assert!(!ui.on(&p));
        assert_eq!(
            estimate_headline(&HistoryEstimate::default()),
            "No Claude Code transcripts found in your workspace folders yet — new sessions are indexed as they happen."
        );
    }

    #[test]
    fn enables_only_after_the_confirm_button_never_on_cancel() {
        let mut ui = HistoryUi::default();
        let p = off();
        ui.toggle(&p);
        ui.estimated(Ok(estimate()));
        ui.cancel();
        assert_eq!(ui.phase, Phase::Idle);
        // The switch again while the estimate is up closes it too.
        ui.toggle(&p);
        assert_eq!(ui.toggle(&p), None);
        assert_eq!(ui.phase, Phase::Idle);

        ui.toggle(&p);
        ui.estimated(Ok(estimate()));
        assert_eq!(ui.confirm_enable(), Effect::Enable);
        assert!(ui.on(&p), "on while it installs");
        assert!(ui.busy());
        ui.enabled(Ok(()));
        assert!(ui.shows_stats(&on()));
    }

    #[test]
    fn a_failed_enable_shows_the_error_and_stays_off() {
        let mut ui = HistoryUi::default();
        let p = off();
        ui.toggle(&p);
        ui.estimated(Ok(estimate()));
        ui.confirm_enable();
        ui.enabled(Err("Model checksum mismatch".into()));
        assert_eq!(ui.shown_error(&p), "Model checksum mismatch");
        assert!(!ui.on(&p));
        assert_eq!(ui.phase, Phase::Idle, "Try again / Remove downloaded files are offered");
        assert_eq!(ui.start_enable(), Effect::Estimate);
        assert!(ui.error.is_empty());

        let mut ui = HistoryUi::default();
        ui.toggle(&p);
        ui.estimated(Err("no access".into()));
        assert_eq!(ui.error, "Could not check your transcripts: no access");
    }

    #[test]
    fn install_and_indexing_progress_with_pause_and_resume() {
        let ui = HistoryUi::default();
        let mut s = status(ProgressState::Installing);
        s.enabled = false;
        s.progress.downloaded_bytes = Some((100. * MB) as u64);
        s.progress.total_bytes = Some((400. * MB) as u64);
        let p = Props { enabled: false, distill: false, status: Some(s.clone()) };
        assert!(ui.on(&p), "an install still running counts as on");
        assert_eq!(progress_line(&s.progress), ("Downloading the runtime and model… 100 MB of 400 MB".into(), Some(0.25)));
        assert_eq!(ui.pause_or_resume(&p), Effect::Pause);

        let mut s = status(ProgressState::Paused);
        s.progress.embedded_chunks = Some(300);
        s.progress.total_chunks = Some(1200);
        let p = Props { enabled: true, distill: false, status: Some(s.clone()) };
        assert_eq!(progress_line(&s.progress).0, "Indexing paused — 300 of 1,200 chunks embedded");
        assert_eq!(ui.pause_or_resume(&p), Effect::Resume);

        let mut s = status(ProgressState::Paused);
        s.progress.downloaded_bytes = Some(0);
        assert_eq!(progress_line(&s.progress), ("Download paused — 0 B".into(), None));

        let mut failed = status(ProgressState::Error);
        failed.progress.error = None;
        assert_eq!(ui.shown_error(&Props { status: Some(failed), ..on() }), "Workspace history stopped with an error.");
    }

    #[test]
    fn lists_per_workspace_stats() {
        let w = HistoryWorkspaceStats { workspace_id: "ws-1".into(), sessions: 12, chunks: 340, embedded_chunks: 340, db_bytes: (3. * MB) as u64 };
        let names = vec![("ws-1".to_owned(), "swapit".to_owned())];
        assert_eq!(workspace_label(&names, "ws-1"), "swapit");
        assert_eq!(workspace_label(&names, "0123456789"), "Other workspace (01234567)");
        assert_eq!(workspace_line(&w), "12 sessions · 340 chunks · 3.0 MB");
        let partial = HistoryWorkspaceStats { sessions: 1, embedded_chunks: 100, ..w };
        assert_eq!(workspace_line(&partial), "1 session · 340 chunks (100 embedded) · 3.0 MB");
    }

    #[test]
    fn asks_before_turning_off_and_either_choice_reaches_disable() {
        let mut ui = HistoryUi::default();
        let p = on();
        assert_eq!(ui.toggle(&p), None, "the switch never disables by itself");
        assert_eq!(ui.phase, Phase::ConfirmOff);
        assert!(!ui.shows_stats(&p));
        assert_eq!(ui.confirm_disable(false), Effect::Disable { delete_data: false });
        assert!(!ui.on(&p), "off while disabling");
        ui.disabled(Ok(()));

        let mut ui = HistoryUi::default();
        ui.toggle(&p);
        assert_eq!(ui.confirm_disable(true), Effect::Disable { delete_data: true });
        // The switch again closes the question.
        let mut ui = HistoryUi::default();
        ui.toggle(&p);
        ui.toggle(&p);
        assert_eq!(ui.phase, Phase::Idle);
        assert!(confirm_off_text((3. * MB) as u64).contains("(indexes: 3.0 MB)"));
        assert!(!confirm_off_text(0).contains("indexes:"));
    }

    fn d_estimate() -> HistoryDistillEstimate {
        HistoryDistillEstimate {
            model: "claude-haiku-4-5".into(),
            price_input_per_m_tok: 1.,
            price_output_per_m_tok: 5.,
            pending_groups: 42,
            pending_input_tokens: 1_200_000,
            pending_output_tokens: 84_000,
            backlog_usd: 3.1,
            unsettled_groups: 2,
            monthly_groups: 30,
            monthly_input_tokens: 900_000,
            monthly_output_tokens: 60_000,
            monthly_usd: 2.2,
            spent_usd: 0.,
            notes: 0,
        }
    }

    #[test]
    fn distill_shows_the_estimate_before_anything_can_be_enabled() {
        let mut ui = HistoryUi::default();
        let p = on();
        assert!(ui.wants_distill_estimate(&p));
        assert_eq!(ui.take_distill_estimate(&p), Some(Effect::DistillEstimate));
        assert_eq!(ui.take_distill_estimate(&p), None, "once per opening");
        assert!(!ui.wants_distill_estimate(&p));
        ui.distill_estimated(Ok(d_estimate()));
        let e = ui.d_estimate.as_ref().unwrap();
        let (backlog, bold) = backlog_text(e);
        assert_eq!(backlog, "42 groups of settled sessions, ~1.2M input + 84k output tokens ≈ $3.10 once");
        assert_eq!(&backlog[bold.unwrap()], "$3.10");
        assert_eq!(ongoing_text(e).0, "≈ $2.20 / month at your last 30 days' pace (30 groups, ~900k input tokens)");
        let price = price_note(e);
        assert!(price.starts_with("claude-haiku-4-5 list price: $1.00 per million input tokens, $5.00 per million output tokens."));
        assert!(price.ends_with(" 2 more groups are from sessions active in the last 30 minutes and will be counted once they settle."));
        assert!(!ui.d_on(&p));

        // Hidden while history is off or being turned off.
        let mut hidden = HistoryUi::default();
        assert_eq!(hidden.take_distill_estimate(&off()), None);
        hidden.phase = Phase::ConfirmOff;
        assert_eq!(hidden.take_distill_estimate(&p), None);
    }

    #[test]
    fn distill_enables_only_from_the_confirm_step() {
        let mut ui = HistoryUi::default();
        let p = on();
        assert_eq!(ui.d_toggle(&p), None);
        assert_eq!(ui.d_phase, DistillPhase::Confirm);
        assert_eq!(distill_confirm_headline(Some(&d_estimate())), "Run Claude Haiku on 42 groups (≈ $3.10 at API rates)?");
        assert_eq!(distill_confirm_headline(None), "Run Claude Haiku on your settled sessions?");
        ui.d_cancel();
        assert_eq!(ui.d_phase, DistillPhase::Idle);
        ui.d_toggle(&p);
        assert_eq!(ui.d_confirm_enable(), Effect::DistillEnable);
        assert!(ui.d_on(&p));
        ui.distill_enabled(Err("Turn on workspace history first.".into()));
        assert!(!ui.d_on(&p));
        assert_eq!(ui.d_shown_error(None), "Turn on workspace history first.");
    }

    #[test]
    fn distill_turns_off_at_once_and_deletes_only_after_confirming() {
        let mut ui = HistoryUi::default();
        let p = Props { distill: true, ..on() };
        assert_eq!(ui.d_toggle(&p), Some(Effect::DistillDisable));
        // Asked again once notes are off again.
        ui.d_asked = true;
        assert!(ui.wants_distill_estimate(&p));
        assert_eq!(ui.take_distill_estimate(&p), None);
        assert!(!ui.d_asked);

        ui.d_ask_delete();
        assert_eq!(ui.d_phase, DistillPhase::ConfirmDelete);
        assert_eq!(ui.d_confirm_delete(), Effect::DeleteNotes);
        ui.notes_deleted(Ok(()));
        assert_eq!(ui.d_phase, DistillPhase::Idle);
    }

    #[test]
    fn distill_status_lines() {
        let mut s =
            HistoryDistillStatus { enabled: true, state: DistillState::Waiting, groups_pending: 3, error: Some("rate limited".into()), ..Default::default() };
        let ui = HistoryUi::default();
        assert_eq!(ui.d_shown_error(Some(&s)), "rate limited");
        assert_eq!(distill_state_line(&s), "Waiting to retry.");
        s.state = DistillState::Idle;
        assert_eq!(ui.d_shown_error(Some(&s)), "");
        assert_eq!(distill_state_line(&s), "Waiting for the next pass.");
        s.groups_pending = 0;
        assert_eq!(distill_state_line(&s), "Up to date.");
        s.groups_unsettled = 1;
        s.groups_failed = 2;
        assert_eq!(
            distill_spent_note(0.00042, &s),
            "Spent so far: $0.0004, as reported by Claude Code (on a subscription this is plan usage, not a charge). 1 group wait for their session to settle. 2 gave unusable output and were skipped."
        );
    }

    #[test]
    fn each_opening_is_a_new_session() {
        assert_ne!(HistoryUi::default().session, HistoryUi::default().session);
    }
}
