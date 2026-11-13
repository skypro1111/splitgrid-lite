//! Light structural analysis over a statement's significant tokens: matching
//! parentheses, CTEs, set-operation parts, FROM/JOIN/DML sources with their
//! aliases, select-list items and the output columns of subqueries.
//!
//! Deliberately not a parser: every function tolerates missing pieces, since
//! completion runs on half-typed statements.

use std::ops::Range;

use sql_core::{Catalog, Dialect, Schema, Table};

use crate::ident::unquote;
use crate::keywords::{is_reserved, is_value_keyword};
use crate::lexer::{Kind, Token};

/// Subquery nesting beyond this is not analysed (keeps pathological input cheap).
const MAX_DEPTH: u8 = 8;

#[derive(Clone, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
}

struct CteDef {
    name: String,
    explicit: Option<Vec<String>>,
    body: Range<usize>,
}

/// Something rows come from: a table, a CTE, a subquery or a table function.
#[derive(Clone, Debug)]
pub struct Source<'a> {
    /// The name it is referenced by: the alias, else the table's own name.
    pub name: String,
    pub table: Option<(&'a Schema, &'a Table)>,
    /// Output columns of a CTE / subquery / aliased function.
    pub derived: Vec<String>,
    pub is_cte: bool,
    /// Index of its first token.
    pub at: usize,
    /// The INSERT / UPDATE / DELETE / MERGE target.
    pub target: bool,
}

impl<'a> Source<'a> {
    /// Columns with their type (empty for derived ones).
    pub fn columns(&self) -> Vec<(&str, &str)> {
        match self.table {
            Some((_, t)) => t
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str()))
                .collect(),
            None => self.derived.iter().map(|c| (c.as_str(), "")).collect(),
        }
    }
}

/// A select-list item.
#[derive(Clone, Debug, Default)]
pub struct Item {
    pub range: Range<usize>,
    /// The output column name, when it can be told.
    pub name: Option<String>,
    /// A plain column reference: (qualifier, column).
    pub column: Option<(Option<String>, String)>,
    /// `*` (None) or `q.*` (Some(q)).
    pub star: Option<Option<String>>,
}

pub struct Model<'a> {
    pub src: &'a str,
    /// Significant tokens only.
    pub toks: Vec<Token>,
    /// For each `(`, the index of its `)` (or `toks.len()` when unclosed).
    close: Vec<usize>,
    pub d: Dialect,
    pub cat: &'a Catalog,
    /// The partial word under the caret: never taken for a table or alias.
    pub partial: Option<usize>,
}

