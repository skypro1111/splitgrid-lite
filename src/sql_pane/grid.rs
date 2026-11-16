//! The result grid of the SQL pane: the Electron DataGrid's look, DataGrip's
//! data-editor behaviour. Virtualized both ways by hand — only the rows and
//! columns inside the viewport become elements, positioned from our own
//! scroll offsets, so the header and the row-number gutter stay put for free.
//! Hit-testing is arithmetic on one listener per layer instead of a listener
//! per cell. The data, staging and math live in `model` (window-free, tested).

mod model;

pub use model::parse_input;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;
use sql_core::{Column, ForeignKey, Row, TableRef, Value};
use sql_lang::{EditSource, RowEdit};

use crate::assets::icon;
use crate::theme::*;
use model::{GridModel, Kind, ROW_H, RowState, Selection};

const HEADER_H: f32 = 26.;
const FONT_SIZE: f32 = 12.;
const TYPE_FONT_SIZE: f32 = 10.5;
const ICON: f32 = 11.;
const SCROLLBAR: f32 = 8.;
/// Grab width of a header's resize edge.
const RESIZE_GRAB: f32 = 6.;

const ZEBRA: Rgba = hexa(0xffffff, 0.022);
const SELECTED: Rgba = hexa(0x228df2, 0.28);
const GUTTER_SELECTED: Rgba = hexa(0x228df2, 0.4);
const MODIFIED: Rgba = hexa(0xe0a23a, 0.22);
const INSERTED: Rgba = hexa(0x15ac91, 0.12);
const DELETED: Rgba = hexa(0xf14c4c, 0.14);
const THUMB: Rgba = hexa(0xffffff, 0.18);

actions!(result_grid, [SelectAll, CopyCells, CopyWithHeaders, AddRow, DeleteRows, SetNull]);

/// Bound once, on the first grid: the grid's context beats the app's global
/// ⌘N / ⇧⌘N, so these need bindings rather than key-down handling.
struct KeysBound;
impl Global for KeysBound {}

fn bind_keys(cx: &mut App) {
    if cx.has_global::<KeysBound>() {
        return;
    }
    cx.set_global(KeysBound);
    const CTX: Option<&str> = Some("ResultGrid");
    cx.bind_keys([
        KeyBinding::new("cmd-a", SelectAll, CTX),
        KeyBinding::new("cmd-c", CopyCells, CTX),
        KeyBinding::new("cmd-shift-c", CopyWithHeaders, CTX),
        KeyBinding::new("cmd-n", AddRow, CTX),
        KeyBinding::new("cmd-backspace", DeleteRows, CTX),
        KeyBinding::new("cmd-delete", DeleteRows, CTX),
        KeyBinding::new("cmd-shift-n", SetNull, CTX),
    ]);
}

pub enum GridEvent {
    /// Pending edits changed (count, for the toolbar's Submit/Revert).
    EditsChanged(usize),
    /// The user clicked the → on a foreign-key cell: open `table` filtered by these column values.
    OpenReference { table: TableRef, filter: Vec<(String, Value)> },
    /// Selected cell changed (row, col) — the value panel follows it.
    Selected(Option<(usize, usize)>),
}

impl EventEmitter<GridEvent> for ResultGrid {}

/// Text widths for autosizing, from the font's advances (ASCII cached).
struct Measure {
    text: Arc<TextSystem>,
    font: FontId,
    ascii: [f32; 128],
}

impl Measure {
    fn new(cx: &App) -> Self {
        let text = cx.text_system().clone();
        let font = text.resolve_font(&font(crate::theme::ui_font()));
        let advance = |ch: char| text.advance(font, gpui_kit::px(FONT_SIZE), ch).map(|s| f32::from(s.width)).unwrap_or(FONT_SIZE * 0.6);
        let ascii = std::array::from_fn(|i| advance(i as u8 as char));
        Measure { text: text.clone(), font, ascii }
    }

    /// Width of `s` at `size` (advances scale linearly with the size).
    fn width(&self, s: &str, size: f32) -> f32 {
        let w: f32 = s
            .chars()
            .map(|ch| match self.ascii.get(ch as usize) {
                Some(w) => *w,
                None => self.text.advance(self.font, gpui_kit::px(FONT_SIZE), ch).map(|s| f32::from(s.width)).unwrap_or(FONT_SIZE),
            })
            .sum();
        w * size / FONT_SIZE
    }
}

struct CellEditor {
    row: usize,
    col: usize,
    input: Entity<InputState>,
    _sub: Subscription,
}

#[derive(Clone, Copy)]
enum Drag {
    Cells,
    Rows,
    Cols,
    Resize { col: usize, start_x: f32, start_w: f32 },
    /// Pointer offset from the thumb's top/left edge.
    VBar { grab: f32 },
    HBar { grab: f32 },
}

