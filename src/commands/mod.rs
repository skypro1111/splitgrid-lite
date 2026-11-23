//! The sidebar's Commands popover (the Electron app's CommandsButton): a
//! heuristically captured "recent" feed plus favorites at two binding levels,
//! this workspace and the whole environment. Clicking a command types it into
//! the focused terminal (not run); ▶ types it and presses Enter.
//!
//! The Prompts popover (crate::prompts) shares this module's editor state and
//! the "type into the focused terminal" path.

pub mod capture;
pub mod model;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::{AppView, PopoverKind};
use crate::model::workspace::now_ms;
use crate::theme::*;
use model::{ENV_FAVORITES, RECENT, RecentCommand, SavedCommand, WS_FAVORITES};

const COMMANDS_W: f32 = 340.;
/// The popovers' estimated height, to keep them on screen.
pub(crate) const POPOVER_EST_H: f32 = 440.;

/// Which list a favorite belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Workspace,
    Env,
}

/// The Commands / Prompts popovers' own state, one field on AppView.
pub(crate) struct SnippetsUi {
    /// "Add a favorite command…"
    pub cmd_input: Entity<InputState>,
    /// Where that input saves to.
    pub level: Level,
    pub prompt_title: Entity<InputState>,
    pub prompt_body: Entity<TextareaState>,
    /// The prompt editor: None closed, Some(None) a new prompt, Some(Some(id)) editing that one.
    pub editing: Option<Option<String>>,
    /// Keeps keyboard focus (Escape) in a popover while no field has it.
    pub focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SnippetsUi {
    pub fn new(window: &mut Window, cx: &mut Context<AppView>) -> Self {
        let cmd_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add a favorite command…  (Enter)"));
        let prompt_title = cx.new(|cx| InputState::new(window, cx).placeholder("Title (optional)"));
        let prompt_body = cx.new(|cx| TextareaState::new(window, cx).placeholder("Prompt text…  (⌘Enter to save)"));
        let subscriptions = vec![
            cx.subscribe_in(&cmd_input, window, |this: &mut AppView, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit_command_draft(window, cx);
                }
            }),
            // ⌘Enter in the prompt body saves (the textarea handles that key itself).
            cx.subscribe_in(&prompt_body, window, |this: &mut AppView, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary: true, .. } = ev {
                    this.save_prompt_draft(window, cx);
                }
            }),
        ];
        SnippetsUi { cmd_input, level: Level::Workspace, prompt_title, prompt_body, editing: None, focus: cx.focus_handle(), _subscriptions: subscriptions }
    }
}

