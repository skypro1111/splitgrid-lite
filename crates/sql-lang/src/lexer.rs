//! A tolerant, dialect-aware SQL tokenizer. It never fails: unfinished strings,
//! quoted identifiers and comments run to the end of the input with
//! `closed == false`, because the editor text is usually mid-typing.
//!
//! Byte-oriented: every token boundary is an ASCII byte, so non-ASCII text
//! (always UTF-8 continuation/lead bytes >= 0x80) is treated as identifier
//! characters and slicing `src[start..end]` is always valid.

use sql_core::Dialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Space,
    Comment,
    /// A bare identifier or keyword (also MSSQL `#temp` and `@@system` names).
    Word,
    /// A delimited identifier: `"x"`, `` `x` ``, `[x]`.
    Quoted,
    /// A string literal, including prefixed (`E''`, `N''`, `X''`) and dollar-quoted ones.
    Str,
    Number,
    /// `:name`, `@name`, `$1`, `?`, `?3`, `$name` (SQLite).
    Param,
    Op,
    /// `( ) , . [ ] { }` and `;` when it is not the active terminator.
    Punct(u8),
    /// The active statement terminator: `;`, or what a MySQL `DELIMITER` set.
    Delimiter,
    /// A whole MySQL client `DELIMITER xx` line.
    DelimiterCmd,
    /// A whole MSSQL `GO [n]` batch-separator line.
    Go,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    /// False for a string, quoted identifier or block comment that never ends.
    pub closed: bool,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, Kind::Space | Kind::Comment)
    }

    pub fn is_punct(&self, c: u8) -> bool {
        self.kind == Kind::Punct(c)
    }

    /// A bare word equal to `kw`, ignoring ASCII case.
    pub fn is_kw(&self, src: &str, kw: &str) -> bool {
        self.kind == Kind::Word && self.text(src).eq_ignore_ascii_case(kw)
    }

    pub fn is_ident(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Quoted)
    }

    /// A line comment (`--`, `#`), which ends at the newline it doesn't include.
    pub fn is_line_comment(&self, src: &str) -> bool {
        self.kind == Kind::Comment && !src[self.start..].starts_with("/*")
    }
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_op_byte(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'<'
            | b'>'
            | b'='
            | b'~'
            | b'!'
            | b'@'
            | b'#'
            | b'%'
            | b'^'
            | b'&'
            | b'|'
            | b'?'
            | b':'
    )
}

struct Lexer<'a> {
    b: &'a [u8],
    d: Dialect,
    /// A MySQL `DELIMITER` other than `;`.
    custom: Option<Vec<u8>>,
    out: Vec<Token>,
}

pub fn lex(src: &str, d: Dialect) -> Vec<Token> {
    let mut lx = Lexer {
        b: src.as_bytes(),
        d,
        custom: None,
        out: Vec::with_capacity(src.len() / 3 + 4),
    };
    lx.run();
    lx.out
}

