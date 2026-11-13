//! Keystroke → bytes for the PTY (xterm conventions).
//!
//! Pure so it can be unit-tested: the view hands in gpui's key name, the
//! modifier state and the terminal modes that change the encoding.

use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Cmd on macOS: never sent to the shell, the app owns those shortcuts.
    pub cmd: bool,
}

impl Mods {
    /// xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

/// `key` is gpui's key name ("a", "enter", "up", "f5"…), `key_char` what the
/// layout would type (Option-composed characters included).
pub fn encode(key: &str, key_char: Option<&str>, mods: Mods, app_cursor: bool) -> Option<Cow<'static, [u8]>> {
    if mods.cmd {
        return None;
    }
    let s = |b: &'static [u8]| Some(Cow::Borrowed(b));
    let owned = |v: Vec<u8>| Some(Cow::Owned(v));

    // Cursor keys and Home/End: SS3 in application mode, CSI with modifiers.
    let cursor = match key {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        "home" => Some(b'H'),
        "end" => Some(b'F'),
        _ => None,
    };
    if let Some(c) = cursor {
        return if mods.any() {
            owned(format!("\x1b[1;{}{}", mods.param(), c as char).into_bytes())
        } else if app_cursor {
            owned(vec![0x1b, b'O', c])
        } else {
            owned(vec![0x1b, b'[', c])
        };
    }

    // Keys with a `CSI n ~` form.
    let tilde = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(n) = tilde {
        return if mods.any() {
            owned(format!("\x1b[{n};{}~", mods.param()).into_bytes())
        } else {
            owned(format!("\x1b[{n}~").into_bytes())
        };
    }

    let ss3 = match key {
        "f1" => Some(b'P'),
        "f2" => Some(b'Q'),
        "f3" => Some(b'R'),
        "f4" => Some(b'S'),
        _ => None,
    };
    if let Some(c) = ss3 {
        return if mods.any() {
            owned(format!("\x1b[1;{}{}", mods.param(), c as char).into_bytes())
        } else {
            owned(vec![0x1b, b'O', c])
        };
    }

    match key {
        "enter" => return if mods.alt { s(b"\x1b\r") } else { s(b"\r") },
        "backspace" => {
            return if mods.ctrl {
                s(b"\x08")
            } else if mods.alt {
                s(b"\x1b\x7f")
            } else {
                s(b"\x7f")
            };
        }
        "tab" => return if mods.shift { s(b"\x1b[Z") } else { s(b"\t") },
        "escape" => return s(b"\x1b"),
        _ => {}
    }

    // Ctrl + key: the C0 control code.
    if mods.ctrl {
        let ch = if key == "space" { Some(' ') } else { single_char(key) };
        if let Some(c) = ch {
            let code = match c {
                'a'..='z' => Some(c as u8 - b'a' + 1),
                'A'..='Z' => Some(c as u8 - b'A' + 1),
                ' ' | '@' | '2' => Some(0),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '-' | '7' => Some(0x1f),
                '?' | '8' => Some(0x7f),
                _ => None,
            };
            if let Some(b) = code {
                return owned(if mods.alt { vec![0x1b, b] } else { vec![b] });
            }
        }
    }

    // Plain text, Option-composed characters included (Terminal.app's default).
    let text = key_char.filter(|t| !t.is_empty()).map(str::to_owned).or_else(|| match key {
        "space" => Some(" ".to_owned()),
        _ => single_char(key).map(|c| c.to_string()),
    })?;
    Some(Cow::Owned(text.into_bytes()))
}

fn single_char(key: &str) -> Option<char> {
    let mut it = key.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Text pasted into the terminal, wrapped when the app asked for bracketed paste.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    // Line endings as the Enter key sends them.
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A pasted end marker would let the paste break out of the bracket.
        let body = body.replace("\x1b[201~", "");
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: &str, ch: Option<&str>, mods: Mods) -> Vec<u8> {
        encode(key, ch, mods, false).map(|c| c.into_owned()).unwrap_or_default()
    }

    const NONE: Mods = Mods { shift: false, alt: false, ctrl: false, cmd: false };

    #[test]
    fn plain_text_uses_key_char() {
        assert_eq!(enc("a", Some("a"), NONE), b"a");
        assert_eq!(enc("a", Some("A"), Mods { shift: true, ..NONE }), b"A");
        assert_eq!(enc("s", Some("ß"), Mods { alt: true, ..NONE }), "ß".as_bytes());
        assert_eq!(enc("space", None, NONE), b" ");
    }

    #[test]
    fn control_codes() {
        assert_eq!(enc("c", Some("c"), Mods { ctrl: true, ..NONE }), [3]);
        assert_eq!(enc("d", None, Mods { ctrl: true, ..NONE }), [4]);
        assert_eq!(enc("space", None, Mods { ctrl: true, ..NONE }), [0]);
        assert_eq!(enc("[", None, Mods { ctrl: true, ..NONE }), [0x1b]);
        assert_eq!(enc("enter", None, NONE), b"\r");
        assert_eq!(enc("backspace", None, NONE), [0x7f]);
        assert_eq!(enc("escape", None, NONE), [0x1b]);
        assert_eq!(enc("tab", None, Mods { shift: true, ..NONE }), b"\x1b[Z");
    }

    #[test]
    fn cursor_keys_follow_mode_and_modifiers() {
        assert_eq!(enc("up", None, NONE), b"\x1b[A");
        assert_eq!(encode("up", None, NONE, true).unwrap().as_ref(), b"\x1bOA");
        assert_eq!(enc("left", None, Mods { alt: true, ..NONE }), b"\x1b[1;3D");
        assert_eq!(enc("right", None, Mods { ctrl: true, shift: true, ..NONE }), b"\x1b[1;6C");
    }

    #[test]
    fn function_and_editing_keys() {
        assert_eq!(enc("f1", None, NONE), b"\x1bOP");
        assert_eq!(enc("f5", None, NONE), b"\x1b[15~");
        assert_eq!(enc("delete", None, NONE), b"\x1b[3~");
        assert_eq!(enc("pageup", None, Mods { shift: true, ..NONE }), b"\x1b[5;2~");
    }

    #[test]
    fn cmd_is_left_to_the_app() {
        assert!(encode("v", Some("v"), Mods { cmd: true, ..NONE }, false).is_none());
    }

    #[test]
    fn paste_normalises_newlines_and_brackets() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }
}
