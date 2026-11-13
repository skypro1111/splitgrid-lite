//! Schema-aware completion. The statement around the caret is analysed as a
//! whole (sources after the caret count, like DataGrip): its query levels,
//! CTEs, sources and the clause the caret is in decide what is offered.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use sql_core::{Catalog, Dialect, RoutineKind, Schema, Table, TableKind};

use crate::ident::{force_quote, quote_ident, unquote};
use crate::keywords::{functions, is_structural};
use crate::lexer::{Kind, Token, lex};
use crate::matcher::{Quality, quality};
use crate::scope::{Cte, Model, Source};
use crate::{Completion, CompletionKind, CompletionResult, split};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clause {
    /// The start of a statement (or of a part after UNION).
    Start,
    Select,
    From,
    Join,
    On,
    Using,
    Where,
    GroupBy,
    Having,
    OrderBy,
    Limit,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Returning,
    Table,
    Conflict,
    Other,
}

/// Ranks: lower sorts first within a match-quality group.
mod rank {
    pub const JOIN: u8 = 0;
    pub const COLUMN: u8 = 1;
    pub const CTE: u8 = 1;
    pub const OUTER_COLUMN: u8 = 2;
    pub const TABLE: u8 = 2;
    pub const ALIAS: u8 = 3;
    pub const SNIPPET: u8 = 3;
    pub const OTHER_TABLE: u8 = 3;
    pub const FUNCTION: u8 = 4;
    pub const SCHEMA: u8 = 4;
    pub const KEYWORD: u8 = 5;
}

struct Cand {
    item: Completion,
    rank: u8,
    quality: Quality,
    /// The match keeps the typed case (`us` → `users` before `UserProfiles`).
    exact_case: bool,
    /// Order within equal rank and quality (keyword lists are ordered by likelihood).
    prio: u16,
}

struct Ctx<'a, 'm> {
    m: &'m Model<'a>,
    d: Dialect,
    cat: &'a Catalog,
    prefix: String,
    /// The user opened a quote: insert names quoted.
    forced: bool,
    upper: bool,
    out: Vec<Cand>,
}

pub fn complete(sql: &str, offset: usize, d: Dialect, cat: &Catalog) -> CompletionResult {
    let mut offset = offset.min(sql.len());
    while !sql.is_char_boundary(offset) {
        offset -= 1;
    }
    let none = || CompletionResult {
        replace: offset..offset,
        items: Vec::new(),
    };
    let all = lex(sql, d);
    let pieces = split::pieces(sql, &all, d);
    let Some(piece) = pieces
        .iter()
        .find(|p| p.span.start <= offset && offset <= p.span.end)
    else {
        return none();
    };
    let toks = &all[narrow(sql, &all, piece.toks.clone(), offset)];

    // The partial word at the caret; nothing inside literals, comments, numbers or parameters.
    let mut partial = None;
    for t in toks {
        let at_end = offset == t.end
            && (!t.closed
                || t.is_line_comment(sql)
                || t.is_ident()
                || matches!(t.kind, Kind::Number | Kind::Param));
        if t.start < offset && (offset < t.end || at_end) {
            match t.kind {
                Kind::Word | Kind::Quoted => partial = Some(*t),
                Kind::Space | Kind::Punct(_) | Kind::Op | Kind::Delimiter => {}
                _ => return none(),
            }
        }
    }
    let sig: Vec<Token> = toks.iter().filter(|t| !t.is_trivia()).copied().collect();
    let (p, replace, prefix, forced) = match partial {
        Some(t) => {
            let p = sig
                .iter()
                .position(|s| s.start == t.start)
                .unwrap_or(sig.len());
            let typed = &sql[t.start..offset];
            (
                p,
                t.start..t.end,
                if t.kind == Kind::Quoted {
                    unquote(typed)
                } else {
                    typed.to_owned()
                },
                t.kind == Kind::Quoted,
            )
        }
        None => (
            sig.iter().take_while(|t| t.end <= offset).count(),
            offset..offset,
            String::new(),
            false,
        ),
    };
    let upper = keyword_case(sql, &sig[..p], &prefix);
    let mut m = Model::new(sql, sig, d, cat);
    m.partial = partial.map(|_| p);
    let mut cx = Ctx {
        m: &m,
        d,
        cat,
        prefix,
        forced,
        upper,
        out: Vec::new(),
    };
    cx.run(p);
    CompletionResult {
        replace,
        items: cx.finish(),
    }
}

/// Upper-case keywords unless the user writes them in lower case.
fn keyword_case(src: &str, before: &[Token], prefix: &str) -> bool {
    if prefix.bytes().any(|b| b.is_ascii_alphabetic()) {
        return !prefix.bytes().any(|b| b.is_ascii_lowercase())
            || prefix.bytes().any(|b| b.is_ascii_uppercase());
    }
    before
        .iter()
        .rev()
        .find(|t| t.kind == Kind::Word && is_structural(t.text(src)))
        .is_none_or(|t| t.text(src).bytes().any(|b| b.is_ascii_uppercase()))
}

/// The sub-statement around the caret inside a piece: MSSQL batches keep
/// `;`-separated statements together, and an unterminated statement followed
/// by a blank line and another statement keyword shouldn't merge with it.
fn narrow(src: &str, all: &[Token], r: Range<usize>, offset: usize) -> Range<usize> {
    const STARTS: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "CREATE", "ALTER", "DROP", "TRUNCATE",
        "MERGE", "EXPLAIN", "CALL", "EXEC",
    ];
    let (mut start, mut depth, mut last_sig) = (r.start, 0i32, None::<Token>);
    for i in r.clone() {
        let t = all[i];
        let boundary = match t.kind {
            Kind::Delimiter => Some(i + 1),
            Kind::Punct(b'(') => {
                depth += 1;
                None
            }
            Kind::Punct(b')') => {
                depth = (depth - 1).max(0);
                None
            }
            Kind::Word if depth == 0 && STARTS.iter().any(|w| t.is_kw(src, w)) => {
                let blank = i > r.start
                    && all[i - 1].kind == Kind::Space
                    && all[i - 1].text(src).matches('\n').count() >= 2;
                let ended = last_sig.is_some_and(|p| {
                    matches!(
                        p.kind,
                        Kind::Quoted | Kind::Str | Kind::Number | Kind::Punct(b')')
                    ) || (p.kind == Kind::Word && !is_structural(p.text(src)))
                });
                (blank && ended).then_some(i)
            }
            _ => None,
        };
        if !t.is_trivia() {
            last_sig = Some(t);
        }
        let Some(next_start) = boundary else { continue };
        if t.start > offset || (t.kind == Kind::Delimiter && t.start >= offset) {
            return start..i;
        }
        start = next_start;
        if t.kind == Kind::Delimiter {
            last_sig = None;
        }
    }
    start..r.end
}

