//! One terminal: a local PTY running the user's shell, or a remote shell over
//! SSH (crate::ssh). Either way the bytes are parsed by alacritty_terminal off
//! the UI thread.
//!
//! The I/O side (alacritty's event loop for a PTY, the SSH task for a remote
//! shell) feeds the VT parser under the `Term` lock and reports through
//! [`Listener`]. The UI only locks the `Term` to paint and to resize; it never
//! parses, so a flood of output can't stall a frame.

pub mod colors;
pub mod keys;
pub mod mouse;
pub mod procinfo;
pub mod view;

use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

/// What the I/O side tells the UI.
pub enum TermEvent {
    Term(Event),
    Remote(RemoteStatus),
    /// A password prompt appeared at the end of a remote shell's output (or went away).
    PasswordPrompt(bool),
}

/// A remote shell's connection state.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteStatus {
    Connecting,
    Connected,
    Failed(String),
    /// The session ended: `clean` when the shell exited (logout), not when the link dropped.
    Closed { clean: bool },
}

/// Forwards the I/O side's events to the UI thread.
#[derive(Clone)]
pub struct Listener(UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.unbounded_send(TermEvent::Term(event));
    }
}

impl Listener {
    pub fn status(&self, status: RemoteStatus) {
        let _ = self.0.unbounded_send(TermEvent::Remote(status));
    }

    pub fn password_prompt(&self, visible: bool) {
        let _ = self.0.unbounded_send(TermEvent::PasswordPrompt(visible));
    }
}

/// Input for a remote shell's task.
pub enum RemoteCmd {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// Type the saved password the session holds, then Enter.
    ApplyPassword,
    Close,
}

/// What the SSH task needs to drive a terminal.
pub struct RemoteEnds {
    pub term: Arc<FairMutex<Term<Listener>>>,
    pub events: Listener,
    pub commands: tokio::sync::mpsc::UnboundedReceiver<RemoteCmd>,
    pub size: GridSize,
}

enum Io {
    Local {
        sender: EventLoopSender,
        /// Our own handle on the PTY master: asks the kernel which process group
        /// owns the terminal (the foreground job), without touching the I/O thread.
        master: Option<OwnedFd>,
    },
    Remote(tokio::sync::mpsc::UnboundedSender<RemoteCmd>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl GridSize {
    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows,
            num_cols: self.cols,
            cell_width: self.cell_width.round() as u16,
            cell_height: self.cell_height.round() as u16,
        }
    }

    fn term_size(self) -> TermSize {
        TermSize::new(self.cols as usize, self.rows as usize)
    }
}

pub struct Terminal {
    pub term: Arc<FairMutex<Term<Listener>>>,
    io: Io,
    size: GridSize,
}

impl Terminal {
    /// Spawn the user's login shell. The receiver yields the I/O thread's
    /// events (wakeups, title, bell, exit); the view drains it.
    pub fn spawn(size: GridSize, cwd: Option<std::path::PathBuf>, extra_env: HashMap<String, String>) -> anyhow::Result<(Self, UnboundedReceiver<TermEvent>)> {
        let (tx, rx) = unbounded();
        let listener = Listener(tx);

        let mut env = extra_env;
        env.insert("TERM".to_owned(), "xterm-256color".to_owned());
        env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
        env.insert("TERM_PROGRAM".to_owned(), "SplitGrid".to_owned());
        let options = tty::Options {
            shell: None, // the user's login shell, as alacritty resolves it
            working_directory: cwd.or_else(std::env::home_dir),
            drain_on_exit: true,
            env,
        };

        let term = Term::new(Config::default(), &size.term_size(), listener.clone());
        let term = Arc::new(FairMutex::new(term));
        let pty = tty::new(&options, size.window_size(), 0)?;
        // SAFETY: dup of a valid open fd; we own the copy.
        let dup = unsafe { libc::dup(pty.file().as_raw_fd()) };
        let master = (dup >= 0).then(|| unsafe { OwnedFd::from_raw_fd(dup) });
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();

        Ok((Self { term, io: Io::Local { sender, master }, size }, rx))
    }

