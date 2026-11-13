//! Query parameters: which placeholders a statement needs values for, and the
//! rewrite into the driver's native placeholders.

use std::collections::{HashMap, HashSet};

use sql_core::{Dialect, Value};

use crate::lexer::{Kind, Token, lex};
use crate::{Param, StatementKind};

/// The parameter tokens of `stmt` with their names: `:name`, `@name`, `$1` as
/// written, and each bare `?` numbered `?1`, `?2`… so every one gets its own value.
fn param_tokens(stmt: &str, d: Dialect) -> Vec<(Token, String)> {
    let toks = lex(stmt, d);
    let sig: Vec<Token> = toks.into_iter().filter(|t| !t.is_trivia()).collect();
    if is_routine_definition(stmt, &sig, d) {
        return Vec::new();
    }
    let variables = variables(stmt, &sig, d);
    let mut question = 0;
    sig.iter()
        .filter(|t| t.kind == Kind::Param)
        .filter_map(|t| {
            let text = t.text(stmt);
            let name = if text == "?" {
                question += 1;
                format!("?{question}")
            } else {
                text.to_owned()
            };
            (!variables.contains(&name.to_ascii_lowercase())).then_some((*t, name))
        })
        .collect()
}

/// A CREATE FUNCTION/PROCEDURE/TRIGGER: `@x`/`:new` inside are the routine's own.
fn is_routine_definition(src: &str, sig: &[Token], d: Dialect) -> bool {
    crate::split::classify(src, sig, d) == StatementKind::Ddl
        && sig.iter().take(12).any(|t| {
            ["FUNCTION", "PROCEDURE", "PROC", "TRIGGER"]
                .iter()
                .any(|w| t.is_kw(src, w))
        })
}

/// `@names` that are variables of the statement itself, not parameters: declared
/// (`DECLARE @x`), assigned (`SET @x =`, `SELECT @x =`, `@x :=`, `INTO @x`), or
/// MSSQL named procedure arguments (`EXEC p @arg = 1`). Lower-cased.
fn variables(src: &str, sig: &[Token], d: Dialect) -> HashSet<String> {
    let mut out = HashSet::new();
    if !matches!(d, Dialect::Mysql | Dialect::Mssql) {
        return out;
    }
    let mut in_declare = false;
    let mut in_exec = false;
    for (i, t) in sig.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| &sig[p]);
        let next = sig.get(i + 1);
        if t.kind == Kind::Delimiter {
            in_declare = false;
            in_exec = false;
        } else if t.kind == Kind::Word {
            let statement_start = prev.is_none_or(|p| {
                p.kind == Kind::Delimiter
                    || p.is_kw(src, "BEGIN")
                    || p.is_kw(src, "THEN")
                    || p.is_kw(src, "ELSE")
            });
            if t.is_kw(src, "DECLARE") {
                in_declare = true;
            } else if statement_start && (t.is_kw(src, "EXEC") || t.is_kw(src, "EXECUTE")) {
                in_exec = true;
            } else if statement_start {
                in_declare = false;
            }
        }
        if t.kind != Kind::Param || !t.text(src).starts_with('@') {
            continue;
        }
        let prev_is = |w: &str| prev.is_some_and(|p| p.is_kw(src, w));
        let next_op = |op: &str| next.is_some_and(|n| n.kind == Kind::Op && n.text(src) == op);
        let declared = in_declare && (prev_is("DECLARE") || prev.is_some_and(|p| p.is_punct(b',')));
        let assigned = next_op(":=")
            || prev_is("INTO")
            || ((prev_is("SET") || prev_is("SELECT")) && next_op("="));
        let named_arg = in_exec && d == Dialect::Mssql && next_op("=");
        if declared || assigned || named_arg {
            out.insert(t.text(src).to_ascii_lowercase());
        }
    }
    out
}

pub fn params(stmt: &str, d: Dialect) -> Vec<Param> {
    let mut seen = HashSet::new();
    param_tokens(stmt, d)
        .into_iter()
        .filter(|(_, name)| seen.insert(name.clone()))
        .map(|(t, name)| Param {
            name,
            range: t.start..t.end,
        })
        .collect()
}

