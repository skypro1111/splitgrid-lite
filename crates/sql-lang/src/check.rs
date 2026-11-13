//! What a statement does before it runs: dangerous writes to confirm, and
//! whether its result rows can be edited in place.

use std::ops::Range;

use sql_core::{Catalog, Dialect, TableRef};

use crate::lexer::{Kind, Token, lex};
use crate::scope::Model;
use crate::{Danger, EditSource};

fn significant(stmt: &str, d: Dialect) -> Vec<Token> {
    lex(stmt, d)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect()
}

/// The `;`-separated statements of a batch (MSSQL keeps variable batches whole).
fn sub_statements(toks: &[Token]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.kind == Kind::Delimiter {
            out.push(start..i);
            start = i + 1;
        }
    }
    out.push(start..toks.len());
    out.retain(|r| !r.is_empty());
    out
}

pub fn dangers(stmt: &str, d: Dialect) -> Vec<Danger> {
    let cat = Catalog::default();
    let m = Model::new(stmt, significant(stmt, d), d, &cat);
    let mut out = Vec::new();
    for r in sub_statements(&m.toks) {
        // A routine's body runs later, when it's called.
        let routine = m.kw_any(r.start, &["CREATE", "ALTER"])
            && (r.start..r.end.min(r.start + 12))
                .any(|i| m.kw_any(i, &["FUNCTION", "PROCEDURE", "PROC", "TRIGGER", "EVENT"]));
        if !routine {
            check(&m, r, &mut out);
        }
    }
    let mut unique = Vec::new();
    for x in out {
        if !unique.contains(&x) {
            unique.push(x);
        }
    }
    unique
}

/// Statement starts inside `r`: its start, and (in procedural batches) the
/// statements after `BEGIN` / `THEN` / `ELSE`, each with its extent.
fn check(m: &Model, r: Range<usize>, out: &mut Vec<Danger>) {
    let (bodies, main) = m.cte_bodies(r.start, r.end);
    for b in bodies {
        check(m, b, out);
    }
    let mut starts = vec![main];
    let mut i = main;
    while i < r.end {
        if i > main
            && m.kw_any(i - 1, &["BEGIN", "THEN", "ELSE"])
            && !m.kw_any(i, &["TRAN", "TRANSACTION", "TRY", "CATCH"])
        {
            starts.push(i);
        }
        i = m.step(i);
    }
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(r.end);
        let has_where = || {
            let mut j = s;
            while j < end {
                if m.kw(j, "WHERE") {
                    return true;
                }
                j = m.step(j);
            }
            false
        };
        if m.kw(s, "UPDATE") && !has_where() {
            out.push(Danger::UpdateWithoutWhere);
        } else if m.kw(s, "DELETE") && !has_where() {
            out.push(Danger::DeleteWithoutWhere);
        } else if m.kw(s, "DROP") {
            out.push(Danger::Drop);
        } else if m.kw(s, "TRUNCATE") {
            out.push(Danger::Truncate);
        } else if m.kw(s, "ALTER") && drops_data(m, s, end) {
            out.push(Danger::Drop);
        }
    }
}

/// `ALTER TABLE … DROP [COLUMN] c` (dropping a constraint or default loses no data).
fn drops_data(m: &Model, s: usize, end: usize) -> bool {
    const HARMLESS: &[&str] = &[
        "CONSTRAINT",
        "DEFAULT",
        "NOT",
        "INDEX",
        "KEY",
        "PRIMARY",
        "FOREIGN",
        "CHECK",
        "IDENTITY",
        "EXPRESSION",
    ];
    let mut j = s;
    while j < end {
        if m.kw(j, "DROP") && !m.kw_any(j + 1, HARMLESS) {
            return true;
        }
        j = m.step(j);
    }
    false
}

