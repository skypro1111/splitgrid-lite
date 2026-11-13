//! SQL the data editor generates: staged row edits, the table data view's
//! query, and EXPLAIN wrappers.

use sql_core::{Dialect, TableRef, Value};

use crate::RowEdit;
use crate::ident::{literal, qualified, quote_ident};
use crate::lexer::{Kind, lex};

/// `a = 1 AND b IS NULL`: rows are matched by key only (DataGrip-style), with
/// NULL keys compared NULL-safely.
fn key_where(key: &[(String, Value)], d: Dialect) -> String {
    key.iter()
        .map(|(c, v)| match v {
            Value::Null => format!("{} IS NULL", quote_ident(c, d)),
            v => format!("{} = {}", quote_ident(c, d), literal(v, d)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Edits that can't be written safely are left out: an UPDATE or DELETE
/// without key columns would hit every row, and an UPDATE with nothing to set
/// isn't valid SQL.
pub fn build_edits(table: &TableRef, edits: &[RowEdit], d: Dialect) -> Vec<String> {
    let t = qualified(table, d);
    edits
        .iter()
        .filter_map(|e| match e {
            RowEdit::Update { key, set } if !key.is_empty() && !set.is_empty() => {
                let set = set
                    .iter()
                    .map(|(c, v)| format!("{} = {}", quote_ident(c, d), literal(v, d)))
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!("UPDATE {t} SET {set} WHERE {}", key_where(key, d)))
            }
            RowEdit::Delete { key } if !key.is_empty() => {
                Some(format!("DELETE FROM {t} WHERE {}", key_where(key, d)))
            }
            RowEdit::Insert { values } if values.is_empty() => Some(match d {
                // MySQL has no DEFAULT VALUES; an empty column list is its form.
                Dialect::Mysql => format!("INSERT INTO {t} () VALUES ()"),
                _ => format!("INSERT INTO {t} DEFAULT VALUES"),
            }),
            RowEdit::Insert { values } => {
                let cols = values
                    .iter()
                    .map(|(c, _)| quote_ident(c, d))
                    .collect::<Vec<_>>()
                    .join(", ");
                let vals = values
                    .iter()
                    .map(|(_, v)| literal(v, d))
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!("INSERT INTO {t} ({cols}) VALUES ({vals})"))
            }
            _ => None,
        })
        .collect()
}

/// `text` without a leading keyword phrase the user may have typed (`WHERE`, `ORDER BY`).
fn strip_leading<'a>(text: &'a str, words: &[&str]) -> &'a str {
    let mut rest = text.trim();
    for w in words {
        match rest.get(..w.len()) {
            Some(head)
                if head.eq_ignore_ascii_case(w)
                    && rest[w.len()..].starts_with(|c: char| c.is_whitespace()) =>
            {
                rest = rest[w.len()..].trim_start()
            }
            _ => return text.trim(),
        }
    }
    rest
}

pub fn browse_query(
    table: &TableRef,
    filter: &str,
    order_by: &str,
    limit: usize,
    d: Dialect,
) -> String {
    let filter = strip_leading(filter, &["WHERE"]);
    let order_by = strip_leading(order_by, &["ORDER", "BY"]);
    // A `--` comment in the user's text would swallow the rest of a one-line query.
    let sep = if [filter, order_by].iter().any(|s| s.contains("--")) {
        "\n"
    } else {
        " "
    };
    let mut sql = String::from("SELECT ");
    if d == Dialect::Mssql && limit > 0 {
        sql.push_str(&format!("TOP {limit} "));
    }
    sql.push_str(&format!("* FROM {}", qualified(table, d)));
    if !filter.is_empty() {
        sql.push_str(&format!("{sep}WHERE {filter}"));
    }
    if !order_by.is_empty() {
        sql.push_str(&format!("{sep}ORDER BY {order_by}"));
    }
    if d != Dialect::Mssql && limit > 0 {
        sql.push_str(&format!("{sep}LIMIT {limit}"));
    }
    sql
}

