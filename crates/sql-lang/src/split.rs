//! Splitting a script into statements: terminators outside strings/comments,
//! MySQL `DELIMITER`, MSSQL `GO`, and routine bodies whose `BEGIN … END`
//! blocks contain `;` that don't end the statement.

use std::ops::Range;

use sql_core::Dialect;

use crate::lexer::{Kind, Token, lex};
use crate::{Statement, StatementKind};

/// A segment of the script between two terminators.
#[derive(Clone, Debug)]
pub struct Piece {
    /// Token indices of the segment (terminator excluded).
    pub toks: Range<usize>,
    /// Byte span from just after the previous terminator to the next one.
    pub span: Range<usize>,
}

fn next_sig(toks: &[Token], from: usize) -> Option<&Token> {
    toks[from..].iter().find(|t| !t.is_trivia())
}

const ROUTINE_WORDS: &[&str] = &["FUNCTION", "PROCEDURE", "PROC", "TRIGGER", "EVENT"];

pub fn pieces(src: &str, toks: &[Token], d: Dialect) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut seg = (0usize, 0usize); // (first token, first byte)
    let mut st = SegState::default();
    let push = |out: &mut Vec<Piece>, seg: (usize, usize), idx: usize, byte: usize| {
        out.push(Piece {
            toks: seg.0..idx,
            span: seg.1..byte,
        })
    };
    for (idx, t) in toks.iter().enumerate() {
        match t.kind {
            Kind::Go | Kind::DelimiterCmd => {
                push(&mut out, seg, idx, t.start);
                seg = (idx + 1, t.end);
                st = SegState::default();
            }
            Kind::Delimiter => {
                if st.depth == 0 && !st.until_go {
                    push(&mut out, seg, idx, t.start);
                    seg = (idx + 1, t.end);
                    st = SegState::default();
                }
            }
            Kind::Space | Kind::Comment => {}
            _ => {
                st.word(src, toks, idx, d);
                st.sig += 1;
            }
        }
    }
    push(&mut out, seg, toks.len(), src.len());
    out
}

#[derive(Default)]
struct SegState {
    /// Significant tokens seen in the segment.
    sig: usize,
    first_create: bool,
    /// Still looking for FUNCTION/PROCEDURE/TRIGGER in a CREATE's head.
    scanning_head: bool,
    routine: bool,
    /// Open BEGIN/CASE blocks whose `;` don't end the statement.
    depth: u32,
    /// MSSQL: the segment runs to the next `GO` (a routine, or a batch with variables).
    until_go: bool,
    /// The previous word was END (so `END CASE` doesn't open a CASE).
    after_end: bool,
}

impl SegState {
    fn word(&mut self, src: &str, toks: &[Token], idx: usize, d: Dialect) {
        let t = &toks[idx];
        if self.sig == 0 {
            self.first_create = t.is_kw(src, "CREATE") || t.is_kw(src, "ALTER");
            self.scanning_head = self.first_create;
            if d == Dialect::Mssql && t.is_kw(src, "DECLARE") {
                self.until_go = true;
            }
        }
        if self.scanning_head {
            if t.is_punct(b'(') || self.sig > 12 {
                self.scanning_head = false;
            } else if ROUTINE_WORDS.iter().any(|w| t.is_kw(src, w))
                || (d == Dialect::Mssql && t.is_kw(src, "VIEW"))
            {
                self.routine = true;
                self.scanning_head = false;
                self.until_go |= d == Dialect::Mssql;
            }
        }
        let after_end = std::mem::replace(&mut self.after_end, t.is_kw(src, "END"));
        if t.kind != Kind::Word || !(self.routine || d == Dialect::Mssql) {
            return;
        }
        let next = next_sig(toks, idx + 1);
        let next_is = |w: &str| next.is_some_and(|n| n.is_kw(src, w));
        if t.is_kw(src, "BEGIN") {
            let tran = [
                "TRAN",
                "TRANSACTION",
                "DISTRIBUTED",
                "DIALOG",
                "CONVERSATION",
                "WORK",
            ]
            .iter()
            .any(|w| next_is(w));
            let bare = next.is_none_or(|n| n.kind == Kind::Delimiter);
            if !tran && !(bare && !self.routine) {
                self.depth += 1;
            }
        } else if t.is_kw(src, "CASE") && self.depth > 0 && !after_end {
            self.depth += 1;
        } else if t.is_kw(src, "END")
            && self.depth > 0
            && !["IF", "LOOP", "WHILE", "REPEAT"].iter().any(|w| next_is(w))
        {
            self.depth -= 1;
        }
    }
}