pub struct ResultGrid {
    model: GridModel,
    kinds: Vec<Kind>,
    widths: Vec<f32>,
    /// `model::offsets(&widths)`, kept in step with `widths`.
    offsets: Vec<f32>,
    /// Columns the user sized: autosize leaves them alone.
    resized: Vec<bool>,
    /// Rows the automatic widths have seen so far (up to AUTOSIZE_SAMPLE).
    sampled: usize,
    measure: Measure,
    scroll_x: f32,
    scroll_y: f32,
    /// The grid's window bounds as of the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    selection: Option<Selection>,
    focus: FocusHandle,
    editor: Option<CellEditor>,
    drag: Option<Drag>,
    menu: Option<Point<Pixels>>,
}

impl ResultGrid {
    pub(crate) fn has_menu(&self) -> bool {
        self.menu.is_some()
    }

    pub fn new(columns: Vec<Column>, cx: &mut Context<Self>) -> Self {
        bind_keys(cx);
        let n = columns.len();
        let mut this = ResultGrid {
            kinds: columns.iter().map(|c| Kind::of(&c.type_name)).collect(),
            model: GridModel::new(columns),
            widths: vec![model::MIN_COL_W; n],
            offsets: Vec::new(),
            resized: vec![false; n],
            sampled: 0,
            measure: Measure::new(cx),
            scroll_x: 0.,
            scroll_y: 0.,
            bounds: Rc::default(),
            selection: None,
            focus: cx.focus_handle(),
            editor: None,
            drag: None,
            menu: None,
        };
        this.autosize();
        this
    }

    /// A streamed batch. Widths follow the content until AUTOSIZE_SAMPLE rows are in.
    pub fn append_rows(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        if rows.is_empty() {
            return;
        }
        self.model.append(rows);
        if self.sampled < model::AUTOSIZE_SAMPLE {
            self.autosize();
        }
        cx.notify();
    }

    pub fn row_count(&self) -> usize {
        self.model.row_count()
    }

    pub fn columns(&self) -> &[Column] {
        self.model.columns()
    }

