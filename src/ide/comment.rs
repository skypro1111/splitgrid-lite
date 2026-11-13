//! ⌘/ — toggle comments on the selected lines, the way Monaco does:
//! if every non-blank line is already commented, uncomment them; otherwise
//! comment them all at the block's smallest indent. Languages without line
//! comments (HTML, Markdown, CSS) get the block wrapped instead.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Line(&'static str),
    Block(&'static str, &'static str),
}

/// The comment syntax for a highlighter language name (see files::language).
pub fn syntax(language: &str) -> Option<Syntax> {
    Some(match language {
        "rust" | "javascript" | "typescript" | "tsx" | "go" | "java" | "kotlin" | "swift" | "c" | "cpp" | "csharp" | "php" | "scala"
        | "zig" | "proto" | "json" => Syntax::Line("//"),
        "python" | "ruby" | "bash" | "yaml" | "toml" | "make" | "cmake" | "elixir" | "graphql" => Syntax::Line("#"),
        "sql" | "lua" => Syntax::Line("--"),
        "html" | "markdown" | "svelte" => Syntax::Block("<!--", "-->"),
        "css" => Syntax::Block("/*", "*/"),
        _ => return None,
    })
}

/// The edit: replace `range` (whole lines) with `text`; then select `selection`.
#[derive(Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

/// Byte range of the lines the selection touches (a selection ending at a
/// line's very start doesn't take that line).
fn line_block(text: &str, sel: &Range<usize>) -> Range<usize> {
    let start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let mut end_at = sel.end;
    if sel.end > sel.start && text[..sel.end].ends_with('\n') {
        end_at -= 1;
    }
    let end = text[end_at..].find('\n').map_or(text.len(), |i| end_at + i);
    start..end.max(start)
}

pub fn toggle(text: &str, sel: Range<usize>, syntax: Syntax) -> Edit {
    let block = line_block(text, &sel);
    let lines: Vec<&str> = text[block.clone()].split('\n').collect();
    let new = match syntax {
        Syntax::Line(prefix) => toggle_lines(&lines, prefix),
        Syntax::Block(open, close) => toggle_block(&text[block.clone()], open, close),
    };
    let selection = if sel.is_empty() {
        // One line, edited at its start: the caret keeps its distance to the line's end.
        let from_end = block.end - sel.start;
        let caret = (block.start + new.len()).saturating_sub(from_end).max(block.start);
        caret..caret
    } else {
        block.start..block.start + new.len()
    };
    Edit { range: block, text: new, selection }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn toggle_lines(lines: &[&str], prefix: &str) -> String {
    let content: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if content.is_empty() {
        return lines.join("\n");
    }
    let all_commented = content.iter().all(|l| l.trim_start().starts_with(prefix));
    if all_commented {
        lines
            .iter()
            .map(|l| {
                let i = indent_of(l);
                match l[i..].strip_prefix(prefix) {
                    Some(rest) => format!("{}{}", &l[..i], rest.strip_prefix(' ').unwrap_or(rest)),
                    None => l.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let at = content.iter().map(|l| indent_of(l)).min().unwrap_or(0);
        lines
            .iter()
            .map(|l| if l.trim().is_empty() { l.to_string() } else { format!("{}{prefix} {}", &l[..at], &l[at..]) })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn toggle_block(block: &str, open: &str, close: &str) -> String {
    let i = indent_of(block);
    let body = block[i..].trim_end();
    let trailing = &block[i + body.len()..];
    if let Some(inner) = body.strip_prefix(open).and_then(|b| b.strip_suffix(close)) {
        let inner = inner.strip_prefix(' ').unwrap_or(inner);
        let inner = inner.strip_suffix(' ').unwrap_or(inner);
        format!("{}{inner}{trailing}", &block[..i])
    } else if body.is_empty() {
        block.to_owned()
    } else {
        format!("{}{open} {body} {close}{trailing}", &block[..i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, e: &Edit) -> String {
        format!("{}{}{}", &text[..e.range.start], e.text, &text[e.range.end..])
    }

    #[test]
    fn comments_and_uncomments_the_caret_line() {
        let t = "fn a() {\n    let x = 1;\n}\n";
        let caret = t.find("let").unwrap();
        let e = toggle(t, caret..caret, Syntax::Line("//"));
        let once = apply(t, &e);
        assert_eq!(once, "fn a() {\n    // let x = 1;\n}\n");
        assert!(once[e.selection.start..].starts_with("let"), "caret stays before the same text");
        let c2 = once.find("let").unwrap();
        let back = apply(&once, &toggle(&once, c2..c2, Syntax::Line("//")));
        assert_eq!(back, t);
    }

    #[test]
    fn a_selection_comments_at_the_smallest_indent_and_skips_blank_lines() {
        let t = "  a\n\n    b\nc";
        let sel = 0..t.find('c').unwrap(); // ends at c's line start: c is not taken
        let e = toggle(t, sel, Syntax::Line("#"));
        assert_eq!(apply(t, &e), "  # a\n\n  #   b\nc");
    }

    #[test]
    fn mixed_lines_get_commented_not_toggled() {
        let t = "// a\nb";
        let e = toggle(t, 0..t.len(), Syntax::Line("//"));
        assert_eq!(apply(t, &e), "// // a\n// b");
    }

    #[test]
    fn block_comments_wrap_and_unwrap() {
        let t = "  <div>x</div>\n";
        let e = toggle(t, 3..3, Syntax::Block("<!--", "-->"));
        let once = apply(t, &e);
        assert_eq!(once, "  <!-- <div>x</div> -->\n");
        let back = apply(&once, &toggle(&once, 3..3, Syntax::Block("<!--", "-->")));
        assert_eq!(back, t);
    }

    #[test]
    fn languages() {
        assert_eq!(syntax("rust"), Some(Syntax::Line("//")));
        assert_eq!(syntax("python"), Some(Syntax::Line("#")));
        assert_eq!(syntax("sql"), Some(Syntax::Line("--")));
        assert_eq!(syntax("text"), None);
    }
}
