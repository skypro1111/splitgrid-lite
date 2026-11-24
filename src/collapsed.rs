//! Collapsed panes in the window (model::collapse): their chips in the parent
//! pane's header, and the preview a chip shows when hovered a moment. A click
//! on a chip expands it back into its parent.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::app::AppView;
use crate::grid::tilde;
use crate::model::workspace::Workspace;
use crate::theme::*;

/// How long the pointer rests on a chip before its preview opens.
const OPEN_DELAY: Duration = Duration::from_millis(400);
/// How long a preview stays once the pointer leaves (time to reach it).
const CLOSE_DELAY: Duration = Duration::from_millis(250);
/// How often an open preview reads its terminal again.
const REFRESH: Duration = Duration::from_millis(500);
/// Chips shown in a header before "+N".
const MAX_CHIPS: usize = 5;
const PREVIEW_W: f32 = 640.;
/// The terminal text's padding inside the preview, each side.
const PREVIEW_PAD: f32 = 10.;
/// Monospace advance and line height, per point of font size.
const CHAR_W: f32 = 0.6;
const LINE_H: f32 = 1.3;

#[derive(Default)]
pub(crate) struct CollapsedUi {
    /// The chip under the pointer.
    hovered: Option<String>,
    over_preview: bool,
    /// The pane being previewed.
    pub(crate) preview: Option<String>,
    /// Where each chip was painted, to anchor its preview.
    chips: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    timer: Option<Task<()>>,
    refresh: Option<Task<()>>,
}

impl CollapsedUi {
    /// The preview's anchor (below its chip), for crate::overlay.
    pub(crate) fn anchor(&self) -> Option<Point<Pixels>> {
        let id = self.preview.as_ref()?;
        let b = *self.chips.borrow().get(id)?;
        Some(point(b.origin.x, b.origin.y + b.size.height + px(4.)))
    }
}

impl AppView {
    /// "zsh", a custom name, or the pane type; tinted by whether it runs.
    fn collapsed_label(&self, ws: &Workspace, id: &str) -> (String, Rgba) {
        let c = ws.container(id);
        let pane = self.panes.get(id);
        let name = c
            .and_then(|c| c.content.custom_name.clone())
            .or_else(|| pane.and_then(|p| p.process.as_ref()).map(|p| p.name.clone()))
            .or_else(|| c.map(|c| c.content.kind.label().to_owned()))
            .unwrap_or_else(|| "pane".into());
        (name, if pane.is_some() { DOT_CONNECTED } else { DOT_CONNECTING })
    }

