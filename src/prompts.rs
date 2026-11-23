//! The sidebar's Prompts popover (the Electron app's PromptsButton): reusable
//! multi-line prompts for the whole environment (`envPrompts`), edited in
//! place. Clicking one types it into the focused terminal (not run); ▶ types
//! it and presses Enter.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::AppView;
use crate::commands::model::{self, ENV_PROMPTS, SavedPrompt};
use crate::commands::{clamp_at, frame, glyph_button, section_label};
use crate::model::workspace::now_ms;
use crate::theme::*;

const PROMPTS_W: f32 = 360.;

impl AppView {
    pub(crate) fn env_prompts(&self) -> Vec<SavedPrompt> {
        model::list(&self.state.extra, ENV_PROMPTS)
    }

    /// Open the editor on a new prompt (`None`) or an existing one.
    fn start_prompt_editor(&mut self, prompt: Option<&SavedPrompt>, window: &mut Window, cx: &mut Context<Self>) {
        let (title, body) = prompt.map(|p| (p.title.clone().unwrap_or_default(), p.body.clone())).unwrap_or_default();
        self.snippets.editing = Some(prompt.map(|p| p.id.clone()));
        self.snippets.prompt_title.update(cx, |i, cx| {
            i.set_value(title, window, cx);
            i.focus(window, cx);
        });
        self.snippets.prompt_body.update(cx, |i, cx| i.set_value(body, window, cx));
        cx.notify();
    }

