//! The wire protocol between a client (Lite, the menu-bar helper) and one
//! session daemon.
//!
//! Every message is a frame: `kind: u8`, `len: u32 LE`, then `len` bytes.
//! The first frame a client sends is `HELLO` (magic + the protocol versions it
//! speaks); the daemon answers `HELLO_OK` with the version both will use, or
//! `ERROR` and closes. Those three frames and `TERMINATE` are frozen: their
//! layout never changes in any version, so any Lite can at least tell an
//! incompatible daemon apart from a dead one, and end it on request.
//!
//! After the hello:
//! - `ATTACH` → `ATTACHED` (session size, shell pid; a dup of the PTY master
//!   rides along as SCM_RIGHTS so the client can ask the kernel about the
//!   foreground job), then the ring replayed as `OUTPUT` frames,
//!   `REPLAY_DONE`, then live `OUTPUT`. A new attach displaces the previous
//!   one (`DETACHED` to it).
//! - `INPUT`, `RESIZE`, `SIGNAL`, `DETACH`: no reply.
//! - `INFO` → `INFO_REPLY` (JSON [`crate::SessionInfo`]).
//! - `SET_META` (JSON, merged) → `OK`.
//! - `SNAPSHOT` → `SNAPSHOT_REPLY` (size + recent output), without attaching.
//! - `KILL` / `TERMINATE`: end the shell; every client gets `EXIT`.
//! - Anything unknown → `ERROR`, and the connection stays open.

use std::io;

/// The frozen hello magic.
pub const MAGIC: &[u8; 6] = b"SGPTYD";
/// The protocol this build speaks (newest).
pub const PROTOCOL_VERSION: u16 = 1;
/// The oldest protocol this build still speaks.
pub const MIN_PROTOCOL_VERSION: u16 = 1;
/// Larger frames are a protocol error.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Output is chunked into frames of at most this.
pub const CHUNK: usize = 64 * 1024;

pub mod kind {
    // Client → daemon.
    pub const HELLO: u8 = 0x01; // frozen
    pub const ATTACH: u8 = 0x02;
    pub const INPUT: u8 = 0x03;
    pub const RESIZE: u8 = 0x04;
    pub const SIGNAL: u8 = 0x05;
    pub const INFO: u8 = 0x06;
    pub const DETACH: u8 = 0x07;
    pub const KILL: u8 = 0x08;
    pub const SET_META: u8 = 0x09;
    pub const SNAPSHOT: u8 = 0x0A;
    pub const TERMINATE: u8 = 0x7F; // frozen, accepted before the hello
    // Daemon → client.
    pub const HELLO_OK: u8 = 0x81; // frozen
    pub const ERROR: u8 = 0x82; // frozen
    pub const OUTPUT: u8 = 0x83;
    pub const ATTACHED: u8 = 0x84;
    pub const REPLAY_DONE: u8 = 0x85;
    pub const EXIT: u8 = 0x86;
    pub const INFO_REPLY: u8 = 0x87;
    pub const DETACHED: u8 = 0x88;
    pub const OK: u8 = 0x89;
    pub const SNAPSHOT_REPLY: u8 = 0x8A;
}

/// `SIGNAL` targets.
pub const SIGNAL_SHELL: u8 = 0;
pub const SIGNAL_FOREGROUND: u8 = 1;

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Reassembles frames from a byte stream.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    start: usize,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, if one is buffered.
    pub fn next_frame(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        let avail = &self.buf[self.start..];
        if avail.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(avail[1..5].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(bad(format!("frame of {len} bytes")));
        }
        if avail.len() < 5 + len {
            return Ok(None);
        }
        let kind = avail[0];
        let payload = avail[5..5 + len].to_vec();
        self.start += 5 + len;
        if self.start > 1 << 20 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        Ok(Some((kind, payload)))
    }
}

/// HELLO: magic, then the client's (min, max) protocol versions.
pub fn hello(min: u16, max: u16) -> Vec<u8> {
    let mut p = MAGIC.to_vec();
    p.extend_from_slice(&min.to_le_bytes());
    p.extend_from_slice(&max.to_le_bytes());
    frame(kind::HELLO, &p)
}

pub fn parse_hello(p: &[u8]) -> io::Result<(u16, u16)> {
    if p.len() < 10 || &p[..6] != MAGIC {
        return Err(bad("not a splitgrid-ptyd hello"));
    }
    Ok((u16::from_le_bytes([p[6], p[7]]), u16::from_le_bytes([p[8], p[9]])))
}

/// The version two sides with these ranges agree on: the newest both speak.
pub fn negotiate(ours: (u16, u16), theirs: (u16, u16)) -> Option<u16> {
    let v = ours.1.min(theirs.1);
    (v >= ours.0.max(theirs.0)).then_some(v)
}