impl Lexer<'_> {
    fn push(&mut self, kind: Kind, start: usize, end: usize, closed: bool) {
        self.out.push(Token {
            kind,
            start,
            end,
            closed,
        });
    }

    fn at_custom_delimiter(&self, i: usize) -> bool {
        self.custom
            .as_ref()
            .is_some_and(|d| self.b[i..].starts_with(d))
    }

    fn run(&mut self) {
        let (b, n) = (self.b, self.b.len());
        let mut i = 0;
        let mut line_start = true;
        while i < n {
            let start = i;
            let c = b[i];
            if c.is_ascii_whitespace() {
                while i < n && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                self.push(Kind::Space, start, i, true);
                if b[start..i].contains(&b'\n') {
                    line_start = true;
                }
                continue;
            }
            if line_start {
                if self.d == Dialect::Mysql
                    && let Some((end, delim)) = delimiter_cmd(b, i)
                {
                    self.custom = (delim != b";").then(|| delim.to_vec());
                    self.push(Kind::DelimiterCmd, start, end, true);
                    i = end;
                    line_start = false;
                    continue;
                }
                if self.d == Dialect::Mssql
                    && let Some(end) = go_line(b, i)
                {
                    self.push(Kind::Go, start, end, true);
                    i = end;
                    line_start = false;
                    continue;
                }
            }
            line_start = false;
            if let Some(d) = &self.custom
                && b[i..].starts_with(d)
            {
                i += d.len();
                self.push(Kind::Delimiter, start, i, true);
                continue;
            }
            let (kind, end, closed) = self.token(i);
            debug_assert!(end > i);
            self.push(kind, start, end, closed);
            i = end;
        }
    }

    /// The token starting at `i` (not whitespace, not a line-start command).
    fn token(&self, i: usize) -> (Kind, usize, bool) {
        let (b, n, d) = (self.b, self.b.len(), self.d);
        let c = b[i];
        let next = b.get(i + 1).copied();
        // Comments.
        if c == b'-'
            && next == Some(b'-')
            && (d != Dialect::Mysql
                || b.get(i + 2)
                    .is_none_or(|x| x.is_ascii_whitespace() || x.is_ascii_control()))
        {
            return (Kind::Comment, line_end(b, i), true);
        }
        if c == b'#' && d == Dialect::Mysql {
            return (Kind::Comment, line_end(b, i), true);
        }
        if c == b'/' && next == Some(b'*') {
            let nested = matches!(d, Dialect::Postgres | Dialect::Mssql);
            let (end, closed) = block_comment(b, i, nested);
            return (Kind::Comment, end, closed);
        }
        // Strings and delimited identifiers.
        match c {
            b'\'' => {
                let (end, closed) = quoted(b, i, b'\'', d == Dialect::Mysql);
                return (Kind::Str, end, closed);
            }
            b'"' => {
                let (end, closed) = quoted(b, i, b'"', d == Dialect::Mysql);
                return (
                    if d == Dialect::Mysql {
                        Kind::Str
                    } else {
                        Kind::Quoted
                    },
                    end,
                    closed,
                );
            }
            b'`' => {
                let (end, closed) = quoted(b, i, b'`', false);
                return (Kind::Quoted, end, closed);
            }
            b'[' if matches!(d, Dialect::Mssql | Dialect::Sqlite) => {
                let (end, closed) = quoted(b, i, b']', false);
                return (Kind::Quoted, end, closed);
            }
            _ => {}
        }
        // Postgres `$1` and dollar quotes; SQLite `$name`.
        if c == b'$' {
            if d == Dialect::Postgres {
                if next.is_some_and(|x| x.is_ascii_digit()) {
                    return (Kind::Param, scan(b, i + 1, |x| x.is_ascii_digit()), true);
                }
                if let Some(tag_end) = dollar_tag(b, i) {
                    let tag = &b[i..tag_end];
                    return match find(b, tag_end, tag) {
                        Some(p) => (Kind::Str, p + tag.len(), true),
                        None => (Kind::Str, n, false),
                    };
                }
            }
            if d == Dialect::Sqlite && next.is_some_and(is_word_start) {
                return (Kind::Param, scan(b, i + 1, is_word_byte), true);
            }
        }
        // Words, with literal prefixes (E'', N'', X'', B'', U&'').
        if is_word_start(c) || (c == b'#' && d == Dialect::Mssql) {
            let mut end = i + 1;
            while end < n
                && (is_word_byte(b[end]) || (c == b'#' && b[end] == b'#'))
                && !self.at_custom_delimiter(end)
            {
                end += 1;
            }
            if b.get(end) == Some(&b'\'') {
                let prefix = &b[i..end];
                let one = |ch: u8| prefix.len() == 1 && prefix[0].eq_ignore_ascii_case(&ch);
                if one(b'x') || one(b'b') || one(b'n') || (one(b'e') && d == Dialect::Postgres) {
                    let backslash = d == Dialect::Mysql || one(b'e');
                    let (e, closed) = quoted(b, end, b'\'', backslash);
                    return (Kind::Str, e, closed);
                }
            }
            if d == Dialect::Postgres
                && end == i + 1
                && (c == b'u' || c == b'U')
                && b[end..].starts_with(b"&'")
            {
                let (e, closed) = quoted(b, end + 1, b'\'', false);
                return (Kind::Str, e, closed);
            }
            return (Kind::Word, end, true);
        }
        if c.is_ascii_digit() || (c == b'.' && next.is_some_and(|x| x.is_ascii_digit())) {
            let mut end = i;
            while end < n {
                let x = b[end];
                let exponent_sign = (x == b'+' || x == b'-')
                    && matches!(b[end - 1], b'e' | b'E')
                    && b.get(end + 1).is_some_and(|y| y.is_ascii_digit())
                    && !b[i..end].starts_with(b"0x");
                if !(x.is_ascii_alphanumeric() || x == b'_' || x == b'.' || exponent_sign) {
                    break;
                }
                end += 1;
            }
            return (Kind::Number, end, true);
        }
        // Parameters and variables.
        if c == b':'
            && next != Some(b':')
            && next.is_some_and(is_word_start)
            && (i == 0 || b[i - 1] != b':')
        {
            return (Kind::Param, scan(b, i + 1, is_word_byte), true);
        }
        if c == b'@' && d != Dialect::Postgres {
            if next == Some(b'@') {
                return (Kind::Word, scan(b, i + 2, is_word_byte), true);
            }
            if next.is_some_and(is_word_start) {
                return (Kind::Param, scan(b, i + 1, is_word_byte), true);
            }
        }
        if c == b'?' && d != Dialect::Postgres {
            return (Kind::Param, scan(b, i + 1, |x| x.is_ascii_digit()), true);
        }
        match c {
            b'(' | b')' | b',' | b'.' | b'[' | b']' | b'{' | b'}' => {
                return (Kind::Punct(c), i + 1, true);
            }
            b';' => {
                return (
                    if self.custom.is_some() {
                        Kind::Punct(b';')
                    } else {
                        Kind::Delimiter
                    },
                    i + 1,
                    true,
                );
            }
            _ => {}
        }
        if is_op_byte(c) {
            let mut end = i + 1;
            while end < n && is_op_byte(b[end]) && !self.op_breaks(end) {
                end += 1;
            }
            return (Kind::Op, end, true);
        }
        // Anything else (a stray backslash, a lone `$`…): one ASCII byte,
        // since bytes >= 0x80 always start words.
        (Kind::Op, i + 1, true)
    }

    /// Whether an operator run must stop before `i` because a comment or a parameter starts there.
    fn op_breaks(&self, i: usize) -> bool {
        let (b, d) = (self.b, self.d);
        let next = b.get(i + 1).copied();
        match b[i] {
            b'-' => next == Some(b'-'),
            b'/' => next == Some(b'*'),
            b'#' => d == Dialect::Mysql,
            b':' => b[i - 1] != b':' && next != Some(b':') && next.is_some_and(is_word_start),
            b'@' | b'?' => d != Dialect::Postgres,
            _ => self.at_custom_delimiter(i),
        }
    }
}

