//! The result grid without gpui: the rows the server sent, the edits staged on
//! top of them, the selection, value parsing/formatting, and the layout math
//! the view virtualizes with. Kept window-free so all of it is unit-tested.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Range, RangeInclusive};

use sql_core::{Column, ForeignKey, Row, TableRef, Value};
use sql_lang::{EditSource, RowEdit};

pub const ROW_H: f32 = 24.;
pub const MIN_COL_W: f32 = 60.;
pub const MAX_COL_W: f32 = 400.;
/// Horizontal padding inside a cell, each side.
pub const CELL_PAD_X: f32 = 8.;
/// Rows sampled for the automatic column widths.
pub const AUTOSIZE_SAMPLE: usize = 200;
/// A cell never shows more than this many characters: past ~400px the rest is
/// behind the ellipsis anyway, and a multi-megabyte text must not be shaped.
const MAX_CELL_CHARS: usize = 300;

/// How a row stands relative to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Base,
    Inserted,
    Deleted,
}

/// What a column holds, for parsing typed text and aligning numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    Float,
    Decimal,
    Bool,
    Json,
    Bytes,
    Text,
}

impl Kind {
    /// From the server's type name (`int4`, `varchar(255)`, `double precision`, `_int4`…).
    pub fn of(type_name: &str) -> Kind {
        let t = type_name.trim().to_ascii_lowercase();
        if t.ends_with("[]") || t.starts_with('_') {
            return Kind::Text;
        }
        let base = t.split(['(', ' ']).next().unwrap_or("");
        match base {
            "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint" | "tinyint" | "mediumint" | "serial" | "serial2" | "serial4" | "serial8"
            | "smallserial" | "bigserial" | "long" => Kind::Int,
            "float" | "float4" | "float8" | "real" | "double" | "binary_float" | "binary_double" => Kind::Float,
            "numeric" | "decimal" | "dec" | "money" | "smallmoney" | "number" => Kind::Decimal,
            "bool" | "boolean" | "bit" => Kind::Bool,
            "json" | "jsonb" => Kind::Json,
            "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image" => Kind::Bytes,
            _ => Kind::Text,
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Kind::Int | Kind::Float | Kind::Decimal)
    }
}

/// A rectangular cell selection; `anchor` stays put while shift-moves move `active`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub active: (usize, usize),
}

impl Selection {
    pub fn cell(row: usize, col: usize) -> Self {
        Selection { anchor: (row, col), active: (row, col) }
    }

    pub fn rows(&self) -> RangeInclusive<usize> {
        self.anchor.0.min(self.active.0)..=self.anchor.0.max(self.active.0)
    }

    pub fn cols(&self) -> RangeInclusive<usize> {
        self.anchor.1.min(self.active.1)..=self.anchor.1.max(self.active.1)
    }

    pub fn contains(&self, row: usize, col: usize) -> bool {
        self.rows().contains(&row) && self.cols().contains(&col)
    }

    /// Covers all `n` rows — by the ends, not by counting: selections can span 100k rows.
    pub fn all_rows(&self, n: usize) -> bool {
        self.rows() == (0..=n.wrapping_sub(1))
    }

    pub fn all_cols(&self, n: usize) -> bool {
        self.cols() == (0..=n.wrapping_sub(1))
    }

    /// Clamped to a grid of `rows` × `cols` (rows can disappear on commit or delete).
    pub fn clamp(self, rows: usize, cols: usize) -> Option<Self> {
        if rows == 0 || cols == 0 {
            return None;
        }
        let f = |(r, c): (usize, usize)| (r.min(rows - 1), c.min(cols - 1));
        Some(Selection { anchor: f(self.anchor), active: f(self.active) })
    }
}

#[derive(Default)]
pub struct GridModel {
    columns: Vec<Column>,
    /// Rows as the server has them (after the last commit).
    base: Vec<Row>,
    /// New rows, shown after the base rows. `None` = DEFAULT: left out of the INSERT.
    inserted: Vec<Vec<Option<Value>>>,
    /// Edited base cells, by (row, col). Only cells that differ from the base.
    staged: BTreeMap<(usize, usize), Value>,
    deleted: BTreeSet<usize>,
    source: Option<EditSource>,
    references: Vec<(usize, ForeignKey)>,
}