    /// The current (edited) value; None out of range or for a new row's DEFAULT cell.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Value> {
        self.model.cell(row, col)
    }

    /// Make the grid editable (None = read-only). `references`: for a result column index, the FK it belongs to.
    pub fn set_edit_source(&mut self, source: Option<EditSource>, references: Vec<(usize, ForeignKey)>, cx: &mut Context<Self>) {
        let before = self.model.pending_count();
        self.model.set_source(source, references);
        self.editor = None;
        self.after_rows_changed(cx);
        if before != self.model.pending_count() {
            self.edits_changed(cx);
        }
        // Key/link icons change the header widths.
        self.sampled = 0;
        self.autosize();
        cx.notify();
    }

    pub fn pending_edits(&self) -> Vec<RowEdit> {
        self.model.pending_edits()
    }

    pub fn revert(&mut self, cx: &mut Context<Self>) {
        self.model.revert();
        self.editor = None;
        self.after_rows_changed(cx);
        self.edits_changed(cx);
    }

    /// After a successful submit: staged values become the base, deleted rows go, inserted rows stay.
    pub fn commit_edits(&mut self, cx: &mut Context<Self>) {
        self.model.commit();
        self.editor = None;
        self.after_rows_changed(cx);
        self.edits_changed(cx);
    }

    pub fn add_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.model.add_row() else { return };
        let col = (0..self.model.columns().len()).find(|&c| self.model.column_editable(c)).unwrap_or(0);
        self.set_active((row, col), false, cx);
        self.edits_changed(cx);
    }

    pub fn delete_selected_rows(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else { return };
        if self.model.delete_rows(sel.rows()) {
            self.editor = None;
            self.after_rows_changed(cx);
            self.edits_changed(cx);
        }
    }

    /// Used by the value panel; ignored for read-only cells.
    pub fn set_cell(&mut self, row: usize, col: usize, value: Value, cx: &mut Context<Self>) {
        if self.model.set(row, col, value) {
            self.edits_changed(cx);
        }
    }

    pub fn selected(&self) -> Option<(usize, usize)> {
        self.selection.map(|s| s.active)
    }

    /// TSV to the clipboard, without headers.
    pub fn copy_selection(&self, cx: &mut App) {
        self.copy(false, cx);
    }

    fn copy(&self, headers: bool, cx: &mut App) {
        if let Some(sel) = self.selection {
            cx.write_to_clipboard(ClipboardItem::new_string(self.model.tsv(&sel, headers)));
        }
    }

    fn set_null(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else { return };
        let mut changed = false;
        for r in sel.rows() {
            for c in sel.cols() {
                changed |= self.model.set(r, c, Value::Null);
            }
        }
        if changed {
            self.edits_changed(cx);
        }
    }

    fn edits_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(GridEvent::EditsChanged(self.model.pending_count()));
        cx.notify();
    }

    /// Rows came or went: keep the selection inside the grid.
    fn after_rows_changed(&mut self, cx: &mut Context<Self>) {
        let sel = self.selection.and_then(|s| s.clamp(self.model.row_count(), self.model.columns().len()));
        self.select(sel, cx);
    }

    // ── Widths ──────────────────────────────────────────────────────────────

    fn header_width(&self, col: usize) -> f32 {
        let c = &self.model.columns()[col];
        let icons = [self.model.is_key_column(col), self.model.foreign_key(col).is_some()].iter().filter(|&&b| b).count() as f32;
        // Header names are semibold: a touch wider than the regular advances.
        self.measure.width(&c.name, FONT_SIZE) * 1.06 + 6. + self.measure.width(&c.type_name, TYPE_FONT_SIZE) + icons * (ICON + 4.)
    }

    fn fit_width(&self, col: usize, rows: impl Iterator<Item = usize>) -> f32 {
        let cells = rows.filter_map(|r| self.model.cell(r, col)).map(|v| self.measure.width(&model::cell_text(v), FONT_SIZE));
        model::auto_width(self.header_width(col), cells)
    }

    fn autosize(&mut self) {
        let upto = self.model.row_count().min(model::AUTOSIZE_SAMPLE);
        for col in 0..self.widths.len() {
            if !self.resized[col] {
                self.widths[col] = self.fit_width(col, 0..upto);
            }
        }
        self.sampled = upto;
        self.offsets = model::offsets(&self.widths);
    }

    fn set_width(&mut self, col: usize, w: f32) {
        self.widths[col] = w.max(model::MIN_COL_W);
        self.resized[col] = true;
        self.offsets = model::offsets(&self.widths);
    }

    // ── Geometry ────────────────────────────────────────────────────────────

    fn gutter_w(&self) -> f32 {
        let digits = self.model.row_count().max(1).ilog10() + 1;
        (digits as f32 * 7.5 + 18.).max(36.)
    }

    /// The cell area's size (viewport minus header and gutter).
    fn body_size(&self) -> (f32, f32) {
        let b = self.bounds.get();
        ((crate::ui::unpx(b.size.width) - self.gutter_w()).max(0.), (crate::ui::unpx(b.size.height) - HEADER_H).max(0.))
    }

    fn content_size(&self) -> (f32, f32) {
        (*self.offsets.last().unwrap_or(&0.), self.model.row_count() as f32 * ROW_H)
    }

    fn clamp_scroll(&mut self) {
        let (bw, bh) = self.body_size();
        let (cw, ch) = self.content_size();
        self.scroll_x = self.scroll_x.clamp(0., model::max_scroll(cw, bw));
        self.scroll_y = self.scroll_y.clamp(0., model::max_scroll(ch, bh));
    }

    /// A window position in content coordinates (relative to the first cell, scroll applied).
    fn to_content(&self, pos: Point<Pixels>) -> (f32, f32) {
        let o = self.bounds.get().origin;
        (crate::ui::unpx(pos.x - o.x) - self.gutter_w() + self.scroll_x, crate::ui::unpx(pos.y - o.y) - HEADER_H + self.scroll_y)
    }

    /// The cell at a content position, clamped into the grid (for drags past its edges).
    fn cell_at(&self, (x, y): (f32, f32)) -> Option<(usize, usize)> {
        let (rows, cols) = (self.model.row_count(), self.model.columns().len());
        if rows == 0 || cols == 0 {
            return None;
        }
        let row = ((y.max(0.) / ROW_H) as usize).min(rows - 1);
        let col = model::col_at(&self.offsets, x).unwrap_or(if x < 0. { 0 } else { cols - 1 });
        Some((row, col))
    }

    fn reveal(&mut self, (row, col): (usize, usize)) {
        let (bw, bh) = self.body_size();
        self.scroll_y = model::scroll_into_view(self.scroll_y, row as f32 * ROW_H, (row + 1) as f32 * ROW_H, bh);
        if col + 1 < self.offsets.len() {
            self.scroll_x = model::scroll_into_view(self.scroll_x, self.offsets[col], self.offsets[col + 1], bw);
        }
        self.clamp_scroll();
    }

    fn page_rows(&self) -> usize {
        ((self.body_size().1 / ROW_H) as usize).max(1)
    }

    // ── Selection ───────────────────────────────────────────────────────────

    fn select(&mut self, sel: Option<Selection>, cx: &mut Context<Self>) {
        let before = self.selected();
        self.selection = sel;
        if self.selected() != before {
            cx.emit(GridEvent::Selected(self.selected()));
        }
        cx.notify();
    }

    /// Moves the active cell (extending from the anchor with shift) and scrolls to it.
    fn set_active(&mut self, cell: (usize, usize), extend: bool, cx: &mut Context<Self>) {
        let sel = match self.selection {
            Some(s) if extend => Selection { anchor: s.anchor, active: cell },
            _ => Selection::cell(cell.0, cell.1),
        };
        self.select(Some(sel), cx);
        self.reveal(cell);
    }

    /// Whole rows `from..=to`; the active cell sits in the first column.
    fn select_rows(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let last = self.model.columns().len().saturating_sub(1);
        self.select(Some(Selection { anchor: (from, last), active: (to, 0) }), cx);
        self.reveal((to, self.col_in_view()));
    }

    fn select_cols(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let Some(last) = self.model.row_count().checked_sub(1) else { return };
        self.select(Some(Selection { anchor: (last, from), active: (0, to) }), cx);
    }

    /// A column already on screen, so row-wise reveals don't scroll sideways.
    fn col_in_view(&self) -> usize {
        model::visible_cols(&self.offsets, self.scroll_x, self.body_size().0).start
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let (rows, cols) = (self.model.row_count(), self.model.columns().len());
        if rows > 0 && cols > 0 {
            self.select(Some(Selection { anchor: (rows - 1, cols - 1), active: (0, 0) }), cx);
        }
    }

    // ── Editing ─────────────────────────────────────────────────────────────

    /// Opens the inline editor on the active cell, with `typed` replacing the
    /// value (typing starts an edit) or the value itself (Enter, double-click).
    fn begin_edit(&mut self, typed: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.selected() else { return };
        if !self.model.column_editable(col) || self.model.row_state(row) == RowState::Deleted {
            return;
        }
        let Some(text) = typed.or_else(|| model::edit_text(self.model.cell(row, col))) else { return };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |i, cx| {
            i.set_value(text, window, cx);
            i.focus(window, cx);
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => this.commit_edit(None, window, cx),
            InputEvent::Blur => this.commit_edit(None, window, cx),
            _ => {}
        });
        self.menu = None;
        self.editor = Some(CellEditor { row, col, input, _sub: sub });
        self.reveal((row, col));
        cx.notify();
    }

    /// Stages the editor's text, then moves by `step` (Tab) and returns focus to the grid.
    fn commit_edit(&mut self, step: Option<isize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.input.read(cx).value();
        let original = self.model.original(ed.row, ed.col).cloned();
        let value = model::parse_input(&text, &self.model.columns()[ed.col].type_name, original.as_ref());
        // An untouched DEFAULT cell of a new row stays DEFAULT when nothing was typed.
        let untouched_default = self.model.cell(ed.row, ed.col).is_none() && text.is_empty();
        if !untouched_default && self.model.set(ed.row, ed.col, value) {
            self.edits_changed(cx);
        }
        if let Some(step) = step {
            self.move_by(0, step, false, true, cx);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    // ── Keyboard ────────────────────────────────────────────────────────────

    /// Arrow-style movement; `wrap` carries Tab across row ends.
    fn move_by(&mut self, dr: isize, dc: isize, extend: bool, wrap: bool, cx: &mut Context<Self>) {
        let (rows, cols) = (self.model.row_count() as isize, self.model.columns().len() as isize);
        if rows == 0 || cols == 0 {
            return;
        }
        let (r, c) = self.selected().map_or((0, 0), |(r, c)| (r as isize, c as isize));
        let (mut r, mut c) = (r + dr, c + dc);
        if wrap {
            if c >= cols && r + 1 < rows {
                (r, c) = (r + 1, 0);
            } else if c < 0 && r > 0 {
                (r, c) = (r - 1, cols - 1);
            }
        }
        self.set_active((r.clamp(0, rows - 1) as usize, c.clamp(0, cols - 1) as usize), extend, cx);
    }

    /// While a cell is being edited, Esc / Tab / Enter belong to the grid, not
    /// the inline input (which would swallow Esc and Tab): caught in the capture phase.
    fn on_edit_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_none() {
            return;
        }
        let k = &ev.keystroke;
        match k.key.as_str() {
            "escape" => self.cancel_edit(window, cx),
            "tab" => self.commit_edit(Some(if k.modifiers.shift { -1 } else { 1 }), window, cx),
            "enter" if !k.modifiers.shift => self.commit_edit(None, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// A click anywhere in the grid but the edited cell ends the edit first (staging it).
    fn end_edit_on_click(&mut self, cell: Option<(usize, usize)>, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_some_and(|e| Some((e.row, e.col)) != cell) {
            self.commit_edit(None, window, cx);
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if self.editor.is_some() {
            return;
        }
        let (rows, cols) = (self.model.row_count() as isize, self.model.columns().len() as isize);
        let page = self.page_rows() as isize;
        let at = self.selected().map_or((0, 0), |(r, c)| (r as isize, c as isize));
        let ext = m.shift;
        match k.key.as_str() {
            "up" if m.platform => self.move_by(-at.0, 0, ext, false, cx),
            "down" if m.platform => self.move_by(rows - at.0, 0, ext, false, cx),
            "left" if m.platform => self.move_by(0, -at.1, ext, false, cx),
            "right" if m.platform => self.move_by(0, cols - at.1, ext, false, cx),
            "up" => self.move_by(-1, 0, ext, false, cx),
            "down" => self.move_by(1, 0, ext, false, cx),
            "left" => self.move_by(0, -1, ext, false, cx),
            "right" => self.move_by(0, 1, ext, false, cx),
            "home" if m.platform || m.control => self.move_by(-at.0, -at.1, ext, false, cx),
            "end" if m.platform || m.control => self.move_by(rows - at.0, cols - at.1, ext, false, cx),
            "home" => self.move_by(0, -at.1, ext, false, cx),
            "end" => self.move_by(0, cols - at.1, ext, false, cx),
            "pageup" => self.move_by(-page, 0, ext, false, cx),
            "pagedown" => self.move_by(page, 0, ext, false, cx),
            "tab" => self.move_by(0, if m.shift { -1 } else { 1 }, false, true, cx),
            "enter" | "f2" if !m.platform => self.begin_edit(None, window, cx),
            "escape" if self.menu.is_some() => {
                self.menu = None;
                cx.notify();
            }
            "escape" => {
                if let Some((r, c)) = self.selected() {
                    self.select(Some(Selection::cell(r, c)), cx);
                }
            }
            _ => match k.key_char.as_ref().filter(|ch| !m.platform && !m.control && !m.function && !ch.chars().any(char::is_control)) {
                Some(ch) => self.begin_edit(Some(ch.clone()), window, cx),
                None => return,
            },
        }
        cx.stop_propagation();
    }

    // ── Mouse ───────────────────────────────────────────────────────────────

    fn on_cell_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let pos = self.to_content(ev.position);
        let cell = self.cell_at(pos).filter(|_| pos.1 < self.content_size().1); // not below the last row
        self.end_edit_on_click(cell, window, cx);
        let Some(cell) = cell else { return };
        if self.editor.is_some() {
            return; // a click inside the edited cell
        }
        self.focus.focus(window, cx);
        if ev.button == MouseButton::Right {
            if !self.selection.is_some_and(|s| s.contains(cell.0, cell.1)) {
                self.set_active(cell, false, cx);
            }
            self.menu = Some(ev.position);
            cx.notify();
            return;
        }
        self.set_active(cell, ev.modifiers.shift, cx);
        if ev.click_count == 2 {
            self.begin_edit(None, window, cx);
        } else {
            self.drag = Some(Drag::Cells);
        }
    }

    fn on_gutter_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.end_edit_on_click(None, window, cx);
        self.focus.focus(window, cx);
        let (_, y) = self.to_content(ev.position);
        let Some((row, _)) = self.cell_at((0., y)).filter(|_| y < self.content_size().1) else { return };
        let from = match self.selection {
            Some(s) if ev.modifiers.shift => s.anchor.0,
            _ => row,
        };
        self.select_rows(from, row, cx);
        if ev.button == MouseButton::Right {
            self.menu = Some(ev.position);
        } else {
            self.drag = Some(Drag::Rows);
        }
    }

    fn on_header_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.end_edit_on_click(None, window, cx);
        self.focus.focus(window, cx);
        let (x, _) = self.to_content(ev.position);
        let Some(col) = model::col_at(&self.offsets, x) else { return };
        let from = match self.selection {
            Some(s) if ev.modifiers.shift => s.anchor.1,
            _ => col,
        };
        self.select_cols(from, col, cx);
        self.drag = Some(Drag::Cols);
    }

    fn on_resize_down(&mut self, col: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if ev.click_count == 2 {
            let visible = model::visible_rows(self.scroll_y, self.body_size().1, self.model.row_count());
            let sample = 0..self.model.row_count().min(model::AUTOSIZE_SAMPLE);
            let w = self.fit_width(col, sample.chain(visible));
            self.set_width(col, w);
            self.drag = None;
        } else {
            self.drag = Some(Drag::Resize { col, start_x: crate::ui::unpx(ev.position.x), start_w: self.widths[col] });
        }
        cx.notify();
    }

    fn on_drag_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        let (bw, bh) = self.body_size();
        let (cw, ch) = self.content_size();
        match drag {
            Drag::Resize { col, start_x, start_w } => self.set_width(col, start_w + crate::ui::unpx(pos.x) - start_x),
            Drag::VBar { grab } => {
                let o = self.bounds.get().origin;
                let (thumb, track) = thumb(bh, ch);
                let y = crate::ui::unpx(pos.y - o.y) - HEADER_H - grab;
                self.scroll_y = y / (bh - thumb).max(1.) * track;
            }
            Drag::HBar { grab } => {
                let o = self.bounds.get().origin;
                let (thumb, track) = thumb(bw, cw);
                let x = crate::ui::unpx(pos.x - o.x) - self.gutter_w() - grab;
                self.scroll_x = x / (bw - thumb).max(1.) * track;
            }
            Drag::Cells | Drag::Rows | Drag::Cols => {
                let (x, y) = self.to_content(pos);
                let Some(cell) = self.cell_at((x, y)) else { return };
                let Some(sel) = self.selection else { return };
                let active = match drag {
                    Drag::Rows => (cell.0, 0),
                    Drag::Cols => (0, cell.1),
                    _ => cell,
                };
                self.selection = Some(Selection { anchor: sel.anchor, active });
                // Dragging past an edge scrolls toward it.
                let (sx, sy) = (x - self.scroll_x, y - self.scroll_y);
                if !matches!(drag, Drag::Rows) {
                    self.scroll_x += if sx < 0. { sx } else { (sx - bw).max(0.) };
                }
                if !matches!(drag, Drag::Cols) {
                    self.scroll_y += if sy < 0. { sy } else { (sy - bh).max(0.) };
                }
                if active != sel.active {
                    cx.emit(GridEvent::Selected(Some(active)));
                }
            }
        }
        self.clamp_scroll();
        cx.notify();
    }

    fn on_wheel(&mut self, ev: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let d = ev.delta.pixel_delta(px(ROW_H));
        self.scroll_x -= crate::ui::unpx(d.x);
        self.scroll_y -= crate::ui::unpx(d.y);
        self.clamp_scroll();
        cx.stop_propagation();
        cx.notify();
    }

    fn open_reference(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        if let Some((table, filter)) = self.model.reference(row, col) {
            cx.emit(GridEvent::OpenReference { table, filter });
        }
    }
}

