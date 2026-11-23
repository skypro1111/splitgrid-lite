//! The session's recent raw output, replayed on attach.

use std::collections::VecDeque;

use crate::modes::ModeTracker;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// Bytes that fell out of the front, ever.
    evicted: u64,
    /// The terminal modes as of the ring's first byte.
    base: ModeTracker,
    /// The terminal modes now (for bounded replays, which skip computing
    /// the exact modes at their cut).
    current: ModeTracker,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self { buf: VecDeque::with_capacity(cap.min(1 << 20)), cap, evicted: 0, base: ModeTracker::default(), current: ModeTracker::default() }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Bytes ever dropped from the front.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.current.feed(bytes);
        let bytes = if bytes.len() > self.cap {
            // Only the tail can stay; the rest goes straight to the mode tracker.
            let (gone, keep) = bytes.split_at(bytes.len() - self.cap);
            self.base.feed(gone);
            self.evicted += gone.len() as u64;
            keep
        } else {
            bytes
        };
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        if overflow > 0 {
            let gone: Vec<u8> = self.buf.drain(..overflow).collect();
            self.base.feed(&gone);
            self.evicted += overflow as u64;
        }
        self.buf.extend(bytes);
    }

    /// What to feed a fresh terminal: everything kept, or — when the start has
    /// been dropped — the restored modes, then the output from the first line
    /// start on (so the replay doesn't open mid-sequence). `max` keeps only
    /// the last `max` bytes (the menu's "View…"); such a tail starts in the
    /// modes as they are now, an approximation that costs nothing.
    pub fn replay(&self, max: Option<usize>) -> Vec<u8> {
        let skip = max.map(|m| self.buf.len().saturating_sub(m)).unwrap_or(0);
        let mut body: Vec<u8> = self.buf.range(skip..).copied().collect();
        if self.evicted == 0 && skip == 0 {
            return body;
        }
        // Start after the first newline (when there is one in reach).
        let cut = body.iter().take(64 * 1024).position(|&c| c == b'\n').map(|i| i + 1).unwrap_or(0);
        let modes = if skip == 0 {
            let mut m = self.base.clone();
            m.feed(&body[..cut]);
            m
        } else {
            self.current.clone()
        };
        body.drain(..cut);
        let mut out = modes.restore_sequence();
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_everything_under_capacity() {
        let mut r = Ring::new(16);
        r.push(b"abc");
        r.push(b"def");
        assert_eq!(r.replay(None), b"abcdef");
        assert_eq!(r.evicted(), 0);
    }

    #[test]
    fn drops_the_oldest_and_restarts_at_a_line() {
        let mut r = Ring::new(10);
        r.push(b"line1\nline2\n");
        assert_eq!(r.len(), 10);
        assert_eq!(r.evicted(), 2);
        // Kept "ne1\nline2\n": the partial first line is skipped.
        assert_eq!(r.replay(None), b"line2\n");
    }

    #[test]
    fn a_huge_write_keeps_its_tail() {
        let mut r = Ring::new(4);
        r.push(b"0123456789");
        assert_eq!(r.len(), 4);
        assert_eq!(r.evicted(), 6);
        // No newline in reach: everything kept.
        assert_eq!(r.replay(None), b"6789");
    }

    #[test]
    fn evicted_modes_are_restored_first() {
        let mut r = Ring::new(12);
        r.push(b"\x1b[?1049h");
        r.push(b"xx\nscreen!\n");
        let out = r.replay(None);
        assert!(out.starts_with(b"\x1b[?1049h"), "{:?}", String::from_utf8_lossy(&out));
        assert!(out.ends_with(b"screen!\n"));
    }

    #[test]
    fn bounded_replay_takes_the_tail() {
        let mut r = Ring::new(100);
        r.push(b"aaaa\nbbbb\ncccc\n");
        assert_eq!(r.replay(Some(7)), b"cccc\n");
    }

    #[test]
    fn wraps_many_times() {
        let mut r = Ring::new(1000);
        for i in 0..10_000 {
            r.push(format!("{i}\n").as_bytes());
        }
        assert_eq!(r.len(), 1000);
        let out = r.replay(None);
        assert!(out.ends_with(b"9999\n"));
        assert!(out.len() <= 1000);
    }
}