    /// A terminal whose bytes come from elsewhere (an SSH channel): the caller
    /// hands the returned ends to the task that talks to the server.
    pub fn remote(size: GridSize) -> (Self, UnboundedReceiver<TermEvent>, RemoteEnds) {
        let (tx, rx) = unbounded();
        let listener = Listener(tx);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size.term_size(), listener.clone())));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let ends = RemoteEnds { term: term.clone(), events: listener, commands: cmd_rx, size };
        (Self { term, io: Io::Remote(cmd_tx), size }, rx, ends)
    }

    /// Ask a remote session to type its saved password (it never leaves the session).
    pub fn apply_password(&self) {
        if let Io::Remote(tx) = &self.io {
            let _ = tx.send(RemoteCmd::ApplyPassword);
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.io, Io::Remote(_))
    }

    /// The foreground process (name, cwd), e.g. ("claude", "/path/to/repo").
    pub fn foreground(&self) -> Option<procinfo::Process> {
        match &self.io {
            Io::Local { master, .. } => procinfo::foreground(master.as_ref()?.as_raw_fd()),
            Io::Remote(_) => None,
        }
    }

    /// The shell's pid (the PTY session's leader); None for a remote shell.
    pub fn session_leader(&self) -> Option<i32> {
        match &self.io {
            // SAFETY: a plain syscall on an fd we own.
            Io::Local { master, .. } => Some(unsafe { libc::tcgetsid(master.as_ref()?.as_raw_fd()) }).filter(|p| *p > 0),
            Io::Remote(_) => None,
        }
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        match &self.io {
            Io::Local { sender, .. } => {
                let _ = sender.send(Msg::Input(bytes.into()));
            }
            Io::Remote(tx) => {
                let _ = tx.send(RemoteCmd::Input(bytes.into().into_owned()));
            }
        }
    }

    /// Resize the grid and the PTY; a no-op when nothing changed.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size || size.cols == 0 || size.rows == 0 {
            return;
        }
        self.size = size;
        self.term.lock().resize(size.term_size());
        match &self.io {
            Io::Local { sender, .. } => {
                let _ = sender.send(Msg::Resize(size.window_size()));
            }
            Io::Remote(tx) => {
                let _ = tx.send(RemoteCmd::Resize { cols: size.cols, rows: size.rows });
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        match &self.io {
            Io::Local { sender, .. } => {
                let _ = sender.send(Msg::Shutdown);
            }
            Io::Remote(tx) => {
                let _ = tx.send(RemoteCmd::Close);
            }
        }
    }
}

#[cfg(test)]
mod throughput {
    //! Parser throughput without a window: the ceiling for `cat bigfile`.
    //! `cargo test --release -- --ignored --nocapture throughput`
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn parse_colored_output() {
        let mut chunk = Vec::new();
        for i in 0..2000u32 {
            chunk.extend_from_slice(
                format!("\x1b[38;5;{}m{:>6}\x1b[0m src/main/some/path/file_{i}.rs: line of ordinary log text, äöü ✓\r\n", i % 256, i)
                    .as_bytes(),
            );
        }
        let total_target = 256 * 1024 * 1024;
        let reps = total_target / chunk.len();

        let mut term = Term::new(Config::default(), &TermSize::new(200, 60), VoidListener);
        let mut parser: Processor = Processor::new();
        let start = Instant::now();
        for _ in 0..reps {
            parser.advance(&mut term, &chunk);
        }
        let secs = start.elapsed().as_secs_f64();
        let mb = (chunk.len() * reps) as f64 / 1e6;
        println!("parsed {mb:.0} MB in {secs:.2}s = {:.0} MB/s", mb / secs);
    }
}