const AGGREGATES: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "ARRAY_AGG",
    "STRING_AGG",
    "GROUP_CONCAT",
    "JSON_AGG",
    "JSONB_AGG",
    "JSON_OBJECT_AGG",
    "JSONB_OBJECT_AGG",
    "JSON_ARRAYAGG",
    "JSON_OBJECTAGG",
    "BOOL_AND",
    "BOOL_OR",
    "EVERY",
    "STDDEV",
    "STDDEV_POP",
    "STDDEV_SAMP",
    "VARIANCE",
    "VAR_POP",
    "VAR_SAMP",
    "LISTAGG",
    "BIT_AND",
    "BIT_OR",
    "BIT_XOR",
    "STRING_AGG",
    "TOTAL",
    "XMLAGG",
    "PERCENTILE_CONT",
    "PERCENTILE_DISC",
    "MODE",
];

pub fn editable_source(
    stmt: &str,
    d: Dialect,
    cat: &Catalog,
    result_columns: &[String],
) -> Option<EditSource> {
    let mut toks = significant(stmt, d);
    while toks.last().is_some_and(|t| t.kind == Kind::Delimiter) {
        toks.pop();
    }
    if toks.iter().any(|t| t.kind == Kind::Delimiter) {
        return None;
    }
    let m = Model::new(stmt, toks, d, cat);
    if !m.kw(0, "SELECT") {
        return None;
    }
    let parts = m.parts(0, m.len());
    let [part] = parts.as_slice() else {
        return None;
    };
    // Top-level shape: one FROM table, nothing that groups, combines or de-duplicates rows.
    const BLOCKING: &[&str] = &[
        "DISTINCT",
        "JOIN",
        "APPLY",
        "GROUP",
        "HAVING",
        "WINDOW",
        "INTO",
        "PIVOT",
        "UNPIVOT",
        "NATURAL",
        "STRAIGHT_JOIN",
    ];
    let mut i = 0;
    while i < m.len() {
        if m.kw_any(i, BLOCKING) {
            return None;
        }
        i = m.step(i);
    }
    let sources = m.sources(part.clone(), &[], 0);
    let [source] = sources.as_slice() else {
        return None;
    };
    let (schema, table) = source.table?;
    let items = m.select_items(part.clone());
    for item in &items {
        if computes_over_rows(&m, item.range.clone()) {
            return None;
        }
    }
    let key_columns: Vec<String> = if !table.primary_key.is_empty() {
        table.primary_key.clone()
    } else {
        let not_null = |c: &String| {
            table
                .columns
                .iter()
                .any(|col| col.name == *c && !col.nullable)
        };
        table
            .indexes
            .iter()
            .find(|ix| ix.unique && !ix.columns.is_empty() && ix.columns.iter().all(not_null))?
            .columns
            .clone()
    };
    let column = |name: &str| {
        table
            .columns
            .iter()
            .find(|c| c.name == name)
            .or_else(|| {
                table
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
            })
            .map(|c| c.name.clone())
    };
    let ours = |q: &Option<String>| {
        q.as_ref().is_none_or(|q| {
            q.eq_ignore_ascii_case(&source.name) || q.eq_ignore_ascii_case(&table.name)
        })
    };
    // (output name, table column) for each result column, `*` expanded.
    let mut outputs: Vec<(Option<String>, Option<String>)> = Vec::new();
    for item in &items {
        match (&item.star, &item.column) {
            (Some(q), _) if ours(q) => outputs.extend(
                table
                    .columns
                    .iter()
                    .map(|c| (Some(c.name.clone()), Some(c.name.clone()))),
            ),
            (Some(_), _) => return None,
            (None, Some((q, c))) if ours(q) => outputs.push((item.name.clone(), column(c))),
            _ => outputs.push((item.name.clone(), None)),
        }
    }
    let column_map: Vec<Option<String>> = if outputs.len() == result_columns.len() {
        outputs.into_iter().map(|o| o.1).collect()
    } else {
        result_columns
            .iter()
            .map(|rc| {
                outputs
                    .iter()
                    .find(|o| o.0.as_ref().is_some_and(|n| n.eq_ignore_ascii_case(rc)))
                    .and_then(|o| o.1.clone())
            })
            .collect()
    };
    if !key_columns
        .iter()
        .all(|k| column_map.iter().any(|c| c.as_ref() == Some(k)))
    {
        return None;
    }
    Some(EditSource {
        table: TableRef::new(Some(&schema.name), &table.name),
        key_columns,
        column_map,
    })
}