/// The neutral count on the Commands / Prompts buttons.
pub(crate) fn count_badge(n: usize, collapsed: bool) -> Div {
    div()
        .absolute()
        .top(px(if collapsed { -2. } else { 4. }))
        .right(px(if collapsed { -2. } else { 8. }))
        .min_w(px(13.))
        .h(px(13.))
        .px(px(3.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .bg(bg_hover())
        .border_1()
        .border_color(border())
        .text_color(TEXT_MUTED)
        .text_size(px(9.))
        .line_height(px(13.))
        .font_weight(FontWeight::BOLD)
        .child(n.to_string())
}

pub(crate) fn shadow() -> Vec<BoxShadow> {
    vec![BoxShadow { color: c(hexa(0x000000, 0.45)), offset: point(px(0.), px(8.)), blur_radius: px(24.), spread_radius: px(0.), inset: false }]
}

/// A small square glyph button (▶, ×, W, E, ✎) that lights up `hover` on hover.
pub(crate) fn glyph_button(id: impl Into<ElementId>, glyph: &'static str, color: Rgba, hover: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(22.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .cursor_pointer()
        .hover(move |d| d.text_color(hover).bg(bg_hover()))
        .child(glyph)
}

pub(crate) fn section_label(text: impl Into<SharedString>) -> Div {
    div().px(px(12.)).pt(px(8.)).pb(px(4.)).text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(TEXT_MUTED).child(text.into().to_uppercase())
}

/// The popover frame both popovers share: header, then `body`.
pub(crate) fn frame(id: &'static str, width: f32, title: &'static str, focus: &FocusHandle, cx: &mut Context<AppView>) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(width))
        .flex()
        .flex_col()
        .bg(bg_surface())
        .border_1()
        .border_color(border())
        .rounded(px(8.))
        .shadow(shadow())
        .overflow_hidden()
        .occlude()
        .track_focus(focus)
        .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popover(cx)))
        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
            if ev.keystroke.key == "escape" {
                this.close_popover(cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .px(px(12.))
                .py(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(border())
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(TEXT_PRIMARY)
                .child(title)
                .child(div().font_weight(FontWeight::NORMAL).text_color(TEXT_MUTED).child("click = paste · ▶ = run")),
        )
}

/// Keep a popover on screen: the window height bounds how low it may start.
pub(crate) fn clamp_at(at: Point<Pixels>, window: &Window) -> Point<Pixels> {
    let max_top = window.viewport_size().height - px(POPOVER_EST_H);
    let top = if window.is_fullscreen() { px(0.) } else { px(crate::chrome::TITLE_H) };
    point(at.x, at.y.min(max_top).max(top))
}

impl AppView {
    // ── Data ────────────────────────────────────────────────────────────────

    pub(crate) fn env_command_favorites(&self) -> Vec<SavedCommand> {
        model::list(&self.state.extra, ENV_FAVORITES)
    }

    pub(crate) fn workspace_command_favorites(&self) -> Vec<SavedCommand> {
        self.state.active().map(|w| model::list(&w.extra, WS_FAVORITES)).unwrap_or_default()
    }

    pub(crate) fn recent_commands(&self) -> Vec<RecentCommand> {
        model::list(&self.state.extra, RECENT)
    }

    /// Favorites in both levels, for the Commands button's badge.
    pub(crate) fn command_favorite_count(&self) -> usize {
        self.env_command_favorites().len() + self.workspace_command_favorites().len()
    }

    /// Edit one level's favorites; the workspace level is the active workspace's.
    fn update_favorites(&mut self, level: Level, edit: impl FnOnce(&mut Vec<SavedCommand>), cx: &mut Context<Self>) {
        match level {
            Level::Env => model::update(&mut self.state.extra, ENV_FAVORITES, edit),
            Level::Workspace => {
                let Some(ws) = self.state.active_mut() else {
                    return;
                };
                model::update(&mut ws.extra, WS_FAVORITES, edit);
                ws.updated_at = now_ms();
            }
        }
        self.changed(cx);
    }

    /// A terminal of this window reported a command typed at its prompt.
    pub(crate) fn record_recent_command(&mut self, command: &str, cx: &mut Context<Self>) {
        let mut recorded = false;
        model::update::<RecentCommand>(&mut self.state.extra, RECENT, |l| recorded = model::record_recent(l, command, now_ms()));
        if recorded {
            self.changed(cx);
        }
    }

    pub(crate) fn add_command_favorite(&mut self, level: Level, command: String, cx: &mut Context<Self>) {
        self.update_favorites(
            level,
            |l| {
                model::add_favorite(l, &command, None, now_ms());
            },
            cx,
        );
    }

    fn remove_command_favorite(&mut self, level: Level, id: String, cx: &mut Context<Self>) {
        self.update_favorites(level, |l| l.retain(|f| f.id != id), cx);
    }

    fn remove_recent_command(&mut self, command: String, cx: &mut Context<Self>) {
        model::update::<RecentCommand>(&mut self.state.extra, RECENT, |l| l.retain(|r| r.command != command));
        self.changed(cx);
    }

    fn clear_recent_commands(&mut self, cx: &mut Context<Self>) {
        model::update::<RecentCommand>(&mut self.state.extra, RECENT, Vec::clear);
        self.changed(cx);
    }

    pub(crate) fn submit_command_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.snippets.cmd_input.read(cx).value().trim().to_string();
        if draft.is_empty() {
            return;
        }
        self.add_command_favorite(self.snippets.level, draft, cx);
        self.snippets.cmd_input.update(cx, |i, cx| i.set_value("", window, cx));
    }

    // ── Popover plumbing ────────────────────────────────────────────────────

    /// The Commands / Prompts popover is opening: where the keyboard goes.
    pub(crate) fn open_snippets(&mut self, kind: &PopoverKind, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            PopoverKind::Commands => self.snippets.cmd_input.update(cx, |i, cx| i.focus(window, cx)),
            PopoverKind::Prompts if self.snippets.editing.is_some() => self.snippets.prompt_body.update(cx, |i, cx| i.focus(window, cx)),
            _ => self.snippets.focus.focus(window, cx),
        }
    }

    /// Type `text` into the active workspace's focused terminal (or its first
    /// one), then Enter when `execute` — raw, as the Electron app's
    /// runCommandInTerminal sends it, so the capture sees it too. Closes the
    /// popover so the terminal has the keyboard again.
    pub(crate) fn type_into_terminal(&mut self, text: &str, execute: bool, cx: &mut Context<Self>) {
        let Some(ws) = self.state.active() else {
            return;
        };
        let target = ws
            .focused_container_id
            .clone()
            .filter(|id| self.panes.contains_key(id))
            .or_else(|| ws.layout_tree.as_ref()?.leaf_ids().into_iter().find(|id| self.panes.contains_key(id)));
        let Some(pane) = target.and_then(|id| self.panes.get(&id)) else {
            return;
        };
        let payload = if execute { format!("{text}\n") } else { text.to_owned() };
        pane.view.update(cx, |v, cx| v.type_text(&payload, cx));
        self.close_popover(cx);
    }

    // ── The Commands popover ───────────────────────────────────────────────

    pub(crate) fn render_commands_popover(&self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let ws_favs = self.workspace_command_favorites();
        let env_favs = self.env_command_favorites();
        let recent = self.recent_commands();
        let ws_name = self.state.active().map(|w| w.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "This workspace".into());
        let favorited = |cmd: &str| ws_favs.iter().chain(&env_favs).any(|f| f.command == cmd);
        let mono = fonts().terminal;

        let fav_row = |f: &SavedCommand, level: Level, cx: &mut Context<Self>| {
            let key = format!("{}-{}", if level == Level::Env { "e" } else { "w" }, f.id);
            let (paste, run, id) = (f.command.clone(), f.command.clone(), f.id.clone());
            let text = div()
                .id(SharedString::from(format!("cmd-fav-{key}")))
                .flex_1()
                .min_w_0()
                .flex()
                .items_baseline()
                .gap(px(6.))
                .overflow_hidden()
                .cursor_pointer()
                .font_family(mono.clone())
                .text_size(px(12.))
                .text_color(TEXT_PRIMARY)
                .map(|d| match &f.label {
                    Some(label) => d
                        .child(div().flex_shrink_0().max_w(px(160.)).truncate().child(label.clone()))
                        .child(div().min_w_0().truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(f.command.clone())),
                    None => d.child(div().min_w_0().truncate().child(f.command.clone())),
                })
                .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&paste, false, cx)));
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .py(px(5.))
                .hover(|d| d.bg(bg_hover()))
                .child(text)
                .child(
                    glyph_button(SharedString::from(format!("cmd-fav-run-{key}")), "▶", TEXT_MUTED, ACCENT_GREEN)
                        .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&run, true, cx))),
                )
                .child(
                    glyph_button(SharedString::from(format!("cmd-fav-del-{key}")), "×", TEXT_MUTED, ACCENT_RED)
                        .text_size(px(14.))
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_command_favorite(level, id.clone(), cx))),
                )
                .into_any_element()
        };

        let sub_label = |text: String| section_label(text).pt(px(2.)).pb(px(2.)).text_size(px(9.)).opacity(0.85);

        let mut rows: Vec<AnyElement> = Vec::new();
        if !ws_favs.is_empty() || !env_favs.is_empty() {
            rows.push(section_label("★ Favorites").into_any_element());
        }
        if !ws_favs.is_empty() {
            rows.push(sub_label(ws_name.clone()).into_any_element());
            rows.extend(ws_favs.iter().map(|f| fav_row(f, Level::Workspace, cx)));
        }
        if !env_favs.is_empty() {
            rows.push(sub_label("Environment".into()).into_any_element());
            rows.extend(env_favs.iter().map(|f| fav_row(f, Level::Env, cx)));
        }

        rows.push(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(section_label("Recent"))
                .when(!recent.is_empty(), |d| {
                    d.child(
                        div()
                            .id("cmd-recent-clear")
                            .mr(px(8.))
                            .px(px(8.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded(px(4.))
                            .text_size(px(10.))
                            .text_color(TEXT_MUTED)
                            .cursor_pointer()
                            .hover(|d| d.text_color(TEXT_PRIMARY))
                            .child("Clear")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_recent_commands(cx))),
                    )
                })
                .into_any_element(),
        );
        if recent.is_empty() {
            rows.push(
                div()
                    .px(px(12.))
                    .pt(px(6.))
                    .pb(px(12.))
                    .text_size(px(11.))
                    .text_color(TEXT_MUTED)
                    .child("Commands you run in terminals show up here.")
                    .into_any_element(),
            );
        }
        for (ix, r) in recent.iter().enumerate() {
            let star = if favorited(&r.command) { ACCENT_AMBER } else { TEXT_MUTED };
            let (paste, run, to_ws, to_env, del) = (r.command.clone(), r.command.clone(), r.command.clone(), r.command.clone(), r.command.clone());
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(12.))
                    .py(px(5.))
                    .hover(|d| d.bg(bg_hover()))
                    .child(
                        div()
                            .id(("cmd-recent", ix))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .text_color(TEXT_PRIMARY)
                            .child(r.command.clone())
                            .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&paste, false, cx))),
                    )
                    .child(
                        glyph_button(("cmd-recent-run", ix), "▶", TEXT_MUTED, ACCENT_GREEN)
                            .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&run, true, cx))),
                    )
                    .child(
                        glyph_button(("cmd-recent-ws", ix), "W", star, ACCENT_AMBER)
                            .on_click(cx.listener(move |this, _, _, cx| this.add_command_favorite(Level::Workspace, to_ws.clone(), cx))),
                    )
                    .child(
                        glyph_button(("cmd-recent-env", ix), "E", star, ACCENT_AMBER)
                            .on_click(cx.listener(move |this, _, _, cx| this.add_command_favorite(Level::Env, to_env.clone(), cx))),
                    )
                    .child(
                        glyph_button(("cmd-recent-del", ix), "×", TEXT_MUTED, ACCENT_RED)
                            .text_size(px(14.))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_recent_command(del.clone(), cx))),
                    )
                    .into_any_element(),
            );
        }

        // Manual add: level toggle + input.
        let level = self.snippets.level;
        let pill =
            |id: &'static str, glyph: &'static str, l: Level, cx: &mut Context<Self>| {
                let active = level == l;
                div()
                    .id(id)
                    .flex_none()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .border_1()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .map(|d| {
                        if active {
                            d.border_color(ACCENT_GREEN).bg(ACCENT_GREEN).text_color(ON_BADGE)
                        } else {
                            d.border_color(border()).text_color(TEXT_MUTED)
                        }
                    })
                    .child(glyph)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.snippets.level = l;
                        cx.notify();
                    }))
            };
        let has_draft = !self.snippets.cmd_input.read(cx).value().trim().is_empty();
        let footer = div()
            .px(px(12.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .border_t_1()
            .border_color(border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(pill("cmd-level-ws", "W", Level::Workspace, cx))
                    .child(pill("cmd-level-env", "E", Level::Env, cx))
                    .child(div().min_w_0().truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(if level == Level::Workspace {
                        ws_name
                    } else {
                        "Environment".into()
                    })),
            )
            .child(
                div().flex().gap(px(6.)).child(div().flex_1().font_family(mono).child(Input::new(&self.snippets.cmd_input).small())).child(
                    div()
                        .id("cmd-add")
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(border())
                        .bg(bg_hover())
                        .text_size(px(12.))
                        .map(|d| if has_draft { d.cursor_pointer() } else { d.opacity(0.5) })
                        .child("Add")
                        .on_click(cx.listener(|this, _, window, cx| this.submit_command_draft(window, cx))),
                ),
            );

        deferred(
            anchored().position(clamp_at(at, window)).snap_to_window_with_margin(px(8.)).child(
                frame("commands-popover", COMMANDS_W, "Commands", &self.snippets.focus, cx)
                    .child(crate::overlay::recorder(at, 0))
                    .child(div().id("cmd-list").max_h(px(360.)).overflow_y_scroll().children(rows))
                    .child(footer),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
