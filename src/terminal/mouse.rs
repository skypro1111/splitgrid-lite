//! Mouse → terminal: which cell a pixel is in, and the escape sequences that
//! report mouse events to programs that asked for them (vim, htop, less…).
//! Pure, so both are unit-tested.

/// A pointer position in viewport cells: 0-based column and row, plus whether it
/// is in the cell's right half (selection snaps by halves, like every terminal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPos {
    pub col: usize,
    pub row: usize,
    pub right_half: bool,
}

/// `x`, `y` relative to the grid's top-left, in pixels. Clamped into the grid.
pub fn cell_at(x: f32, y: f32, cell_w: f32, cell_h: f32, cols: usize, rows: usize) -> CellPos {
    let fx = (x / cell_w).max(0.);
    let col = (fx.floor() as usize).min(cols.saturating_sub(1));
    let row = ((y / cell_h).max(0.).floor() as usize).min(rows.saturating_sub(1));
    CellPos { col, row, right_half: fx - fx.floor() >= 0.5 || fx.floor() as usize >= cols }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// Motion with no button held (any-event tracking).
    None,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// The xterm button code: base button, +32 for motion, +4/8/16 for modifiers.
fn code(button: Button, action: Action, mods: MouseMods, sgr: bool) -> u8 {
    let base = match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::None => 3,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    // Legacy encoding has no release per button: release is "button 3".
    let base = if action == Action::Release && !sgr { 3 } else { base };
    let motion = if action == Action::Motion { 32 } else { 0 };
    base + motion + 4 * mods.shift as u8 + 8 * mods.alt as u8 + 16 * mods.ctrl as u8
}

/// The report for one event at a 0-based cell; None when the legacy encoding
/// can't express the position (beyond column/row 223).
pub fn report(button: Button, action: Action, pos: CellPos, mods: MouseMods, sgr: bool) -> Option<Vec<u8>> {
    let b = code(button, action, mods, sgr);
    let (x, y) = (pos.col + 1, pos.row + 1);
    if sgr {
        let end = if action == Action::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{b};{x};{y}{end}").into_bytes());
    }
    if x > 223 || y > 223 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', 32 + b, 32 + x as u8, 32 + y as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: MouseMods = MouseMods { shift: false, alt: false, ctrl: false };

    #[test]
    fn pixels_to_cells() {
        assert_eq!(cell_at(0., 0., 8., 16., 80, 24), CellPos { col: 0, row: 0, right_half: false });
        assert_eq!(cell_at(13., 17., 8., 16., 80, 24), CellPos { col: 1, row: 1, right_half: true });
        // Outside the grid clamps to its edge.
        let p = cell_at(10_000., -5., 8., 16., 80, 24);
        assert_eq!((p.col, p.row), (79, 0));
    }

    #[test]
    fn sgr_reports() {
        let at = CellPos { col: 4, row: 9, right_half: false };
        assert_eq!(report(Button::Left, Action::Press, at, NONE, true).unwrap(), b"\x1b[<0;5;10M");
        assert_eq!(report(Button::Left, Action::Release, at, NONE, true).unwrap(), b"\x1b[<0;5;10m");
        assert_eq!(report(Button::Left, Action::Motion, at, NONE, true).unwrap(), b"\x1b[<32;5;10M");
        assert_eq!(report(Button::WheelDown, Action::Press, at, MouseMods { ctrl: true, ..NONE }, true).unwrap(), b"\x1b[<81;5;10M");
    }

    #[test]
    fn legacy_reports() {
        let at = CellPos { col: 0, row: 0, right_half: false };
        assert_eq!(report(Button::Right, Action::Press, at, NONE, false).unwrap(), [0x1b, b'[', b'M', 34, 33, 33]);
        assert_eq!(report(Button::Right, Action::Release, at, NONE, false).unwrap(), [0x1b, b'[', b'M', 35, 33, 33]);
        assert!(report(Button::Left, Action::Press, CellPos { col: 300, row: 0, right_half: false }, NONE, false).is_none());
    }
}
