//! Settings → Skills: every bundled sg-* skill, whether it's in
//! ~/.claude/skills, and a switch for each.
//!
//! A skill only does something with its feature on (sg-sql answers "disabled"
//! without the SQL bridge), so switching one on also turns its feature on, and
//! "Enable all" turns on every feature they need. Write access (SQL writes,
//! SFTP uploads) stays as it is: that remains a separate choice in General.
//! Switching a skill off keeps its feature and only leaves the skill out
//! (`agentSkillsDisabled`). Workspace history starts with a consent and a
//! download, so its skill points to the History card instead.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{SettingsTab, card, ghost_button, header_button, set_values, toggle};
use crate::agent::install::{Installer, SKILLS, SKILLS_OFF_KEY, SkillState, Wanted};
use crate::app::AppView;
use crate::theme::*;
use crate::ui::px;

/// What each skill gives an agent, for the list.
fn summary(name: &str) -> &'static str {
    match name {
        "sg-browser" => "Open URLs, dev servers and local HTML files in the browser pane, take screenshots, read the console and network, and fill in forms. Also shows results such as reports, diagrams and tables as pages.",
        "sg-terminal" => "List, read and drive the other terminals of the workspace (run commands, send keys, Ctrl-C), and open panes for long-running processes in the agent's zone.",
        "sg-agent" => "Inspect and drive Claude Code panes. Lite has no Claude Code pane yet, so this one only matters for the SplitGrid app.",
        "sg-sql" => "Query the workspace's saved database connections through the SQL pane: schema, queries, EXPLAIN, export. Read-only unless writes are allowed in General.",
        "sg-sftp" => "Move files to and from the workspace's remote hosts with the saved SSH credentials. Read-only unless uploads are allowed in General.",
        "sg-history" => "Search earlier Claude Code sessions of the workspace: past decisions, and which session changed a file or line.",
        _ => "",
    }
}

fn status(state: Option<SkillState>) -> (&'static str, Rgba) {
    match state {
        Some(SkillState::Ours) => ("Installed", ACCENT_GREEN),
        Some(SkillState::Foreign) => ("Installed by the SplitGrid app or edited", ACCENT_YELLOW),
        Some(SkillState::Absent) => ("Not installed", TEXT_MUTED),
        None => ("Claude Code not found", TEXT_MUTED),
    }
}

/// The names in `agentSkillsDisabled`, with `name` added or taken out.
fn skills_off_with(all: &serde_json::Map<String, Value>, name: &str, off: bool) -> Value {
    let mut names: Vec<String> = all.get(SKILLS_OFF_KEY).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    names.retain(|n| n != name);
    if off {
        names.push(name.to_owned());
    }
    names.into()
}

impl AppView {
    pub(super) fn render_skills(&self, cx: &mut Context<Self>) -> AnyElement {
        let all = self.store.app_settings();
        let wanted = Wanted::from_settings(&all);
        let history_on = all.get("historyEnabled").and_then(Value::as_bool).unwrap_or(false);
        let installer = Installer::new(self.store.dir());
        let states = installer.skill_states();
        let skills_dir = std::env::home_dir().unwrap_or_default().join(".claude/skills");
        let every_on = SKILLS.iter().all(|s| wanted.installs(s) || (s.name == "sg-history" && !history_on));

        let mut head = card(
            "Agent skills",
            Some("Skills teach Claude Code how to use SplitGrid: the browser pane, terminals, SQL, SFTP and workspace history. They are installed into ~/.claude/skills and work in the terminals of this app. Switching a skill on also turns on the feature it needs; write access to databases and remote files stays off until you allow it in General."),
        )
        .child(
            div()
                .mt(px(14.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    header_button("skills-enable-all", "Enable all", true)
                        .when(every_on, |d| d.opacity(0.5).cursor_default())
                        .when(!every_on, |d| d.on_click(cx.listener(|this, _, _, cx| this.enable_all_skills(cx)))),
                )
                .child(ghost_button("skills-open-dir", "Open skills folder").on_click(move |_, _, cx| {
                    let dir = if skills_dir.exists() { skills_dir.clone() } else { skills_dir.parent().map(|p| p.to_path_buf()).unwrap_or_default() };
                    cx.reveal_path(&dir);
                })),
        );
        if states.is_none() {
            head = head.child(div().mt(px(12.)).text_size(px(11.)).line_height(px(17.)).text_color(TEXT_MUTED).child(
                "Claude Code isn't set up on this Mac (no ~/.claude), so there is nowhere to install skills yet. Run claude once, then come back.",
            ));
        }

        let mut list = card("Skills", None);
        for (i, skill) in SKILLS.iter().enumerate() {
            let name = skill.name;
            let history = name == "sg-history";
            let checked = wanted.installs(skill);
            let (label, color) = status(states.as_ref().map(|s| s[i]));
            let mut row = div().mt(px(if i == 0 { 14. } else { 18. })).child(toggle(
                name,
                name,
                Some(summary(name)),
                checked,
                false,
                move |this, on, cx| this.set_skill(name, on, cx),
                cx,
            ));
            row = row.child(
                div()
                    .mt(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .child(div().size(px(6.)).rounded_full().bg(color))
                    .child(div().text_color(color).child(label))
                    .when(history && !history_on, |d| {
                        d.child(div().text_color(TEXT_MUTED).child("· needs Workspace history, which downloads a local model after you confirm")).child(
                            div()
                                .id("skills-history-setup")
                                .text_color(ACCENT)
                                .cursor_pointer()
                                .hover(|d| d.underline())
                                .child("Set up in General")
                                .on_click(cx.listener(|this, _, _, cx| this.open_settings(SettingsTab::General, cx))),
                        )
                    }),
            );
            list = list.child(row);
        }

        div().id("settings-skills").size_full().overflow_y_scroll().bg(bg_primary()).p(px(20.)).child(head).child(list).into_any_element()
    }

    /// One skill's switch: on turns its feature on too; off leaves the feature
    /// and only keeps the skill out of ~/.claude/skills.
    fn set_skill(&mut self, name: &'static str, on: bool, cx: &mut Context<Self>) {
        let all = self.store.app_settings();
        let history_on = all.get("historyEnabled").and_then(Value::as_bool).unwrap_or(false);
        if on && name == "sg-history" && !history_on {
            // Consent and a download first: that happens on the History card.
            self.open_settings(SettingsTab::General, cx);
            return;
        }
        let Some(skill) = SKILLS.iter().find(|s| s.name == name) else { return };
        let mut patch: Vec<(&str, Value)> = vec![(SKILLS_OFF_KEY, skills_off_with(&all, name, !on))];
        if on {
            patch.extend(skill.needs.iter().map(|k| (*k, Value::Bool(true))));
        }
        set_values(self, &patch, cx);
    }

    /// Every skill on, with the features they need (history only once set up).
    fn enable_all_skills(&mut self, cx: &mut Context<Self>) {
        let mut patch: Vec<(&str, Value)> = vec![(SKILLS_OFF_KEY, Value::Array(Vec::new()))];
        for key in SKILLS.iter().flat_map(|s| s.needs.iter()) {
            if !patch.iter().any(|(k, _)| k == key) {
                patch.push((key, Value::Bool(true)));
            }
        }
        set_values(self, &patch, cx);
    }
}
