//! ⌘⇧D — duplicate, as Sublime and VS Code do: a selection is inserted again
//! right after itself (the copy selected); a bare caret duplicates its line
//! below (the caret moves onto the copy, same column).

use super::comment::Edit;

/// The edit for `sel` (byte offsets) in `text`: insert `text` at `range.start`
/// (an empty range), then select `selection`.
pub fn duplicate(text: &str, sel: std::ops::Range<usize>) -> Edit {
    if !sel.is_empty() {
        let copy = text[sel.clone()].to_owned();
        let at = sel.end;
        return Edit { range: at..at, selection: at..at + copy.len(), text: copy };
    }
    let start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[sel.start..].find('\n').map_or(text.len(), |i| sel.start + i);
    let line = &text[start..end];
    let inserted = format!("\n{line}");
    let caret = sel.start + inserted.len();
    Edit { range: end..end, text: inserted, selection: caret..caret }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, e: &Edit) -> String {
        format!("{}{}{}", &text[..e.range.start], e.text, &text[e.range.end..])
    }

    #[test]
    fn caret_duplicates_its_line_and_moves_down() {
        let t = "a\nlet x = 1;\nb";
        let c = t.find("x").unwrap();
        let e = duplicate(t, c..c);
        let out = apply(t, &e);
        assert_eq!(out, "a\nlet x = 1;\nlet x = 1;\nb");
        assert_eq!(e.selection.start, out.rfind("x =").unwrap());
    }

    #[test]
    fn last_line_without_newline() {
        let t = "one\ntwo";
        let e = duplicate(t, 7..7);
        assert_eq!(apply(t, &e), "one\ntwo\ntwo");
        assert_eq!(e.selection, 11..11);
    }

    #[test]
    fn selection_is_copied_after_itself_and_the_copy_selected() {
        let t = "foo(bar)";
        let e = duplicate(t, 4..7);
        let out = apply(t, &e);
        assert_eq!(out, "foo(barbar)");
        assert_eq!(&out[e.selection.clone()], "bar");
        assert_eq!(e.selection, 7..10);
        // Several lines too.
        let t = "a\nb\n";
        assert_eq!(apply(t, &duplicate(t, 0..4)), "a\nb\na\nb\n");
    }
}