pub fn explain_query(stmt: &str, d: Dialect, analyze: bool) -> Option<String> {
    let mut body = stmt.trim();
    // Drop trailing terminators so the wrapper stays one statement.
    while let Some(last) = lex(body, d).iter().rev().find(|t| !t.is_trivia()).copied() {
        if last.kind != Kind::Delimiter {
            break;
        }
        body = body[..last.start].trim_end();
    }
    if lex(body, d)
        .iter()
        .find(|t| !t.is_trivia())
        .is_some_and(|t| t.is_kw(body, "EXPLAIN"))
    {
        return (d != Dialect::Mssql).then(|| body.to_owned());
    }
    match d {
        Dialect::Postgres if analyze => {
            Some(format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {body}"))
        }
        Dialect::Postgres => Some(format!("EXPLAIN (FORMAT JSON) {body}")),
        Dialect::Mysql if analyze => Some(format!("EXPLAIN ANALYZE {body}")),
        Dialect::Mysql => Some(format!("EXPLAIN FORMAT=JSON {body}")),
        Dialect::Sqlite => Some(format!("EXPLAIN QUERY PLAN {body}")),
        Dialect::Mssql => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;

    fn kv(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn edits_per_dialect() {
        let t = TableRef::new(Some("public"), "Users");
        let edits = vec![
            RowEdit::Update {
                key: kv(&[("id", Value::Int(1))]),
                set: kv(&[
                    ("name", Value::Text("O'Neil".into())),
                    ("order", Value::Null),
                ]),
            },
            RowEdit::Insert {
                values: kv(&[
                    ("name", Value::Text("x".into())),
                    ("active", Value::Bool(true)),
                ]),
            },
            RowEdit::Insert { values: vec![] },
            RowEdit::Delete {
                key: kv(&[("a", Value::Int(1)), ("b", Value::Null)]),
            },
        ];
        assert_eq!(
            build_edits(&t, &edits, Postgres),
            vec![
                "UPDATE public.\"Users\" SET name = 'O''Neil', \"order\" = NULL WHERE id = 1",
                "INSERT INTO public.\"Users\" (name, active) VALUES ('x', TRUE)",
                "INSERT INTO public.\"Users\" DEFAULT VALUES",
                "DELETE FROM public.\"Users\" WHERE a = 1 AND b IS NULL",
            ]
        );
        assert_eq!(
            build_edits(&t, &edits, Mysql),
            vec![
                "UPDATE public.Users SET name = 'O''Neil', `order` = NULL WHERE id = 1",
                "INSERT INTO public.Users (name, active) VALUES ('x', TRUE)",
                "INSERT INTO public.Users () VALUES ()",
                "DELETE FROM public.Users WHERE a = 1 AND b IS NULL",
            ]
        );
        let mssql = build_edits(&TableRef::new(Some("dbo"), "t"), &edits[..2], Mssql);
        assert_eq!(
            mssql,
            vec![
                "UPDATE dbo.t SET name = 'O''Neil', [order] = NULL WHERE id = 1",
                "INSERT INTO dbo.t (name, active) VALUES ('x', 1)"
            ]
        );
        let sqlite = build_edits(&TableRef::new(None, "t"), &edits[3..], Sqlite);
        assert_eq!(sqlite, vec!["DELETE FROM t WHERE a = 1 AND b IS NULL"]);
    }

    #[test]
    fn unsafe_edits_are_left_out() {
        let t = TableRef::new(None, "t");
        let edits = vec![
            RowEdit::Update {
                key: vec![],
                set: kv(&[("a", Value::Int(1))]),
            },
            RowEdit::Update {
                key: kv(&[("id", Value::Int(1))]),
                set: vec![],
            },
            RowEdit::Delete { key: vec![] },
        ];
        assert!(build_edits(&t, &edits, Postgres).is_empty());
    }

    #[test]
    fn browse_queries() {
        let t = TableRef::new(Some("public"), "users");
        assert_eq!(
            browse_query(&t, "", "", 500, Postgres),
            "SELECT * FROM public.users LIMIT 500"
        );
        assert_eq!(
            browse_query(&t, " where id > 1 ", "ORDER BY name desc", 10, Postgres),
            "SELECT * FROM public.users WHERE id > 1 ORDER BY name desc LIMIT 10"
        );
        assert_eq!(
            browse_query(&t, "whereabouts = 1", "", 0, Postgres),
            "SELECT * FROM public.users WHERE whereabouts = 1"
        );
        assert_eq!(
            browse_query(
                &TableRef::new(Some("dbo"), "order"),
                "a = 1",
                "b",
                100,
                Mssql
            ),
            "SELECT TOP 100 * FROM dbo.[order] WHERE a = 1 ORDER BY b"
        );
        assert_eq!(
            browse_query(&TableRef::new(None, "t"), "a = 1 -- note", "", 5, Sqlite),
            "SELECT * FROM t\nWHERE a = 1 -- note\nLIMIT 5"
        );
        assert_eq!(
            browse_query(&TableRef::new(Some("db"), "t"), "", "id", 5, Mysql),
            "SELECT * FROM db.t ORDER BY id LIMIT 5"
        );
    }

    #[test]
    fn explain_per_dialect() {
        let q = "SELECT * FROM t;  ";
        assert_eq!(
            explain_query(q, Postgres, false).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT * FROM t")
        );
        assert_eq!(
            explain_query(q, Postgres, true).as_deref(),
            Some("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT * FROM t")
        );
        assert_eq!(
            explain_query(q, Mysql, false).as_deref(),
            Some("EXPLAIN FORMAT=JSON SELECT * FROM t")
        );
        assert_eq!(
            explain_query(q, Mysql, true).as_deref(),
            Some("EXPLAIN ANALYZE SELECT * FROM t")
        );
        assert_eq!(
            explain_query(q, Sqlite, true).as_deref(),
            Some("EXPLAIN QUERY PLAN SELECT * FROM t")
        );
        assert_eq!(explain_query(q, Mssql, false), None);
        assert_eq!(
            explain_query("explain select 1", Postgres, false).as_deref(),
            Some("explain select 1")
        );
        assert_eq!(
            explain_query("SELECT ';'", Postgres, false).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT ';'")
        );
    }
}
