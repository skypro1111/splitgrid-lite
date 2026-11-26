//! Links in the terminal that ⌘-click opens: OSC 8 hyperlinks (Claude Code
//! and other CLIs print file paths and URLs as these, see `add_shell_env`'s
//! FORCE_HYPERLINK), else URLs and absolute paths in the text itself, read
//! across soft-wrapped rows (Electron's OSC8LinkProvider + UrlRegexProvider).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use regex::Regex;

/// A link under the pointer: what it points at, and the cells it spans (inclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub target: Target,
    pub start: Point,
    pub end: Point,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Url(String),
    File(PathBuf),
}

/// A file the in-app image viewer (crate::image_view) shows.
pub fn is_image(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"))
}

/// The link at `point` (a grid point: scrollback lines are negative), if any.
pub fn link_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<Link> {
    hyperlink_at(term, point).or_else(|| text_link_at(term, point))
}

/// The OSC 8 hyperlink on the cell at `point`, spread over the neighbouring
/// cells (wrapped rows included) that carry the same one.
fn hyperlink_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<Link> {
    let grid = term.grid();
    let link = grid[point.line][point.column].hyperlink()?;
    let target = target(link.uri())?;
    let (top, bottom, last_col) = (Line(-(grid.history_size() as i32)), Line(grid.screen_lines() as i32 - 1), grid.columns() - 1);
    let prev = |p: Point| -> Option<Point> {
        match p.column.0 {
            0 if p.line > top => Some(Point::new(p.line - 1, Column(last_col))),
            0 => None,
            c => Some(Point::new(p.line, Column(c - 1))),
        }
    };
    let next = |p: Point| -> Option<Point> {
        match p.column.0 {
            c if c == last_col && p.line < bottom => Some(Point::new(p.line + 1, Column(0))),
            c if c == last_col => None,
            c => Some(Point::new(p.line, Column(c + 1))),
        }
    };
    let same = |p: Point| grid[p.line][p.column].hyperlink().as_ref() == Some(&link);
    let (mut start, mut end) = (point, point);
    while let Some(p) = prev(start).filter(|p| same(*p)) {
        start = p;
    }
    while let Some(p) = next(end).filter(|p| same(*p)) {
        end = p;
    }
    Some(Link { target, start, end })
}

/// URLs, and absolute or ~ paths, at the start of the text or after a space,
/// quote or bracket. A path stops at a `:` so `file.rs:12:3` opens file.rs.
static TEXT_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s'"`(\[<{=])((?:https?|file)://[^\s<>"'`]+|~?/[^\s<>"'`:|]+)"#).expect("link regex")
});

/// A URL or path in the logical line (soft-wrapped rows joined) under `point`.
fn text_link_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<Link> {
    let grid = term.grid();
    let (top, bottom, cols) = (Line(-(grid.history_size() as i32)), Line(grid.screen_lines() as i32 - 1), grid.columns());
    let wraps = |line: Line| grid[line][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    let mut first = point.line;
    while first > top && wraps(first - 1) {
        first = first - 1;
    }
    let mut last = point.line;
    while last < bottom && wraps(last) {
        last = last + 1;
    }

    // The text, and the cell each of its chars came from (by byte offset).
    let mut text = String::new();
    let mut cells: Vec<(usize, Point)> = Vec::new();
    let mut line = first;
    loop {
        for col in 0..cols {
            let cell = &grid[line][Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            cells.push((text.len(), Point::new(line, Column(col))));
            text.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        if line == last {
            break;
        }
        line = line + 1;
    }
    let at = cells.iter().rposition(|(_, p)| *p <= point).map(|i| cells[i].0)?;

    for caps in TEXT_LINK.captures_iter(&text) {
        let m = caps.get(1)?;
        let raw = m.as_str().trim_end_matches(['.', ',', ';', ')', ']', '}', '>', '!', '?', '\'', '"']);
        let (from, to) = (m.start(), m.start() + raw.len());
        if raw.is_empty() || at < from || at >= to {
            continue;
        }
        let cell_of = |byte: usize| cells.iter().take_while(|(b, _)| *b <= byte).last().map(|(_, p)| *p);
        let target = target(raw)?;
        return Some(Link { target, start: cell_of(from)?, end: cell_of(to - 1)? });
    }
    None
}

/// What a link's text or OSC 8 URI points at.
pub fn target(raw: &str) -> Option<Target> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(Target::Url(raw.to_owned()));
    }
    let path = if let Some(rest) = raw.strip_prefix("file://") {
        // file://host/path: the host (usually empty or localhost) is dropped.
        let path = &rest[rest.find('/')?..];
        PathBuf::from(percent_decode(path))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        std::env::home_dir()?.join(rest)
    } else if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        return None;
    };
    Some(Target::File(path))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    fn term(cols: usize, output: &str) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, 6), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, output.as_bytes());
        term
    }

    fn at(line: i32, col: usize) -> Point {
        Point::new(Line(line), Column(col))
    }

    #[test]
    fn osc8_hyperlink_gives_its_uri_not_the_shown_text() {
        let t = term(40, "see \x1b]8;;file:///tmp/a%20b/shot.png\x1b\\shot (75KB)\x1b]8;;\x1b\\ done");
        let link = link_at(&t, at(0, 6)).expect("link");
        assert_eq!(link.target, Target::File("/tmp/a b/shot.png".into()));
        assert_eq!((link.start, link.end), (at(0, 4), at(0, 14)));
        assert!(is_image(Path::new("/tmp/a b/shot.png")));
        assert_eq!(link_at(&t, at(0, 1)), None);
    }

    #[test]
    fn plain_paths_and_urls_in_text() {
        let t = term(60, "open /tmp/x/shot.png, then https://example.com/a?b=1.");
        assert_eq!(link_at(&t, at(0, 8)).unwrap().target, Target::File("/tmp/x/shot.png".into()));
        assert_eq!(link_at(&t, at(0, 30)).unwrap().target, Target::Url("https://example.com/a?b=1".into()));
        assert_eq!(link_at(&t, at(0, 2)), None);
    }

    #[test]
    fn a_path_is_read_across_a_soft_wrap() {
        let t = term(12, "$ /private/tmp/claude/shot.png");
        let link = link_at(&t, at(2, 1)).expect("link on the wrapped row");
        assert_eq!(link.target, Target::File("/private/tmp/claude/shot.png".into()));
        assert_eq!(link.start, at(0, 2));
    }

    #[test]
    fn a_line_number_suffix_is_not_part_of_the_path() {
        let t = term(40, "src/x at /repo/src/main.rs:12:3");
        assert_eq!(link_at(&t, at(0, 12)).unwrap().target, Target::File("/repo/src/main.rs".into()));
    }
}
