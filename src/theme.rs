//! SplitGrid's look: the palette of the Electron app (renderer/styles/global.css)
//! and the few composite values its components use inline. Everything visual
//! reads from here so the two apps stay identical.

// The full palette, mirrored even where this build doesn't draw a colour yet.
#![allow(dead_code)]

use gpui_kit::{Hsla, Rgba};

/// `#rrggbb`.
pub const fn hex(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a: 1.,
    }
}

/// `#rrggbb` with an alpha 0–1.
pub const fn hexa(hex: u32, a: f32) -> Rgba {
    let mut c = self::hex(hex);
    c.a = a;
    c
}

pub fn c(v: Rgba) -> Hsla {
    v.into()
}

// --- global.css :root ---
pub const BG_PRIMARY: Rgba = hex(0x141414);
pub const BG_SURFACE: Rgba = hex(0x1a1a1a);
pub const BG_INPUT: Rgba = hex(0x1e1e1e);
pub const BG_HOVER: Rgba = hex(0x262626);
/// The editor surface (--bg-editor).
pub const EDITOR_BG: Rgba = hex(0x181818);
pub const TEXT_PRIMARY: Rgba = hex(0xd4d4d4);
pub const TEXT_MUTED: Rgba = hex(0x7a797a);
pub const ACCENT: Rgba = hex(0x228df2);
pub const ACCENT_GREEN: Rgba = hex(0x15ac91);
pub const ACCENT_RED: Rgba = hex(0xf14c4c);
pub const ACCENT_YELLOW: Rgba = hex(0xe5b95c);
pub const ACCENT_BLUE: Rgba = hex(0x4c9df3);
pub const ACCENT_PURPLE: Rgba = hex(0x5b51ec);
/// `var(--accent-amber, #e0a23a)`: the variable is never defined, the fallback renders.
pub const ACCENT_AMBER: Rgba = hex(0xe0a23a);
pub const BORDER: Rgba = hex(0x383838);
pub const BORDER_SUBTLE: Rgba = hexa(0xffffff, 0.06);

/// Text on green/amber badges and the done checkbox.
pub const ON_BADGE: Rgba = hex(0x0b1f1a);

// --- Panes (global.css .container-focused / .terminal-dim-overlay) ---
/// Brighter than the CSS 0.55: there is no outer glow, so the line alone has to carry it.
pub const FOCUS_RING: Rgba = hexa(0x359dff, 0.85);
pub const FOCUS_GLOW: Rgba = hexa(0x359dff, 0.3);
pub const UNFOCUSED_DIM: Rgba = hexa(0x000000, 0.24);

// Pane header status dot (XtermTerminal.tsx).
pub const DOT_CONNECTED: Rgba = hex(0x3fa266);
pub const DOT_CONNECTING: Rgba = hex(0xd2943e);
pub const DOT_ERROR: Rgba = hex(0xfc6b83);

/// SF Pro is what `-apple-system` resolves to.
pub const UI_FONT: &str = ".SystemUIFont";

/// Point gpui-component's theme (inputs, textareas, menus) at the same palette.
pub fn apply_to_components(cx: &mut gpui_kit::App) {
    use gpui_kit::component::{Theme, ThemeMode};
    Theme::change(ThemeMode::Dark, None, cx);
    let t = Theme::global_mut(cx);
    t.font_family = UI_FONT.into();
    t.font_size = gpui_kit::px(12.);
    t.radius = gpui_kit::px(6.);
    t.background = c(BG_PRIMARY);
    t.foreground = c(TEXT_PRIMARY);
    t.muted = c(BG_HOVER);
    t.muted_foreground = c(TEXT_MUTED);
    t.border = c(BORDER);
    t.input = c(BORDER);
    t.ring = c(ACCENT);
    t.primary = c(ACCENT);
    t.caret = c(TEXT_PRIMARY);
    t.selection = c(hexa(0xffffff, 0.15));
    t.popover = c(BG_SURFACE);
    t.popover_foreground = c(TEXT_PRIMARY);
    t.accent = c(BG_HOVER);
    t.accent_foreground = c(TEXT_PRIMARY);
    t.secondary = c(BG_HOVER);
    t.list_hover = c(BG_HOVER);
    t.mono_font_family = crate::ide::EDITOR_FONT.into();
    t.highlight_theme = std::sync::Arc::new(editor_theme());
}

/// The editor's colours: SplitGrid's Monaco theme (`splitgrid-dark`, on vs-dark,
/// monacoTheme.ts) for the surfaces, vs-dark's token colours for the syntax.
fn editor_theme() -> gpui_kit::component::highlighter::HighlightTheme {
    const JSON: &str = r##"{
        "name": "SplitGrid Dark",
        "appearance": "dark",
        "style": {
            "editor.background": "#181818",
            "editor.foreground": "#D4D4D4",
            "editor.active_line.background": "#262626",
            "editor.line_number": "#505050",
            "editor.active_line_number": "#D4D4D4",
            "editor.invisible": "#3B3B3B",
            "editor.gutter.background": "#181818",
            "syntax": {
                "attribute": { "color": "#9CDCFE" },
                "boolean": { "color": "#569CD6" },
                "comment": { "color": "#6A9955" },
                "comment.doc": { "color": "#6A9955" },
                "constant": { "color": "#4FC1FF" },
                "constructor": { "color": "#4EC9B0" },
                "embedded": { "color": "#D4D4D4" },
                "emphasis": { "font_style": "italic" },
                "emphasis.strong": { "font_weight": 700 },
                "enum": { "color": "#4EC9B0" },
                "function": { "color": "#DCDCAA" },
                "keyword": { "color": "#569CD6" },
                "label": { "color": "#C8C8C8" },
                "link_text": { "color": "#569CD6" },
                "link_uri": { "color": "#CE9178" },
                "number": { "color": "#B5CEA8" },
                "operator": { "color": "#D4D4D4" },
                "preproc": { "color": "#C586C0" },
                "property": { "color": "#9CDCFE" },
                "punctuation": { "color": "#D4D4D4" },
                "punctuation.bracket": { "color": "#D4D4D4" },
                "punctuation.delimiter": { "color": "#D4D4D4" },
                "punctuation.list_marker": { "color": "#6796E6" },
                "punctuation.special": { "color": "#569CD6" },
                "string": { "color": "#CE9178" },
                "string.escape": { "color": "#D7BA7D" },
                "string.regex": { "color": "#D16969" },
                "string.special": { "color": "#CE9178" },
                "string.special.symbol": { "color": "#CE9178" },
                "tag": { "color": "#569CD6" },
                "tag.doctype": { "color": "#808080" },
                "text.literal": { "color": "#CE9178" },
                "title": { "color": "#569CD6", "font_weight": 700 },
                "type": { "color": "#4EC9B0" },
                "variable": { "color": "#9CDCFE" },
                "variable.special": { "color": "#569CD6" },
                "variant": { "color": "#4FC1FF" }
            }
        }
    }"##;
    serde_json::from_str(JSON).expect("editor theme")
}
