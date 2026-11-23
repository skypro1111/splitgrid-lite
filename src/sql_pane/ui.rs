//! The SQL workbench's small pieces, in the Electron app's look (.sql-btn,
//! .sql-modal, the selector strip): buttons, fields, a modal card.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::theme::*;

pub const MONO: &str = "Menlo";
/// The selector strip and the results toolbar (.sql-selector-strip).
pub const BAR_H: f32 = 34.;
pub const BG_BAR: Rgba = hex(0x171717);

/// A bundled Lucide icon (only the default set ships in the binary).
pub fn lucide(name: &str, size: f32, color: Rgba) -> Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(color)
}

/// .sql-btn: a bordered text button, optionally with an icon; `primary` fills it with the accent.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.))
        .px(px(10.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .rounded(px(5.))
        .border_1()
        .border_color(border())
        .bg(bg_surface())
        .text_size(px(11.))
        .text_color(TEXT_PRIMARY)
        .cursor_pointer()
        .hover(|d| d.bg(bg_hover()))
        .child(label.into())
}

pub fn primary(b: Stateful<Div>) -> Stateful<Div> {
    b.bg(ACCENT).border_color(ACCENT).text_color(hex(0xffffff)).hover(|d| d.opacity(0.9))
}

pub fn disabled(b: Stateful<Div>, off: bool) -> Stateful<Div> {
    b.when(off, |d| d.opacity(0.4).cursor_default())
}

/// A square toolbar button with an icon (and a tooltip-ish title on hover via `title`).
pub fn tool(id: impl Into<ElementId>, icon: &str, color: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .hover(|d| d.bg(bg_hover()))
        .child(lucide(icon, 14., color))
}

/// A labelled toolbar button: icon + text, borderless (DataGrip's toolbar).
pub fn tool_text(id: impl Into<ElementId>, icon: &str, color: Rgba, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.))
        .px(px(7.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(TEXT_PRIMARY)
        .hover(|d| d.bg(bg_hover()))
        .child(lucide(icon, 13., color))
        .child(label.into())
}

/// An input in the modal's style: dark well, accent border when focused.
pub fn field(state: &Entity<InputState>, mono: bool, window: &Window, cx: &App) -> Div {
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    div()
        .w_full()
        .px(px(8.))
        .py(px(3.))
        .rounded(px(5.))
        .bg(bg_input())
        .border_1()
        .border_color(if focused { ACCENT } else { border() })
        .text_size(px(12.))
        .when(mono, |d| d.font_family(MONO))
        .child(Input::new(state).appearance(false).small())
}

pub fn label(text: impl Into<SharedString>) -> Div {
    div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_MUTED).child(text.into().to_uppercase())
}

/// The dim backdrop and the card of a modal over the pane (.sql-modal).
pub fn modal(id: &'static str, width: f32, body: impl IntoElement) -> Div {
    div().absolute().inset_0().flex().items_center().justify_center().bg(hexa(0, 0.45)).child(
        div()
            .id(id)
            .occlude()
            .w(px(width))
            .max_w(relative(0.94))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(10.))
            .bg(bg_surface())
            .border_1()
            .border_color(border())
            .shadow(vec![BoxShadow { color: c(hexa(0, 0.5)), offset: point(px(0.), px(12.)), blur_radius: px(32.), spread_radius: px(0.), inset: false }])
            .text_color(TEXT_PRIMARY)
            .child(body),
    )
}

pub fn modal_title(text: impl Into<SharedString>) -> Div {
    div().text_size(px(13.)).font_weight(FontWeight::BOLD).child(text.into())
}

/// A context menu at a point, closed by a click outside.
pub fn popup(id: &'static str, at: Point<Pixels>, items: Vec<AnyElement>, on_close: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    deferred(
        anchored().position(at).snap_to_window_with_margin(px(4.)).child(
            div()
                .id(id)
                .child(crate::overlay::recorder(at, 0))
                .min_w(px(180.))
                .max_h(px(360.))
                .overflow_y_scroll()
                .py(px(4.))
                .flex()
                .flex_col()
                .bg(bg_surface())
                .border_1()
                .border_color(border())
                .rounded(px(6.))
                .shadow(vec![BoxShadow { color: c(hexa(0, 0.4)), offset: point(px(0.), px(8.)), blur_radius: px(24.), spread_radius: px(0.), inset: false }])
                .occlude()
                .on_mouse_down_out(move |_, window, cx| on_close(window, cx))
                .children(items),
        ),
    )
    .with_priority(1)
    .into_any_element()
}

pub fn menu_item(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .mx(px(4.))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(TEXT_PRIMARY)
        .cursor_pointer()
        .hover(|d| d.bg(bg_hover()))
        .child(text.into())
}

pub fn menu_sep() -> Div {
    div().my(px(4.)).h(px(1.)).bg(border())
}

/// Dialect badge and colour (ConnectionDialog's DIALECT_BADGE / brand colours).
pub fn dialect_badge(dialect: &str) -> (&'static str, Rgba) {
    match dialect {
        "postgres" => ("PG", hex(0x4169e1)),
        "mysql" => ("MY", hex(0x4479a1)),
        "mariadb" => ("MR", hex(0xc0765a)),
        "sqlite" => ("LT", hex(0x4fc1ff)),
        "mssql" => ("MS", hex(0xcc2927)),
        _ => ("?", TEXT_MUTED),
    }
}

pub fn badge(dialect: &str) -> Div {
    let (text, color) = dialect_badge(dialect);
    div()
        .w(px(22.))
        .h(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .bg(color)
        .text_size(px(8.))
        .font_weight(FontWeight::BOLD)
        .text_color(hex(0xffffff))
        .child(text)
}

/// HH:MM:SS, local time, for the Output log.
pub fn clock() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as libc::time_t).unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes the struct we own.
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// "1.2 s", "34 ms".
pub fn elapsed(ms: u128) -> String {
    if ms >= 1000 { format!("{:.1} s", ms as f64 / 1000.) } else { format!("{ms} ms") }
}

/// 1234567 → "1,234,567".
pub fn count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats() {
        assert_eq!(super::count(0), "0");
        assert_eq!(super::count(999), "999");
        assert_eq!(super::count(100000), "100,000");
        assert_eq!(super::count(1234567), "1,234,567");
        assert_eq!(super::elapsed(34), "34 ms");
        assert_eq!(super::elapsed(1260), "1.3 s");
        assert_eq!(super::clock().len(), 8);
    }
}