/// Whether a select item aggregates or windows (subqueries aside).
fn computes_over_rows(m: &Model, r: Range<usize>) -> bool {
    let mut i = r.start;
    while i < r.end {
        if m.is_query_group(i) {
            i = m.step(i);
            continue;
        }
        if m.kw(i, "OVER")
            || (m.toks[i].kind == Kind::Word
                && m.punct(i + 1, b'(')
                && AGGREGATES.iter().any(|a| m.kw(i, a)))
        {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;
    use sql_core::{ColumnInfo, Index, Schema, Table};

    #[test]
    fn dangerous_statements() {
        use Danger::*;
        let cases: &[(Dialect, &str, &[Danger])] = &[
            (Postgres, "UPDATE t SET a = 1", &[UpdateWithoutWhere]),
            (Postgres, "UPDATE t SET a = 1 WHERE id = 2", &[]),
            (
                Postgres,
                "update t set a = (select b from s where s.id = 1)",
                &[UpdateWithoutWhere],
            ),
            (Postgres, "UPDATE t SET a = 1 FROM s WHERE s.id = t.id", &[]),
            (Postgres, "DELETE FROM t", &[DeleteWithoutWhere]),
            (
                Postgres,
                "DELETE FROM t WHERE id IN (SELECT id FROM s)",
                &[],
            ),
            (Postgres, "DELETE FROM t USING s", &[DeleteWithoutWhere]),
            (
                Postgres,
                "WITH x AS (SELECT id FROM s WHERE a) DELETE FROM t",
                &[DeleteWithoutWhere],
            ),
            (
                Postgres,
                "WITH x AS (SELECT id FROM s) DELETE FROM t WHERE id IN (SELECT id FROM x)",
                &[],
            ),
            (
                Postgres,
                "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
                &[DeleteWithoutWhere],
            ),
            (Postgres, "SELECT * FROM t WHERE false", &[]),
            (Postgres, "SELECT 'DELETE FROM t'", &[]),
            (Postgres, "-- DELETE FROM t\nSELECT 1", &[]),
            (Postgres, "DROP TABLE t", &[Drop]),
            (Postgres, "drop index i", &[Drop]),
            (Postgres, "TRUNCATE t", &[Truncate]),
            (Postgres, "ALTER TABLE t DROP COLUMN c", &[Drop]),
            (Mysql, "ALTER TABLE t DROP c", &[Drop]),
            (Postgres, "ALTER TABLE t DROP CONSTRAINT c", &[]),
            (Postgres, "ALTER TABLE t ALTER COLUMN c DROP NOT NULL", &[]),
            (
                Postgres,
                "INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET a = 1",
                &[],
            ),
            (
                Postgres,
                "CREATE TABLE t (a int REFERENCES s ON DELETE CASCADE)",
                &[],
            ),
            (
                Postgres,
                "CREATE FUNCTION f() RETURNS void LANGUAGE sql BEGIN ATOMIC DELETE FROM t; END",
                &[],
            ),
            (Mysql, "DELETE FROM t LIMIT 10", &[DeleteWithoutWhere]),
            (
                Mssql,
                "DECLARE @x int = 1; UPDATE t SET a = @x; DELETE FROM s WHERE id = @x",
                &[UpdateWithoutWhere],
            ),
            (
                Mssql,
                "IF 1 = 1 BEGIN DELETE FROM t END",
                &[DeleteWithoutWhere],
            ),
            (Mssql, "IF 1 = 1 BEGIN DELETE FROM t WHERE a = 1 END", &[]),
            (Mssql, "CREATE PROCEDURE p AS DELETE FROM t", &[]),
        ];
        for (d, sql, want) in cases {
            assert_eq!(dangers(sql, *d), *want, "{d:?}: {sql}");
        }
    }

    fn catalog() -> Catalog {
        let col = |n: &str, nullable: bool| ColumnInfo {
            name: n.into(),
            type_name: "int".into(),
            nullable,
            ..Default::default()
        };
        Catalog {
            database: "db".into(),
            search_path: vec!["public".into()],
            schemas: vec![Schema {
                name: "public".into(),
                tables: vec![
                    Table {
                        name: "users".into(),
                        columns: vec![col("id", false), col("name", true), col("email", true)],
                        primary_key: vec!["id".into()],
                        ..Default::default()
                    },
                    Table {
                        name: "tags".into(),
                        columns: vec![col("code", false), col("label", true), col("alt", true)],
                        indexes: vec![
                            Index {
                                name: "alt_uq".into(),
                                columns: vec!["alt".into()],
                                unique: true,
                            },
                            Index {
                                name: "code_uq".into(),
                                columns: vec!["code".into()],
                                unique: true,
                            },
                        ],
                        ..Default::default()
                    },
                    Table {
                        name: "logs".into(),
                        columns: vec![col("msg", true)],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
        }
    }

    fn cols(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| (*s).to_owned()).collect()
    }

    fn map(sql: &str, result: &[&str]) -> Option<Vec<Option<String>>> {
        editable_source(sql, Postgres, &catalog(), &cols(result)).map(|e| e.column_map)
    }

    #[test]
    fn editable_results() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            map("SELECT * FROM users", &["id", "name", "email"]),
            Some(vec![s("id"), s("name"), s("email")])
        );
        assert_eq!(
            map(
                "select u.* from public.users u where u.id > 1 order by 1 limit 5;",
                &["id", "name", "email"]
            ),
            Some(vec![s("id"), s("name"), s("email")])
        );
        assert_eq!(
            map(
                "SELECT id, name AS who, upper(email) e FROM users",
                &["id", "who", "e"]
            ),
            Some(vec![s("id"), s("name"), None])
        );
        assert_eq!(
            map("SELECT ID, \"name\" FROM users", &["id", "name"]),
            Some(vec![s("id"), s("name")])
        );
        assert_eq!(
            map(
                "SELECT id, (SELECT count(*) FROM logs) n FROM users",
                &["id", "n"]
            ),
            Some(vec![s("id"), None])
        );
        // Key: a unique index over NOT NULL columns when there's no primary key.
        let e = editable_source(
            "SELECT code, label FROM tags",
            Postgres,
            &catalog(),
            &cols(&["code", "label"]),
        )
        .unwrap();
        assert_eq!(e.key_columns, vec!["code"]);
        assert_eq!(e.table, TableRef::new(Some("public"), "tags"));
        // Not editable.
        for sql in [
            "SELECT name FROM users",
            "SELECT DISTINCT id FROM users",
            "SELECT u.id FROM users u JOIN tags t ON true",
            "SELECT id FROM users, tags",
            "SELECT id, count(*) FROM users GROUP BY id",
            "SELECT count(*) FROM users",
            "SELECT id, row_number() OVER () FROM users",
            "SELECT id FROM users UNION SELECT id FROM users",
            "WITH x AS (SELECT 1) SELECT id FROM users",
            "SELECT id FROM (SELECT id FROM users) s",
            "SELECT msg FROM logs",
            "SELECT label FROM tags",
            "SELECT id FROM nowhere",
            "UPDATE users SET name = 1",
            "SELECT 1; SELECT 2",
        ] {
            assert_eq!(map(sql, &["id", "name", "code"]), None, "{sql}");
        }
    }

    #[test]
    fn editable_maps_by_name_when_counts_differ() {
        // The server returned fewer columns than the list suggests: match by name.
        assert_eq!(
            map("SELECT id, name FROM users", &["name", "id", "extra"]),
            Some(vec![Some("name".into()), Some("id".into()), None])
        );
    }
}