impl GridModel {
    pub fn new(columns: Vec<Column>) -> Self {
        GridModel { columns, ..Default::default() }
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// A streamed batch. Rows are padded/truncated to the column count so a
    /// misbehaving driver can't make cell lookups panic.
    pub fn append(&mut self, rows: Vec<Row>) {
        let n = self.columns.len();
        self.base.extend(rows.into_iter().map(|mut r| {
            r.resize(n, Value::Null);
            r
        }));
    }

    pub fn row_count(&self) -> usize {
        self.base.len() + self.inserted.len()
    }

    /// The current value; `None` out of range or for a new row's DEFAULT cell.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Value> {
        match row.checked_sub(self.base.len()) {
            Some(i) => self.inserted.get(i)?.get(col)?.as_ref(),
            None => self.staged.get(&(row, col)).or_else(|| self.base[row].get(col)),
        }
    }

    /// The server's value of a base cell, ignoring staged edits.
    pub fn original(&self, row: usize, col: usize) -> Option<&Value> {
        self.base.get(row)?.get(col)
    }

    pub fn row_state(&self, row: usize) -> RowState {
        if row >= self.base.len() {
            RowState::Inserted
        } else if self.deleted.contains(&row) {
            RowState::Deleted
        } else {
            RowState::Base
        }
    }

    pub fn is_modified(&self, row: usize, col: usize) -> bool {
        self.staged.contains_key(&(row, col))
    }

    /// Changing the source drops staged edits: they were made against the old one.
    pub fn set_source(&mut self, source: Option<EditSource>, references: Vec<(usize, ForeignKey)>) {
        if source != self.source {
            self.revert();
        }
        self.source = source;
        self.references = references;
    }

    pub fn editable(&self) -> bool {
        self.source.is_some()
    }

    /// Mapped to a table column (computed columns are read-only).
    pub fn column_editable(&self, col: usize) -> bool {
        self.mapped(col).is_some()
    }

    fn mapped(&self, col: usize) -> Option<&str> {
        self.source.as_ref()?.column_map.get(col)?.as_deref()
    }

    pub fn is_key_column(&self, col: usize) -> bool {
        let Some(s) = &self.source else { return false };
        self.mapped(col).is_some_and(|name| s.key_columns.iter().any(|k| k == name))
    }

    pub fn foreign_key(&self, col: usize) -> Option<&ForeignKey> {
        self.references.iter().find(|(c, _)| *c == col).map(|(_, fk)| fk)
    }

    /// The result column showing table column `name`.
    fn result_col(&self, name: &str) -> Option<usize> {
        match &self.source {
            Some(s) => s.column_map.iter().position(|m| m.as_deref() == Some(name)),
            None => self.columns.iter().position(|c| c.name == name),
        }
    }

    /// Stage `value`. Returns whether anything changed. Writing a base cell's
    /// original value back un-stages it; deleted rows and read-only columns refuse.
    pub fn set(&mut self, row: usize, col: usize, value: Value) -> bool {
        if !self.column_editable(col) {
            return false;
        }
        match self.row_state(row) {
            RowState::Deleted => false,
            RowState::Inserted => {
                let Some(cell) = self.inserted.get_mut(row - self.base.len()).and_then(|r| r.get_mut(col)) else { return false };
                let changed = cell.as_ref() != Some(&value);
                *cell = Some(value);
                changed
            }
            RowState::Base => {
                if self.base[row].get(col) == Some(&value) {
                    self.staged.remove(&(row, col)).is_some()
                } else {
                    self.staged.insert((row, col), value.clone()) != Some(value)
                }
            }
        }
    }

    /// Appends an all-DEFAULT row; returns its index.
    pub fn add_row(&mut self) -> Option<usize> {
        self.editable().then(|| {
            self.inserted.push(vec![None; self.columns.len()]);
            self.row_count() - 1
        })
    }

    /// Marks base rows deleted and drops new ones. Returns whether anything changed.
    pub fn delete_rows(&mut self, rows: RangeInclusive<usize>) -> bool {
        if !self.editable() {
            return false;
        }
        let base = self.base.len();
        let mut changed = false;
        for r in rows.rev() {
            if r >= base {
                if r - base < self.inserted.len() {
                    self.inserted.remove(r - base);
                    changed = true;
                }
            } else {
                changed |= self.deleted.insert(r);
            }
        }
        changed
    }

    pub fn revert(&mut self) {
        self.staged.clear();
        self.deleted.clear();
        self.inserted.clear();
    }

