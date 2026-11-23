//! Raw output → the text a terminal would show: the menu helper's "View…".

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// Parse `bytes` into a `cols`×`rows` terminal keeping `scrollback` lines of
/// history, and return history + screen as text: wrapped rows joined,
/// trailing blanks trimmed.
pub fn render_text(bytes: &[u8], cols: u16, rows: u16, scrollback: usize) -> String {
    let (cols, rows) = (cols.max(2) as usize, rows.max(1) as usize);
    let config = Config { scrolling_history: scrollback, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);

    let grid = term.grid();
    let mut out = String::new();
    for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
        let row = &grid[Line(line)];
        let mut text = String::with_capacity(cols);
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                text.extend(zw.iter());
            }
        }
        if row[Column(cols - 1)].flags.contains(Flags::WRAPLINE) {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
            out.push('\n');
        }
    }
    let trimmed = out.trim_end_matches(['\n', ' ']).len();
    out.truncate(trimmed);
    out
}

// ── Styled rendering: what View shows, cell for cell ────────────────────────

/// SplitGrid's terminal theme (src/terminal/colors.rs in the app): black, red,
/// green, yellow, blue, magenta, cyan, white, then the bright ones. Alpha is
/// pre-blended over the default background where the app's theme carries it.
const BASE16: [u32; 16] = [
    0x242424, 0xfc6b83, 0x3fa266, 0xd2943e, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe4e4e4, //
    0x494949, 0xfc6b83, 0x70b489, 0xf1b467, 0x87a6c4, 0xb48ead, 0x88c0d0, 0xe4e4e4,
];
/// The default foreground (#e4e4e4 at 0xeb alpha over #141414).
pub const FOREGROUND: u32 = 0xd3d3d3;
pub const BACKGROUND: u32 = 0x141414;

/// One run of cells sharing a style. Colours are 0xRRGGBB; `bg` None = default.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub fg: u32,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

fn rgb(c: Rgb) -> u32 {
    ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32
}

/// xterm's 256-colour table above the 16 named ones.
fn indexed(i: u8) -> u32 {
    match i {
        0..=15 => BASE16[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            (step(i / 36) << 16) | (step((i / 6) % 6) << 8) | step(i % 6)
        }
        _ => {
            let v = 8 + 10 * (i as u32 - 232);
            (v << 16) | (v << 8) | v
        }
    }
}

fn resolve(term: &Term<VoidListener>, c: Color, fg: bool, bold: bool) -> Option<u32> {
    let colors = term.colors();
    match c {
        Color::Spec(rgb_) => Some(rgb(rgb_)),
        Color::Indexed(i) => {
            // Bold brightens the first 8, as xterm and the app do.
            let i = if bold && i < 8 { i + 8 } else { i };
            Some(colors[i as usize].map(rgb).unwrap_or_else(|| indexed(i)))
        }
        Color::Named(n) => {
            let n = if bold { n.to_bright() } else { n };
            if let Some(set) = colors[n] {
                return Some(rgb(set));
            }
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground => Some(FOREGROUND),
                NamedColor::DimForeground => Some(0x9a9a9a),
                NamedColor::Background => {
                    if fg {
                        Some(BACKGROUND)
                    } else {
                        None
                    }
                }
                NamedColor::Cursor => Some(FOREGROUND),
                n if (n as usize) < 16 => Some(BASE16[n as usize]),
                n if (n as usize) >= NamedColor::DimBlack as usize && (n as usize) <= NamedColor::DimWhite as usize => {
                    Some(BASE16[n as usize - NamedColor::DimBlack as usize])
                }
                _ => Some(FOREGROUND),
            }
        }
    }
}