/// The statement's byte range inside its piece: from the first token that
/// isn't whitespace or a comment trailing the previous terminator's line, to
/// the end of the last significant token.
fn trimmed(src: &str, toks: &[Token], p: &Piece) -> Option<Range<usize>> {
    let seg = &toks[p.toks.clone()];
    // Trailing `;` inside an MSSQL batch kept whole are terminators too.
    let last = seg
        .iter()
        .rev()
        .find(|t| !t.is_trivia() && t.kind != Kind::Delimiter)?;
    let mut seen_nl = p.span.start == 0 || src[..p.span.start].ends_with('\n');
    let mut start = None;
    for t in seg {
        match t.kind {
            Kind::Space => seen_nl |= t.text(src).contains('\n'),
            Kind::Comment if !seen_nl => {}
            _ => {
                start = Some(t.start);
                break;
            }
        }
    }
    Some(start?..last.end)
}

pub fn classify(src: &str, toks: &[Token], d: Dialect) -> StatementKind {
    let sig: Vec<&Token> = toks
        .iter()
        .filter(|t| !t.is_trivia() && !t.is_punct(b'('))
        .take(64)
        .collect();
    let Some(first) = sig.first() else {
        return StatementKind::Other;
    };
    let w = first.text(src).to_ascii_uppercase();
    let next = |i: usize| {
        sig.get(i)
            .map(|t| t.text(src).to_ascii_uppercase())
            .unwrap_or_default()
    };
    match w.as_str() {
        "SELECT" | "VALUES" | "TABLE" => StatementKind::Select,
        "WITH" => with_kind(src, toks),
        "INSERT" | "REPLACE" => StatementKind::Insert,
        "UPDATE" => StatementKind::Update,
        "DELETE" => StatementKind::Delete,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "COMMENT" | "RENAME" | "GRANT" | "REVOKE" => {
            StatementKind::Ddl
        }
        "BEGIN" if d == Dialect::Mssql => match next(1).as_str() {
            "TRAN" | "TRANSACTION" | "DISTRIBUTED" => StatementKind::Transaction,
            _ => StatementKind::Other,
        },
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "ABORT" | "END" => {
            StatementKind::Transaction
        }
        "SAVE" if d == Dialect::Mssql => StatementKind::Transaction,
        _ => StatementKind::Other,
    }
}

/// `WITH … SELECT` is a select unless a CTE body modifies data.
fn with_kind(src: &str, toks: &[Token]) -> StatementKind {
    let mut depth = 0i32;
    let mut dml_in_cte = false;
    for t in toks.iter().filter(|t| !t.is_trivia()) {
        if t.is_punct(b'(') {
            depth += 1;
        } else if t.is_punct(b')') {
            depth -= 1;
        } else if t.kind == Kind::Word {
            let dml = ["INSERT", "UPDATE", "DELETE", "MERGE"]
                .iter()
                .any(|w| t.is_kw(src, w));
            if depth > 0 && dml {
                dml_in_cte = true;
            } else if depth == 0 {
                let kind = match t.text(src).to_ascii_uppercase().as_str() {
                    "SELECT" | "VALUES" | "TABLE" if !dml_in_cte => StatementKind::Select,
                    "SELECT" | "VALUES" | "TABLE" | "MERGE" => StatementKind::Other,
                    "INSERT" => StatementKind::Insert,
                    "UPDATE" => StatementKind::Update,
                    "DELETE" => StatementKind::Delete,
                    _ => continue,
                };
                return kind;
            }
        }
    }
    StatementKind::Other
}

pub fn split(sql: &str, d: Dialect) -> Vec<Statement> {
    let toks = lex(sql, d);
    pieces(sql, &toks, d)
        .iter()
        .filter_map(|p| {
            let range = trimmed(sql, &toks, p)?;
            Some(Statement {
                text: sql[range.clone()].to_owned(),
                kind: classify(sql, &toks[p.toks.clone()], d),
                range,
            })
        })
        .collect()
}

pub fn statement_at(sql: &str, offset: usize, d: Dialect) -> Option<Statement> {
    let stmts = split(sql, d);
    let offset = offset.min(sql.len());
    if let Some(s) = stmts
        .iter()
        .find(|s| s.range.start <= offset && offset <= s.range.end)
    {
        return Some(s.clone());
    }
    let line_of = |at: usize| sql[..at].bytes().filter(|&b| b == b'\n').count();
    let caret_line = line_of(offset);
    // After a statement on the same line (`SELECT 1; |`) or on the line above.
    let before = stmts.iter().rev().find(|s| s.range.end <= offset);
    if let Some(s) = before
        && caret_line - line_of(s.range.end) <= 1
        && only_terminator_gap(sql, s.range.end, offset, d)
    {
        return Some(s.clone());
    }
    // Just before a statement on the same line (`  |SELECT 1`).
    stmts
        .iter()
        .find(|s| {
            s.range.start >= offset
                && sql[offset..s.range.start].trim().is_empty()
                && !sql[offset..s.range.start].contains('\n')
        })
        .cloned()
}

