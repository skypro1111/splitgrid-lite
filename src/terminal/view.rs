//! The terminal pane: paints the `Term` grid and turns input into PTY bytes.
//!
//! Each frame locks the `Term` once, copies the visible cells into rows of
//! (text, style runs, background spans), unlocks, and paints: background
//! rectangles, one shaped line per row with glyphs snapped to the cell grid
//! (`force_width`), then the cursor. gpui caches shaped lines across frames,
//! so rows that didn't change cost almost nothing.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use futures::StreamExt;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use super::colors;
use super::keys::{self, Mods};
use super::mouse::{self, Action, Button, CellPos, MouseMods};
use super::{GridSize, RemoteStatus, TermEvent, Terminal};

/// The terminal font, as Settings → Appearance names it (Menlo by default).
fn font_family() -> SharedString {
    crate::theme::fonts().terminal
}
/// SplitGrid's default terminal size before Settings → Appearance changes it; each pane can zoom (8–28).
pub const DEFAULT_FONT_SIZE: f32 = 13.;
pub const MIN_FONT_SIZE: f32 = 8.;
pub const MAX_FONT_SIZE: f32 = 28.;
/// How long a "paste saved password" offer stands.
const OFFER_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// xterm's `lineHeight: 1`: a row is the font's own height (ascent + descent), rounded up.
const PADDING: f32 = 0.;

/// What the pane's owner hears about.
pub enum TerminalEvent {
    /// The shell exited: the pane can close.
    Exited,
    /// The program set a new window title.
    TitleChanged,
    /// A remote shell's connection state changed.
    Remote(RemoteStatus),
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// A shell that is running, waiting for its view.
pub struct Spawned {
    terminal: Terminal,
    events: futures::channel::mpsc::UnboundedReceiver<TermEvent>,
}

pub struct TerminalView {
    terminal: Terminal,
    focus: FocusHandle,
    title: SharedString,
    exited: bool,
    /// Fractional scroll not yet worth a whole line.
    scroll_rem: f32,
    /// A remote shell's connection state (None for a local shell).
    remote: Option<RemoteStatus>,
    /// The connection's label and whether its offer is the sudo password,
    /// when it offers a saved password at prompts.
    offer: Option<(SharedString, bool)>,
    /// A password prompt is showing and the offer stands (since when).
    offer_since: Option<Instant>,
    /// Show the last frame's build+paint time in the corner (SPLITGRID_HUD=1).
    hud: bool,
    font_size: f32,
    /// Where the grid was painted last frame and with what cell size, for
    /// turning pointer positions into cells.
    layout: Rc<Cell<Option<Layout>>>,
    /// A left-button drag is extending the selection.
    selecting: bool,
    /// The button a mouse-reporting program saw pressed, for drag/release reports.
    reported_button: Option<Button>,
    /// A layout tween is running: don't resize the terminal until it settles.
    resize_hold: bool,
}

/// The grid's bounds on screen and its cell size, as of the last frame.
type Layout = (Bounds<Pixels>, Metrics);

#[derive(Clone, Copy)]
struct Metrics {
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
}

struct Row {
    text: String,
    runs: Vec<TextRun>,
    /// (first column, columns, colour) of every cell run whose background isn't the default.
    bgs: Vec<(usize, usize, Hsla)>,
}

struct Frame {
    rows: Vec<Row>,
    /// (row, column, shape, colour).
    cursor: Option<(usize, usize, CursorShape, Hsla)>,
    metrics: Metrics,
    built_in: std::time::Duration,
}

impl TerminalView {
    /// Start the user's shell in `cwd` (home when None or missing). Spawning
    /// can fail, building the view can't: hand the result to [`Self::new`].
    /// A local shell; `env` is added to the inherited environment.
    pub fn spawn(cwd: Option<std::path::PathBuf>, env: std::collections::HashMap<String, String>) -> anyhow::Result<Spawned> {
        let initial = GridSize { cols: 80, rows: 24, cell_width: 8., cell_height: 17. };
        let (terminal, events) = Terminal::spawn(initial, cwd.filter(|p| p.is_dir()), env)?;
        Ok(Spawned { terminal, events })
    }