/// Scrollbar thumb length and the scrollable range, for a viewport and content length.
fn thumb(view: f32, content: f32) -> (f32, f32) {
    ((view * view / content.max(1.)).clamp(24f32.min(view), view), model::max_scroll(content, view))
}

impl Render for ResultGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.clamp_scroll();
        let gutter = self.gutter_w();
        let (bw, bh) = self.body_size();
        let (cw, ch) = self.content_size();
        let rows = model::visible_rows(self.scroll_y, bh, self.model.row_count());
        let cols = model::visible_cols(&self.offsets, self.scroll_x, bw);
        let row_left = self.offsets.get(cols.start).copied().unwrap_or(0.) - self.scroll_x;
        let row_top = |r: usize| r as f32 * ROW_H - self.scroll_y;
        let sel = self.selection;
        let ncols = self.model.columns().len();

        // Bounds recorder; while dragging, also the window-wide move/up listeners
        // (a drag must follow the pointer outside the grid).
        let rec = self.bounds.clone();
        let entity = cx.entity().downgrade();
        let id = cx.entity_id();
        let dragging = self.drag.is_some();
        let recorder = canvas(
            move |b, window, _| {
                if rec.get() != b {
                    rec.set(b);
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
            },
            move |_, _, window, _| {
                if !dragging {
                    return;
                }
                let e = entity.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        e.update(cx, |this, cx| this.on_drag_move(ev.position, cx)).ok();
                    }
                });
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        entity
                            .update(cx, |this, cx| {
                                this.drag = None;
                                cx.notify();
                            })
                            .ok();
                    }
                });
            },
        )
        .absolute()
        .size_full();

        // Cells.
        let mut body_rows = Vec::with_capacity(rows.len());
        for r in rows.clone() {
            let state = self.model.row_state(r);
            let mut cells = Vec::with_capacity(cols.len());
            for c in cols.clone() {
                let value = self.model.cell(r, c);
                let selected = sel.is_some_and(|s| s.contains(r, c));
                let bg = if selected {
                    Some(SELECTED)
                } else if self.model.is_modified(r, c) {
                    Some(MODIFIED)
                } else {
                    None
                };
                let content = match value {
                    None => div().text_color(TEXT_MUTED).italic().opacity(0.6).child("<default>"),
                    Some(Value::Null) => div().text_color(TEXT_MUTED).italic().opacity(0.6).child("NULL"),
                    Some(v) => div().when(model::right_aligned(v, self.kinds[c]), |d| d.text_right()).child(model::cell_text(v)),
                };
                let link = self.model.foreign_key(c).is_some() && value.is_some_and(|v| !v.is_null());
                cells.push(
                    div()
                        .flex_none()
                        .w(px(self.widths[c]))
                        .h_full()
                        .px(px(model::CELL_PAD_X))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .border_r_1()
                        .border_b_1()
                        .border_color(BORDER_SUBTLE)
                        .when_some(bg, |d, bg| d.bg(bg))
                        .child(content.flex_1().min_w_0().truncate())
                        .when(link, |d| {
                            d.group("sql-grid-cell").child(
                                div()
                                    .id(("sql-grid-ref", r * self.offsets.len() + c))
                                    .flex_none()
                                    .opacity(0.)
                                    .group_hover("sql-grid-cell", |s| s.opacity(1.))
                                    .cursor_pointer()
                                    .child(svg().path(icon("sql-arrow-right")).size(px(ICON)).text_color(ACCENT_BLUE))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.open_reference(r, c, cx);
                                        }),
                                    ),
                            )
                        })
                        .into_any_element(),
                );
            }
            body_rows.push(
                div()
                    .absolute()
                    .top(px(row_top(r)))
                    .left(px(row_left))
                    .h(px(ROW_H))
                    .flex()
                    .when(r % 2 == 1, |d| d.bg(ZEBRA))
                    .map(|d| match state {
                        RowState::Inserted => d.bg(INSERTED),
                        RowState::Deleted => d.bg(DELETED).line_through().text_color(ACCENT_RED),
                        RowState::Base => d,
                    })
                    .children(cells)
                    .into_any_element(),
            );
        }

        // The active cell's outline, and the inline editor over it.
        let cell_box = |(r, c): (usize, usize)| (self.offsets.get(c).copied().unwrap_or(0.) - self.scroll_x, row_top(r), self.widths.get(c).copied().unwrap_or(0.));
        let outline = sel.filter(|_| self.focus.contains_focused(window, cx) && self.editor.is_none()).map(|s| {
            let (x, y, w) = cell_box(s.active);
            div().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(ROW_H)).border_1().border_color(ACCENT)
        });
        let editor = self.editor.as_ref().map(|e| {
            let (x, y, w) = cell_box((e.row, e.col));
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w))
                .h(px(ROW_H))
                .flex()
                .items_center()
                .bg(bg_primary())
                .border_1()
                .border_color(ACCENT)
                // A click outside the grid (the console, the tree…) ends the edit too.
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.commit_edit(None, window, cx)))
                .child(Input::new(&e.input).appearance(false).xsmall())
        });

        let body = div()
            .absolute()
            .top(px(HEADER_H))
            .left(px(gutter))
            .right_0()
            .bottom_0()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_cell_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_cell_down))
            .children(body_rows)
            .children(outline)
            .children(editor);

        // Row numbers.
        let numbers = rows.clone().map(|r| {
            let in_sel = sel.is_some_and(|s| s.rows().contains(&r));
            let whole = in_sel && sel.is_some_and(|s| s.all_cols(ncols));
            let color = match self.model.row_state(r) {
                RowState::Inserted => ACCENT_GREEN,
                RowState::Deleted => ACCENT_RED,
                RowState::Base if in_sel => TEXT_PRIMARY,
                RowState::Base => TEXT_MUTED,
            };
            div()
                .absolute()
                .top(px(row_top(r)))
                .left_0()
                .right_0()
                .h(px(ROW_H))
                .pr(px(6.))
                .flex()
                .items_center()
                .justify_end()
                .border_b_1()
                .border_color(BORDER_SUBTLE)
                .text_color(color)
                .when(whole, |d| d.bg(GUTTER_SELECTED))
                .when(in_sel && !whole, |d| d.bg(bg_hover()))
                .child((r + 1).to_string())
        });
        let gutter_col = div()
            .absolute()
            .top(px(HEADER_H))
            .left_0()
            .w(px(gutter))
            .bottom_0()
            .overflow_hidden()
            .bg(editor_bg())
            .border_r_1()
            .border_color(border())
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_gutter_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_gutter_down))
            .children(numbers);

        // Header.
        let mut heads = Vec::with_capacity(cols.len());
        let mut handles = Vec::with_capacity(cols.len());
        for c in cols.clone() {
            let column = &self.model.columns()[c];
            let selected = sel.is_some_and(|s| s.cols().contains(&c) && s.all_rows(self.model.row_count()));
            heads.push(
                div()
                    .flex_none()
                    .w(px(self.widths[c]))
                    .h_full()
                    .px(px(model::CELL_PAD_X))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .overflow_hidden()
                    .border_r_1()
                    .border_color(BORDER_SUBTLE)
                    .when(selected, |d| d.bg(SELECTED))
                    .when(self.model.is_key_column(c), |d| d.child(svg().path(icon("sql-key")).flex_none().size(px(ICON)).text_color(ACCENT_YELLOW)))
                    .when(self.model.foreign_key(c).is_some(), |d| d.child(svg().path(icon("sql-link")).flex_none().size(px(ICON)).text_color(ACCENT_BLUE)))
                    .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(column.name.clone()))
                    .child(div().min_w_0().truncate().text_size(px(TYPE_FONT_SIZE)).text_color(TEXT_MUTED).child(column.type_name.clone())),
            );
            handles.push(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(self.offsets[c + 1] - self.scroll_x - RESIZE_GRAB / 2.))
                    .w(px(RESIZE_GRAB))
                    .cursor_col_resize()
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.on_resize_down(c, ev, cx))),
            );
        }
        let header = div()
            .absolute()
            .top_0()
            .left(px(gutter))
            .right_0()
            .h(px(HEADER_H))
            .overflow_hidden()
            .bg(bg_input())
            .border_b_1()
            .border_color(border())
            .text_color(TEXT_PRIMARY)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_header_down))
            .child(div().absolute().top_0().bottom_0().left(px(row_left)).flex().children(heads))
            .children(handles);
        let corner = div()
            .absolute()
            .top_0()
            .left_0()
            .w(px(gutter))
            .h(px(HEADER_H))
            .bg(bg_input())
            .border_r_1()
            .border_b_1()
            .border_color(border())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                    this.select_all(cx);
                }),
            );

        // Scrollbars (overlay thumbs, draggable).
        let vbar = (ch > bh).then(|| {
            let (len, track) = thumb(bh, ch);
            let top = self.scroll_y / track.max(1.) * (bh - len);
            div()
                .absolute()
                .right(px(1.))
                .top(px(HEADER_H + top))
                .w(px(SCROLLBAR))
                .h(px(len))
                .rounded(px(SCROLLBAR / 2.))
                .bg(THUMB)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let o = this.bounds.get().origin;
                        this.drag = Some(Drag::VBar { grab: crate::ui::unpx(ev.position.y - o.y) - HEADER_H - top });
                        cx.notify();
                    }),
                )
        });
        let hbar = (cw > bw).then(|| {
            let (len, track) = thumb(bw, cw);
            let left = self.scroll_x / track.max(1.) * (bw - len);
            div()
                .absolute()
                .bottom(px(1.))
                .left(px(gutter + left))
                .h(px(SCROLLBAR))
                .w(px(len))
                .rounded(px(SCROLLBAR / 2.))
                .bg(THUMB)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let o = this.bounds.get().origin;
                        this.drag = Some(Drag::HBar { grab: crate::ui::unpx(ev.position.x - o.x) - this.gutter_w() - left });
                        cx.notify();
                    }),
                )
        });

        let menu = self.menu.map(|at| self.render_menu(at, cx));

        div()
            .id("sql-result-grid")
            .key_context("ResultGrid")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(editor_bg())
            .font_family(crate::theme::ui_font())
            .text_size(px(FONT_SIZE))
            .text_color(TEXT_PRIMARY)
            .capture_key_down(cx.listener(Self::on_edit_key))
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll(cx.listener(Self::on_wheel))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &CopyCells, _, cx| this.copy(false, cx)))
            .on_action(cx.listener(|this, _: &CopyWithHeaders, _, cx| this.copy(true, cx)))
            .on_action(cx.listener(|this, _: &AddRow, _, cx| this.add_row(cx)))
            .on_action(cx.listener(|this, _: &DeleteRows, _, cx| this.delete_selected_rows(cx)))
            .on_action(cx.listener(|this, _: &SetNull, _, cx| this.set_null(cx)))
            .child(recorder)
            .child(body)
            .child(gutter_col)
            .child(header)
            .child(corner)
            .children(vbar)
            .children(hbar)
            .children(menu)
    }
}

