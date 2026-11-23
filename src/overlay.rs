//! Where the app's popovers and context menus are drawn, so browser panes (web
//! views drawn above everything gpui paints) hide only where one covers them.
//!
//! A menu adds `recorder(at, slot)` as a child of its outermost box; once it
//! has painted, its bounds are kept here under the point it was opened at.
//! Whoever owns the menu reports that point while it is open (`Cover`), so a
//! closed menu's bounds are never used and need no clearing.

use std::collections::HashMap;

use gpui_kit::*;

/// Bounds painted last, by (opened-at point, part of the menu).
#[derive(Default)]
struct Painted(HashMap<(Point<Pixels>, u8), Bounds<Pixels>>);

impl Global for Painted {}

/// An open popover or menu: the point it was opened at, which of its parts
/// (a flyout is a separate part), and what it may cover before it has painted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Cover {
    pub at: Point<Pixels>,
    pub slot: u8,
    pub guess: Bounds<Pixels>,
}

impl Cover {
    /// A context menu: until it paints, assume only the clicked point.
    pub fn menu(at: Point<Pixels>) -> Self {
        Cover { at, slot: 0, guess: Bounds { origin: at, size: size(px(1.), px(1.)) } }
    }
}

/// A child of a menu's outermost box (as last child, absolutely placed) that
/// records where the menu ended up and redraws when that changed.
pub(crate) fn recorder(at: Point<Pixels>, slot: u8) -> impl IntoElement {
    canvas(
        move |b, window, cx| {
            let prev = cx.default_global::<Painted>().0.insert((at, slot), b);
            if prev != Some(b) {
                // Browser panes it doesn't cover may show (next frame).
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The areas `open` menus cover; bounds of menus no longer open are dropped.
pub(crate) fn covered(open: &[Cover], cx: &mut App) -> Vec<Bounds<Pixels>> {
    let painted = &mut cx.default_global::<Painted>().0;
    painted.retain(|k, _| open.iter().any(|c| (c.at, c.slot) == *k));
    open.iter().map(|c| painted.get(&(c.at, c.slot)).copied().unwrap_or(c.guess)).collect()
}