    /// Open an SSH shell; the view shows "connecting" until the server answers.
    pub fn connect_ssh(params: crate::ssh::SshParams, check: crate::ssh::HostKeyChecker) -> Spawned {
        let initial = GridSize { cols: 80, rows: 24, cell_width: 8., cell_height: 17. };
        let (terminal, events, ends) = Terminal::remote(initial);
        crate::ssh::start(params, check, ends);
        Spawned { terminal, events }
    }

    pub fn new(spawned: Spawned, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Spawned { terminal, mut events } = spawned;
        let terminal_is_remote = terminal.is_remote();

        // Drain the I/O thread's events. A burst of output sends many wakeups;
        // they collapse into one notify, and gpui renders at most once a frame.
        cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = events.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = events.try_recv() {
                    batch.push(ev);
                }
                let alive = this.update(cx, |this, cx| {
                    for ev in batch {
                        this.handle_event(ev, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            terminal,
            focus: cx.focus_handle(),
            title: "shell".into(),
            exited: false,
            scroll_rem: 0.,
            hud: std::env::var_os("SPLITGRID_HUD").is_some(),
            font_size: crate::theme::fonts().terminal_size,
            layout: Rc::new(Cell::new(None)),
            remote: terminal_is_remote.then_some(RemoteStatus::Connecting),
            offer: None,
            offer_since: None,
            selecting: false,
            reported_button: None,
            resize_hold: false,
        }
    }

    /// Keep the grid (and the shell's window size) as it is while the pane's
    /// bounds tween (layout_anim.rs); released, the next frame resizes once.
    pub fn set_resize_hold(&mut self, hold: bool) {
        self.resize_hold = hold;
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Zoom the pane (clamped to 8–28pt). The grid follows on the next frame.
    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if size != self.font_size {
            self.font_size = size;
            cx.notify();
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The job in the foreground (name, cwd).
    pub fn foreground(&self) -> Option<super::procinfo::Process> {
        self.terminal.foreground()
    }

    /// Type text into the terminal as a paste (bracketed when the program asked for it).
    pub fn paste_text(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.scroll_to_bottom();
        self.terminal.write(keys::paste(text, bracketed));
    }

    pub fn set_password_offer(&mut self, label: SharedString, sudo: bool) {
        self.offer = Some((label, sudo));
    }

    fn dismiss_offer(&mut self, cx: &mut Context<Self>) {
        if self.offer_since.take().is_some() {
            cx.notify();
        }
    }

    /// Write raw bytes (typed text, a key) as if from the keyboard.
    pub fn write_input(&self, bytes: Vec<u8>) {
        self.scroll_to_bottom();
        self.terminal.write(bytes);
    }

    /// The shell's pid; None for a remote shell.
    pub fn shell_pid(&self) -> Option<i32> {
        self.terminal.session_leader()
    }

    /// The grid's (columns, rows).
    pub fn grid_size(&self) -> (usize, usize) {
        let term = self.terminal.term.lock();
        (term.columns(), term.screen_lines())
    }

    /// Scrollback and screen as plain text: wrapped rows joined, trailing
    /// blanks trimmed (what an agent reads with `sg-terminal read`).
    pub fn text(&self) -> String {
        let term = self.terminal.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let mut out = String::new();
        for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            let mut text = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw.iter());
                }
            }
            if cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE) {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                out.push('\n');
            }
        }
        out.trim_end_matches('\n').to_owned()
    }

    /// A remote shell's last reported state; None for a local one.
    pub fn remote_status(&self) -> Option<&RemoteStatus> {
        self.remote.as_ref()
    }