impl ResultGrid {
    fn render_menu(&self, at: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: &'static str, keys: &'static str, enabled: bool| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(5.))
                .rounded(px(4.))
                .flex()
                .gap(px(16.))
                .text_size(px(FONT_SIZE))
                .text_color(if enabled { TEXT_PRIMARY } else { TEXT_MUTED })
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(bg_hover())))
                .child(div().flex_1().child(label))
                .child(div().text_color(TEXT_MUTED).child(keys))
        };
        let act = |f: fn(&mut Self, &mut Context<Self>)| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                this.menu = None;
                f(this, cx);
                cx.notify();
            })
        };
        let editable = self.model.editable();
        let cells_editable = self.selection.is_some_and(|s| s.cols().any(|c| self.model.column_editable(c)));
        deferred(
            anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("sql-grid-menu")
                    .min_w(px(190.))
                    .p(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .bg(bg_surface())
                    .border_1()
                    .border_color(border())
                    .rounded(px(6.))
                    .shadow(vec![BoxShadow { color: c(hexa(0, 0.4)), offset: point(px(0.), px(4.)), blur_radius: px(16.), spread_radius: px(0.), inset: false }])
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(item("g-copy", "Copy", "⌘C", true).on_click(act(|this, cx| this.copy(false, cx))))
                    .child(item("g-copy-h", "Copy with headers", "⇧⌘C", true).on_click(act(|this, cx| this.copy(true, cx))))
                    .when(editable, |d| {
                        d.child(div().my(px(3.)).h(px(1.)).bg(BORDER_SUBTLE))
                            .child(item("g-null", "Set NULL", "⇧⌘N", cells_editable).when(cells_editable, |d| d.on_click(act(Self::set_null))))
                            .child(item("g-add", "Add row", "⌘N", true).on_click(act(Self::add_row)))
                            .child(item("g-del", "Delete row(s)", "⌘⌫", true).on_click(act(Self::delete_selected_rows)))
                    }),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