/// HELLO_OK: the agreed version, the shell's pid.
pub fn hello_ok(version: u16, pid: i32) -> Vec<u8> {
    let mut p = version.to_le_bytes().to_vec();
    p.extend_from_slice(&pid.to_le_bytes());
    frame(kind::HELLO_OK, &p)
}

pub fn parse_hello_ok(p: &[u8]) -> io::Result<(u16, i32)> {
    if p.len() < 6 {
        return Err(bad("short HELLO_OK"));
    }
    Ok((u16::from_le_bytes([p[0], p[1]]), i32::from_le_bytes(p[2..6].try_into().unwrap())))
}

/// A terminal size: cells and the cell's pixel size (0 when unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Size {
    pub fn encode(self) -> Vec<u8> {
        [self.cols, self.rows, self.cell_width, self.cell_height].iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    pub fn decode(p: &[u8]) -> io::Result<Self> {
        if p.len() < 8 {
            return Err(bad("short size"));
        }
        let u = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        Ok(Self { cols: u(0), rows: u(2), cell_width: u(4), cell_height: u(6) })
    }
}

/// ATTACHED: the session's size, the shell's pid.
pub fn attached(size: Size, pid: i32) -> Vec<u8> {
    let mut p = size.encode();
    p.extend_from_slice(&pid.to_le_bytes());
    frame(kind::ATTACHED, &p)
}

pub fn parse_attached(p: &[u8]) -> io::Result<(Size, i32)> {
    if p.len() < 12 {
        return Err(bad("short ATTACHED"));
    }
    Ok((Size::decode(p)?, i32::from_le_bytes(p[8..12].try_into().unwrap())))
}

/// SNAPSHOT_REPLY: the session's size, then the bytes.
pub fn snapshot_reply(size: Size, bytes: &[u8]) -> Vec<u8> {
    let mut p = size.encode();
    p.extend_from_slice(bytes);
    frame(kind::SNAPSHOT_REPLY, &p)
}

pub fn parse_snapshot_reply(p: &[u8]) -> io::Result<(Size, Vec<u8>)> {
    Ok((Size::decode(p)?, p[8..].to_vec()))
}

/// EXIT: the shell's exit code (128 + signal when it was killed; -1 unknown).
pub fn exit(code: i32) -> Vec<u8> {
    frame(kind::EXIT, &code.to_le_bytes())
}

pub fn parse_exit(p: &[u8]) -> i32 {
    p.get(..4).and_then(|b| b.try_into().ok()).map(i32::from_le_bytes).unwrap_or(-1)
}

pub fn error(msg: &str) -> Vec<u8> {
    frame(kind::ERROR, msg.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_split_anywhere() {
        let mut wire = frame(kind::INPUT, b"hello");
        wire.extend(hello(1, 3));
        wire.extend(frame(kind::DETACH, b""));
        for split in 0..wire.len() {
            let mut d = Decoder::default();
            d.push(&wire[..split]);
            let mut got = Vec::new();
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
            d.push(&wire[split..]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
            assert_eq!(got.len(), 3, "split at {split}");
            assert_eq!(got[0], (kind::INPUT, b"hello".to_vec()));
            assert_eq!(parse_hello(&got[1].1).unwrap(), (1, 3));
            assert_eq!(got[2], (kind::DETACH, vec![]));
        }
    }

    #[test]
    fn oversized_frame_is_an_error() {
        let mut d = Decoder::default();
        let mut wire = vec![kind::INPUT];
        wire.extend(((MAX_FRAME + 1) as u32).to_le_bytes());
        d.push(&wire);
        assert!(d.next_frame().is_err());
    }

    #[test]
    fn negotiation_picks_the_newest_common_version() {
        assert_eq!(negotiate((1, 1), (1, 1)), Some(1));
        assert_eq!(negotiate((1, 3), (2, 5)), Some(3));
        assert_eq!(negotiate((1, 1), (2, 2)), None);
        assert_eq!(negotiate((3, 4), (1, 2)), None);
    }

    #[test]
    fn fixed_layouts_round_trip() {
        let s = Size { cols: 120, rows: 40, cell_width: 8, cell_height: 17 };
        let f = attached(s, 4242);
        assert_eq!(parse_attached(&f[5..]).unwrap(), (s, 4242));
        let f = hello_ok(1, -7);
        assert_eq!(parse_hello_ok(&f[5..]).unwrap(), (1, -7));
        let f = snapshot_reply(s, b"abc");
        assert_eq!(parse_snapshot_reply(&f[5..]).unwrap(), (s, b"abc".to_vec()));
        assert_eq!(parse_exit(&exit(130)[5..]), 130);
    }
}