    fn close_prompt_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.snippets.editing = None;
        self.snippets.prompt_title.update(cx, |i, cx| i.set_value("", window, cx));
        self.snippets.prompt_body.update(cx, |i, cx| i.set_value("", window, cx));
        self.snippets.focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn save_prompt_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.snippets.editing.clone() else {
            return;
        };
        let title = self.snippets.prompt_title.read(cx).value().to_string();
        let body = self.snippets.prompt_body.read(cx).value().to_string();
        let mut saved = false;
        model::update::<SavedPrompt>(&mut self.state.extra, ENV_PROMPTS, |l| saved = model::save_prompt(l, editing.as_deref(), &title, &body, now_ms()));
        if saved {
            self.changed(cx);
            self.close_prompt_editor(window, cx);
        }
    }

    fn delete_prompt(&mut self, id: String, cx: &mut Context<Self>) {
        model::update::<SavedPrompt>(&mut self.state.extra, ENV_PROMPTS, |l| l.retain(|p| p.id != id));
        if self.snippets.editing.as_ref().is_some_and(|e| e.as_deref() == Some(id.as_str())) {
            self.snippets.editing = None;
        }
        self.changed(cx);
    }

    pub(crate) fn render_prompts_popover(&self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let prompts = self.env_prompts();
        let editing = self.snippets.editing.clone();

        let rows: Vec<AnyElement> = prompts
            .iter()
            .map(|p| {
                let (paste, run, edit, del) = (p.body.clone(), p.body.clone(), p.clone(), p.id.clone());
                let headline = p.title.clone().unwrap_or_else(|| p.first_line().to_owned());
                let detail = if p.title.is_some() {
                    p.first_line().to_owned()
                } else if p.body.contains('\n') {
                    "…".to_owned()
                } else {
                    String::new()
                };
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .px(px(12.))
                    .py(px(6.))
                    .hover(|d| d.bg(bg_hover()))
                    .child(
                        div()
                            .id(SharedString::from(format!("prompt-{}", p.id)))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .cursor_pointer()
                            .line_height(relative(1.4))
                            .child(div().truncate().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_PRIMARY).child(headline))
                            .when(!detail.is_empty(), |d| d.child(div().truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(detail)))
                            .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&paste, false, cx))),
                    )
                    .child(
                        glyph_button(SharedString::from(format!("prompt-run-{}", p.id)), "▶", TEXT_MUTED, ACCENT_GREEN)
                            .on_click(cx.listener(move |this, _, _, cx| this.type_into_terminal(&run, true, cx))),
                    )
                    .child(
                        glyph_button(SharedString::from(format!("prompt-edit-{}", p.id)), "✎", TEXT_MUTED, TEXT_PRIMARY)
                            .font_weight(FontWeight::NORMAL)
                            .on_click(cx.listener(move |this, _, window, cx| this.start_prompt_editor(Some(&edit), window, cx))),
                    )
                    .child(
                        glyph_button(SharedString::from(format!("prompt-del-{}", p.id)), "×", TEXT_MUTED, ACCENT_RED)
                            .text_size(px(14.))
                            .on_click(cx.listener(move |this, _, _, cx| this.delete_prompt(del.clone(), cx))),
                    )
                    .into_any_element()
            })
            .collect();

        let footer = match &editing {
            // The editor: a new prompt or an existing one.
            Some(id) => {
                let can_save = !self.snippets.prompt_body.read(cx).value().trim().is_empty();
                let button = |id: &'static str, label: &'static str| {
                    div()
                        .id(id)
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(border())
                        .text_size(px(12.))
                        .cursor_pointer()
                        .child(label)
                };
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(border())
                    // Escape closes the editor (not the popover); ⌘/Ctrl+Enter saves (from the
                    // title field: the body's own ⌘Enter arrives as InputEvent::PressEnter).
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        let ks = &ev.keystroke;
                        if ks.key == "escape" {
                            this.close_prompt_editor(window, cx);
                            cx.stop_propagation();
                        } else if ks.key == "enter" && (ks.modifiers.platform || ks.modifiers.control) {
                            this.save_prompt_draft(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(section_label(if id.is_some() { "Edit prompt" } else { "New prompt" }).px(px(0.)).pt(px(0.)))
                    .child(Input::new(&self.snippets.prompt_title).small())
                    .child(div().h(px(96.)).text_size(px(12.)).child(Textarea::new(&self.snippets.prompt_body).h_full()))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(6.))
                            .child(
                                button("prompt-cancel", "Cancel")
                                    .text_color(TEXT_MUTED)
                                    .hover(|d| d.text_color(TEXT_PRIMARY))
                                    .on_click(cx.listener(|this, _, window, cx| this.close_prompt_editor(window, cx))),
                            )
                            .child(
                                button("prompt-save", "Save")
                                    .px(px(12.))
                                    .bg(bg_hover())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(TEXT_PRIMARY)
                                    .when(!can_save, |d| d.opacity(0.5))
                                    .on_click(cx.listener(|this, _, window, cx| this.save_prompt_draft(window, cx))),
                            ),
                    )
                    .into_any_element()
            }
            None => div()
                .px(px(12.))
                .py(px(8.))
                .border_t_1()
                .border_color(border())
                .child(
                    div()
                        .id("prompt-new")
                        .h(px(30.))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_dashed()
                        .border_color(border())
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(TEXT_MUTED)
                        .cursor_pointer()
                        .hover(|d| d.text_color(TEXT_PRIMARY).bg(bg_hover()))
                        .child("+ New prompt")
                        .on_click(cx.listener(|this, _, window, cx| this.start_prompt_editor(None, window, cx))),
                )
                .into_any_element(),
        };

        let empty = prompts.is_empty() && editing.is_none();
        deferred(
            anchored().position(clamp_at(at, window)).snap_to_window_with_margin(px(8.)).child(
                frame("prompts-popover", PROMPTS_W, "Prompts", &self.snippets.focus, cx)
                    .child(crate::overlay::recorder(at, 0))
                    .child(
                        div()
                            .id("prompt-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .when(empty, |d| {
                                d.child(
                                    div()
                                        .p(px(12.))
                                        .text_size(px(11.))
                                        .text_color(TEXT_MUTED)
                                        .child("Save reusable prompts here and paste them into any terminal."),
                                )
                            })
                            .children(rows),
                    )
                    .child(footer),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