/// A query level: the statement, or a parenthesised subquery.
struct Level {
    lo: usize,
    hi: usize,
    /// The `(` opening it.
    open: Option<usize>,
    main: usize,
}

impl<'a> Ctx<'a, '_> {
    fn run(&mut self, p: usize) {
        let m = self.m;
        // The qualifier chain before the caret: `s.t.|`.
        let mut k = p;
        let mut quals = Vec::new();
        while k >= 2 && m.punct(k - 1, b'.') && m.toks[k - 2].is_ident() {
            quals.insert(0, m.name(k - 2));
            k -= 2;
        }
        // Open groups around the caret, outermost first.
        let groups: Vec<usize> = (0..k)
            .filter(|&i| m.punct(i, b'(') && m.close_of(i) >= k)
            .collect();
        let mut levels = vec![Level {
            lo: 0,
            hi: m.len(),
            open: None,
            main: 0,
        }];
        levels.extend(
            groups
                .iter()
                .filter(|&&o| m.is_query_group(o))
                .map(|&o| Level {
                    lo: o + 1,
                    hi: m.close_of(o),
                    open: Some(o),
                    main: 0,
                }),
        );
        let mut ctes: Vec<Cte> = Vec::new();
        for l in &mut levels {
            let (local, main) = m.ctes(l.lo, l.hi, &ctes, 0);
            ctes.extend(local);
            l.main = main;
        }
        let level = levels.last().expect("the statement level");
        // In the WITH list, or inside a CTE body that isn't a query yet.
        if k < level.main || groups.iter().any(|&o| o >= level.lo && o < level.main) {
            return self.in_with_list(k, level);
        }
        let parts = m.parts(level.main, level.hi);
        let part = parts
            .iter()
            .rev()
            .find(|r| r.start <= k)
            .cloned()
            .unwrap_or(level.main..level.hi);
        // Columns of enclosing queries are visible in correlated subqueries.
        let mut outer: Vec<Source<'a>> = Vec::new();
        for (j, l) in levels.iter().enumerate().skip(1) {
            let o = l.open.expect("subquery levels have an opening paren");
            let up = &levels[j - 1];
            let up_part = m
                .parts(up.main, up.hi)
                .into_iter()
                .rev()
                .find(|r| r.start <= o)
                .unwrap_or(up.main..up.hi);
            let (up_clause, _) = self.clause(up_part.clone(), o);
            let from_item = matches!(up_clause, Clause::From | Clause::Join)
                && !m.kw(o.wrapping_sub(1), "LATERAL");
            if !from_item && !m.kw(o.wrapping_sub(1), "AS") {
                outer.extend(m.sources(up_part, &ctes, 0));
            } else {
                outer.clear();
            }
        }
        let sources = m.sources(part.clone(), &ctes, 0);
        let (clause, group) = self.clause(part.clone(), k);
        let prev = (k > level.lo).then(|| k - 1);
        let scope = Scope {
            sources,
            outer,
            ctes,
            part,
            clause,
            group,
            prev,
            k,
            in_subquery: level.open.is_some(),
        };
        if quals.is_empty() {
            self.unqualified(&scope);
        } else {
            self.qualified(&scope, &quals);
        }
    }

    /// The clause the caret is in, scanning the part's top-level tokens up to
    /// `k`; and the outermost expression group holding the caret, if any.
    fn clause(&self, part: Range<usize>, k: usize) -> (Clause, Option<usize>) {
        let m = &self.m;
        let mut clause = Clause::Start;
        let mut i = part.start;
        let starts_with = |w: &str| m.kw(part.start, w);
        while i < k.min(part.end) {
            if m.punct(i, b'(') && m.close_of(i) >= k {
                return (clause, Some(i));
            }
            if m.toks[i].kind == Kind::Word {
                let prev = |w: &str| i > part.start && m.kw(i - 1, w);
                clause = match m.text(i).to_ascii_uppercase().as_str() {
                    "SELECT" => Clause::Select,
                    "FROM" => Clause::From,
                    "JOIN" | "APPLY" | "STRAIGHT_JOIN" => Clause::Join,
                    "ON" if m.kw(i + 1, "CONFLICT") => Clause::Conflict,
                    "ON" if m.kw(i + 1, "DUPLICATE") => Clause::Other,
                    "ON" => Clause::On,
                    "USING" if starts_with("DELETE") || starts_with("MERGE") => Clause::From,
                    "USING" => Clause::Using,
                    "WHERE" => Clause::Where,
                    "HAVING" => Clause::Having,
                    "BY" if prev("GROUP") => Clause::GroupBy,
                    "BY" if prev("ORDER") => Clause::OrderBy,
                    "LIMIT" | "OFFSET" | "FETCH" => Clause::Limit,
                    "INTO" => Clause::Into,
                    "VALUES" => Clause::Values,
                    "UPDATE" if prev("DO") || prev("KEY") => Clause::Set,
                    "UPDATE" if prev("FOR") => Clause::Other,
                    "UPDATE" => Clause::Update,
                    "SET" => Clause::Set,
                    "DELETE" => Clause::Delete,
                    "RETURNING" | "OUTPUT" => Clause::Returning,
                    "TABLE" | "TRUNCATE" => Clause::Table,
                    "DESCRIBE" | "DESC" if i == part.start => Clause::Table,
                    "CREATE" | "ALTER" | "DROP" | "WINDOW" | "FOR"
                        if clause != Clause::Conflict =>
                    {
                        Clause::Other
                    }
                    _ => clause,
                };
            }
            i = m.step(i);
        }
        (clause, None)
    }

    fn kw(&self, w: &str) -> String {
        if self.upper {
            w.to_owned()
        } else {
            w.to_ascii_lowercase()
        }
    }

    fn quote(&self, name: &str) -> String {
        if self.forced {
            force_quote(name, self.d)
        } else {
            quote_ident(name, self.d)
        }
    }

    fn push(&mut self, key: &str, item: impl FnOnce(&Self) -> Completion, rank: u8, prio: u16) {
        // An empty key matches only an empty prefix (snippets that aren't words).
        let q = if key.is_empty() {
            self.prefix.is_empty().then_some(Quality::Prefix)
        } else {
            quality(&self.prefix, key)
        };
        if let Some(quality) = q {
            let exact_case = key.starts_with(self.prefix.as_str());
            let item = item(self);
            self.out.push(Cand {
                item,
                rank,
                quality,
                exact_case,
                prio,
            });
        }
    }

    fn keywords(&mut self, words: &[&str]) {
        for (i, w) in words.iter().enumerate() {
            self.push(
                w,
                |c| {
                    let k = c.kw(w);
                    Completion {
                        label: k.clone(),
                        kind: CompletionKind::Keyword,
                        insert: k,
                        detail: None,
                    }
                },
                rank::KEYWORD,
                i as u16,
            );
        }
    }

    fn finish(self) -> Vec<Completion> {
        let mut out = self.out;
        out.sort_by_cached_key(|c| {
            (
                c.quality == Quality::Substring,
                c.rank,
                c.quality,
                !c.exact_case,
                c.prio,
                c.item.label.to_lowercase(),
            )
        });
        let mut seen = HashSet::new();
        out.into_iter()
            .filter(|c| seen.insert((c.item.label.clone(), c.item.kind)))
            .map(|c| c.item)
            .collect()
    }

    // ----- positions -----

    fn in_with_list(&mut self, k: usize, level: &Level) {
        let m = self.m;
        let prev = (k > level.lo).then(|| k - 1);
        match prev {
            Some(i) if m.punct(i, b'(') && m.kw(i.wrapping_sub(1), "AS") => {
                self.keywords(&["SELECT", "VALUES"])
            }
            Some(i) if m.punct(i, b')') => {
                self.keywords(&["SELECT", "INSERT INTO", "UPDATE", "DELETE FROM"])
            }
            Some(i) if m.toks[i].is_ident() && !m.kw(i, "WITH") => self.keywords(&["AS"]),
            Some(i) if m.kw(i, "WITH") && matches!(self.d, Dialect::Postgres | Dialect::Sqlite) => {
                self.keywords(&["RECURSIVE"])
            }
            _ => {}
        }
    }

    fn unqualified(&mut self, s: &Scope<'a>) {
        let m = self.m;
        let prev_word = s
            .prev
            .filter(|&i| m.toks[i].kind == Kind::Word)
            .map(|i| m.text(i).to_ascii_uppercase());
        if let Some(w) = prev_word.as_deref()
            && self.follow_ups(w, s)
        {
            return;
        }
        let prev_is = |ws: &[&str]| s.prev.is_some_and(|i| m.kw_any(i, ws));
        let prev_punct = |c: u8| s.prev.is_some_and(|i| m.punct(i, c));
        let after_operand = s.prev.is_some_and(|i| self.after_operand(i));
        match s.clause {
            Clause::Start => match s.prev {
                None if s.in_subquery => self.keywords(&["SELECT", "WITH", "VALUES"]),
                None => self.statement_keywords(),
                Some(i) if m.kw_any(i, &["CALL", "EXEC", "EXECUTE"]) => {
                    self.routines(RoutineKind::Procedure)
                }
                Some(i) if m.punct(i, b')') => {
                    self.keywords(&["SELECT", "INSERT INTO", "UPDATE", "DELETE FROM"])
                }
                _ => {}
            },
            Clause::From
            | Clause::Join
            | Clause::Into
            | Clause::Update
            | Clause::Table
            | Clause::Delete => {
                if let Some(g) = s.group {
                    if s.clause == Clause::Into {
                        self.insert_columns(s, g);
                    } else if matches!(s.clause, Clause::From | Clause::Join)
                        && (m.kw_any(g.wrapping_sub(1), &["FROM", "JOIN", "LATERAL"])
                            || m.punct(g.wrapping_sub(1), b','))
                    {
                        if prev_punct(b'(') {
                            self.keywords(&["SELECT", "VALUES", "WITH"]);
                        }
                    } else if matches!(s.clause, Clause::From | Clause::Join) {
                        self.expression(s);
                    }
                } else if prev_is(&[
                    "FROM",
                    "JOIN",
                    "INTO",
                    "UPDATE",
                    "TABLE",
                    "USING",
                    "LATERAL",
                    "ONLY",
                    "APPLY",
                    "TRUNCATE",
                    "DESCRIBE",
                    "DESC",
                    "STRAIGHT_JOIN",
                ]) || prev_punct(b',')
                {
                    if prev_is(&["TRUNCATE"]) {
                        self.keywords(&["TABLE"]);
                    }
                    self.tables(s, prev_is(&["JOIN"]));
                } else if after_operand {
                    self.after_table(s);
                }
            }
            Clause::Conflict => {
                if let Some(g) = s.group {
                    if m.kw(g.wrapping_sub(1), "CONFLICT") {
                        let cols = s
                            .target()
                            .map(|t| {
                                t.columns()
                                    .into_iter()
                                    .map(|(c, ty)| (c.to_owned(), ty.to_owned()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.plain_columns(cols, &[], false);
                    }
                } else if prev_is(&["DO"]) {
                    self.keywords(&["UPDATE SET", "NOTHING"]);
                } else if after_operand {
                    self.keywords(&["DO UPDATE SET", "DO NOTHING"]);
                }
            }
            Clause::Set => {
                if s.group.is_none() && (prev_is(&["SET", "UPDATE"]) || prev_punct(b',')) {
                    let cols = s
                        .target()
                        .map(|t| {
                            t.columns()
                                .into_iter()
                                .map(|(c, ty)| (c.to_owned(), ty.to_owned()))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.plain_columns(cols, &[], false);
                } else if after_operand && s.group.is_none() {
                    let mut kws = vec!["WHERE"];
                    if matches!(self.d, Dialect::Postgres | Dialect::Mssql) {
                        kws.push("FROM");
                    }
                    if matches!(self.d, Dialect::Postgres | Dialect::Sqlite) {
                        kws.push("RETURNING");
                    }
                    self.keywords(&kws);
                } else {
                    self.expression(s);
                }
            }
            Clause::Values => {
                if s.group.is_some() {
                    if !after_operand {
                        self.functions();
                        self.keywords(&["DEFAULT", "NULL", "TRUE", "FALSE", "CURRENT_TIMESTAMP"]);
                    }
                } else if prev_punct(b')') {
                    match self.d {
                        Dialect::Postgres | Dialect::Sqlite => {
                            self.keywords(&["ON CONFLICT", "RETURNING"])
                        }
                        Dialect::Mysql => self.keywords(&["ON DUPLICATE KEY UPDATE"]),
                        Dialect::Mssql => {}
                    }
                }
            }
            Clause::Limit => {
                if after_operand && s.group.is_none() {
                    self.keywords(&["OFFSET"]);
                }
            }
            Clause::Other => {}
            _ => self.expression(s),
        }
    }

    /// Keywords that complete the previous word (`GROUP` → `BY`); true when the position is settled.
    fn follow_ups(&mut self, w: &str, s: &Scope<'a>) -> bool {
        let d = self.d;
        let list: Vec<&str> = match w {
            "GROUP" | "ORDER" => vec!["BY"],
            "PARTITION" if s.group.is_some() => vec!["BY"],
            "LEFT" | "RIGHT" | "FULL" => vec!["JOIN", "OUTER JOIN"],
            "INNER" => vec!["JOIN"],
            "CROSS" | "OUTER" if d == Dialect::Mssql => vec!["JOIN", "APPLY"],
            "CROSS" | "OUTER" => vec!["JOIN"],
            "NATURAL" => vec!["JOIN", "LEFT JOIN"],
            "INSERT" | "REPLACE" if s.clause == Clause::Start => vec!["INTO"],
            "DELETE" if s.clause == Clause::Delete => vec!["FROM"],
            "IS" => vec!["NULL", "NOT NULL", "TRUE", "FALSE", "DISTINCT FROM"],
            "UNION" | "INTERSECT" | "EXCEPT" => vec!["ALL", "SELECT"],
            "NULLS" => vec!["FIRST", "LAST"],
            "PRIMARY" | "FOREIGN" => vec!["KEY"],
            "EXPLAIN" if s.clause == Clause::Start => match d {
                Dialect::Postgres => vec![
                    "SELECT",
                    "ANALYZE",
                    "(FORMAT JSON)",
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                ],
                Dialect::Sqlite => vec!["QUERY PLAN", "SELECT"],
                _ => vec![
                    "SELECT",
                    "ANALYZE",
                    "FORMAT=JSON",
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                ],
            },
            "CREATE" if s.clause == Clause::Other => {
                let mut v = vec!["TABLE", "VIEW", "INDEX", "UNIQUE INDEX"];
                match d {
                    Dialect::Postgres => v.extend([
                        "OR REPLACE",
                        "SCHEMA",
                        "MATERIALIZED VIEW",
                        "FUNCTION",
                        "TRIGGER",
                        "SEQUENCE",
                        "EXTENSION",
                        "TYPE",
                    ]),
                    Dialect::Mysql => v.extend([
                        "OR REPLACE",
                        "DATABASE",
                        "PROCEDURE",
                        "FUNCTION",
                        "TRIGGER",
                        "EVENT",
                    ]),
                    Dialect::Sqlite => v.extend(["TEMP TABLE", "VIRTUAL TABLE", "TRIGGER"]),
                    Dialect::Mssql => v.extend([
                        "OR ALTER",
                        "SCHEMA",
                        "PROCEDURE",
                        "FUNCTION",
                        "TRIGGER",
                        "SEQUENCE",
                        "TYPE",
                    ]),
                }
                v
            }
            "DROP" if s.clause == Clause::Other && s.prev == Some(s.part.start) => vec![
                "TABLE",
                "VIEW",
                "INDEX",
                "SCHEMA",
                "FUNCTION",
                "TABLE IF EXISTS",
            ],
            "ALTER" if s.clause == Clause::Other && s.prev == Some(s.part.start) => {
                vec!["TABLE", "VIEW", "INDEX", "SCHEMA"]
            }
            _ => return false,
        };
        self.keywords(&list);
        true
    }

    /// Whether the token at `i` completes an operand, so an operator or clause keyword is next.
    fn after_operand(&self, i: usize) -> bool {
        let m = self.m;
        if m.op(i, "*") {
            // `SELECT *`, `t.*`, `count(*)`.
            return i == 0
                || m.kw(i - 1, "SELECT")
                || m.punct(i - 1, b',')
                || m.punct(i - 1, b'.')
                || m.punct(i - 1, b'(');
        }
        if !m.ends_operand(i) {
            return false;
        }
        // MSSQL `TOP 10` / `TOP (10)` precede the select list.
        let top = |j: usize| j > 0 && m.kw(j - 1, "TOP");
        !(top(i)
            || (m.punct(i, b')')
                && (0..i)
                    .rev()
                    .find(|&o| m.punct(o, b'(') && m.close_of(o) == i)
                    .is_some_and(top)))
    }

    fn statement_keywords(&mut self) {
        let mut v = vec![
            "SELECT",
            "INSERT INTO",
            "UPDATE",
            "DELETE FROM",
            "WITH",
            "CREATE TABLE",
            "ALTER TABLE",
            "DROP TABLE",
            "EXPLAIN",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "CREATE INDEX",
            "CREATE VIEW",
            "TRUNCATE TABLE",
            "VALUES",
            "GRANT",
        ];
        match self.d {
            Dialect::Postgres => v.extend([
                "SET",
                "SHOW",
                "COPY",
                "VACUUM",
                "ANALYZE",
                "CREATE SCHEMA",
                "CREATE FUNCTION",
                "TABLE",
                "DO",
                "LISTEN",
                "NOTIFY",
            ]),
            Dialect::Mysql => v.extend([
                "SET",
                "SHOW TABLES",
                "SHOW",
                "USE",
                "DESCRIBE",
                "REPLACE INTO",
                "CALL",
                "CREATE PROCEDURE",
                "START TRANSACTION",
            ]),
            Dialect::Sqlite => v.extend([
                "PRAGMA",
                "ATTACH DATABASE",
                "VACUUM",
                "REPLACE INTO",
                "CREATE TRIGGER",
            ]),
            Dialect::Mssql => v.extend([
                "DECLARE",
                "SET",
                "EXEC",
                "USE",
                "MERGE INTO",
                "PRINT",
                "IF",
                "CREATE PROCEDURE",
                "BEGIN TRAN",
            ]),
        }
        if self.d == Dialect::Mssql {
            v.retain(|w| !matches!(*w, "BEGIN" | "VALUES"));
        }
        self.keywords(&v);
    }

    fn after_table(&mut self, s: &Scope<'a>) {
        let m = self.m;
        let stmt = |w: &str| m.kw(s.part.start, w);
        match s.clause {
            Clause::Into if stmt("MERGE") => self.keywords(&["USING"]),
            Clause::Into => self.after_insert_target(s),
            Clause::Update => self.keywords(&["SET", "AS"]),
            Clause::Table => {
                if m.kw_any(s.part.start, &["ALTER"]) {
                    self.keywords(&[
                        "ADD",
                        "ADD COLUMN",
                        "DROP COLUMN",
                        "ALTER COLUMN",
                        "RENAME TO",
                        "RENAME COLUMN",
                    ]);
                }
            }
            Clause::Delete => self.keywords(&["FROM"]),
            Clause::From if stmt("DELETE") => {
                let mut v = vec!["WHERE"];
                if self.d == Dialect::Postgres {
                    v.extend(["USING", "RETURNING"]);
                }
                self.keywords(&v);
            }
            Clause::From | Clause::Join => {
                let joined = s.clause == Clause::Join;
                let mut v: Vec<&str> = if joined {
                    vec!["ON", "USING", "AS"]
                } else {
                    vec!["WHERE", "AS"]
                };
                v.extend(["JOIN", "LEFT JOIN", "INNER JOIN", "RIGHT JOIN"]);
                if self.d != Dialect::Mysql {
                    v.push("FULL JOIN");
                }
                v.push("CROSS JOIN");
                if self.d == Dialect::Mssql {
                    v.extend(["CROSS APPLY", "OUTER APPLY"]);
                }
                if joined {
                    v.push("WHERE");
                }
                v.extend(["GROUP BY", "ORDER BY"]);
                v.extend(self.limit_keyword());
                v.extend(["UNION", "UNION ALL"]);
                if matches!(self.d, Dialect::Postgres | Dialect::Mysql) {
                    v.push("FOR UPDATE");
                }
                if self.d == Dialect::Postgres {
                    v.push("TABLESAMPLE");
                }
                if self.d == Dialect::Mssql {
                    v.push("WITH (NOLOCK)");
                }
                self.keywords(&v);
            }
            _ => {}
        }
    }

    fn limit_keyword(&self) -> Option<&'static str> {
        (self.d != Dialect::Mssql).then_some("LIMIT")
    }

    fn after_insert_target(&mut self, s: &Scope<'a>) {
        let m = self.m;
        let after_list = s.prev.is_some_and(|i| m.punct(i, b')'));
        let Some(target) = s.target() else {
            return self.keywords(&["VALUES", "SELECT"]);
        };
        if !after_list {
            let cols: Vec<String> = match target.table {
                Some((_, t)) => t
                    .columns
                    .iter()
                    .filter(|c| !c.generated)
                    .map(|c| self.quote(&c.name))
                    .collect(),
                None => target.derived.iter().map(|c| self.quote(c)).collect(),
            };
            if !cols.is_empty() {
                let values = self.kw("VALUES");
                let label = format!("({}) {values} (…)", cols.join(", "));
                let insert = format!("({}) {values} (", cols.join(", "));
                self.push(
                    "",
                    |_| Completion {
                        label,
                        kind: CompletionKind::Snippet,
                        insert,
                        detail: Some(format!("{} columns", cols.len())),
                    },
                    rank::SNIPPET,
                    0,
                );
            }
        }
        let values = self.kw("VALUES");
        self.push(
            "VALUES",
            |_| Completion {
                label: format!("{values} (…)"),
                kind: CompletionKind::Snippet,
                insert: format!("{values} ("),
                detail: None,
            },
            rank::SNIPPET,
            1,
        );
        let mut v = vec!["VALUES", "SELECT"];
        if !after_list && self.d != Dialect::Mysql {
            v.push("DEFAULT VALUES");
        }
        self.keywords(&v);
    }

    /// `INSERT INTO t (|`: the target's columns not listed yet.
    fn insert_columns(&mut self, s: &Scope<'a>, g: usize) {
        let m = self.m;
        let Some(target) = s.target() else { return };
        if !s.prev.is_some_and(|i| m.punct(i, b'(') || m.punct(i, b',')) {
            return;
        }
        let listed: Vec<String> = (g + 1..s.k)
            .filter(|&j| m.toks[j].is_ident())
            .map(|j| m.name(j).to_lowercase())
            .collect();
        let cols = target
            .columns()
            .into_iter()
            .map(|(c, t)| (c.to_owned(), t.to_owned()))
            .collect();
        self.plain_columns(cols, &listed, true);
    }

    /// Unqualified columns of one table, by name or (INSERT column lists) in table order.
    fn plain_columns(&mut self, cols: Vec<(String, String)>, skip: &[String], ordinal: bool) {
        for (i, (c, ty)) in cols.into_iter().enumerate() {
            if skip.contains(&c.to_lowercase()) {
                continue;
            }
            let prio = if ordinal { i as u16 } else { 0 };
            self.push(
                &c,
                |cx| Completion {
                    insert: cx.quote(&c),
                    label: c.clone(),
                    kind: CompletionKind::Column,
                    detail: (!ty.is_empty()).then_some(ty),
                },
                rank::COLUMN,
                prio,
            );
        }
    }

    /// Tables, views, CTEs and schemas; after JOIN, whole joins from foreign keys first.
    fn tables(&mut self, s: &Scope<'a>, join: bool) {
        if join {
            self.fk_joins(s);
        }
        for cte in &s.ctes {
            self.push(
                &cte.name,
                |cx| Completion {
                    label: cte.name.clone(),
                    kind: CompletionKind::Table,
                    insert: cx.quote(&cte.name),
                    detail: Some("CTE".into()),
                },
                rank::CTE,
                0,
            );
        }
        let first = self.unqualified_owner();
        let cat = self.cat;
        for schema in &cat.schemas {
            for t in &schema.tables {
                let visible = first
                    .get(t.name.as_str())
                    .is_some_and(|&owner| std::ptr::eq(owner, schema));
                self.push(
                    &t.name,
                    |cx| {
                        let (label, insert) = if visible {
                            (t.name.clone(), cx.quote(&t.name))
                        } else {
                            (
                                format!("{}.{}", schema.name, t.name),
                                format!("{}.{}", cx.quote(&schema.name), cx.quote(&t.name)),
                            )
                        };
                        Completion {
                            label,
                            kind: table_kind(t),
                            insert,
                            detail: Some(schema.name.clone()),
                        }
                    },
                    if visible {
                        rank::TABLE
                    } else {
                        rank::OTHER_TABLE
                    },
                    0,
                );
            }
            self.push(
                &schema.name,
                |cx| Completion {
                    label: schema.name.clone(),
                    kind: CompletionKind::Schema,
                    insert: cx.quote(&schema.name),
                    detail: Some("schema".into()),
                },
                rank::SCHEMA,
                0,
            );
        }
    }

    /// For each table name, the search-path schema an unqualified reference resolves to.
    fn unqualified_owner(&self) -> HashMap<&'a str, &'a Schema> {
        let mut first = HashMap::new();
        for schema in self.m.search_path() {
            for t in &schema.tables {
                first.entry(t.name.as_str()).or_insert(schema);
            }
        }
        first
    }

    /// How a table is written in a query: bare when the search path finds it, else schema-qualified.
    fn table_text(&self, schema: &Schema, t: &Table) -> String {
        let bare = self
            .m
            .search_path()
            .into_iter()
            .find_map(|s| s.tables.iter().find(|x| x.name == t.name).map(|_| s))
            .is_some_and(|s| std::ptr::eq(s, schema));
        if bare {
            quote_ident(&t.name, self.d)
        } else {
            format!(
                "{}.{}",
                quote_ident(&schema.name, self.d),
                quote_ident(&t.name, self.d)
            )
        }
    }

    /// `orders o ON o.user_id = u.id` for each foreign key between a source and another table, both directions.
    fn fk_joins(&mut self, s: &Scope<'a>) {
        let used: HashSet<String> = s.sources.iter().map(|x| x.name.to_lowercase()).collect();
        let cat = self.cat;
        let mut joins: Vec<(String, String, String)> = Vec::new(); // (match key, text, fk name)
        for src in &s.sources {
            let Some((src_schema, src_table)) = src.table else {
                continue;
            };
            let src_name = quote_ident(&src.name, self.d);
            // Outgoing: this source references another table.
            for fk in &src_table.foreign_keys {
                let Some((rs, rt)) = self.fk_target(src_schema, fk) else {
                    continue;
                };
                let alias = make_alias(&rt.name, &used, self.d);
                let on = pairs(&fk.columns, &fk.ref_columns, |c, r| {
                    format!(
                        "{alias}.{} = {src_name}.{}",
                        quote_ident(r, self.d),
                        quote_ident(c, self.d)
                    )
                });
                joins.push((
                    rt.name.clone(),
                    format!("{} {alias} ON {on}", self.table_text(rs, rt)),
                    fk.name.clone(),
                ));
            }
            // Incoming: other tables reference this source.
            for schema in &cat.schemas {
                for t in &schema.tables {
                    for fk in &t.foreign_keys {
                        // Name check first: resolving every foreign key in a big catalog is the slow part.
                        if !fk.references.name.eq_ignore_ascii_case(&src_table.name)
                            || !self
                                .fk_target(schema, fk)
                                .is_some_and(|(_, rt)| std::ptr::eq(rt, src_table))
                        {
                            continue;
                        }
                        let alias = make_alias(&t.name, &used, self.d);
                        let on = pairs(&fk.columns, &fk.ref_columns, |c, r| {
                            format!(
                                "{alias}.{} = {src_name}.{}",
                                quote_ident(c, self.d),
                                quote_ident(r, self.d)
                            )
                        });
                        joins.push((
                            t.name.clone(),
                            format!("{} {alias} ON {on}", self.table_text(schema, t)),
                            fk.name.clone(),
                        ));
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        for (i, (key, text, fk)) in joins.into_iter().enumerate() {
            if seen.insert(text.clone()) {
                self.push(
                    &key,
                    |_| Completion {
                        label: text.clone(),
                        kind: CompletionKind::Join,
                        insert: text,
                        detail: Some(fk),
                    },
                    rank::JOIN,
                    i as u16,
                );
            }
        }
    }

    /// The table a foreign key references (an unqualified reference looks in the referencing schema first).
    fn fk_target(
        &self,
        from: &'a Schema,
        fk: &sql_core::ForeignKey,
    ) -> Option<(&'a Schema, &'a Table)> {
        let r = &fk.references;
        match &r.schema {
            Some(s) => self.m.resolve_table(&[s.clone(), r.name.clone()]),
            None => from
                .tables
                .iter()
                .find(|t| t.name == r.name)
                .map(|t| (from, t))
                .or_else(|| self.m.resolve_table(std::slice::from_ref(&r.name))),
        }
    }

    /// After `JOIN t x ON`: the foreign-key conditions between `x` and the sources before it.
    fn fk_conditions(&mut self, s: &Scope<'a>) {
        let Some(joined) = s.sources.iter().rev().find(|x| x.at < s.k) else {
            return;
        };
        let Some((_, jt)) = joined.table else { return };
        let mut conds = Vec::new();
        for other in s.sources.iter().filter(|x| x.at < joined.at) {
            let Some((os, ot)) = other.table else {
                continue;
            };
            let (jn, on) = (
                quote_ident(&joined.name, self.d),
                quote_ident(&other.name, self.d),
            );
            let (js, _) = joined.table.expect("checked above");
            for fk in &jt.foreign_keys {
                if self
                    .fk_target(js, fk)
                    .is_some_and(|(_, t)| std::ptr::eq(t, ot))
                {
                    conds.push(pairs(&fk.columns, &fk.ref_columns, |c, r| {
                        format!(
                            "{jn}.{} = {on}.{}",
                            quote_ident(c, self.d),
                            quote_ident(r, self.d)
                        )
                    }));
                }
            }
            for fk in &ot.foreign_keys {
                if self
                    .fk_target(os, fk)
                    .is_some_and(|(_, t)| std::ptr::eq(t, jt))
                {
                    conds.push(pairs(&fk.columns, &fk.ref_columns, |c, r| {
                        format!(
                            "{jn}.{} = {on}.{}",
                            quote_ident(r, self.d),
                            quote_ident(c, self.d)
                        )
                    }));
                }
            }
        }
        for (i, c) in conds.into_iter().enumerate() {
            self.push(
                &joined.name,
                |_| Completion {
                    label: c.clone(),
                    kind: CompletionKind::Join,
                    insert: c,
                    detail: Some("foreign key".into()),
                },
                rank::JOIN,
                i as u16,
            );
        }
    }

    /// An operand position (select list, WHERE, ON …) or what may follow one.
    fn expression(&mut self, s: &Scope<'a>) {
        let m = self.m;
        let prev = s.prev;
        if prev.is_some_and(|i| m.kw(i, "AS")) {
            return;
        }
        if let Some(i) = prev
            && self.after_operand(i)
        {
            return self.after_operand_keywords(s);
        }
        // `OVER (|`
        if let Some(g) = s.group
            && prev == Some(g)
            && m.kw(g.wrapping_sub(1), "OVER")
        {
            return self.keywords(&["PARTITION BY", "ORDER BY"]);
        }
        if s.clause == Clause::On && prev.is_some_and(|i| m.kw(i, "ON")) {
            self.fk_conditions(s);
        }
        self.columns(s);
        if s.clause == Clause::Select
            && s.group.is_none()
            && prev.is_some_and(|i| m.kw_any(i, &["SELECT", "DISTINCT", "ALL"]) || m.punct(i, b','))
        {
            self.star(&s.sources, false);
        }
        if matches!(s.clause, Clause::OrderBy | Clause::GroupBy | Clause::Having) {
            self.output_aliases(s);
        }
        for src in &s.sources {
            self.push(
                &src.name,
                |cx| Completion {
                    label: src.name.clone(),
                    kind: CompletionKind::Alias,
                    insert: cx.quote(&src.name),
                    detail: Some(source_detail(src)),
                },
                rank::ALIAS,
                0,
            );
        }
        self.functions();
        let mut kws = vec!["NOT", "EXISTS", "CASE", "NULL", "TRUE", "FALSE"];
        if s.clause == Clause::Select && prev.is_some_and(|i| m.kw(i, "SELECT")) {
            kws.splice(0..0, ["DISTINCT"]);
            if self.d == Dialect::Mssql {
                kws.insert(1, "TOP");
            }
        }
        if prev.is_some_and(|i| m.punct(i, b'(')) {
            kws.insert(0, "SELECT");
        }
        if s.clause == Clause::Where && s.group.is_none() {
            kws.extend(["CURRENT_DATE", "CURRENT_TIMESTAMP"]);
        }
        self.keywords(&kws);
    }

    fn after_operand_keywords(&mut self, s: &Scope<'a>) {
        let d = self.d;
        let ops: &[&str] = &[
            "AND",
            "OR",
            "IS NULL",
            "IS NOT NULL",
            "IN",
            "LIKE",
            "BETWEEN",
            "NOT",
        ];
        let mut v: Vec<&str> = Vec::new();
        let in_group = s.group.is_some();
        match s.clause {
            Clause::Select if in_group => v.extend(["AS", "AND", "OR", "FROM"]),
            Clause::Select => {
                v.extend(["FROM", "AS"]);
                if self.m.kw(s.part.start, "SELECT")
                    && !(s.part.start..s.part.end).any(|i| self.m.kw(i, "FROM"))
                {
                    v.extend(["UNION"]);
                }
                v.extend(ops);
                v.push("OVER");
            }
            Clause::Where | Clause::On | Clause::Having | Clause::Set => {
                v.extend(ops);
                if d == Dialect::Postgres {
                    v.push("ILIKE");
                }
                if !in_group {
                    match s.clause {
                        Clause::On => v.extend([
                            "WHERE",
                            "JOIN",
                            "LEFT JOIN",
                            "INNER JOIN",
                            "GROUP BY",
                            "ORDER BY",
                        ]),
                        Clause::Where => v.extend(["GROUP BY", "ORDER BY"]),
                        Clause::Having => v.extend(["ORDER BY"]),
                        _ => {}
                    }
                    if s.clause != Clause::Set {
                        v.extend(self.limit_keyword());
                        v.extend(["UNION"]);
                    }
                    if s.clause == Clause::Where
                        && matches!(d, Dialect::Postgres | Dialect::Sqlite)
                        && self.m.kw_any(s.part.start, &["UPDATE", "DELETE"])
                    {
                        v.push("RETURNING");
                    }
                }
            }
            Clause::GroupBy if !in_group => {
                v.extend(["HAVING", "ORDER BY"]);
                v.extend(self.limit_keyword());
                if d == Dialect::Postgres {
                    v.extend(["ROLLUP", "CUBE"]);
                }
            }
            Clause::OrderBy if !in_group => {
                v.extend(["ASC", "DESC"]);
                if matches!(d, Dialect::Postgres | Dialect::Sqlite) {
                    v.extend(["NULLS FIRST", "NULLS LAST"]);
                }
                v.extend(self.limit_keyword());
                v.push("OFFSET");
                if d == Dialect::Mssql {
                    v.push("OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY");
                }
            }
            Clause::Returning => v.push("AS"),
            _ => v.extend(ops),
        }
        self.keywords(&v);
    }

    /// Columns of every source in scope; names shared by several sources are qualified.
    fn columns(&mut self, s: &Scope<'a>) {
        let mut count: HashMap<String, usize> = HashMap::new();
        for src in &s.sources {
            for (c, _) in src.columns() {
                *count.entry(c.to_lowercase()).or_default() += 1;
            }
        }
        for (rank, list) in [(rank::COLUMN, &s.sources), (rank::OUTER_COLUMN, &s.outer)] {
            for src in list {
                for (c, ty) in src.columns() {
                    let qualify = rank == rank::OUTER_COLUMN
                        || count.get(&c.to_lowercase()).copied().unwrap_or(0) > 1;
                    self.push(
                        c,
                        |cx| {
                            let (label, insert) = if qualify {
                                (
                                    format!("{}.{c}", src.name),
                                    format!("{}.{}", quote_ident(&src.name, cx.d), cx.quote(c)),
                                )
                            } else {
                                (c.to_owned(), cx.quote(c))
                            };
                            let detail = Some(if ty.is_empty() {
                                src.name.clone()
                            } else {
                                ty.to_owned()
                            });
                            Completion {
                                label,
                                kind: CompletionKind::Column,
                                insert,
                                detail,
                            }
                        },
                        rank,
                        0,
                    );
                }
            }
        }
    }

    /// `*` expanded to the column list of `srcs`, qualified when there are
    /// several. After a typed `q.` the first column follows the qualifier and
    /// the rest repeat it.
    fn star(&mut self, srcs: &[Source<'a>], after_qualifier: bool) {
        let many = srcs.len() > 1 || after_qualifier;
        let mut cols = Vec::new();
        for src in srcs {
            for (c, _) in src.columns() {
                let q = quote_ident(c, self.d);
                let bare = !many || (after_qualifier && cols.is_empty());
                cols.push(if bare {
                    q
                } else {
                    format!("{}.{q}", quote_ident(&src.name, self.d))
                });
            }
        }
        if cols.is_empty() {
            return;
        }
        let n = cols.len();
        let insert = cols.join(", ");
        let label = match srcs {
            [src] if after_qualifier => format!("{}.* → {}", src.name, preview(&insert)),
            _ => format!("* → {}", preview(&insert)),
        };
        self.push(
            "",
            |_| Completion {
                label,
                kind: CompletionKind::Snippet,
                insert,
                detail: Some(format!("{n} columns")),
            },
            rank::SNIPPET,
            0,
        );
    }

    /// Select-list aliases, usable in ORDER BY (and GROUP BY / HAVING where the dialect allows).
    fn output_aliases(&mut self, s: &Scope<'a>) {
        for item in self.m.select_items(s.part.clone()) {
            if let Some(name) = item.name.filter(|_| item.column.is_none()) {
                self.push(
                    &name,
                    |cx| Completion {
                        insert: cx.quote(&name),
                        label: name.clone(),
                        kind: CompletionKind::Alias,
                        detail: Some("column alias".into()),
                    },
                    rank::ALIAS,
                    0,
                );
            }
        }
    }

    fn functions(&mut self) {
        for (name, sig) in functions(self.d) {
            self.push(
                name,
                |cx| {
                    let n = cx.kw(name);
                    Completion {
                        label: n.clone(),
                        kind: CompletionKind::Function,
                        insert: format!("{n}("),
                        detail: Some(sig.into()),
                    }
                },
                rank::FUNCTION,
                0,
            );
        }
        self.routines(RoutineKind::Function);
    }

    fn routines(&mut self, kind: RoutineKind) {
        for schema in self.m.search_path() {
            for r in schema.routines.iter().filter(|r| r.kind == kind) {
                self.push(
                    &r.name,
                    |cx| {
                        let name = cx.quote(&r.name);
                        let insert = if kind == RoutineKind::Function {
                            format!("{name}(")
                        } else {
                            name
                        };
                        Completion {
                            label: r.name.clone(),
                            kind: CompletionKind::Function,
                            insert,
                            detail: Some(format!(
                                "{}{}",
                                r.signature,
                                r.returns
                                    .as_ref()
                                    .map(|t| format!(" → {t}"))
                                    .unwrap_or_default()
                            )),
                        }
                    },
                    rank::FUNCTION,
                    0,
                );
            }
        }
    }

    /// After `q.` (or `s.t.`): a source's or table's columns, or a schema's tables.
    fn qualified<'s>(&mut self, s: &'s Scope<'a>, quals: &[String]) {
        let m = self.m;
        let chain_prev = s.prev;
        let table_position = matches!(
            s.clause,
            Clause::From
                | Clause::Join
                | Clause::Into
                | Clause::Update
                | Clause::Table
                | Clause::Delete
        ) && s.group.is_none()
            && (chain_prev.is_some_and(|i| {
                m.kw_any(
                    i,
                    &[
                        "FROM", "JOIN", "INTO", "UPDATE", "TABLE", "USING", "LATERAL", "ONLY",
                        "APPLY", "TRUNCATE", "DESCRIBE",
                    ],
                ) || m.punct(i, b',')
            }));
        let last = quals.last().expect("a qualifier");
        if table_position {
            if let Some(schema) = m.schema(last) {
                self.schema_tables(schema);
            }
            return;
        }
        if quals.len() == 1 {
            let find = |list: &'s [Source<'a>]| {
                list.iter()
                    .position(|x| x.name == *last)
                    .or_else(|| list.iter().position(|x| x.name.eq_ignore_ascii_case(last)))
                    .map(|i| &list[i..=i])
            };
            if let Some(src) = find(&s.sources).or_else(|| find(&s.outer)) {
                self.source_columns(&src[0]);
                if s.clause == Clause::Select && s.group.is_none() {
                    self.star(src, true);
                }
                return;
            }
            if let Some(cte) = s.ctes.iter().find(|c| c.name.eq_ignore_ascii_case(last)) {
                let cols = cte
                    .columns
                    .iter()
                    .map(|c| (c.clone(), String::new()))
                    .collect();
                return self.plain_columns(cols, &[], false);
            }
            if let Some(schema) = m.schema(last) {
                return self.schema_tables(schema);
            }
        }
        if let Some((_, t)) = m.resolve_table(quals) {
            let cols = t
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.type_name.clone()))
                .collect();
            self.plain_columns(cols, &[], false);
        } else if let Some(schema) = m.schema(last) {
            self.schema_tables(schema);
        }
    }

    fn source_columns(&mut self, src: &Source<'a>) {
        let cols = src
            .columns()
            .into_iter()
            .map(|(c, t)| (c.to_owned(), t.to_owned()))
            .collect();
        self.plain_columns(cols, &[], false);
    }

    fn schema_tables(&mut self, schema: &'a Schema) {
        for t in &schema.tables {
            self.push(
                &t.name,
                |cx| Completion {
                    label: t.name.clone(),
                    kind: table_kind(t),
                    insert: cx.quote(&t.name),
                    detail: Some(schema.name.clone()),
                },
                rank::TABLE,
                0,
            );
        }
        for r in schema
            .routines
            .iter()
            .filter(|r| r.kind == RoutineKind::Function)
        {
            self.push(
                &r.name,
                |cx| Completion {
                    label: r.name.clone(),
                    kind: CompletionKind::Function,
                    insert: format!("{}(", cx.quote(&r.name)),
                    detail: Some(r.signature.clone()),
                },
                rank::FUNCTION,
                0,
            );
        }
    }
}

struct Scope<'a> {
    sources: Vec<Source<'a>>,
    outer: Vec<Source<'a>>,
    ctes: Vec<Cte>,
    part: Range<usize>,
    clause: Clause,
    /// The outermost expression group (inside the part) holding the caret.
    group: Option<usize>,
    /// The token before the caret's word (or qualifier chain), within the part.
    prev: Option<usize>,
    /// The caret's token index (start of the qualifier chain).
    k: usize,
    /// The caret's query is a parenthesised subquery.
    in_subquery: bool,
}

impl<'a> Scope<'a> {
    /// The DML target; an MSSQL `UPDATE u … FROM t u` target is the FROM source it names.
    fn target(&self) -> Option<&Source<'a>> {
        let t = self.sources.iter().find(|s| s.target)?;
        if t.table.is_none() && t.derived.is_empty() {
            return self
                .sources
                .iter()
                .find(|o| !o.target && o.name.eq_ignore_ascii_case(&t.name))
                .or(Some(t));
        }
        Some(t)
    }
}

fn table_kind(t: &Table) -> CompletionKind {
    match t.kind {
        TableKind::View | TableKind::MaterializedView => CompletionKind::View,
        TableKind::Table | TableKind::Foreign => CompletionKind::Table,
    }
}

fn source_detail(s: &Source) -> String {
    match s.table {
        Some((schema, t)) => format!("{}.{}", schema.name, t.name),
        None if s.is_cte => "CTE".into(),
        None => "subquery".into(),
    }
}

fn pairs(cols: &[String], refs: &[String], f: impl Fn(&str, &str) -> String) -> String {
    cols.iter()
        .zip(refs)
        .map(|(c, r)| f(c, r))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A short alias from the name's word initials (`order_items` → `oi`), made
/// unique against the names already in scope.
fn make_alias(table: &str, used: &HashSet<String>, d: Dialect) -> String {
    let b = table.as_bytes();
    let mut base: String = (0..b.len())
        .filter(|&i| {
            b[i].is_ascii_alphabetic()
                && (i == 0
                    || !b[i - 1].is_ascii_alphanumeric()
                    || (b[i - 1].is_ascii_lowercase() && b[i].is_ascii_uppercase()))
        })
        .map(|i| b[i].to_ascii_lowercase() as char)
        .collect();
    if base.is_empty() {
        base.push('t');
    }
    let ok = |a: &str| !used.contains(a) && !crate::ident::needs_quoting(a, d);
    if ok(&base) {
        return base;
    }
    (1..)
        .map(|n| format!("{base}{n}"))
        .find(|a| ok(a))
        .expect("an unused alias")
}

#[cfg(test)]
#[path = "complete_tests.rs"]
mod tests;

fn preview(s: &str) -> String {
    if s.chars().count() <= 40 {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(39).collect::<String>())
    }
}
