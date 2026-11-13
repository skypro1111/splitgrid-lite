//! Spotting a password prompt at the end of a remote shell's output (the
//! Electron app's ssh-prompt.ts): `[sudo] password for me:`, `Password:`,
//! `пароль для me:` — with the cursor still on that line.

use std::sync::LazyLock;

use regex::Regex;

static CSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
static OSC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap());
static PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:\[sudo\]\s*)?(?:password|passwd|пароль)(?:\s+(?:for|для)\s+[^:\n]*)?[ \t]*:[ \t]*$").unwrap());

/// Recent output, enough to see the current line.
#[derive(Default)]
pub struct PromptWatch {
    tail: String,
    /// A prompt was seen and not yet answered or scrolled past.
    pub pending: bool,
}

/// What a chunk of output did to the prompt state.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Appeared,
    Gone,
}

impl PromptWatch {
    pub fn feed(&mut self, bytes: &[u8]) -> Change {
        self.tail.push_str(&String::from_utf8_lossy(bytes));
        if self.tail.len() > 1024 {
            let mut cut = self.tail.len() - 1024;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
        let plain = OSC.replace_all(&self.tail, "");
        let plain = CSI.replace_all(&plain, "");
        let start = plain.char_indices().rev().nth(159).map(|(i, _)| i).unwrap_or(0);
        let at_prompt = PROMPT.is_match(&plain[start..]);
        match (at_prompt, self.pending) {
            (true, false) => {
                self.pending = true;
                Change::Appeared
            }
            (false, true) => {
                self.pending = false;
                Change::Gone
            }
            _ => Change::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(text: &str) -> bool {
        let mut w = PromptWatch::default();
        w.feed(text.as_bytes()) == Change::Appeared
    }

    #[test]
    fn recognises_prompts() {
        assert!(seen("$ sudo ls\r\n[sudo] password for me: "));
        assert!(seen("Password:"));
        assert!(seen("\x1b[1mPassword:\x1b[0m "));
        assert!(seen("[sudo] пароль для me: "));
        assert!(!seen("password: hunter2\r\n$ "), "only at the end of the output");
        assert!(!seen("Enter passphrase for key: "), "a passphrase isn't a password prompt");
    }

    #[test]
    fn appears_once_and_clears_on_new_output() {
        let mut w = PromptWatch::default();
        assert_eq!(w.feed(b"Password: "), Change::Appeared);
        assert_eq!(w.feed(b""), Change::None);
        assert_eq!(w.feed(b"\r\nok\r\n$ "), Change::Gone);
    }
}
