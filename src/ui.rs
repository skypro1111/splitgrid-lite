//! Whole-window UI zoom (the Electron app's ⌘+ / ⌘− / ⌘0 page zoom).
//!
//! gpui has no page zoom, so SplitGrid scales its own layout: UI modules
//! import [`px`] from here instead of gpui's, and it multiplies by the current
//! factor. gpui-component's widgets are rem-based and follow the theme's font
//! size, which [`apply`] scales too.
//!
//! The rule for geometry code: constants, app state and anything persisted
//! (sidebar width, column widths, scroll offsets) stay in *unscaled* units.
//! [`px`] scales on the way out to layout; [`unpx`] unscales layout bounds and
//! mouse positions on the way in. Pixels compared with Pixels need neither.
//!
//! One factor for the whole app (Chromium keeps one zoom per origin, so the
//! Electron windows share it too), persisted as a zoom *level* in
//! `native-settings.json`.

use std::cell::Cell;

use gpui_kit::{App, Pixels};

/// One ⌘+ / ⌘− step, in Chromium zoom levels (WorkspaceGrid.tsx's applyWindowZoom).
pub const LEVEL_STEP: f32 = 0.5;
/// The Electron app stops zooming out at −3 (≈58%).
pub const MIN_LEVEL: f32 = -3.;
/// Chromium tops out at 500%; past ~300% the UI is unusable, so stop there.
pub const MAX_LEVEL: f32 = 6.;

/// gpui-component's base sizes at 100% (theme.rs).
pub const BASE_FONT_SIZE: f32 = 12.;
pub const BASE_MONO_FONT_SIZE: f32 = 13.;
pub const BASE_RADIUS: f32 = 6.;

thread_local! {
    static ZOOM: Cell<(f32, f32)> = const { Cell::new((0., 1.)) };
}

/// Chromium's level → factor: each level is 20% more.
pub fn factor(level: f32) -> f32 {
    1.2f32.powf(level)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    In,
    Out,
    Reset,
}

/// The level after one step from `level`, kept on the 0.5 grid and in range.
pub fn step(level: f32, step: Step) -> f32 {
    let snapped = (level / LEVEL_STEP).round() * LEVEL_STEP;
    match step {
        Step::In => (snapped + LEVEL_STEP).min(MAX_LEVEL),
        Step::Out => (snapped - LEVEL_STEP).max(MIN_LEVEL),
        Step::Reset => 0.,
    }
}

/// A persisted level made safe to use (a hand-edited file may hold anything).
pub fn sanitize(level: f32) -> f32 {
    if level.is_finite() { level.clamp(MIN_LEVEL, MAX_LEVEL) } else { 0. }
}

/// The current zoom level.
pub fn level() -> f32 {
    ZOOM.with(|z| z.get().0)
}

/// The current zoom factor (1.0 = 100%).
pub fn zoom() -> f32 {
    ZOOM.with(|z| z.get().1)
}

/// Layout pixels: `v` unscaled units at the current zoom. Shadows gpui's `px`
/// in UI modules (`use crate::ui::px;` wins over the `gpui_kit::*` glob).
#[inline]
pub fn px(v: f32) -> Pixels {
    gpui_kit::px(v * zoom())
}

/// Layout pixels (bounds, mouse positions) back to unscaled units.
#[inline]
pub fn unpx(p: Pixels) -> f32 {
    f32::from(p) / zoom()
}

fn set_level(level: f32) {
    let level = sanitize(level);
    ZOOM.with(|z| z.set((level, factor(level))));
}

/// Switch the whole UI to `level`: the factor [`px`] uses, gpui-component's
/// theme sizes (its rem size follows the theme font size), and a redraw of
/// every window. Doesn't persist; callers do.
pub fn apply(level: f32, cx: &mut App) {
    set_level(level);
    let z = zoom();
    let t = gpui_kit::component::Theme::global_mut(cx);
    t.font_size = gpui_kit::px(BASE_FONT_SIZE * z);
    t.mono_font_size = gpui_kit::px(BASE_MONO_FONT_SIZE * z);
    t.radius = gpui_kit::px(BASE_RADIUS * z);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_chromium_levels() {
        assert_eq!(step(0., Step::In), 0.5);
        assert_eq!(step(0.5, Step::Out), 0.);
        assert_eq!(step(2.5, Step::Reset), 0.);
        assert!((factor(0.) - 1.).abs() < 1e-6);
        assert!((factor(1.) - 1.2).abs() < 1e-6);
        assert!((factor(-1.) - 1. / 1.2).abs() < 1e-6);
        // Off-grid values snap before stepping.
        assert_eq!(step(0.3, Step::In), 1.);
    }

    #[test]
    fn steps_stop_at_the_ends() {
        let mut l = 0.;
        for _ in 0..40 {
            l = step(l, Step::Out);
        }
        assert_eq!(l, MIN_LEVEL);
        for _ in 0..40 {
            l = step(l, Step::In);
        }
        assert_eq!(l, MAX_LEVEL);
        assert!(factor(MAX_LEVEL) < 3.01);
    }

    #[test]
    fn sanitize_and_scale() {
        assert_eq!(sanitize(f32::NAN), 0.);
        assert_eq!(sanitize(100.), MAX_LEVEL);
        assert_eq!(sanitize(-100.), MIN_LEVEL);
        set_level(2.);
        assert!((zoom() - 1.44).abs() < 1e-5);
        assert!((f32::from(px(10.)) - 14.4).abs() < 1e-4);
        assert!((unpx(px(10.)) - 10.).abs() < 1e-4);
        set_level(0.);
        assert_eq!(f32::from(px(10.)), 10.);
    }
}