/// Whether `sql[from..to]` holds nothing but a terminator, whitespace and comments.
fn only_terminator_gap(sql: &str, from: usize, to: usize, d: Dialect) -> bool {
    lex(&sql[from..to], d)
        .iter()
        .all(|t| t.is_trivia() || t.kind == Kind::Delimiter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;

    fn texts(sql: &str, d: Dialect) -> Vec<String> {
        split(sql, d).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splitting_table() {
        let cases: &[(Dialect, &str, &[&str])] = &[
            (Postgres, "SELECT 1; SELECT 2;", &["SELECT 1", "SELECT 2"]),
            (
                Postgres,
                "SELECT 1;;  ; SELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
            (
                Postgres,
                "SELECT 'a;b'; SELECT \"x;y\"",
                &["SELECT 'a;b'", "SELECT \"x;y\""],
            ),
            (
                Postgres,
                "SELECT 1 -- ; no\n; SELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
            (
                Postgres,
                "SELECT /* ; /* ; */ ; */ 1; SELECT 2",
                &["SELECT /* ; /* ; */ ; */ 1", "SELECT 2"],
            ),
            (Postgres, "-- only a comment\n/* and another */", &[]),
            (Postgres, "SELECT 1; -- trailing\n", &["SELECT 1"]),
            (
                Postgres,
                "SELECT 1; -- one\nSELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
            (
                Postgres,
                "SELECT 1;\n-- about two\nSELECT 2",
                &["SELECT 1", "-- about two\nSELECT 2"],
            ),
            (
                Postgres,
                "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()",
                &[
                    "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql",
                    "SELECT f()",
                ],
            ),
            (
                Postgres,
                "DO $body$ BEGIN PERFORM 1; END $body$; SELECT 1",
                &["DO $body$ BEGIN PERFORM 1; END $body$", "SELECT 1"],
            ),
            (
                Postgres,
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT 2; END; SELECT 3",
                &[
                    "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT 2; END",
                    "SELECT 3",
                ],
            ),
            (
                Postgres,
                "BEGIN; UPDATE t SET a = 1; COMMIT;",
                &["BEGIN", "UPDATE t SET a = 1", "COMMIT"],
            ),
            (
                Postgres,
                "SELECT 'unterminated; SELECT 2",
                &["SELECT 'unterminated; SELECT 2"],
            ),
            (
                Mysql,
                "SELECT \"a;b\"; SELECT `c;d`",
                &["SELECT \"a;b\"", "SELECT `c;d`"],
            ),
            (
                Mysql,
                "SELECT 'it\\'s;'; SELECT 2",
                &["SELECT 'it\\'s;'", "SELECT 2"],
            ),
            (
                Mysql,
                "SELECT 1 # ; c\n; SELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
            (
                Mysql,
                "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nCALL p();",
                &[
                    "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                    "CALL p()",
                ],
            ),
            (
                Mysql,
                "DELIMITER $$\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END$$\nDELIMITER ;",
                &["CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END"],
            ),
            (
                Mysql,
                "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; CASE WHEN 1 THEN SELECT 2; END CASE; END; SELECT 3",
                &[
                    "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; CASE WHEN 1 THEN SELECT 2; END CASE; END",
                    "SELECT 3",
                ],
            ),
            (
                Mysql,
                "BEGIN; SELECT 1; COMMIT",
                &["BEGIN", "SELECT 1", "COMMIT"],
            ),
            (
                Sqlite,
                "CREATE TRIGGER tr AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE c SET x = 1; END; SELECT 1",
                &[
                    "CREATE TRIGGER tr AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE c SET x = 1; END",
                    "SELECT 1",
                ],
            ),
            (
                Sqlite,
                "SELECT [a;b] FROM t; SELECT 2",
                &["SELECT [a;b] FROM t", "SELECT 2"],
            ),
            (
                Mssql,
                "SELECT 1\nGO\nSELECT 2\ngo 2\nSELECT 3",
                &["SELECT 1", "SELECT 2", "SELECT 3"],
            ),
            (
                Mssql,
                "SELECT 1; SELECT [x;y]",
                &["SELECT 1", "SELECT [x;y]"],
            ),
            (
                Mssql,
                "DECLARE @x int = 1; SELECT @x;\nGO\nSELECT 2",
                &["DECLARE @x int = 1; SELECT @x", "SELECT 2"],
            ),
            (
                Mssql,
                "CREATE PROCEDURE p AS SELECT 1; SELECT 2;\nGO\nEXEC p",
                &["CREATE PROCEDURE p AS SELECT 1; SELECT 2", "EXEC p"],
            ),
            (
                Mssql,
                "IF 1 = 1 BEGIN SELECT 1; SELECT 2; END; SELECT 3",
                &["IF 1 = 1 BEGIN SELECT 1; SELECT 2; END", "SELECT 3"],
            ),
            (
                Mssql,
                "BEGIN TRY SELECT 1; END TRY BEGIN CATCH SELECT 2; END CATCH; SELECT 3",
                &[
                    "BEGIN TRY SELECT 1; END TRY BEGIN CATCH SELECT 2; END CATCH",
                    "SELECT 3",
                ],
            ),
            (
                Mssql,
                "BEGIN TRAN; UPDATE t SET a = 1; COMMIT",
                &["BEGIN TRAN", "UPDATE t SET a = 1", "COMMIT"],
            ),
            (
                Mssql,
                "SELECT 1 /* GO\n */; SELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
        ];
        for (d, sql, want) in cases {
            assert_eq!(texts(sql, *d), *want, "{d:?}: {sql}");
        }
    }

    #[test]
    fn ranges_point_into_the_script() {
        let sql = "  SELECT 1 ;\n\nUPDATE t SET a = 1";
        for s in split(sql, Postgres) {
            assert_eq!(&sql[s.range.clone()], s.text);
        }
        assert_eq!(split(sql, Postgres)[0].range, 2..10);
    }

    #[test]
    fn kinds() {
        let cases: &[(Dialect, &str, StatementKind)] = &[
            (Postgres, "select 1", StatementKind::Select),
            (
                Postgres,
                "(SELECT 1) UNION (SELECT 2)",
                StatementKind::Select,
            ),
            (Postgres, "VALUES (1)", StatementKind::Select),
            (
                Postgres,
                "WITH a AS (SELECT 1) SELECT * FROM a",
                StatementKind::Select,
            ),
            (
                Postgres,
                "WITH a AS (SELECT 1) INSERT INTO t SELECT * FROM a",
                StatementKind::Insert,
            ),
            (
                Postgres,
                "WITH a AS (SELECT 1) UPDATE t SET x = 1",
                StatementKind::Update,
            ),
            (
                Postgres,
                "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
                StatementKind::Other,
            ),
            (Postgres, "INSERT INTO t VALUES (1)", StatementKind::Insert),
            (Mysql, "REPLACE INTO t VALUES (1)", StatementKind::Insert),
            (Postgres, "UPDATE t SET a = 1", StatementKind::Update),
            (Postgres, "DELETE FROM t", StatementKind::Delete),
            (Postgres, "TRUNCATE t", StatementKind::Ddl),
            (Postgres, "CREATE TABLE t (a int)", StatementKind::Ddl),
            (Postgres, "COMMENT ON TABLE t IS 'x'", StatementKind::Ddl),
            (Postgres, "BEGIN", StatementKind::Transaction),
            (Mysql, "START TRANSACTION", StatementKind::Transaction),
            (Postgres, "SAVEPOINT a", StatementKind::Transaction),
            (Mssql, "BEGIN TRAN", StatementKind::Transaction),
            (Mssql, "BEGIN SELECT 1 END", StatementKind::Other),
            (Postgres, "EXPLAIN SELECT 1", StatementKind::Other),
            (Sqlite, "PRAGMA table_info(t)", StatementKind::Other),
            (Postgres, "-- note\nSELECT 1", StatementKind::Select),
        ];
        for (d, sql, want) in cases {
            assert_eq!(split(sql, *d)[0].kind, *want, "{sql}");
        }
    }

    #[test]
    fn statement_at_caret() {
        let sql = "SELECT 1;\nSELECT 2;   \n\n\n  SELECT 3";
        let at = |o: usize| statement_at(sql, o, Postgres).map(|s| s.text);
        assert_eq!(at(0).as_deref(), Some("SELECT 1"));
        assert_eq!(at(8).as_deref(), Some("SELECT 1")); // just before `;`
        assert_eq!(at(9).as_deref(), Some("SELECT 1")); // just after `;`
        assert_eq!(at(10).as_deref(), Some("SELECT 2")); // line start of SELECT 2
        assert_eq!(at(21).as_deref(), Some("SELECT 2")); // trailing spaces after `;`
        assert_eq!(at(23).as_deref(), Some("SELECT 2")); // the next, empty line
        assert_eq!(at(24), None); // two lines below
        assert_eq!(at(25).as_deref(), Some("SELECT 3")); // indentation before SELECT 3
        assert_eq!(at(sql.len()).as_deref(), Some("SELECT 3"));
        assert_eq!(statement_at("", 0, Postgres), None);
        assert_eq!(
            statement_at("SELECT 1;\n", 10, Postgres)
                .map(|s| s.text)
                .as_deref(),
            Some("SELECT 1")
        );
    }
}
