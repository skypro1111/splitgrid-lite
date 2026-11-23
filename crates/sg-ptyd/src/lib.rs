//! Persistent local terminals for SplitGrid Lite.
//!
//! One `splitgrid-ptyd` process per terminal session (iTerm2's model, not a
//! tmux-style mux): it owns the PTY master and the shell, always reads the PTY
//! (so the shell never blocks while Lite is gone), keeps the last few MB of raw
//! output in a ring, and serves a small versioned protocol ([`proto`]) on a
//! unix socket in a 0700 per-user directory. A crash or a bad build of one
//! daemon takes one terminal with it, never all of them, and an app update can
//! start new sessions on a new daemon while old ones keep running on the old
//! binary.
//!
//! Lite attaches with [`client::Conn`]; the menu-bar helper
//! (`splitgrid-sessions`) lists sessions with [`client::scan`] and groups them
//! with [`tree`].

pub mod client;
pub mod daemon;
pub mod launchd;
pub mod lock;
pub mod meta;
pub mod modes;
pub mod paths;
pub mod proto;
pub mod ring;
pub mod screen;
pub mod sys;
pub mod tree;

/// The daemon build's version (shown in Settings and in `info`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The daemon's executable name, in the app bundle and in the install dir.
pub const DAEMON_BIN: &str = "splitgrid-ptyd";
/// The menu-bar helper's executable name.
pub const MENU_BIN: &str = "splitgrid-sessions";
/// Raw output a session keeps for re-attach (and for the menu's "View…").
pub const DEFAULT_RING_BYTES: usize = 4 * 1024 * 1024;

pub use meta::{Meta, SessionInfo};