pub fn bind(stmt: &str, d: Dialect, values: &HashMap<String, Value>) -> (String, Vec<Value>) {
    let mut sql = String::with_capacity(stmt.len());
    let mut out = Vec::new();
    let mut numbered: HashMap<String, usize> = HashMap::new();
    let mut at = 0;
    for (t, name) in param_tokens(stmt, d) {
        sql.push_str(&stmt[at..t.start]);
        at = t.end;
        let value = || values.get(&name).cloned().unwrap_or(Value::Null);
        match d {
            Dialect::Mysql | Dialect::Sqlite => {
                out.push(value());
                sql.push('?');
            }
            Dialect::Postgres | Dialect::Mssql => {
                let n = match numbered.get(&name) {
                    Some(&n) => n,
                    None => {
                        out.push(value());
                        numbered.insert(name, out.len());
                        out.len()
                    }
                };
                sql.push_str(&if d == Dialect::Postgres {
                    format!("${n}")
                } else {
                    format!("@P{n}")
                });
            }
        }
    }
    sql.push_str(&stmt[at..]);
    (sql, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;

    fn names(stmt: &str, d: Dialect) -> Vec<String> {
        params(stmt, d).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn finds_parameters() {
        let cases: &[(Dialect, &str, &[&str])] = &[
            (
                Postgres,
                "SELECT * FROM t WHERE a = :id AND b = :id OR c = :name",
                &[":id", ":name"],
            ),
            (
                Postgres,
                "SELECT a::int, b::text FROM t WHERE c = :c",
                &[":c"],
            ),
            (Postgres, "SELECT $1, $2, $1", &["$1", "$2"]),
            (
                Postgres,
                "SELECT ':x', \":y\", $$ :z $$ -- :w\n /* :v */",
                &[],
            ),
            (Postgres, "SELECT j ? 'k' FROM t", &[]),
            (
                Mysql,
                "SELECT * FROM t WHERE a = ? AND b = ?",
                &["?1", "?2"],
            ),
            (Mysql, "SELECT * FROM t WHERE a = @a", &["@a"]),
            (Mysql, "SET @x = 1", &[]),
            (Mysql, "SELECT @n := @n + 1, @p FROM t", &["@p"]),
            (Mysql, "SELECT a INTO @v FROM t WHERE b = @w", &["@w"]),
            (Mysql, "SELECT @@version", &[]),
            (
                Mssql,
                "SELECT * FROM t WHERE a = @a AND b = @b",
                &["@a", "@b"],
            ),
            (
                Mssql,
                "DECLARE @x int = 1, @y int; SELECT * FROM t WHERE a = @x AND b = @y AND c = @z",
                &["@z"],
            ),
            (Mssql, "SELECT @total = SUM(a) FROM t WHERE b = @b", &["@b"]),
            (Mssql, "EXEC p @arg = @value", &["@value"]),
            (Mssql, "SELECT @@ROWCOUNT", &[]),
            (Mssql, "CREATE PROCEDURE p @a int AS SELECT @a", &[]),
            (
                Sqlite,
                "SELECT ?, ?3, :a, @b, $c",
                &["?1", "?3", ":a", "@b", "$c"],
            ),
        ];
        for (d, sql, want) in cases {
            assert_eq!(names(sql, *d), *want, "{d:?}: {sql}");
        }
        assert_eq!(params("SELECT :id", Postgres)[0].range, 7..10);
    }

    #[test]
    fn binds_native_placeholders() {
        let vals: HashMap<String, Value> = [
            (":id", Value::Int(7)),
            (":n", Value::Text("x".into())),
            ("?1", Value::Int(1)),
            ("?2", Value::Int(2)),
            ("@a", Value::Bool(true)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let (sql, v) = bind(
            "SELECT * FROM t WHERE a = :id AND b = :n AND c = :id AND d = :missing",
            Postgres,
            &vals,
        );
        assert_eq!(
            sql,
            "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $1 AND d = $3"
        );
        assert_eq!(v, vec![Value::Int(7), Value::Text("x".into()), Value::Null]);

        let (sql, v) = bind(
            "SELECT * FROM t WHERE a = :id AND c = :id AND b = ?",
            Mysql,
            &vals,
        );
        assert_eq!(sql, "SELECT * FROM t WHERE a = ? AND c = ? AND b = ?");
        assert_eq!(v, vec![Value::Int(7), Value::Int(7), Value::Int(1)]);

        let (sql, v) = bind(
            "DECLARE @x int = 1; SELECT * FROM t WHERE a = @a AND b = @a AND c = @x",
            Mssql,
            &vals,
        );
        assert_eq!(
            sql,
            "DECLARE @x int = 1; SELECT * FROM t WHERE a = @P1 AND b = @P1 AND c = @x"
        );
        assert_eq!(v, vec![Value::Bool(true)]);

        let (sql, v) = bind("SELECT ':id' WHERE x = :id::int", Postgres, &vals);
        assert_eq!(sql, "SELECT ':id' WHERE x = $1::int");
        assert_eq!(v, vec![Value::Int(7)]);
    }
}
