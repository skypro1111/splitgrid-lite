//! ⌘-clicked links from a terminal (crate::terminal::links): an image opens in
//! a viewer over the window, so screenshots an agent saves can be looked at
//! without leaving the app; URLs and other files go to their default app.

use std::path::PathBuf;

use gpui_kit::*;

use crate::app::AppView;
use crate::terminal::links::{Target, is_image};
use crate::theme::*;
use crate::ui::px;

pub(crate) struct ImagePreview {
    path: PathBuf,
    focus: FocusHandle,
}

impl AppView {
    pub(crate) fn open_link(&mut self, target: Target, cx: &mut Context<Self>) {
        match target {
            Target::Url(url) => cx.open_url(&url),
            Target::File(path) if is_image(&path) => {
                self.image_preview = Some(ImagePreview { path, focus: cx.focus_handle() });
                cx.notify();
            }
            Target::File(path) if path.exists() => cx.open_with_system(&path),
            Target::File(path) => log::warn!("link to a missing file: {}", path.display()),
        }
    }

    pub(crate) fn close_image_preview(&mut self, cx: &mut Context<Self>) {
        if self.image_preview.take().is_some() {
            self.focus_pending_now();
            cx.notify();
        }
    }

    /// The viewer: the image fitted to the window over a dark backdrop. Esc or a
    /// click outside the image closes it.
    pub(crate) fn render_image_preview(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.image_preview.as_ref()?;
        // Take the keyboard so Esc reaches the viewer, not the pane beneath.
        if !preview.focus.is_focused(window) {
            preview.focus.focus(window, cx);
        }
        let path = preview.path.clone();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = path.parent().map(|p| crate::grid::tilde(&p.to_string_lossy())).unwrap_or_default();
        let (open_path, reveal_path) = (path.clone(), path.clone());
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(4.))
                .rounded(px(6.))
                .text_size(px(12.))
                .text_color(TEXT_MUTED)
                .cursor_pointer()
                .hover(|d| d.bg(hexa(0xffffff, 0.08)).text_color(TEXT_PRIMARY))
                .child(label)
        };

        Some(
            div()
                .id("image-preview")
                .track_focus(&preview.focus)
                .key_context("ImagePreview")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .bg(hexa(0x000000, 0.85))
                .occlude()
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" || ev.keystroke.key == "space" {
                        this.close_image_preview(cx);
                        cx.stop_propagation();
                    }
                }))
                .on_click(cx.listener(|this, _, _, cx| this.close_image_preview(cx)))
                .child(
                    div()
                        .flex_none()
                        .id("image-preview-bar")
                        .h(px(40.))
                        .px(px(16.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(div().flex_none().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_PRIMARY).child(name))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(dir))
                        .child(button("image-open", "Open in Preview").on_click(cx.listener(move |this, _, _, cx| {
                            cx.open_with_system(&open_path);
                            this.close_image_preview(cx);
                        })))
                        .child(button("image-reveal", "Show in Finder").on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&reveal_path))))
                        .child(button("image-close", "Close  esc").on_click(cx.listener(|this, _, _, cx| this.close_image_preview(cx)))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p(px(24.))
                        .pt(px(0.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            img(path)
                                .size_full()
                                .object_fit(ObjectFit::ScaleDown)
                                .with_fallback(|| {
                                    div()
                                        .size_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_size(px(13.))
                                        .text_color(TEXT_MUTED)
                                        .child("Can't show this image: the file is missing or unreadable")
                                        .into_any_element()
                                }),
                        ),
                )
                .into_any_element(),
        )
    }
}