fn scan(b: &[u8], mut i: usize, f: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && f(b[i]) {
        i += 1;
    }
    i
}

fn line_end(b: &[u8], i: usize) -> usize {
    scan(b, i, |x| x != b'\n' && x != b'\r')
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// A quoted run opened by `b[i]`, closed by `close` (doubled to escape it).
fn quoted(b: &[u8], i: usize, close: u8, backslash: bool) -> (usize, bool) {
    let n = b.len();
    let mut j = i + 1;
    while j < n {
        let c = b[j];
        if backslash && c == b'\\' {
            j += 2;
            continue;
        }
        if c == close {
            if b.get(j + 1) == Some(&close) {
                j += 2;
                continue;
            }
            return (j + 1, true);
        }
        j += 1;
    }
    (n, false)
}

fn block_comment(b: &[u8], i: usize, nested: bool) -> (usize, bool) {
    let n = b.len();
    let mut depth = 1;
    let mut j = i + 2;
    while j < n {
        if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
            depth -= 1;
            j += 2;
            if depth == 0 || !nested {
                return (j, true);
            }
        } else if nested && b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    (n, false)
}

/// The end of a `$tag$` opener at `i`, if there is one.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j) == Some(&b'$') {
        return Some(j + 1);
    }
    if !b.get(j).copied().is_some_and(is_word_start) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn word_at(b: &[u8], i: usize, w: &[u8]) -> bool {
    b.len() >= i + w.len()
        && b[i..i + w.len()].eq_ignore_ascii_case(w)
        && !b.get(i + w.len()).copied().is_some_and(is_word_byte)
}