/// History + screen as rows of styled runs, exactly as the terminal lays them
/// out at `cols` (wrapped rows stay separate rows, as on screen). Trailing
/// blank rows are dropped.
pub fn render_styled(bytes: &[u8], cols: u16, rows: u16, scrollback: usize) -> Vec<Vec<Run>> {
    let (cols, rows) = (cols.max(2) as usize, rows.max(1) as usize);
    let config = Config { scrolling_history: scrollback, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);

    let grid = term.grid();
    let mut out: Vec<Vec<Run>> = Vec::new();
    for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
        let row = &grid[Line(line)];
        let mut runs: Vec<Run> = Vec::new();
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let f = cell.flags;
            let bold = f.contains(Flags::BOLD);
            let mut fg = resolve(&term, cell.fg, true, bold).unwrap_or(FOREGROUND);
            let mut bg = resolve(&term, cell.bg, false, false);
            if f.contains(Flags::DIM) {
                let half = |c: u32, s: u32| (((c >> s) & 0xff) * 2 / 3) << s;
                fg = half(fg, 16) | half(fg, 8) | half(fg, 0);
            }
            if f.contains(Flags::INVERSE) {
                let old_bg = bg.unwrap_or(BACKGROUND);
                bg = Some(fg);
                fg = old_bg;
            }
            if f.contains(Flags::HIDDEN) {
                fg = bg.unwrap_or(BACKGROUND);
            }
            let mut ch = String::new();
            ch.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                ch.extend(zw.iter());
            }
            let style = Run {
                text: String::new(),
                fg,
                bg,
                bold,
                italic: f.contains(Flags::ITALIC),
                underline: f.intersects(Flags::ALL_UNDERLINES),
                strike: f.contains(Flags::STRIKEOUT),
            };
            match runs.last_mut() {
                Some(r) if Run { text: String::new(), ..r.clone() } == style => r.text.push_str(&ch),
                _ => runs.push(Run { text: ch, ..style }),
            }
        }
        // Trailing default-background blanks carry nothing.
        while let Some(r) = runs.last_mut() {
            if r.bg.is_some() {
                break;
            }
            let t = r.text.trim_end().len();
            r.text.truncate(t);
            if r.text.is_empty() {
                runs.pop();
            } else {
                break;
            }
        }
        out.push(runs);
    }
    while out.last().is_some_and(|r| r.is_empty()) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_and_colours() {
        let s = render_text(b"hello\r\n\x1b[31mred\x1b[0m world\r\n", 20, 5, 100);
        assert_eq!(s, "hello\nred world");
    }

    #[test]
    fn wrapping_and_overwrite() {
        // 10 columns: the long line wraps and comes back joined; \r overwrites.
        let s = render_text(b"0123456789abcdef\r\nxxxx\rab\r\n", 10, 5, 100);
        assert_eq!(s, "0123456789abcdef\nabxx");
    }

    #[test]
    fn scrollback_is_bounded_and_screen_clears() {
        let mut b = Vec::new();
        for i in 0..50 {
            b.extend(format!("line {i}\r\n").as_bytes());
        }
        let s = render_text(&b, 20, 5, 10);
        assert!(s.ends_with("line 49"), "{s}");
        assert!(!s.contains("line 10\n"));
        // A clear + cursor home leaves only what came after on screen.
        let s = render_text(b"old\r\n\x1b[2J\x1b[Hnew", 20, 5, 0);
        assert_eq!(s.trim(), "new");
    }

    #[test]
    fn styled_rows_keep_layout_and_colours() {
        let rows = render_styled(b"ab\x1b[1;31mRED\x1b[0m \x1b[38;2;1;2;3mx\x1b[0m\r\n0123456789abc\r\n", 10, 5, 100);
        // Row 0: "ab", bold bright red "RED", " ", truecolour "x".
        assert_eq!(rows[0][0].text, "ab");
        assert_eq!(rows[0][0].fg, FOREGROUND);
        assert_eq!((rows[0][1].text.as_str(), rows[0][1].fg, rows[0][1].bold), ("RED", BASE16[9], true));
        assert_eq!((rows[0][3].text.as_str(), rows[0][3].fg), ("x", 0x010203));
        // A 13-char line at 10 columns stays two rows, as on screen.
        assert_eq!(rows[1][0].text, "0123456789");
        assert_eq!(rows[2][0].text, "abc");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn inverse_and_256_colours() {
        let rows = render_styled(b"\x1b[7mI\x1b[0m\x1b[48;5;196m \x1b[0m\x1b[38;5;244mg", 10, 2, 0);
        assert_eq!((rows[0][0].fg, rows[0][0].bg), (BACKGROUND, Some(FOREGROUND)));
        assert_eq!(rows[0][1].bg, Some(0xff0000));
        assert_eq!(rows[0][2].fg, 0x808080);
    }

    #[test]
    fn alternate_screen_shows_the_tui() {
        let s = render_text(b"shell$ vim\r\n\x1b[?1049h\x1b[H~\r\n~ tui", 20, 4, 100);
        assert!(s.contains("~ tui"));
        assert!(!s.contains("shell$"));
    }
}