    /// After a successful submit: staged values become the base, deleted rows
    /// go, new rows join the base (DEFAULT cells show NULL until a re-run).
    pub fn commit(&mut self) {
        for ((r, c), v) in std::mem::take(&mut self.staged) {
            self.base[r][c] = v;
        }
        let deleted = std::mem::take(&mut self.deleted);
        if !deleted.is_empty() {
            let mut i = 0;
            self.base.retain(|_| {
                i += 1;
                !deleted.contains(&(i - 1))
            });
        }
        for row in std::mem::take(&mut self.inserted) {
            self.base.push(row.into_iter().map(|v| v.unwrap_or(Value::Null)).collect());
        }
    }

    /// What `pending_edits` would return, counted without building it.
    pub fn pending_count(&self) -> usize {
        let mut n = self.deleted.len() + self.inserted.len();
        let mut last = None;
        for &(r, _) in self.staged.keys() {
            if last != Some(r) && !self.deleted.contains(&r) {
                n += 1;
            }
            last = Some(r);
        }
        n
    }

    /// Updates and deletes in row order (keyed by the key columns' ORIGINAL
    /// values, so editing a key still finds the row), then inserts.
    pub fn pending_edits(&self) -> Vec<RowEdit> {
        let Some(source) = &self.source else { return Vec::new() };
        let key = |row: usize| -> Option<Vec<(String, Value)>> {
            source.key_columns.iter().map(|k| Some((k.clone(), self.base[row].get(self.result_col(k)?)?.clone()))).collect()
        };
        let mut updates: BTreeMap<usize, Vec<(String, Value)>> = BTreeMap::new();
        for (&(r, c), v) in &self.staged {
            if let Some(name) = self.mapped(c).filter(|_| !self.deleted.contains(&r)) {
                updates.entry(r).or_default().push((name.to_owned(), v.clone()));
            }
        }
        let rows: BTreeSet<usize> = updates.keys().chain(&self.deleted).copied().collect();
        let mut out: Vec<RowEdit> = rows
            .into_iter()
            .filter_map(|r| {
                let key = key(r)?;
                Some(match updates.remove(&r) {
                    Some(set) => RowEdit::Update { key, set },
                    None => RowEdit::Delete { key },
                })
            })
            .collect();
        out.extend(self.inserted.iter().map(|row| RowEdit::Insert {
            values: row.iter().enumerate().filter_map(|(c, v)| Some((self.mapped(c)?.to_owned(), v.clone()?))).collect(),
        }));
        out
    }

    /// For a foreign-key cell: the referenced table and the filter that finds
    /// the row it points at. None when the key is (partly) NULL or unset.
    pub fn reference(&self, row: usize, col: usize) -> Option<(TableRef, Vec<(String, Value)>)> {
        let fk = self.foreign_key(col)?;
        let filter = fk
            .columns
            .iter()
            .zip(&fk.ref_columns)
            .map(|(c, rc)| {
                let v = self.cell(row, self.result_col(c)?)?;
                (!v.is_null()).then(|| (rc.clone(), v.clone()))
            })
            .collect::<Option<Vec<_>>>()?;
        Some((fk.references.clone(), filter))
    }

    /// The selection as TSV, spreadsheet-quoted; NULL copies as an empty field.
    pub fn tsv(&self, sel: &Selection, headers: bool) -> String {
        let cols = sel.cols();
        let mut lines = Vec::with_capacity(sel.rows().end() - sel.rows().start() + 2);
        if headers {
            lines.push(cols.clone().filter_map(|c| self.columns.get(c)).map(|c| tsv_field(&c.name)).collect::<Vec<_>>().join("\t"));
        }
        for r in sel.rows().filter(|&r| r < self.row_count()) {
            let fields: Vec<String> = cols.clone().map(|c| self.cell(r, c).map(|v| if v.is_null() { String::new() } else { tsv_field(&v.display()) }).unwrap_or_default()).collect();
            lines.push(fields.join("\t"));
        }
        lines.join("\n")
    }
}

fn tsv_field(s: &str) -> String {
    if s.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() }
}

