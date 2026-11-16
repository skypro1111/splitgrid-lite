//! The app background colour (the Electron app's shared/background-theme.ts).
//!
//! The user picks one base colour (`backgroundColor` in app-settings.json,
//! shared with the Electron app); every other surface (panels, inputs, hovers,
//! borders, the editor and terminal backgrounds) is derived from it by lifting
//! lightness in fixed steps with the base's hue and saturation. The UI is dark
//! only, so a picked colour is clamped into a dark, muted range rather than
//! refused. The default base reproduces the hard-coded palette exactly.

use gpui_kit::Rgba;

use crate::theme::{Palette, hex};

pub const DEFAULT_COLOR: &str = "#141414";

const MIN_LIGHTNESS: f32 = 0.02;
const MAX_LIGHTNESS: f32 = 0.24;
const MAX_SATURATION: f32 = 0.5;

// Lightness added to the base per surface: the deltas between #141414 and global.css.
const EDITOR: f32 = 0.016;
const SURFACE: f32 = 0.024;
const INPUT: f32 = 0.04;
const HOVER: f32 = 0.071;
const BORDER: f32 = 0.142;
const TERMINAL_BLACK: f32 = 0.062;

/// The curated bases (BACKGROUND_PRESETS): neutral first, then subtle tints.
pub const PRESETS: [(&str, &str); 7] = [
    ("Graphite", DEFAULT_COLOR),
    ("Ink", "#0b0b0b"),
    ("Slate", "#15181d"),
    ("Midnight", "#101720"),
    ("Forest", "#0f1a16"),
    ("Plum", "#16121f"),
    ("Cocoa", "#191411"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Hsl {
    h: f32,
    s: f32,
    l: f32,
}

/// `#rgb` / `#rrggbb`, the `#` optional.
fn parse_hex(input: &str) -> Option<(u8, u8, u8)> {
    let h = input.trim().trim_start_matches('#');
    if h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        3 => {
            let d: Vec<String> = h.chars().map(|c| format!("{c}{c}")).collect();
            Some((byte(&d[0])?, byte(&d[1])?, byte(&d[2])?))
        }
        6 => Some((byte(&h[0..2])?, byte(&h[2..4])?, byte(&h[4..6])?)),
        _ => None,
    }
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> Hsl {
    let (r, g, b) = (r as f32 / 255., g as f32 / 255., b as f32 / 255.);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    let d = max - min;
    if d == 0. {
        return Hsl { h: 0., s: 0., l };
    }
    let s = d / (1. - (2. * l - 1.).abs());
    let mut h = if max == r {
        ((g - b) / d) % 6.
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    };
    h *= 60.;
    if h < 0. {
        h += 360.;
    }
    Hsl { h, s, l }
}

fn hsl_to_rgb(Hsl { h, s, l }: Hsl) -> (u8, u8, u8) {
    let c = (1. - (2. * l - 1.).abs()) * s;
    let x = c * (1. - ((h / 60.) % 2. - 1.).abs());
    let m = l - c / 2.;
    let (r, g, b) = match h {
        h if h < 60. => (c, x, 0.),
        h if h < 120. => (x, c, 0.),
        h if h < 180. => (0., c, x),
        h if h < 240. => (0., x, c),
        h if h < 300. => (x, 0., c),
        _ => (c, 0., x),
    };
    let byte = |v: f32| ((v + m) * 255.).round().clamp(0., 255.) as u8;
    (byte(r), byte(g), byte(b))
}

fn to_hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// A typed colour, clamped into the supported dark range; None when it isn't a colour.
pub fn parse(input: &str) -> Option<String> {
    let (r, g, b) = parse_hex(input)?;
    let hsl = rgb_to_hsl(r, g, b);
    Some(to_hex(hsl_to_rgb(Hsl { h: hsl.h, s: hsl.s.clamp(0., MAX_SATURATION), l: hsl.l.clamp(MIN_LIGHTNESS, MAX_LIGHTNESS) })))
}

/// `parse`, falling back to the default base.
pub fn normalize(input: Option<&str>) -> String {
    input.and_then(parse).unwrap_or_else(|| DEFAULT_COLOR.to_owned())
}

fn rgba((r, g, b): (u8, u8, u8)) -> Rgba {
    hex(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

/// The whole surface ramp from one base colour.
pub fn derive(input: Option<&str>) -> Palette {
    let base = normalize(input);
    let (r, g, b) = parse_hex(&base).expect("normalized colour");
    let hsl = rgb_to_hsl(r, g, b);
    let lift = |delta: f32| rgba(hsl_to_rgb(Hsl { l: (hsl.l + delta).clamp(0., 1.), ..hsl }));
    Palette {
        primary: rgba((r, g, b)),
        surface: lift(SURFACE),
        input: lift(INPUT),
        hover: lift(HOVER),
        editor: lift(EDITOR),
        border: lift(BORDER),
        terminal_black: lift(TERMINAL_BLACK),
    }
}

/// The base colour the settings name, as `#rrggbb`.
pub fn from_settings(s: &serde_json::Map<String, serde_json::Value>) -> String {
    normalize(s.get("backgroundColor").and_then(serde_json::Value::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_base_reproduces_the_fixed_palette() {
        let p = derive(None);
        assert_eq!(p.primary, hex(0x141414));
        assert_eq!(p.surface, hex(0x1a1a1a));
        assert_eq!(p.input, hex(0x1e1e1e));
        assert_eq!(p.hover, hex(0x262626));
        assert_eq!(p.editor, hex(0x181818));
        assert_eq!(p.border, hex(0x383838));
    }

    #[test]
    fn colours_are_clamped_into_a_dark_range() {
        assert_eq!(parse("#ffffff").as_deref(), Some("#3d3d3d"));
        assert_eq!(parse("000").as_deref(), Some("#050505"));
        assert_eq!(parse("#101720").as_deref(), Some("#101720"));
        assert_eq!(parse("red"), None);
        assert_eq!(parse("#12345"), None);
        assert_eq!(normalize(Some("nope")), DEFAULT_COLOR);
        // Every preset is already in range.
        for (_, c) in PRESETS {
            assert_eq!(parse(c).as_deref(), Some(c));
        }
    }

    #[test]
    fn a_tint_keeps_its_hue_across_the_ramp() {
        let p = derive(Some("#101720"));
        assert!(p.primary.b > p.primary.r && p.border.b > p.border.r);
        assert!(p.border.b > p.surface.b && p.surface.b > p.primary.b);
    }
}

#[cfg(test)]
mod electron_parity {
    use super::*;

    fn css(c: Rgba) -> String {
        let b = |v: f32| (v * 255.).round() as u8;
        format!("#{:02x}{:02x}{:02x}", b(c.r), b(c.g), b(c.b))
    }

    /// Values from the Electron app's deriveBackgroundTheme for the same inputs.
    #[test]
    fn derives_what_the_electron_app_derives() {
        let cases = [
            ("#ffffff", "#3d3d3d", "#434343", "#4f4f4f", "#616161", "#414141", "#4d4d4d"),
            ("000", "#050505", "#0b0b0b", "#171717", "#292929", "#090909", "#151515"),
            ("#101720", "#101720", "#141d28", "#1c2838", "#283a50", "#131b25", "#1b2635"),
            ("#0f1a16", "#0f1a16", "#13221d", "#1c3129", "#29483d", "#121f1a", "#1b2e27"),
            ("#ff0000", "#5c1f1f", "#652222", "#772828", "#923131", "#622121", "#742727"),
            ("#2a3f5f", "#263855", "#2a3e5d", "#31486e", "#3c5987", "#293c5b", "#30466b"),
        ];
        for (input, base, surface, hover, border, editor, black) in cases {
            assert_eq!(parse(input).as_deref(), Some(base), "{input}");
            let p = derive(Some(input));
            assert_eq!(
                [css(p.primary), css(p.surface), css(p.hover), css(p.border), css(p.editor), css(p.terminal_black)],
                [base, surface, hover, border, editor, black],
                "{input}"
            );
        }
    }
}