    fn handle_event(&mut self, ev: TermEvent, cx: &mut Context<Self>) {
        let ev = match ev {
            TermEvent::Term(e) => e,
            TermEvent::PasswordPrompt(visible) => {
                if visible && self.offer.is_some() {
                    self.offer_since = Some(Instant::now());
                    // The offer lapses after 30s, as in SplitGrid.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(OFFER_TTL).await;
                        let _ = this.update(cx, |this, cx| {
                            if this.offer_since.is_some_and(|t| t.elapsed() >= OFFER_TTL) {
                                this.dismiss_offer(cx);
                            }
                        });
                    })
                    .detach();
                } else {
                    self.offer_since = None;
                }
                cx.notify();
                return;
            }
            TermEvent::Remote(status) => {
                let clean_exit = status == RemoteStatus::Closed { clean: true };
                self.remote = Some(status.clone());
                cx.emit(TerminalEvent::Remote(status));
                if clean_exit && !self.exited {
                    self.exited = true;
                    cx.emit(TerminalEvent::Exited);
                }
                return;
            }
        };
        match ev {
            Event::Title(t) => {
                self.title = t.into();
                cx.emit(TerminalEvent::TitleChanged);
            }
            Event::ResetTitle => {
                self.title = "shell".into();
                cx.emit(TerminalEvent::TitleChanged);
            }
            // Replies the terminal owes the program (DA, DSR, colour queries…).
            Event::PtyWrite(s) => self.terminal.write(s.into_bytes()),
            Event::ColorRequest(i, fmt) => {
                let colors = *self.terminal.term.lock().colors();
                let rgb = colors[i].unwrap_or_else(|| match i {
                    256 => colors::resolve(Color::Named(NamedColor::Foreground), &colors),
                    257 => colors::resolve(Color::Named(NamedColor::Background), &colors),
                    258 => colors::resolve(Color::Named(NamedColor::Cursor), &colors),
                    i => colors::indexed(i.min(255) as u8),
                });
                self.terminal.write(fmt(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                let size = self.terminal.size.window_size();
                self.terminal.write(fmt(size).into_bytes());
            }
            Event::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            // OSC 52 reads are refused: any program's output (a remote host over
            // SSH, a `cat` of a crafted file) could otherwise read the clipboard.
            // Writes (OSC 52 copy) stay allowed.
            Event::ClipboardLoad(..) => {}
            Event::Exit | Event::ChildExit(_) if !self.exited => {
                self.exited = true;
                cx.emit(TerminalEvent::Exited);
            }
            _ => {}
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        // The saved-password offer: Enter takes it, Escape dismisses it, any other key dismisses it and types.
        if self.offer_since.is_some() && !matches!(ks.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
            let plain = !(m.shift || m.alt || m.control || m.platform);
            match ks.key.as_str() {
                "enter" if plain => {
                    self.offer_since = None;
                    self.terminal.apply_password();
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    self.dismiss_offer(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => self.dismiss_offer(cx),
            }
        }
        let mods = Mods { shift: m.shift, alt: m.alt, ctrl: m.control, cmd: m.platform };

        if mods.cmd {
            // ⌘ with arrows and ⌫ edit the shell's line, like the Electron app
            // (GridTerminal): to its start/end, a word-jump up/down, delete it all.
            let line_edit: Option<&'static [u8]> = if mods.ctrl || mods.alt {
                None
            } else {
                match ks.key.as_str() {
                    "left" => Some(b"\x01"),
                    "right" => Some(b"\x05"),
                    "up" => Some(b"\x1b[1;5A"),
                    "down" => Some(b"\x1b[1;5B"),
                    "backspace" => Some(b"\x15"),
                    _ => None,
                }
            };
            if let Some(bytes) = line_edit {
                self.scroll_to_bottom();
                self.terminal.write(bytes);
                cx.stop_propagation();
                return;
            }
            match ks.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
                        self.scroll_to_bottom();
                        self.terminal.write(keys::paste(&text, bracketed));
                    }
                    cx.stop_propagation();
                }
                "c" => {
                    // Copy the selection; with nothing selected, interrupt (Electron does the same).
                    match self.terminal.term.lock().selection_to_string().filter(|t| !t.is_empty()) {
                        Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                        None => self.terminal.write(&b"\x03"[..]),
                    }
                    cx.stop_propagation();
                }
                "a" => {
                    let mut term = self.terminal.term.lock();
                    let start = GridPoint::new(Line(-(term.history_size() as i32)), Column(0));
                    let end = GridPoint::new(Line(term.screen_lines() as i32 - 1), Column(term.columns() - 1));
                    let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
                    sel.update(end, Side::Right);
                    term.selection = Some(sel);
                    drop(term);
                    cx.notify();
                    cx.stop_propagation();
                }
                "k" => {
                    // Like Terminal.app: clear scrollback, then ask the shell to redraw.
                    self.terminal.term.lock().grid_mut().clear_history();
                    self.terminal.write(&b"\x0c"[..]);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        let app_cursor = self.mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = keys::encode(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            self.terminal.term.lock().selection = None;
            self.scroll_to_bottom();
            self.terminal.write(bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line_height = metrics(cx, self.font_size).line_height;
        let dy = event.delta.pixel_delta(line_height).y / line_height;
        self.scroll_rem += dy;
        let lines = self.scroll_rem.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_rem -= lines as f32;

        let mode = self.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            // The program tracks the mouse: it gets wheel events, one per line.
            if let Some(pos) = self.cell_at(event.position) {
                let button = if lines > 0 { Button::WheelUp } else { Button::WheelDown };
                let mods = mouse_mods(&event.modifiers);
                let sgr = mode.contains(TermMode::SGR_MOUSE);
                for _ in 0..lines.unsigned_abs() {
                    if let Some(bytes) = mouse::report(button, Action::Press, pos, mods, sgr) {
                        self.terminal.write(bytes);
                    }
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen apps (less, vim) get arrow keys instead of scrollback.
            let key: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.terminal.write(key.repeat(lines.unsigned_abs() as usize));
        } else {
            self.terminal.term.lock().scroll_display(Scroll::Delta(lines));
        }
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        *self.terminal.term.lock().mode()
    }

    /// The viewport cell under a window position, from last frame's layout.
    fn cell_at(&self, pos: Point<Pixels>) -> Option<CellPos> {
        let (bounds, m) = self.layout.get()?;
        let term = self.terminal.term.lock();
        let rel = pos - bounds.origin - point(px(PADDING), px(PADDING));
        Some(mouse::cell_at(rel.x.into(), rel.y.into(), m.cell_width.into(), m.line_height.into(), term.columns(), term.screen_lines()))
    }

    /// A viewport cell as a grid point (scrollback lines are negative).
    fn grid_point(&self, pos: CellPos) -> (GridPoint, Side) {
        let offset = self.terminal.term.lock().grid().display_offset() as i32;
        let side = if pos.right_half { Side::Right } else { Side::Left };
        (GridPoint::new(Line(pos.row as i32 - offset), Column(pos.col)), side)
    }

    /// Mouse reporting applies when the program asked for it and Shift isn't held
    /// (Shift always selects, as in every terminal).
    fn reports_mouse(&self, shift: bool) -> bool {
        !shift && self.mode().intersects(TermMode::MOUSE_MODE)
    }

    fn send_mouse(&self, button: Button, action: Action, pos: CellPos, mods: MouseMods) {
        let sgr = self.mode().contains(TermMode::SGR_MOUSE);
        if let Some(bytes) = mouse::report(button, action, pos, mods, sgr) {
            self.terminal.write(bytes);
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let Some(pos) = self.cell_at(ev.position) else { return };
        let button = match ev.button {
            MouseButton::Left => Button::Left,
            MouseButton::Middle => Button::Middle,
            MouseButton::Right => Button::Right,
            _ => return,
        };
        if self.reports_mouse(ev.modifiers.shift) {
            self.reported_button = Some(button);
            self.send_mouse(button, Action::Press, pos, mouse_mods(&ev.modifiers));
            return;
        }
        if button != Button::Left {
            return;
        }
        let (point, side) = self.grid_point(pos);
        let ty = match ev.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let mut term = self.terminal.term.lock();
        // Shift-click extends the existing selection.
        match term.selection.as_mut() {
            Some(sel) if ev.modifiers.shift && ev.click_count == 1 => sel.update(point, side),
            _ => term.selection = Some(Selection::new(ty, point, side)),
        }
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pos) = self.cell_at(ev.position) else { return };
        if self.selecting && ev.pressed_button == Some(MouseButton::Left) {
            let (point, side) = self.grid_point(pos);
            if let Some(sel) = self.terminal.term.lock().selection.as_mut() {
                sel.update(point, side);
            }
            cx.notify();
            return;
        }
        let mode = self.mode();
        if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) || ev.modifiers.shift {
            return;
        }
        match (self.reported_button, mode.contains(TermMode::MOUSE_MOTION)) {
            (Some(b), _) if ev.pressed_button.is_some() => self.send_mouse(b, Action::Motion, pos, mouse_mods(&ev.modifiers)),
            (_, true) if ev.pressed_button.is_none() => self.send_mouse(Button::None, Action::Motion, pos, mouse_mods(&ev.modifiers)),
            _ => {}
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            // A plain click without a drag leaves no selection behind.
            let mut term = self.terminal.term.lock();
            if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                term.selection = None;
            }
            drop(term);
            cx.notify();
        }
        if let Some(button) = self.reported_button.take()
            && let Some(pos) = self.cell_at(ev.position)
        {
            self.send_mouse(button, Action::Release, pos, mouse_mods(&ev.modifiers));
        }
    }

    fn scroll_to_bottom(&self) {
        let mut term = self.terminal.term.lock();
        if term.grid().display_offset() != 0 {
            term.scroll_display(Scroll::Bottom);
        }
    }
}

fn metrics(cx: &App, font_size: f32) -> Metrics {
    let text = cx.text_system();
    let font_id = text.resolve_font(&font(font_family()));
    let size = px(font_size);
    let cell_width = text.advance(font_id, size, 'm').map(|s| s.width).unwrap_or(px(font_size * 0.6));
    let natural = text.ascent(font_id, size) + text.descent(font_id, size).abs();
    Metrics { font_size: size, cell_width, line_height: gpui_kit::px(f32::from(natural).ceil()) }
}

/// Copy the visible grid into paintable rows. Runs under the `Term` lock: keep it lean.
fn build_frame(view: &TerminalView, metrics: Metrics, focused: bool) -> Frame {
    let started = Instant::now();
    let term = view.terminal.term.lock();
    let content = term.renderable_content();
    let colors = content.colors;
    let offset = content.display_offset as i32;
    let rows_n = term.screen_lines();
    let cols_n = term.columns();
    let default_bg = colors::resolve(Color::Named(NamedColor::Background), colors);

    // The cursor, when it is on screen and the program shows it.
    let cur = content.cursor;
    let cursor_row = cur.point.line.0 + offset;
    let cursor = (cur.shape != CursorShape::Hidden && (0..rows_n as i32).contains(&cursor_row)).then(|| {
        let shape = if focused { cur.shape } else { CursorShape::HollowBlock };
        let color = colors::hsla(colors::resolve(Color::Named(NamedColor::Cursor), colors));
        (cursor_row as usize, cur.point.column.0, shape, color)
    });

    // Under a block cursor the glyph takes the cell's background colour.
    let selection = content.selection;
    // xterm theme selectionBackground #E4E4E41E.
    let selection_bg = Hsla::from(Rgba { r: 0xe4 as f32 / 255., g: 0xe4 as f32 / 255., b: 0xe4 as f32 / 255., a: 0x1e as f32 / 255. });
    let block_at = cursor.and_then(|(r, c, shape, _)| (shape == CursorShape::Block).then_some((r, c)));

    let base = font(font_family());
    let mut rows: Vec<Row> = (0..rows_n)
        .map(|_| Row { text: String::with_capacity(cols_n), runs: Vec::new(), bgs: Vec::new() })
        .collect();

    for indexed in content.display_iter {
        let row_ix = (indexed.point.line.0 + offset) as usize;
        let Some(row) = rows.get_mut(row_ix) else { continue };
        let cell = indexed.cell;
        let col = indexed.point.column.0;

        let bold = cell.flags.contains(Flags::BOLD);
        let (mut fg_c, mut bg_c) = (cell.fg, cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg_c, &mut bg_c);
        }
        let bg = colors::resolve(bg_c, colors);
        let mut fg_h = colors::paint(fg_c, colors, bold);
        if cell.flags.intersects(Flags::DIM) {
            fg_h.a *= 0.5;
        }
        if cell.flags.contains(Flags::HIDDEN) || block_at == Some((row_ix, col)) {
            fg_h = colors::hsla(bg);
        }

        let selected = selection.is_some_and(|s| s.contains(indexed.point));
        if bg != default_bg || selected {
            let bg_h = if selected { selection_bg } else { colors::paint(bg_c, colors, false) };
            match row.bgs.last_mut() {
                Some((start, len, c)) if *start + *len == col && *c == bg_h => *len += 1,
                _ => row.bgs.push((col, 1, bg_h)),
            }
        }

        // A wide glyph's spacer stays a space so every column keeps its slot.
        let ch = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == '\0' { ' ' } else { cell.c };
        let start = row.text.len();
        row.text.push(ch);
        let len = row.text.len() - start;

        let mut f = base.clone();
        if cell.flags.contains(Flags::BOLD) {
            f.weight = FontWeight::BOLD;
        }
        if cell.flags.contains(Flags::ITALIC) {
            f.style = FontStyle::Italic;
        }
        let underline = cell.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(fg_h),
            wavy: cell.flags.contains(Flags::UNDERCURL),
        });
        let strike = cell.flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle { thickness: px(1.), color: Some(fg_h) });

        match row.runs.last_mut() {
            Some(run)
                if run.color == fg_h && run.font == f && run.underline == underline && run.strikethrough == strike =>
            {
                run.len += len
            }
            _ => row.runs.push(TextRun {
                len,
                font: f,
                color: fg_h,
                background_color: None,
                underline,
                strikethrough: strike,
            }),
        }
    }