/// A cell's text: one line (newlines as ⏎), capped in length.
pub fn cell_text(v: &Value) -> String {
    let one_line = |s: &str| s.chars().filter(|&c| c != '\r').take(MAX_CELL_CHARS).map(|c| if c == '\n' { '⏎' } else if c == '\t' { ' ' } else { c }).collect();
    match v {
        Value::Text(s) | Value::Other(s) | Value::Decimal(s) | Value::Uuid(s) | Value::Date(s) | Value::Time(s) | Value::Timestamp(s) => one_line(s),
        Value::Json(j) => one_line(&j.to_string()),
        v => v.display(),
    }
}

/// The text an inline editor starts with; None when the value can't round-trip
/// through a single-line input (multi-line text, long binary) — the value panel edits those.
pub fn edit_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::Bytes(b)) if b.len() > 64 => None,
        Some(v) => Some(v.display()).filter(|s| !s.contains(['\n', '\r'])),
    }
}

/// Typed text back to a value by the column's type. Text equal to the
/// original's keeps the original (so a no-op edit un-stages, whatever its
/// variant). Empty input is NULL for numbers and booleans, an empty string
/// otherwise; anything unparsable stays text for the server to judge.
pub fn parse_input(text: &str, type_name: &str, original: Option<&Value>) -> Value {
    if let Some(o) = original.filter(|o| !o.is_null())
        && edit_text(Some(o)).as_deref() == Some(text)
    {
        return o.clone();
    }
    let kind = Kind::of(type_name);
    let t = text.trim();
    if t.is_empty() && (kind.is_numeric() || kind == Kind::Bool) {
        return Value::Null;
    }
    let parsed = match kind {
        Kind::Int => t.parse().ok().map(Value::Int),
        Kind::Float => t.parse().ok().map(Value::Float),
        Kind::Decimal => t.parse::<f64>().is_ok().then(|| Value::Decimal(t.to_owned())),
        Kind::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Some(Value::Bool(true)),
            "false" | "f" | "no" | "n" | "off" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        Kind::Json => serde_json::from_str(t).ok().map(Value::Json),
        Kind::Bytes => t.strip_prefix("0x").or_else(|| t.strip_prefix("\\x")).and_then(parse_hex).map(Value::Bytes),
        Kind::Text => None,
    };
    parsed.unwrap_or_else(|| Value::Text(text.to_owned()))
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Right-aligned like a number column.
pub fn right_aligned(v: &Value, kind: Kind) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_) | Value::Decimal(_)) || (kind.is_numeric() && !v.is_null())
}

// ── Layout math ─────────────────────────────────────────────────────────────

/// A column's automatic width: the widest of the header and the sampled
/// cells' text widths plus padding, within [MIN_COL_W, MAX_COL_W].
pub fn auto_width(header: f32, cells: impl IntoIterator<Item = f32>) -> f32 {
    let widest = cells.into_iter().fold(header, f32::max);
    (widest + 2. * CELL_PAD_X + 2.).clamp(MIN_COL_W, MAX_COL_W)
}

/// Left edges of the columns, plus the total width as the last entry.
pub fn offsets(widths: &[f32]) -> Vec<f32> {
    let mut x = 0.;
    let mut out = Vec::with_capacity(widths.len() + 1);
    out.push(0.);
    for w in widths {
        x += w;
        out.push(x);
    }
    out
}

/// Rows intersecting `[scroll, scroll + height)`.
pub fn visible_rows(scroll: f32, height: f32, rows: usize) -> Range<usize> {
    let first = (scroll.max(0.) / ROW_H).floor() as usize;
    let end = ((scroll.max(0.) + height.max(0.)) / ROW_H).ceil() as usize;
    first.min(rows)..end.min(rows)
}

/// Columns intersecting `[scroll, scroll + width)`, from `offsets`.
pub fn visible_cols(offsets: &[f32], scroll: f32, width: f32) -> Range<usize> {
    let n = offsets.len().saturating_sub(1);
    let first = offsets[1..].partition_point(|&right| right <= scroll);
    let end = offsets[..n].partition_point(|&left| left < scroll + width);
    first.min(n)..end.max(first.min(n))
}

/// The column under content x, if any.
pub fn col_at(offsets: &[f32], x: f32) -> Option<usize> {
    let n = offsets.len().checked_sub(1)?;
    (x >= 0. && x < offsets[n]).then(|| offsets[1..].partition_point(|&right| right <= x))
}

