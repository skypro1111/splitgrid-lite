//! Heuristic capture of commands typed into a terminal — a port of the
//! Electron app's renderer/utils/commandCapture.ts.
//!
//! Input bytes are buffered per terminal and a finished line is reported on
//! Enter. It captures plainly typed commands well but can't reconstruct lines
//! edited through history recall (↑/↓), reverse search or tab completion:
//! those involve escape sequences that can't be replayed against the buffer,
//! so such a line is marked dirty and skipped rather than recorded as garbage.
//!
//! SECURITY: at a no-echo prompt (sudo / ssh password / passphrase) the typed
//! secret still flows through input. The current prompt line is watched for a
//! password-style prompt, which "arms" the line so the secret is never
//! recorded. The bias is toward suppression: a false positive merely drops a
//! command from "recent".
//!
//! The Electron app sees the raw output stream; here a local shell's output
//! goes straight into the VT parser, so the terminal view samples the cursor's
//! row (the visible prompt line) before each input chunk and hands it to
//! [`LineCapture::note_prompt_line`] — the same "tail of output since the last
//! newline" the Electron app keeps.

use std::sync::LazyLock;

use regex::Regex;

/// Guard against pasted blobs / runaway lines being stored as "commands".
pub const MAX_COMMAND_LEN: usize = 800;

/// The trailing prompt of common secret entry: "Password:", "[sudo] password
/// for user:", "user@host's password:", "Enter passphrase for key '…':",
/// "PIN:", etc., anchored to the end of the visible prompt line.
static SECRET_PROMPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(password|passphrase|passcode|secret|pin)[^\n]{0,60}:\s*$").unwrap());
/// OSC … BEL/ST.
static OSC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap());
/// CSI / other escapes.
static ESC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b[\[0-9;?]*[ -/]*[@-~]").unwrap());
/// Other control bytes (keeps \t, \n, \r).
static CTRL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f]").unwrap());

/// One terminal's line being typed.
#[derive(Default, Debug)]
pub struct LineCapture {
    buf: String,
    /// An escape sequence or tab appeared: the line can't be trusted and is
    /// skipped when Enter arrives.
    dirty: bool,
    /// The current prompt line (ANSI-stripped, reset at newlines), capped.
    out_tail: String,
    /// The output looked like a secret prompt: the next submitted line is dropped.
    armed_secret: bool,
}

impl LineCapture {
    /// Feed terminal output so a secret prompt arms suppression.
    pub fn note_output(&mut self, data: &str) {
        let cleaned = OSC.replace_all(data, "");
        let cleaned = ESC.replace_all(&cleaned, "");
        let cleaned = CTRL.replace_all(&cleaned, "");
        let mut tail = std::mem::take(&mut self.out_tail);
        tail.push_str(&cleaned);
        if let Some(nl) = tail.rfind('\n') {
            tail.drain(..=nl);
        }
        let skip = tail.chars().count().saturating_sub(160);
        self.out_tail = tail.chars().skip(skip).collect();
        // Once armed, stay armed until the line is submitted (Enter clears it):
        // output that lands mid-entry (a tmux status-line redraw) moves the
        // tail off the prompt and would otherwise let the secret be recorded.
        self.armed_secret = self.armed_secret || SECRET_PROMPT.is_match(&self.out_tail);
    }

    /// The visible line the cursor is on, up to the cursor: what the output
    /// since the last newline amounts to.
    pub fn note_prompt_line(&mut self, line: &str) {
        self.note_output(&format!("\n{line}"));
    }