/// `DELIMITER xx` at a line start: the line's end and the new delimiter.
fn delimiter_cmd(b: &[u8], i: usize) -> Option<(usize, &[u8])> {
    if !word_at(b, i, b"delimiter") {
        return None;
    }
    let mut j = i + 9;
    if !matches!(b.get(j), Some(b' ' | b'\t')) {
        return None;
    }
    j = scan(b, j, |x| x == b' ' || x == b'\t');
    let d_end = scan(b, j, |x| !x.is_ascii_whitespace());
    (d_end > j).then(|| (line_end(b, i), &b[j..d_end]))
}

/// `GO` or `GO 5` alone on a line (a trailing `--` comment is allowed): the line's end.
fn go_line(b: &[u8], i: usize) -> Option<usize> {
    if !word_at(b, i, b"go") {
        return None;
    }
    let mut j = scan(b, i + 2, |x| x == b' ' || x == b'\t');
    j = scan(b, j, |x| x.is_ascii_digit());
    j = scan(b, j, |x| x == b' ' || x == b'\t');
    if b[j..].starts_with(b"--") {
        j = line_end(b, j);
    }
    (j == b.len() || b[j] == b'\n' || b[j] == b'\r').then_some(j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Kind::*;

    fn kinds(src: &str, d: Dialect) -> Vec<(Kind, &str)> {
        lex(src, d)
            .into_iter()
            .filter(|t| t.kind != Space)
            .map(|t| (t.kind, t.text(src)))
            .collect()
    }

    #[test]
    fn tokens_cover_the_input_contiguously() {
        let src =
            "SELECT 'a''b', \"x\", `y`, [z], $$q$$, E'\\'', -- c\n /* d */ 1.5e-3 ::int @v :p ? é;";
        for d in Dialect::ALL {
            let toks = lex(src, d);
            let mut at = 0;
            for t in &toks {
                assert_eq!(t.start, at, "{d:?}");
                at = t.end;
            }
            assert_eq!(at, src.len());
        }
    }

    #[test]
    fn table_driven_lexing() {
        type Case<'a> = (Dialect, &'a str, &'a [(Kind, &'a str)]);
        let cases: &[Case] = &[
            (Dialect::Postgres, "'it''s'", &[(Str, "'it''s'")]),
            (
                Dialect::Postgres,
                "E'a\\'b' x",
                &[(Str, "E'a\\'b'"), (Word, "x")],
            ),
            (
                Dialect::Postgres,
                "'a\\' y'",
                &[(Str, "'a\\'"), (Word, "y"), (Str, "'")],
            ),
            (Dialect::Mysql, "'a\\' x'", &[(Str, "'a\\' x'")]),
            (Dialect::Mysql, "\"str\"", &[(Str, "\"str\"")]),
            (
                Dialect::Postgres,
                "\"Ident\"\"x\"",
                &[(Quoted, "\"Ident\"\"x\"")],
            ),
            (Dialect::Mysql, "`a``b`", &[(Quoted, "`a``b`")]),
            (Dialect::Mssql, "[a]]b]", &[(Quoted, "[a]]b]")]),
            (
                Dialect::Postgres,
                "a[1]",
                &[
                    (Word, "a"),
                    (Punct(b'['), "["),
                    (Number, "1"),
                    (Punct(b']'), "]"),
                ],
            ),
            (
                Dialect::Postgres,
                "$$a;b$$;",
                &[(Str, "$$a;b$$"), (Delimiter, ";")],
            ),
            (
                Dialect::Postgres,
                "$fn$ x $$ y $fn$",
                &[(Str, "$fn$ x $$ y $fn$")],
            ),
            (
                Dialect::Postgres,
                "$1 + $2",
                &[(Param, "$1"), (Op, "+"), (Param, "$2")],
            ),
            (
                Dialect::Postgres,
                "a::int",
                &[(Word, "a"), (Op, "::"), (Word, "int")],
            ),
            (
                Dialect::Postgres,
                "a=:id",
                &[(Word, "a"), (Op, "="), (Param, ":id")],
            ),
            (
                Dialect::Postgres,
                "j ? 'k'",
                &[(Word, "j"), (Op, "?"), (Str, "'k'")],
            ),
            (
                Dialect::Postgres,
                "j @> x",
                &[(Word, "j"), (Op, "@>"), (Word, "x")],
            ),
            (
                Dialect::Mysql,
                "a=?",
                &[(Word, "a"), (Op, "="), (Param, "?")],
            ),
            (
                Dialect::Mysql,
                "@x := 1",
                &[(Param, "@x"), (Op, ":="), (Number, "1")],
            ),
            (Dialect::Mssql, "@@ROWCOUNT", &[(Word, "@@ROWCOUNT")]),
            (Dialect::Mssql, "#tmp ##g", &[(Word, "#tmp"), (Word, "##g")]),
            (Dialect::Mssql, "N'é'", &[(Str, "N'é'")]),
            (
                Dialect::Sqlite,
                "X'00ff' $n ?3",
                &[(Str, "X'00ff'"), (Param, "$n"), (Param, "?3")],
            ),
            (Dialect::Mysql, "# c\nx", &[(Comment, "# c"), (Word, "x")]),
            (
                Dialect::Postgres,
                "a#b",
                &[(Word, "a"), (Op, "#"), (Word, "b")],
            ),
            (
                Dialect::Mysql,
                "1--2",
                &[(Number, "1"), (Op, "--"), (Number, "2")],
            ),
            (
                Dialect::Postgres,
                "1--2",
                &[(Number, "1"), (Comment, "--2")],
            ),
            (
                Dialect::Postgres,
                "/* a /* b */ c */x",
                &[(Comment, "/* a /* b */ c */"), (Word, "x")],
            ),
            (
                Dialect::Mysql,
                "/* a /* b */ c */",
                &[(Comment, "/* a /* b */"), (Word, "c"), (Op, "*/")],
            ),
            (
                Dialect::Postgres,
                "1.5e-3 .5 0x1F",
                &[(Number, "1.5e-3"), (Number, ".5"), (Number, "0x1F")],
            ),
            (
                Dialect::Postgres,
                "a.b",
                &[(Word, "a"), (Punct(b'.'), "."), (Word, "b")],
            ),
            (Dialect::Postgres, "U&'d\\0061'", &[(Str, "U&'d\\0061'")]),
            (Dialect::Postgres, "naïve", &[(Word, "naïve")]),
        ];
        for (d, src, want) in cases {
            assert_eq!(kinds(src, *d), *want, "{d:?} {src}");
        }
    }

    #[test]
    fn unfinished_input_runs_to_the_end_unclosed() {
        for (d, src, kind) in [
            (Dialect::Postgres, "SELECT 'abc", Str),
            (Dialect::Postgres, "SELECT \"Us", Quoted),
            (Dialect::Mssql, "SELECT [Us", Quoted),
            (Dialect::Postgres, "SELECT /* x", Comment),
            (Dialect::Postgres, "SELECT $$ body", Str),
        ] {
            let t = *lex(src, d).last().unwrap();
            assert_eq!((t.kind, t.closed, t.end), (kind, false, src.len()), "{src}");
        }
    }

    #[test]
    fn mysql_delimiter_lines_change_the_terminator() {
        let src =
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2;";
        let k = kinds(src, Dialect::Mysql);
        assert_eq!(k[0], (DelimiterCmd, "DELIMITER $$"));
        assert!(k.contains(&(Punct(b';'), ";")));
        assert!(k.contains(&(Word, "END")));
        assert!(k.contains(&(Delimiter, "$$")));
        assert!(k.contains(&(DelimiterCmd, "DELIMITER ;")));
        assert_eq!(k.last(), Some(&(Delimiter, ";")));
        // Not at a line start: an ordinary word.
        assert_eq!(
            kinds("x delimiter $$", Dialect::Mysql)[1],
            (Word, "delimiter")
        );
    }

    #[test]
    fn mssql_go_only_alone_on_a_line() {
        assert_eq!(
            kinds("SELECT 1\nGO\nSELECT 2", Dialect::Mssql)[2],
            (Go, "GO")
        );
        assert_eq!(
            kinds("SELECT 1\n  go 3 -- again\n", Dialect::Mssql)[2],
            (Go, "go 3 -- again")
        );
        assert_eq!(
            kinds("SELECT 1\nGO SELECT 2", Dialect::Mssql)[2],
            (Word, "GO")
        );
        assert_eq!(kinds("SELECT go\nFROM t", Dialect::Mssql)[1], (Word, "go"));
        assert_eq!(kinds("x\nGO", Dialect::Postgres)[1], (Word, "GO"));
    }
}
