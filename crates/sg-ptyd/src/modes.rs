//! The terminal modes a replay must restore when the start of the stream fell
//! out of the ring: a TUI that entered the alternate screen, turned on mouse
//! reporting or bracketed paste long ago would otherwise come back with the
//! client in the wrong mode. Only DEC private modes (`CSI ? Pn h/l`) that
//! change how later output or input is handled are tracked.

use std::collections::BTreeMap;

/// The DEC private modes worth restoring.
const TRACKED: [u16; 15] = [1, 6, 7, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1047, 1049, 2004];

#[derive(Clone, Debug, Default)]
pub struct ModeTracker {
    modes: BTreeMap<u16, bool>,
    state: State,
    params: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    /// `CSI` seen; deciding whether it is private.
    CsiStart,
    /// `CSI ?` collecting parameters.
    Private,
    /// Some other CSI: skip to its final byte.
    OtherCsi,
}

impl ModeTracker {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => {
                if b == 0x1b {
                    self.state = State::Esc;
                }
            }
            State::Esc => {
                self.state = match b {
                    b'[' => State::CsiStart,
                    // RIS: a full reset.
                    b'c' => {
                        self.modes.clear();
                        State::Ground
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                }
            }
            State::CsiStart => {
                if b == b'?' {
                    self.params.clear();
                    self.state = State::Private;
                } else {
                    self.state = State::OtherCsi;
                    self.step_other(b);
                }
            }
            State::Private => match b {
                b'0'..=b'9' | b';' if self.params.len() < 64 => self.params.push(b),
                b'h' | b'l' => {
                    let on = b == b'h';
                    for p in self.params.split(|c| *c == b';') {
                        if let Some(n) = std::str::from_utf8(p).ok().and_then(|s| s.parse::<u16>().ok())
                            && TRACKED.contains(&n)
                        {
                            self.modes.insert(n, on);
                        }
                    }
                    self.state = State::Ground;
                }
                0x1b => self.state = State::Esc,
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::OtherCsi => self.step_other(b),
        }
    }

    fn step_other(&mut self, b: u8) {
        match b {
            0x40..=0x7e => self.state = State::Ground,
            0x1b => self.state = State::Esc,
            _ => {}
        }
    }

    /// The sequence that puts a fresh terminal into the tracked state.
    pub fn restore_sequence(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // The alternate screen first: what follows draws into it.
        for alt in [47u16, 1047, 1049] {
            if self.modes.get(&alt) == Some(&true) {
                out.extend_from_slice(format!("\x1b[?{alt}h").as_bytes());
            }
        }
        for (n, on) in &self.modes {
            if [47, 1047, 1049].contains(n) {
                continue;
            }
            out.extend_from_slice(format!("\x1b[?{n}{}", if *on { 'h' } else { 'l' }).as_bytes());
        }
        out
    }

    #[cfg(test)]
    fn get(&self, n: u16) -> Option<bool> {
        self.modes.get(&n).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_private_modes_across_chunk_boundaries() {
        let mut t = ModeTracker::default();
        let s = b"hello\x1b[?1049h\x1b[?1000;1006hX\x1b[31m\x1b[?2004h\x1b[?1000l";
        for chunk in s.chunks(3) {
            t.feed(chunk);
        }
        assert_eq!(t.get(1049), Some(true));
        assert_eq!(t.get(1006), Some(true));
        assert_eq!(t.get(1000), Some(false));
        assert_eq!(t.get(2004), Some(true));
        let seq = String::from_utf8(t.restore_sequence()).unwrap();
        assert!(seq.starts_with("\x1b[?1049h"), "{seq:?}");
        assert!(seq.contains("\x1b[?2004h") && seq.contains("\x1b[?1000l"));
    }

    #[test]
    fn ignores_untracked_and_resets_on_ris() {
        let mut t = ModeTracker::default();
        t.feed(b"\x1b[?12h\x1b[?1049h\x1bc");
        assert_eq!(t.get(12), None);
        assert_eq!(t.get(1049), None);
        assert!(t.restore_sequence().is_empty());
    }
}
