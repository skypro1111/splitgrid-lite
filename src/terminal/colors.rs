//! Terminal colours → gpui colours, with SplitGrid's xterm theme
//! (renderer/components/XtermTerminal.tsx + shared/background-theme.ts at the
//! default base #141414).
//!
//! alacritty_terminal keeps only the colours a program set (OSC 4/10/11); the
//! defaults are ours. Two of them carry alpha in the xterm theme (foreground
//! and bright black), so the theme is kept as RGBA.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui_kit::{Hsla, Rgba};

const fn rgba(hex: u32, a: u8) -> (u32, u8) {
    (hex, a)
}

/// black, red, green, yellow, blue, magenta, cyan, white; then the bright ones.
const BASE16: [(u32, u8); 16] = [
    rgba(0x242424, 0xff),
    rgba(0xfc6b83, 0xff),
    rgba(0x3fa266, 0xff),
    rgba(0xd2943e, 0xff),
    rgba(0x81a1c1, 0xff),
    rgba(0xb48ead, 0xff),
    rgba(0x88c0d0, 0xff),
    rgba(0xe4e4e4, 0xff),
    rgba(0xe4e4e4, 0x42),
    rgba(0xfc6b83, 0xff),
    rgba(0x70b489, 0xff),
    rgba(0xf1b467, 0xff),
    rgba(0x87a6c4, 0xff),
    rgba(0xb48ead, 0xff),
    rgba(0x88c0d0, 0xff),
    rgba(0xe4e4e4, 0xff),
];
const FOREGROUND: (u32, u8) = rgba(0xe4e4e4, 0xeb);
/// The default base; the live one follows the app background (Settings → Appearance).
#[cfg(test)]
pub const BACKGROUND: u32 = 0x141414;

fn rgb_u32(c: Rgba) -> u32 {
    let b = |v: f32| (v * 255.).round() as u32;
    (b(c.r) << 16) | (b(c.g) << 8) | b(c.b)
}

/// The terminal background: the app background (Electron's xterm theme follows it too).
pub fn background() -> u32 {
    rgb_u32(crate::theme::palette().primary)
}

/// ANSI black: a hair above the background.
fn black() -> u32 {
    rgb_u32(crate::theme::palette().terminal_black)
}
const CURSOR: (u32, u8) = rgba(0xffffff, 0xff);

fn hex(c: u32) -> Rgb {
    Rgb { r: (c >> 16) as u8, g: (c >> 8) as u8, b: c as u8 }
}

/// xterm's 256-colour palette beyond the base 16: the 6×6×6 cube, then 24 greys.
pub fn indexed(i: u8) -> Rgb {
    match i {
        0 => hex(black()),
        1..=15 => hex(BASE16[i as usize].0),
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) }
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
    }
}

fn named_default(n: NamedColor) -> (u32, u8) {
    use NamedColor::*;
    match n {
        Foreground | BrightForeground => FOREGROUND,
        Background => (background(), 0xff),
        Black => (black(), 0xff),
        Cursor => CURSOR,
        DimForeground => (FOREGROUND.0, 0xa0),
        DimBlack | DimRed | DimGreen | DimYellow | DimBlue | DimMagenta | DimCyan | DimWhite => {
            (BASE16[n as usize - DimBlack as usize].0, 0xa8)
        }
        _ => BASE16[(n as usize).min(15)],
    }
}

/// Opaque colour (for replies to colour queries and background comparisons).
pub fn resolve(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Named(n) => colors[n].unwrap_or_else(|| hex(named_default(n).0)),
        Color::Indexed(i) => colors[i as usize].unwrap_or_else(|| indexed(i)),
    }
}

/// The colour to paint, theme alpha included. `bold` brightens the eight
/// normal colours, as xterm's `drawBoldTextInBrightColors` does.
pub fn paint(color: Color, colors: &Colors, bold: bool) -> Hsla {
    let color = match color {
        Color::Named(n) if bold && (n as usize) < 8 => Color::Indexed(n as u8 + 8),
        Color::Indexed(i) if bold && i < 8 => Color::Indexed(i + 8),
        other => other,
    };
    let (rgb, alpha) = match color {
        Color::Spec(rgb) => (rgb, 0xff),
        Color::Named(n) => match colors[n] {
            Some(rgb) => (rgb, 0xff),
            None => {
                let (h, a) = named_default(n);
                (hex(h), a)
            }
        },
        Color::Indexed(i) => match colors[i as usize] {
            Some(rgb) => (rgb, 0xff),
            None if i < 16 => (hex(BASE16[i as usize].0), BASE16[i as usize].1),
            None => (indexed(i), 0xff),
        },
    };
    let mut h = hsla(rgb);
    h.a = alpha as f32 / 255.;
    h
}

pub fn hsla(c: Rgb) -> Hsla {
    Rgba { r: c.r as f32 / 255., g: c.g as f32 / 255., b: c.b as f32 / 255., a: 1. }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_cube_and_greys() {
        assert_eq!(indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(indexed(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(indexed(255), Rgb { r: 238, g: 238, b: 238 });
    }

    #[test]
    fn program_set_colours_win() {
        let mut colors = Colors::default();
        assert_eq!(resolve(Color::Named(NamedColor::Background), &colors), hex(BACKGROUND));
        colors[NamedColor::Background] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(resolve(Color::Named(NamedColor::Background), &colors), Rgb { r: 1, g: 2, b: 3 });
    }

    #[test]
    fn theme_alpha_and_bold_brightening() {
        let colors = Colors::default();
        let fg = paint(Color::Named(NamedColor::Foreground), &colors, false);
        assert!((fg.a - 0xeb as f32 / 255.).abs() < 1e-3, "foreground is slightly translucent");
        let green = paint(Color::Named(NamedColor::Green), &colors, true);
        assert_eq!(green, paint(Color::Named(NamedColor::BrightGreen), &colors, false), "bold → bright");
        let bright_black = paint(Color::Indexed(8), &colors, false);
        assert!(bright_black.a < 0.3);
    }
}