    /// The chips of the panes collapsed into `parent`, for its header.
    pub(crate) fn render_collapsed_chips(&self, ws: &Workspace, parent: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ids = ws.collapsed_in(parent);
        if ids.is_empty() {
            return None;
        }
        let more = ids.len().saturating_sub(MAX_CHIPS);
        let chips = ids.iter().take(MAX_CHIPS).map(|id| {
            let (name, color) = self.collapsed_label(ws, id);
            let (h_id, c_id, rec_id, rec) = (id.clone(), id.clone(), id.clone(), self.collapsed_ui.chips.clone());
            let previewing = self.collapsed_ui.preview.as_deref() == Some(id.as_str());
            div()
                .id(SharedString::from(format!("chip-{id}")))
                .relative()
                .h(px(18.))
                .max_w(px(110.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(if previewing { ACCENT } else { border() })
                .bg(bg_surface())
                .hover(|d| d.border_color(ACCENT))
                .cursor_pointer()
                .text_size(px(10.))
                .child(div().flex_none().text_color(TEXT_MUTED).child("›_"))
                .child(div().min_w_0().truncate().text_color(color).font_weight(FontWeight::SEMIBOLD).child(name))
                .child(
                    canvas(
                        move |b, _, _| {
                            rec.borrow_mut().insert(rec_id.clone(), b);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| this.chip_hover(&h_id, *hovered, cx)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.collapsed_ui = CollapsedUi { chips: this.collapsed_ui.chips.clone(), ..Default::default() };
                    this.expand_pane(&c_id, cx);
                }))
                .into_any_element()
        });
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .children(chips)
                .when(more > 0, |d| d.child(div().text_size(px(10.)).text_color(TEXT_MUTED).child(format!("+{more}"))))
                .into_any_element(),
        )
    }

    fn chip_hover(&mut self, id: &str, hovered: bool, cx: &mut Context<Self>) {
        let ui = &mut self.collapsed_ui;
        if hovered {
            ui.hovered = Some(id.to_owned());
            if ui.preview.is_some() {
                // Already previewing: moving along the chips switches at once.
                self.show_preview(id.to_owned(), cx);
                return;
            }
            let id = id.to_owned();
            ui.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(OPEN_DELAY).await;
                let _ = this.update(cx, |this, cx| {
                    if this.collapsed_ui.hovered.as_deref() == Some(id.as_str()) {
                        this.show_preview(id, cx);
                    }
                });
            }));
        } else {
            if ui.hovered.as_deref() == Some(id) {
                ui.hovered = None;
            }
            self.schedule_preview_close(cx);
        }
    }

    fn schedule_preview_close(&mut self, cx: &mut Context<Self>) {
        self.collapsed_ui.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CLOSE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                let ui = &this.collapsed_ui;
                if ui.hovered.is_none() && !ui.over_preview {
                    this.close_preview(cx);
                }
            });
        }));
    }

    fn show_preview(&mut self, id: String, cx: &mut Context<Self>) {
        let ui = &mut self.collapsed_ui;
        ui.timer = None;
        ui.preview = Some(id);
        if ui.refresh.is_none() {
            ui.refresh = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(REFRESH).await;
                    let open = this
                        .update(cx, |this, cx| {
                            cx.notify();
                            this.collapsed_ui.preview.is_some()
                        })
                        .unwrap_or(false);
                    if !open {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// Drop the preview (also on switching workspace, collapsing or expanding).
    pub(crate) fn close_preview(&mut self, cx: &mut Context<Self>) {
        let ui = &mut self.collapsed_ui;
        ui.hovered = None;
        ui.over_preview = false;
        ui.timer = None;
        ui.refresh = None;
        if ui.preview.take().is_some() {
            cx.notify();
        }
    }

    /// The open preview: the pane's name and folder, its last lines, and
    /// buttons to expand or close it.
    pub(crate) fn render_collapsed_preview(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.collapsed_ui.preview.clone()?;
        let at = self.collapsed_ui.anchor()?;
        let ws = self.state.workspaces.iter().find(|w| w.is_collapsed(&id))?;
        let (name, color) = self.collapsed_label(ws, &id);
        let pane = self.panes.get(&id);
        let cwd = pane
            .and_then(|p| p.process.as_ref())
            .and_then(|p| p.cwd.clone())
            .or_else(|| ws.container(&id).and_then(|c| c.content.cwd.clone()))
            .map(|p| tilde(&p))
            .unwrap_or_default();
        // The whole screen, scaled down until its width and height fit (within
        // 6–12pt): a preview of the pane, not just its last lines.
        let (cols, lines) = pane.map(|p| p.view.read(cx).screen_rows()).unwrap_or_default();
        let empty = lines.is_empty();
        let max_h = (f32::from(window.viewport_size().height) * 0.6).clamp(160., 560.);
        let fit_w = (PREVIEW_W - 2. * PREVIEW_PAD) / (cols.max(20) as f32 * CHAR_W);
        let fit_h = max_h / (lines.len().max(1) as f32 * LINE_H);
        let font = fit_w.min(fit_h).clamp(6., 12.);
        let line_h = font * LINE_H;
        let fonts = fonts();
        let (e_id, x_id) = (id.clone(), id.clone());
        let button = |key: &str, label: &'static str| {
            div()
                .id(SharedString::from(format!("{key}-{id}")))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(4.))
                .text_size(px(11.))
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(label)
        };
        Some(
            deferred(
                anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("collapsed-preview")
                        .child(crate::overlay::recorder(at, 0))
                        .w(px(PREVIEW_W))
                        .flex()
                        .flex_col()
                        .bg(bg_surface())
                        .border_1()
                        .border_color(border())
                        .rounded(px(8.))
                        .shadow(vec![BoxShadow {
                            color: c(hexa(0x000000, 0.45)),
                            offset: point(px(0.), px(8.)),
                            blur_radius: px(24.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                        .overflow_hidden()
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            this.collapsed_ui.over_preview = *hovered;
                            if !*hovered {
                                this.schedule_preview_close(cx);
                            }
                        }))
                        .child(
                            div()
                                .h(px(30.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .border_b_1()
                                .border_color(border())
                                .child(div().flex_none().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(name))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(10.)).text_color(TEXT_MUTED).child(cwd))
                                .child(button("preview-expand", "Expand").text_color(TEXT_PRIMARY).on_click(cx.listener(move |this, _, _, cx| {
                                    this.close_preview(cx);
                                    this.expand_pane(&e_id, cx);
                                })))
                                .child(button("preview-close", "Close").text_color(ACCENT_RED).on_click(cx.listener(move |this, _, _, cx| {
                                    this.close_preview(cx);
                                    this.close_pane(&x_id, cx);
                                }))),
                        )
                        .child(
                            div()
                                .px(px(PREVIEW_PAD))
                                .py(px(8.))
                                .max_h(px(max_h + 16.))
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .justify_end()
                                .bg(bg_primary())
                                .font_family(fonts.terminal.clone())
                                .text_size(px(font))
                                .line_height(px(line_h))
                                .text_color(TEXT_PRIMARY)
                                .when(empty, |d| d.text_color(TEXT_MUTED).child("No output yet"))
                                .children(lines.into_iter().map(|l| div().flex_none().h(px(line_h)).overflow_hidden().whitespace_nowrap().child(if l.is_empty() { " ".to_owned() } else { l }))),
                        ),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