    drop(term);
    Frame { rows, cursor, metrics, built_in: started.elapsed() }
}

fn paint_frame(bounds: Bounds<Pixels>, frame: Frame, hud: bool, window: &mut Window, cx: &mut App) {
    let started = Instant::now();
    let Metrics { cell_width, line_height, .. } = frame.metrics;
    let origin = bounds.origin + point(px(PADDING), px(PADDING));
    let cell_origin = |row: usize, col: usize| origin + point(cell_width * col as f32, line_height * row as f32);

    for (r, row) in frame.rows.iter().enumerate() {
        for &(col, len, color) in &row.bgs {
            window.paint_quad(fill(Bounds::new(cell_origin(r, col), size(cell_width * len as f32, line_height)), color));
        }
    }

    // The cursor block goes under the text so the glyph stays readable on top.
    if let Some((line, column, shape, color)) = frame.cursor {
        let at = cell_origin(line, column);
        let block = Bounds::new(at, size(cell_width, line_height));
        match shape {
            CursorShape::Block => window.paint_quad(fill(block, color)),
            CursorShape::Underline => {
                window.paint_quad(fill(Bounds::new(at + point(px(0.), line_height - px(2.)), size(cell_width, px(2.))), color))
            }
            CursorShape::Beam => window.paint_quad(fill(Bounds::new(at, size(px(2.), line_height)), color)),
            _ => window.paint_quad(outline(block, color, BorderStyle::Solid)),
        }
    }

    let font_size = frame.metrics.font_size;
    for (r, row) in frame.rows.into_iter().enumerate() {
        if row.text.trim_end().is_empty() {
            continue;
        }
        let shaped = window.text_system().shape_line(row.text.into(), font_size, &row.runs, Some(cell_width));
        let _ = shaped.paint(cell_origin(r, 0), line_height, TextAlign::Left, None, window, cx);
    }

    if hud {
        let total = frame.built_in + started.elapsed();
        let label = format!("{:.2} ms", total.as_secs_f64() * 1000.);
        let run = TextRun {
            len: label.len(),
            font: font(font_family()),
            color: hsla(0.33, 0.8, 0.6, 1.),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(label.into(), px(11.), &[run], None);
        let at = point(bounds.right() - shaped.width - px(8.), bounds.top() + px(4.));
        let _ = shaped.paint(at, px(14.), TextAlign::Left, None, window, cx);
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let hud = self.hud;
        let font_size = self.font_size;
        let focused = self.focus.is_focused(window);

        let body = canvas(
            move |bounds, window, cx| {
                let m = metrics(cx, font_size);
                let inner = bounds.size - size(px(PADDING * 2.), px(PADDING * 2.));
                let cols = (inner.width / m.cell_width).floor().max(2.) as u16;
                let rows = (inner.height / m.line_height).floor().max(1.) as u16;
                let grid = GridSize { cols, rows, cell_width: m.cell_width.into(), cell_height: m.line_height.into() };
                entity.update(cx, |view, _| {
                    if !view.resize_hold {
                        view.terminal.resize(grid);
                    }
                    view.layout.set(Some((bounds, m)));
                });
                let _ = window;
                build_frame(entity.read(cx), m, focused)
            },
            move |bounds, frame, window, cx| paint_frame(bounds, frame, hud, window, cx),
        )
        .size_full();

        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .overflow_hidden()
            .bg(colors::hsla(colors::resolve(Color::Named(NamedColor::Background), &Default::default())))
            .when(self.exited, |d| d.opacity(0.6))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(body)
            .children(self.render_offer(cx))
    }
}

impl TerminalView {
    /// The "↵ Enter to paste saved password" chip, at the cursor (SshPasswordHint).
    fn render_offer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.offer_since?;
        let (label, sudo) = self.offer.clone()?;
        let (_, m) = self.layout.get()?;
        let term = self.terminal.term.lock();
        let cursor = term.grid().cursor.point;
        let offset = term.grid().display_offset() as i32;
        drop(term);
        let row = (cursor.line.0 + offset).max(0) as f32;
        let top = m.line_height * row + px(PADDING);
        let left = m.cell_width * cursor.column.0 as f32 + px(PADDING + 8.);
        let gray = |a: f32| Hsla::from(Rgba { r: 1., g: 1., b: 1., a });
        Some(
            div()
                .absolute()
                .top(top)
                .left(left)
                .h(m.line_height)
                .max_w(relative(0.9))
                .px(px(7.))
                .rounded(px(5.))
                .bg(Hsla::from(Rgba { r: 20. / 255., g: 20. / 255., b: 20. / 255., a: 0.82 }))
                .border_1()
                .border_color(gray(0.06))
                .flex()
                .items_center()
                .gap(px(7.))
                .text_size(px(11.))
                .font_family(crate::theme::ui_font())
                .child(
                    div()
                        .px(px(7.))
                        .py(px(1.))
                        .rounded(px(5.))
                        .bg(gray(0.13))
                        .max_w(px(180.))
                        .truncate()
                        .text_color(Hsla::from(Rgba { r: 0.83, g: 0.83, b: 0.83, a: 1. }))
                        .child(label),
                )
                .child(
                    div()
                        .id("offer-apply")
                        .flex()
                        .cursor_pointer()
                        .text_color(Hsla::from(Rgba { r: 0.478, g: 0.475, b: 0.478, a: 1. }))
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(Hsla::from(Rgba { r: 0.83, g: 0.83, b: 0.83, a: 1. })).child("↵ Enter"))
                        .child(if sudo { " to paste saved sudo password" } else { " to paste saved password" })
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            // Keep the terminal focused.
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.offer_since = None;
                            this.terminal.apply_password();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }
}

fn mouse_mods(m: &Modifiers) -> MouseMods {
    MouseMods { shift: m.shift, alt: m.alt, ctrl: m.control }
}