impl<'a> Model<'a> {
    pub fn new(src: &'a str, toks: Vec<Token>, d: Dialect, cat: &'a Catalog) -> Self {
        let mut close = vec![usize::MAX; toks.len()];
        let mut stack = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            if t.is_punct(b'(') {
                stack.push(i);
            } else if t.is_punct(b')')
                && let Some(o) = stack.pop()
            {
                close[o] = i;
            }
        }
        for o in stack {
            close[o] = toks.len();
        }
        Model {
            src,
            toks,
            close,
            d,
            cat,
            partial: None,
        }
    }

    pub fn len(&self) -> usize {
        self.toks.len()
    }

    /// The index of the `)` closing the `(` at `i` (`len()` when unclosed).
    pub fn close_of(&self, i: usize) -> usize {
        self.close[i]
    }

    pub fn text(&self, i: usize) -> &'a str {
        self.toks[i].text(self.src)
    }

    pub fn kw(&self, i: usize, w: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.is_kw(self.src, w))
    }

    pub fn kw_any(&self, i: usize, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.kw(i, w))
    }

    pub fn punct(&self, i: usize, c: u8) -> bool {
        self.toks.get(i).is_some_and(|t| t.is_punct(c))
    }

    pub fn op(&self, i: usize, op: &str) -> bool {
        self.toks
            .get(i)
            .is_some_and(|t| t.kind == Kind::Op && t.text(self.src) == op)
    }

    /// An identifier that isn't a keyword: a quoted name or a non-reserved word.
    pub fn is_name(&self, i: usize) -> bool {
        self.toks.get(i).is_some_and(|t| {
            t.kind == Kind::Quoted
                || (t.kind == Kind::Word && !is_reserved(t.text(self.src), self.d))
        })
    }

    pub fn name(&self, i: usize) -> String {
        unquote(self.text(i))
    }

    /// The token after `i` at the same depth (skipping a parenthesised group).
    pub fn step(&self, i: usize) -> usize {
        if self.punct(i, b'(') {
            self.close[i].saturating_add(1).min(self.len())
        } else {
            i + 1
        }
    }

    /// Whether the group opened at `i` holds a query.
    pub fn is_query_group(&self, i: usize) -> bool {
        self.punct(i, b'(')
            && self.kw_any(i + 1, &["SELECT", "WITH", "VALUES"])
            && self.partial != Some(i + 1)
    }

    /// The token ends an operand (an identifier, literal, `)` …), so what follows
    /// is an operator or a clause keyword rather than another operand.
    pub fn ends_operand(&self, i: usize) -> bool {
        let t = &self.toks[i];
        match t.kind {
            Kind::Quoted | Kind::Str | Kind::Number | Kind::Param => true,
            Kind::Word => {
                !is_reserved(t.text(self.src), self.d) || is_value_keyword(t.text(self.src))
            }
            Kind::Punct(b')') => true,
            _ => false,
        }
    }

    // ----- catalog lookups -----

    pub fn schema(&self, name: &str) -> Option<&'a Schema> {
        let s = &self.cat.schemas;
        s.iter()
            .find(|s| s.name == name)
            .or_else(|| s.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
    }

    /// A table by dotted name parts (`t`, `s.t`, `db.s.t`); unqualified names
    /// resolve through the search path, exact case first.
    pub fn resolve_table(&self, parts: &[String]) -> Option<(&'a Schema, &'a Table)> {
        let find = |s: &'a Schema, n: &str| {
            s.tables
                .iter()
                .find(|t| t.name == n)
                .or_else(|| s.tables.iter().find(|t| t.name.eq_ignore_ascii_case(n)))
                .map(|t| (s, t))
        };
        match parts {
            [] => None,
            [name] => {
                let path = self.search_path();
                path.iter()
                    .find_map(|s| find(s, name))
                    .or_else(|| self.cat.schemas.iter().find_map(|s| find(s, name)))
            }
            [.., schema, name] => find(self.schema(schema)?, name),
        }
    }

    /// The schemas unqualified names resolve in; a lone schema counts when the path is empty.
    pub fn search_path(&self) -> Vec<&'a Schema> {
        let path: Vec<&Schema> = self
            .cat
            .search_path
            .iter()
            .filter_map(|n| self.cat.schema(n))
            .collect();
        if path.is_empty() && self.cat.schemas.len() == 1 {
            vec![&self.cat.schemas[0]]
        } else {
            path
        }
    }

    // ----- structure -----

    fn cte_defs(&self, lo: usize, hi: usize) -> (Vec<CteDef>, usize) {
        let mut out = Vec::new();
        if !self.kw(lo, "WITH") {
            return (out, lo);
        }
        let mut i = lo + 1;
        if self.kw(i, "RECURSIVE") {
            i += 1;
        }
        while i < hi && self.toks[i].is_ident() && self.partial != Some(i) {
            let name = self.name(i);
            i += 1;
            let mut explicit = None;
            if self.punct(i, b'(') {
                explicit = Some(self.ident_list(i));
                i = self.step(i);
            }
            if !self.kw(i, "AS") {
                break;
            }
            i += 1;
            if self.kw(i, "NOT") {
                i += 1;
            }
            if self.kw(i, "MATERIALIZED") {
                i += 1;
            }
            if !self.punct(i, b'(') {
                break;
            }
            let c = self.close[i].min(hi);
            out.push(CteDef {
                name,
                explicit,
                body: i + 1..c,
            });
            i = c + 1;
            if c >= hi || !self.punct(i, b',') {
                break;
            }
            i += 1;
        }
        (out, i.min(hi))
    }

    /// The CTEs a WITH at `lo` defines (columns inferred unless listed), and where the main statement starts.
    pub fn ctes(&self, lo: usize, hi: usize, outer: &[Cte], depth: u8) -> (Vec<Cte>, usize) {
        let (defs, main) = self.cte_defs(lo, hi);
        let mut visible = outer.to_vec();
        for def in defs {
            let columns = match def.explicit {
                Some(c) => c,
                None => {
                    // A recursive CTE may name itself; it has no columns yet then.
                    visible.push(Cte {
                        name: def.name.clone(),
                        columns: Vec::new(),
                    });
                    let cols = self.output_columns(def.body.clone(), &visible, depth + 1);
                    visible.pop();
                    cols
                }
            };
            visible.push(Cte {
                name: def.name,
                columns,
            });
        }
        (visible.split_off(outer.len()), main)
    }

    /// CTE bodies, for checks that look inside data-modifying CTEs.
    pub fn cte_bodies(&self, lo: usize, hi: usize) -> (Vec<Range<usize>>, usize) {
        let (defs, main) = self.cte_defs(lo, hi);
        (defs.into_iter().map(|d| d.body).collect(), main)
    }

    /// The names in a `(a, b, c)` list at `i`.
    pub fn ident_list(&self, i: usize) -> Vec<String> {
        (i + 1..self.close[i].min(self.len()))
            .filter(|&j| self.toks[j].is_ident())
            .map(|j| self.name(j))
            .collect()
    }

    /// The query parts of a level: set operations separate them, and a SELECT
    /// that doesn't start its part starts a new one (`INSERT … SELECT`,
    /// `CREATE VIEW … AS SELECT`, `EXPLAIN SELECT`), as does a statement after
    /// `BEGIN` / `THEN` / `ELSE` in procedural batches.
    pub fn parts(&self, lo: usize, hi: usize) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut start = lo;
        let mut i = lo;
        while i < hi {
            if self.kw_any(i, &["UNION", "INTERSECT", "EXCEPT", "MINUS"]) {
                out.push(start..i);
                i += 1;
                if self.kw_any(i, &["ALL", "DISTINCT"]) {
                    i += 1;
                }
                start = i;
                continue;
            }
            let after_block = i > lo && self.kw_any(i - 1, &["BEGIN", "THEN", "ELSE"]);
            if i > start
                && (self.kw(i, "SELECT")
                    || (after_block
                        && self.kw_any(i, &["INSERT", "UPDATE", "DELETE", "MERGE", "WITH"])))
            {
                out.push(start..i);
                start = i;
            }
            i = self.step(i);
        }
        out.push(start..hi);
        out
    }

    /// The sources a query part reads from or writes to.
    pub fn sources(&self, part: Range<usize>, ctes: &[Cte], depth: u8) -> Vec<Source<'a>> {
        let mut out: Vec<Source<'a>> = Vec::new();
        let (mut expect, mut target, mut in_from) = (false, false, false);
        let dml_using = self.kw_any(part.start, &["DELETE", "MERGE"]);
        let mut i = part.start;
        while i < part.end {
            if std::mem::take(&mut expect)
                && let Some((s, next)) =
                    self.table_ref(i, part.end, ctes, depth, std::mem::take(&mut target))
            {
                out.push(s);
                i = next;
                continue;
            }
            let t = self.toks[i];
            if t.is_punct(b',') && in_from {
                expect = true;
            } else if t.kind == Kind::Word {
                let prev_kw = |ws: &[&str]| i > part.start && self.kw_any(i - 1, ws);
                match t.text(self.src).to_ascii_uppercase().as_str() {
                    "FROM" | "JOIN" | "APPLY" | "STRAIGHT_JOIN" => (in_from, expect) = (true, true),
                    "USING" if dml_using => (in_from, expect) = (true, true),
                    "INTO" if prev_kw(&["INSERT", "IGNORE", "MERGE", "REPLACE"]) => {
                        (expect, target) = (true, true)
                    }
                    "UPDATE" if i == part.start || prev_kw(&["LOW_PRIORITY", "IGNORE"]) => {
                        (expect, target) = (true, true)
                    }
                    "CONFLICT" if prev_kw(&["ON"]) => {
                        if let Some(t) = out.iter().find(|s| s.target) {
                            out.push(Source {
                                name: "excluded".into(),
                                target: false,
                                ..t.clone()
                            });
                        }
                    }
                    "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "SET" | "VALUES"
                    | "RETURNING" | "WINDOW" | "SELECT" | "OUTPUT" => in_from = false,
                    _ => {}
                }
            }
            i = self.step(i);
        }
        out
    }

    /// A table reference at `i`: `[LATERAL|ONLY] name[.name…] [AS] [alias]`,
    /// `(subquery) [AS] alias [(cols)]` or `func(…) [AS] alias [(cols)]`.
    fn table_ref(
        &self,
        mut i: usize,
        hi: usize,
        ctes: &[Cte],
        depth: u8,
        target: bool,
    ) -> Option<(Source<'a>, usize)> {
        while self.kw_any(i, &["LATERAL", "ONLY"]) {
            i += 1;
        }
        let at = i;
        if i >= hi || self.partial == Some(i) {
            return None;
        }
        if self.punct(i, b'(') {
            let c = self.close[i];
            let mut derived = if c > i + 1 {
                self.output_columns(i + 1..c.min(hi), ctes, depth + 1)
            } else {
                Vec::new()
            };
            let (alias, next) = self.alias(self.step(i), hi);
            let (alias, next) = (alias?, self.column_aliases(next, hi, &mut derived));
            return Some((
                Source {
                    name: alias,
                    table: None,
                    derived,
                    is_cte: false,
                    at,
                    target,
                },
                next,
            ));
        }
        if !self.toks[i].is_ident()
            || (self.toks[i].kind == Kind::Word && is_reserved(self.text(i), self.d))
        {
            return None;
        }
        let mut parts = vec![self.name(i)];
        i += 1;
        while self.punct(i, b'.')
            && i + 1 < hi
            && self.toks[i + 1].is_ident()
            && self.partial != Some(i + 1)
        {
            parts.push(self.name(i + 1));
            i += 2;
        }
        if self.punct(i, b'(') && !target {
            // A table function: `generate_series(1, 3) AS g(n)`.
            let (alias, next) = self.alias(self.step(i), hi);
            let mut derived = Vec::new();
            let next = self.column_aliases(next, hi, &mut derived);
            let name = alias.unwrap_or_else(|| parts.pop().unwrap_or_default());
            return Some((
                Source {
                    name,
                    table: None,
                    derived,
                    is_cte: false,
                    at,
                    target,
                },
                next,
            ));
        }
        let (alias, mut next) = self.alias(i, hi);
        // MSSQL table hints: `t WITH (NOLOCK)`.
        if self.kw(next, "WITH") && self.punct(next + 1, b'(') {
            next = self.step(next + 1);
        }
        let last = parts.last().cloned().unwrap_or_default();
        let cte = (parts.len() == 1)
            .then(|| {
                ctes.iter()
                    .rev()
                    .find(|c| c.name.eq_ignore_ascii_case(&last))
            })
            .flatten();
        let source = match cte {
            Some(c) => Source {
                name: alias.unwrap_or(c.name.clone()),
                table: None,
                derived: c.columns.clone(),
                is_cte: true,
                at,
                target,
            },
            None => {
                let table = self.resolve_table(&parts);
                Source {
                    name: alias.unwrap_or_else(|| table.map_or(last, |(_, t)| t.name.clone())),
                    table,
                    derived: Vec::new(),
                    is_cte: false,
                    at,
                    target,
                }
            }
        };
        Some((source, next))
    }

    /// `[AS] alias` at `i`: the alias and the index after it.
    fn alias(&self, i: usize, hi: usize) -> (Option<String>, usize) {
        if i >= hi || self.partial == Some(i) {
            return (None, i);
        }
        if self.kw(i, "AS") {
            return if i + 1 < hi && self.toks[i + 1].is_ident() && self.partial != Some(i + 1) {
                (Some(self.name(i + 1)), i + 2)
            } else {
                (None, i + 1)
            };
        }
        if self.is_name(i) {
            (Some(self.name(i)), i + 1)
        } else {
            (None, i)
        }
    }

    /// An optional `(a, b)` column alias list at `i`, replacing `cols`.
    fn column_aliases(&self, i: usize, hi: usize, cols: &mut Vec<String>) -> usize {
        if i < hi && self.punct(i, b'(') && !self.is_query_group(i) {
            *cols = self.ident_list(i);
            return self.step(i);
        }
        i
    }

    /// The select-list items of a query part.
    pub fn select_items(&self, part: Range<usize>) -> Vec<Item> {
        let Some(mut i) = (part.start..part.end).find(|&i| self.kw(i, "SELECT")) else {
            return Vec::new();
        };
        i += 1;
        loop {
            if self.kw_any(
                i,
                &[
                    "ALL",
                    "DISTINCT",
                    "HIGH_PRIORITY",
                    "STRAIGHT_JOIN",
                    "PERCENT",
                ],
            ) || self.text_upper_starts(i, "SQL_")
            {
                i += 1;
                if self.kw(i, "ON") && self.punct(i + 1, b'(') {
                    i = self.step(i + 1);
                }
            } else if self.kw(i, "TOP") {
                i = self.step(i + 1);
            } else if self.kw(i, "WITH") && self.kw(i + 1, "TIES") {
                i += 2;
            } else {
                break;
            }
        }
        const END: &[&str] = &[
            "FROM", "INTO", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "WINDOW", "OFFSET",
            "FETCH", "FOR",
        ];
        let mut items = Vec::new();
        let mut start = i;
        while i <= part.end {
            if i == part.end || self.kw_any(i, END) || self.punct(i, b',') {
                if i > start {
                    items.push(self.item(start..i));
                }
                if i == part.end || !self.punct(i, b',') {
                    break;
                }
                start = i + 1;
                i += 1;
                continue;
            }
            i = self.step(i).min(part.end);
        }
        items
    }

    fn text_upper_starts(&self, i: usize, prefix: &str) -> bool {
        self.toks.get(i).is_some_and(|t| {
            t.kind == Kind::Word
                && t.text(self.src).len() > prefix.len()
                && t.text(self.src)[..prefix.len()].eq_ignore_ascii_case(prefix)
        })
    }

    fn item(&self, r: Range<usize>) -> Item {
        let (a, b) = (r.start, r.end);
        let mut item = Item {
            range: r.clone(),
            ..Default::default()
        };
        if b - a == 1 && self.op(a, "*") {
            item.star = Some(None);
            return item;
        }
        if b - a == 3 && self.toks[a].is_ident() && self.punct(a + 1, b'.') && self.op(a + 2, "*") {
            item.star = Some(Some(self.name(a)));
            return item;
        }
        let mut expr_end = b;
        if self.d == Dialect::Mssql && b - a >= 3 && self.toks[a].is_ident() && self.op(a + 1, "=")
        {
            item.name = Some(self.name(a));
            return item;
        }
        if b - a >= 3 && self.kw(b - 2, "AS") && self.toks[b - 1].is_ident() {
            item.name = Some(self.name(b - 1));
            expr_end = b - 2;
        } else if b - a >= 2
            && self.is_name(b - 1)
            && self.ends_operand(b - 2)
            && !self.punct(b - 2, b'.')
        {
            item.name = Some(self.name(b - 1));
            expr_end = b - 1;
        }
        // A plain column reference: `c`, `t.c`, `s.t.c`.
        let plain = (a..expr_end).all(|j| {
            if (j - a) % 2 == 0 {
                self.toks[j].is_ident()
            } else {
                self.punct(j, b'.')
            }
        }) && (expr_end - a) % 2 == 1;
        if plain {
            let col = self.name(expr_end - 1);
            let qualifier = (expr_end - a >= 3).then(|| self.name(expr_end - 3));
            item.name.get_or_insert_with(|| col.clone());
            item.column = Some((qualifier, col));
        } else if item.name.is_none()
            && self.toks[a].kind == Kind::Word
            && self.punct(a + 1, b'(')
            && self.close[a + 1] + 1 == expr_end
        {
            // An unaliased call is named after the function (Postgres, and close enough elsewhere).
            item.name = Some(self.name(a));
        }
        item
    }

    /// The output column names of the query in `r` (a CTE or subquery body).
    pub fn output_columns(&self, r: Range<usize>, outer: &[Cte], depth: u8) -> Vec<String> {
        if depth > MAX_DEPTH || r.is_empty() {
            return Vec::new();
        }
        let (local, main) = self.ctes(r.start, r.end, outer, depth);
        let mut visible = outer.to_vec();
        visible.extend(local);
        let Some(part) = self.parts(main, r.end).into_iter().next() else {
            return Vec::new();
        };
        let items = self.select_items(part.clone());
        let mut sources = None;
        let mut out = Vec::new();
        for item in items {
            match &item.star {
                Some(q) => {
                    let sources =
                        sources.get_or_insert_with(|| self.sources(part.clone(), &visible, depth));
                    for s in sources
                        .iter()
                        .filter(|s| q.as_ref().is_none_or(|q| s.name.eq_ignore_ascii_case(q)))
                    {
                        out.extend(s.columns().into_iter().map(|(c, _)| c.to_owned()));
                    }
                }
                None => out.extend(item.name),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use sql_core::ColumnInfo;

    fn catalog() -> Catalog {
        let t = |name: &str, cols: &[&str]| Table {
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| ColumnInfo {
                    name: (*c).into(),
                    type_name: "int".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        Catalog {
            database: "db".into(),
            search_path: vec!["public".into()],
            schemas: vec![
                Schema {
                    name: "public".into(),
                    tables: vec![
                        t("users", &["id", "name"]),
                        t("orders", &["id", "user_id", "total"]),
                    ],
                    ..Default::default()
                },
                Schema {
                    name: "sales".into(),
                    tables: vec![t("Leads", &["id", "score"])],
                    ..Default::default()
                },
            ],
        }
    }

    fn model<'a>(sql: &'a str, cat: &'a Catalog) -> Model<'a> {
        Model::new(
            sql,
            lex(sql, Dialect::Postgres)
                .into_iter()
                .filter(|t| !t.is_trivia())
                .collect(),
            Dialect::Postgres,
            cat,
        )
    }

    fn names(m: &Model, r: Range<usize>) -> Vec<(String, Option<String>)> {
        m.sources(r, &[], 0)
            .into_iter()
            .map(|s| {
                (
                    s.name,
                    s.table.map(|(sc, t)| format!("{}.{}", sc.name, t.name)),
                )
            })
            .collect()
    }

    #[test]
    fn sources_with_aliases() {
        let cat = catalog();
        let sql = "SELECT * FROM users u JOIN public.orders AS o ON o.user_id = u.id, sales.leads LEFT JOIN nope n ON true WHERE x IN (SELECT 1 FROM orders)";
        let m = model(sql, &cat);
        assert_eq!(
            names(&m, 0..m.len()),
            vec![
                ("u".into(), Some("public.users".into())),
                ("o".into(), Some("public.orders".into())),
                ("Leads".into(), Some("sales.Leads".into())),
                ("n".into(), None),
            ]
        );
    }

    #[test]
    fn dml_targets() {
        let cat = catalog();
        for (sql, want) in [
            ("UPDATE users u SET name = 1 FROM orders o", vec!["u", "o"]),
            (
                "DELETE FROM orders USING users WHERE 1",
                vec!["orders", "users"],
            ),
            (
                "INSERT INTO orders AS x (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET total = 1",
                vec!["x", "excluded"],
            ),
            ("SELECT * FROM users u WHERE u.id = 1", vec!["u"]),
        ] {
            let m = model(sql, &cat);
            assert_eq!(
                names(&m, 0..m.len())
                    .into_iter()
                    .map(|n| n.0)
                    .collect::<Vec<_>>(),
                want,
                "{sql}"
            );
        }
    }

    #[test]
    fn cte_and_subquery_columns() {
        let cat = catalog();
        let sql = "WITH a AS (SELECT id, name AS n, count(*) c, u.* FROM users u), b(x, y) AS (SELECT 1, 2) SELECT * FROM a, b, (SELECT total t FROM orders) s, generate_series(1, 3) g(k)";
        let m = model(sql, &cat);
        let (ctes, main) = m.ctes(0, m.len(), &[], 0);
        assert_eq!(ctes[0].columns, vec!["id", "n", "c", "id", "name"]);
        assert_eq!(ctes[1].columns, vec!["x", "y"]);
        let srcs = m.sources(main..m.len(), &ctes, 0);
        let cols: Vec<(String, Vec<String>)> = srcs
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.columns().iter().map(|c| c.0.to_owned()).collect(),
                )
            })
            .collect();
        assert_eq!(cols[2], ("s".to_owned(), vec!["t".to_owned()]));
        assert_eq!(cols[3], ("g".to_owned(), vec!["k".to_owned()]));
        assert_eq!(
            m.output_columns(main..m.len(), &ctes, 0).len(),
            5 + 2 + 1 + 1
        );
    }

    #[test]
    fn parts_split_on_set_operations_and_nested_selects() {
        let cat = catalog();
        let sql = "INSERT INTO t SELECT a FROM x UNION ALL SELECT b FROM y";
        let m = model(sql, &cat);
        let parts: Vec<String> = m
            .parts(0, m.len())
            .into_iter()
            .map(|r| {
                m.toks[r.start..r.end]
                    .iter()
                    .map(|t| t.text(sql))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(
            parts,
            vec!["INSERT INTO t", "SELECT a FROM x", "SELECT b FROM y"]
        );
    }
}