/// The scroll offset that brings `[start, end)` into a viewport of `size`, moving as little as possible.
pub fn scroll_into_view(scroll: f32, start: f32, end: f32, size: f32) -> f32 {
    if start < scroll {
        start
    } else if end > scroll + size {
        (end - size).min(start)
    } else {
        scroll
    }
}

pub fn max_scroll(content: f32, view: f32) -> f32 {
    (content - view).max(0.)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.into(), type_name: ty.into(), origin: None }
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    /// users(id pk, name, email) + a computed column; 3 rows.
    fn users() -> GridModel {
        let mut m = GridModel::new(vec![col("id", "int4"), col("name", "text"), col("email", "text"), col("n_len", "int4")]);
        m.append(vec![
            vec![Value::Int(1), text("ann"), text("a@x"), Value::Int(3)],
            vec![Value::Int(2), text("bob"), Value::Null, Value::Int(3)],
            vec![Value::Int(3), text("cy"), text("c@x"), Value::Int(2)],
        ]);
        m.set_source(
            Some(EditSource {
                table: TableRef::new(Some("public"), "users"),
                key_columns: vec!["id".into()],
                column_map: vec![Some("id".into()), Some("name".into()), Some("email".into()), None],
            }),
            Vec::new(),
        );
        m
    }

    #[test]
    fn staging_shows_the_new_value_and_original_undoes_it() {
        let mut m = users();
        assert!(m.set(0, 1, text("anna")));
        assert_eq!(m.cell(0, 1), Some(&text("anna")));
        assert!(m.is_modified(0, 1));
        assert_eq!(m.pending_count(), 1);
        assert!(!m.set(0, 1, text("anna")), "same value again is no change");
        assert!(m.set(0, 1, text("ann")), "back to the original un-stages");
        assert!(!m.is_modified(0, 1));
        assert_eq!(m.pending_count(), 0);
    }

    #[test]
    fn read_only_columns_and_sources_refuse_edits() {
        let mut m = users();
        assert!(!m.set(0, 3, Value::Int(9)), "computed column");
        m.set_source(None, Vec::new());
        assert!(!m.set(0, 1, text("x")));
        assert_eq!(m.add_row(), None);
        assert!(!m.delete_rows(0..=0));
    }

    #[test]
    fn pending_edits_key_rows_by_their_original_key() {
        let mut m = users();
        m.set(1, 0, Value::Int(20));
        m.set(1, 2, text("b@x"));
        m.delete_rows(2..=2);
        assert_eq!(
            m.pending_edits(),
            vec![
                RowEdit::Update { key: vec![("id".into(), Value::Int(2))], set: vec![("id".into(), Value::Int(20)), ("email".into(), text("b@x"))] },
                RowEdit::Delete { key: vec![("id".into(), Value::Int(3))] },
            ]
        );
        assert_eq!(m.pending_count(), 2);
    }

    #[test]
    fn a_deleted_row_with_edits_is_just_deleted() {
        let mut m = users();
        m.set(0, 1, text("x"));
        m.delete_rows(0..=0);
        assert_eq!(m.pending_edits(), vec![RowEdit::Delete { key: vec![("id".into(), Value::Int(1))] }]);
        assert_eq!(m.pending_count(), 1);
        assert!(!m.set(0, 1, text("y")), "deleted rows are frozen");
    }

    #[test]
    fn inserted_rows_leave_default_cells_out() {
        let mut m = users();
        let r = m.add_row().unwrap();
        assert_eq!(r, 3);
        assert_eq!(m.row_state(r), RowState::Inserted);
        assert_eq!(m.cell(r, 1), None, "DEFAULT");
        m.set(r, 1, text("dee"));
        m.set(r, 2, Value::Null);
        assert_eq!(m.pending_edits(), vec![RowEdit::Insert { values: vec![("name".into(), text("dee")), ("email".into(), Value::Null)] }]);
    }

    #[test]
    fn deleting_an_inserted_row_drops_it() {
        let mut m = users();
        m.add_row();
        m.add_row();
        m.set(4, 1, text("second"));
        assert!(m.delete_rows(3..=3));
        assert_eq!(m.row_count(), 4);
        assert_eq!(m.cell(3, 1), Some(&text("second")));
        assert_eq!(m.pending_count(), 1);
        assert!(matches!(m.pending_edits().as_slice(), [RowEdit::Insert { .. }]));
    }

    #[test]
    fn revert_drops_everything_staged() {
        let mut m = users();
        m.set(0, 1, text("x"));
        m.delete_rows(1..=1);
        m.add_row();
        m.revert();
        assert_eq!(m.pending_count(), 0);
        assert_eq!(m.row_count(), 3);
        assert_eq!(m.cell(0, 1), Some(&text("ann")));
        assert_eq!(m.row_state(1), RowState::Base);
    }

    #[test]
    fn commit_makes_staging_the_base() {
        let mut m = users();
        m.set(0, 1, text("anna"));
        m.delete_rows(1..=1);
        let r = m.add_row().unwrap();
        m.set(r, 1, text("dee"));
        m.commit();
        assert_eq!(m.pending_count(), 0);
        assert_eq!(m.row_count(), 3);
        assert_eq!(m.cell(0, 1), Some(&text("anna")));
        assert!(!m.is_modified(0, 1));
        assert_eq!(m.cell(1, 1), Some(&text("cy")), "deleted row gone");
        assert_eq!(m.cell(2, 1), Some(&text("dee")));
        assert_eq!(m.cell(2, 0), Some(&Value::Null), "DEFAULT shows NULL until a re-run");
        assert_eq!(m.row_state(2), RowState::Base);
    }

    #[test]
    fn changing_the_source_drops_staged_edits() {
        let mut m = users();
        m.set(0, 1, text("x"));
        m.set_source(None, Vec::new());
        assert_eq!(m.pending_count(), 0);
    }

    #[test]
    fn short_rows_are_padded() {
        let mut m = GridModel::new(vec![col("a", "int"), col("b", "int")]);
        m.append(vec![vec![Value::Int(1)]]);
        assert_eq!(m.cell(0, 1), Some(&Value::Null));
        assert_eq!(m.cell(1, 0), None);
    }

    #[test]
    fn references_filter_by_this_rows_fk_values() {
        let mut m = GridModel::new(vec![col("id", "int"), col("user_id", "int")]);
        m.append(vec![vec![Value::Int(1), Value::Int(7)], vec![Value::Int(2), Value::Null]]);
        let fk = ForeignKey { name: "fk".into(), columns: vec!["user_id".into()], references: TableRef::new(None, "users"), ref_columns: vec!["id".into()] };
        m.set_source(None, vec![(1, fk)]);
        assert_eq!(m.reference(0, 1), Some((TableRef::new(None, "users"), vec![("id".into(), Value::Int(7))])));
        assert_eq!(m.reference(1, 1), None, "NULL points nowhere");
        assert_eq!(m.reference(0, 0), None);
    }

    #[test]
    fn parsing_follows_the_column_type() {
        assert_eq!(parse_input("42", "int4", None), Value::Int(42));
        assert_eq!(parse_input(" 42 ", "bigint", None), Value::Int(42));
        assert_eq!(parse_input("4x", "int", None), text("4x"));
        assert_eq!(parse_input("1.5", "double precision", None), Value::Float(1.5));
        assert_eq!(parse_input("1.50", "numeric(10,2)", None), Value::Decimal("1.50".into()));
        assert_eq!(parse_input("yes", "boolean", None), Value::Bool(true));
        assert_eq!(parse_input("0", "bit", None), Value::Bool(false));
        assert_eq!(parse_input(r#"{"a": 1}"#, "jsonb", None), Value::Json(serde_json::json!({"a": 1})));
        assert_eq!(parse_input("0x0aff", "bytea", None), Value::Bytes(vec![0x0a, 0xff]));
        assert_eq!(parse_input("2024-01-02", "date", None), text("2024-01-02"));
        assert_eq!(parse_input("", "varchar(20)", None), text(""));
        assert_eq!(parse_input("", "int", None), Value::Null);
        assert_eq!(parse_input(" padded ", "text", None), text(" padded "));
        assert_eq!(parse_input("{1,2}", "_int4", None), text("{1,2}"));
    }

    #[test]
    fn unchanged_text_keeps_the_original_value() {
        let ts = Value::Timestamp("2024-01-02 03:04:05".into());
        assert_eq!(parse_input("2024-01-02 03:04:05", "timestamp", Some(&ts)), ts);
        let d = Value::Decimal("1.0".into());
        assert_eq!(parse_input("1.0", "int", Some(&d)), d);
    }

    #[test]
    fn edit_text_refuses_what_a_single_line_would_mangle() {
        assert_eq!(edit_text(None), Some(String::new()));
        assert_eq!(edit_text(Some(&Value::Null)), Some(String::new()));
        assert_eq!(edit_text(Some(&Value::Int(5))), Some("5".into()));
        assert_eq!(edit_text(Some(&text("a\nb"))), None);
        assert_eq!(edit_text(Some(&Value::Bytes(vec![0; 65]))), None);
    }

    #[test]
    fn cell_text_is_one_capped_line() {
        assert_eq!(cell_text(&text("a\r\nb\tc")), "a⏎b c");
        assert_eq!(cell_text(&Value::Json(serde_json::json!({"a": [1, 2]}))), r#"{"a":[1,2]}"#);
        assert_eq!(cell_text(&text(&"x".repeat(10_000))).len(), MAX_CELL_CHARS);
        assert_eq!(cell_text(&Value::Bool(true)), "true");
    }

    #[test]
    fn tsv_quotes_special_fields_and_blanks_nulls() {
        let mut m = users();
        m.set(0, 1, text("say \"hi\"\tthere"));
        let all = Selection { anchor: (0, 0), active: (1, 2) };
        assert_eq!(m.tsv(&all, false), "1\t\"say \"\"hi\"\"\tthere\"\ta@x\n2\tbob\t");
        let one = Selection { anchor: (2, 1), active: (2, 0) };
        assert_eq!(m.tsv(&one, true), "id\tname\n3\tcy");
    }

    #[test]
    fn kinds_from_type_names() {
        assert_eq!(Kind::of("INT UNSIGNED"), Kind::Int);
        assert_eq!(Kind::of("varchar(255)"), Kind::Text);
        assert_eq!(Kind::of("interval"), Kind::Text);
        assert_eq!(Kind::of("point"), Kind::Text);
        assert_eq!(Kind::of("decimal(10,2)"), Kind::Decimal);
        assert_eq!(Kind::of("integer[]"), Kind::Text);
    }

    #[test]
    fn auto_width_pads_and_clamps() {
        assert_eq!(auto_width(10., [5., 120.]), 120. + 2. * CELL_PAD_X + 2.);
        assert_eq!(auto_width(1., []), MIN_COL_W);
        assert_eq!(auto_width(30., [5000.]), MAX_COL_W);
    }

    #[test]
    fn visible_ranges() {
        assert_eq!(visible_rows(0., 100., 1000), 0..5);
        assert_eq!(visible_rows(30., 48., 1000), 1..4);
        assert_eq!(visible_rows(0., 100., 2), 0..2);
        assert_eq!(visible_rows(-5., 24., 10), 0..1);

        let o = offsets(&[100., 50., 200.]);
        assert_eq!(o, vec![0., 100., 150., 350.]);
        assert_eq!(visible_cols(&o, 0., 120.), 0..2);
        assert_eq!(visible_cols(&o, 100., 50.), 1..2);
        assert_eq!(visible_cols(&o, 149., 1000.), 1..3);
        assert_eq!(visible_cols(&o, 400., 100.), 3..3);
        assert_eq!(visible_cols(&offsets(&[]), 0., 100.), 0..0);
        assert_eq!(col_at(&o, 0.), Some(0));
        assert_eq!(col_at(&o, 100.), Some(1));
        assert_eq!(col_at(&o, 349.), Some(2));
        assert_eq!(col_at(&o, 350.), None);
    }

    #[test]
    fn scrolling_into_view_moves_minimally() {
        assert_eq!(scroll_into_view(100., 50., 74., 200.), 50.);
        assert_eq!(scroll_into_view(100., 300., 324., 200.), 124.);
        assert_eq!(scroll_into_view(100., 150., 174., 200.), 100.);
        assert_eq!(scroll_into_view(0., 0., 500., 200.), 0., "wider than the view: show its start");
        assert_eq!(max_scroll(100., 300.), 0.);
    }

    #[test]
    fn selection_clamps_after_rows_go() {
        let s = Selection { anchor: (1, 0), active: (9, 3) };
        assert_eq!(s.clamp(5, 2), Some(Selection { anchor: (1, 0), active: (4, 1) }));
        assert_eq!(s.clamp(0, 2), None);
        assert!(s.contains(5, 2));
        assert!(s.all_cols(4) && !s.all_rows(10) && !s.all_cols(0));
        assert!(!s.contains(0, 2));
    }
}