    /// Feed a raw input chunk; returns the commands it completed.
    pub fn feed_input(&mut self, data: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ch in data.chars() {
            match ch {
                '\r' | '\n' => {
                    let cmd = self.buf.trim();
                    // JS string length: UTF-16 units.
                    if !self.dirty && !self.armed_secret && !cmd.is_empty() && cmd.encode_utf16().count() <= MAX_COMMAND_LEN {
                        out.push(cmd.to_owned());
                    }
                    self.buf.clear();
                    self.dirty = false;
                    self.armed_secret = false;
                }
                // Backspace / DEL: drop the last character.
                '\x7f' | '\x08' => {
                    self.buf.pop();
                }
                // Ctrl-C (cancel line) / Ctrl-U (kill line): start fresh.
                '\x03' | '\x15' => {
                    self.buf.clear();
                    self.dirty = false;
                }
                // An escape sequence (arrows / history / reverse search) or tab
                // completion: the resulting line can't be reconstructed.
                '\x1b' | '\t' => self.dirty = true,
                c if (c as u32) >= 0x20 => self.buf.push(c),
                // Other control characters are ignored.
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Electron app's commandCapture.test.ts, case for case.

    #[test]
    fn records_a_plainly_typed_command_on_enter() {
        let mut c = LineCapture::default();
        assert_eq!(c.feed_input("ls -la\r"), ["ls -la"]);
    }

    #[test]
    fn never_records_the_line_typed_at_a_password_prompt() {
        let mut c = LineCapture::default();
        c.note_output("[sudo] password for sk: ");
        assert!(c.feed_input("hunter2\r").is_empty());
    }

    #[test]
    fn stays_armed_when_output_lands_mid_entry() {
        let mut c = LineCapture::default();
        c.note_output("[sudo] password for sk: ");
        assert!(c.feed_input("hun").is_empty());
        c.note_output("\x1b7\x1b[24;1H[0] 0:zsh*  12:05\x1b8");
        assert!(c.feed_input("ter2\r").is_empty());
    }

    #[test]
    fn disarms_after_the_secret_line_is_submitted() {
        let mut c = LineCapture::default();
        c.note_output("Password: ");
        assert!(c.feed_input("hunter2\r").is_empty());
        c.note_output("\r\n$ ");
        assert_eq!(c.feed_input("git status\r"), ["git status"]);
    }

    // Through the screen-line sampling the terminal view uses.

    #[test]
    fn a_sampled_password_prompt_line_arms() {
        let mut c = LineCapture::default();
        for (line, key) in [("Enter passphrase for key '/k/id': ", "s"), ("Enter passphrase for key '/k/id': ", "3"), ("~ 12:05", "\r")] {
            c.note_prompt_line(line);
            assert!(c.feed_input(key).is_empty());
        }
        c.note_prompt_line("~ $ ");
        c.feed_input("e");
        c.note_prompt_line("~ $ e");
        assert_eq!(c.feed_input("cho hi\n"), ["echo hi"]);
    }

    #[test]
    fn a_sampled_ordinary_prompt_does_not_arm() {
        let mut c = LineCapture::default();
        c.note_prompt_line("user@host:~/src (main) $ ");
        c.feed_input("make");
        c.note_prompt_line("user@host:~/src (main) $ make");
        assert_eq!(c.feed_input("\r"), ["make"]);
    }

    // The rest of the heuristic.

    #[test]
    fn history_recall_and_tab_completion_skip_the_line() {
        let mut c = LineCapture::default();
        assert!(c.feed_input("\x1b[A\r").is_empty());
        assert!(c.feed_input("git ch\tmain\r").is_empty());
        assert_eq!(c.feed_input("pwd\r"), ["pwd"], "the next line is clean again");
    }

    #[test]
    fn backspace_ctrl_c_and_ctrl_u_edit_the_buffer() {
        let mut c = LineCapture::default();
        assert_eq!(c.feed_input("lss\x7f -l\r"), ["ls -l"]);
        assert_eq!(c.feed_input("rm -rf /\x03echo ok\r"), ["echo ok"]);
        assert_eq!(c.feed_input("ab\x1b[D\x15whoami\r"), ["whoami"], "kill-line also clears dirty");
        assert_eq!(c.feed_input("é\x08ü\r"), ["ü"], "backspace drops a whole character");
    }

    #[test]
    fn blank_and_oversized_lines_are_not_recorded() {
        let mut c = LineCapture::default();
        assert!(c.feed_input("   \r").is_empty());
        let long = "x".repeat(MAX_COMMAND_LEN + 1);
        assert!(c.feed_input(&format!("{long}\r")).is_empty());
        let max = "y".repeat(MAX_COMMAND_LEN);
        assert_eq!(c.feed_input(&format!("{max}\r")), [max]);
    }

    #[test]
    fn several_lines_in_one_chunk() {
        let mut c = LineCapture::default();
        assert_eq!(c.feed_input("cd /tmp\nls\n"), ["cd /tmp", "ls"]);
    }

    #[test]
    fn output_tail_is_reset_at_newlines() {
        let mut c = LineCapture::default();
        c.note_output("Password: wrong\r\nSorry, try again.\r\n$ ");
        assert_eq!(c.feed_input("ls\r"), ["ls"]);
    }
}
