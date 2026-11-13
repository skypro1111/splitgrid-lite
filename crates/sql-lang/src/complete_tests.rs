//! Completion scenarios. `|` marks the caret.

use std::time::{Duration, Instant};

use sql_core::{
    Catalog, ColumnInfo, Dialect, ForeignKey, Routine, RoutineKind, Schema, Table, TableKind,
    TableRef,
};

use crate::{Completion, CompletionKind, CompletionResult, complete};

fn col(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        type_name: ty.into(),
        nullable: true,
        ..Default::default()
    }
}

fn fk(name: &str, cols: &[&str], schema: &str, table: &str, refs: &[&str]) -> ForeignKey {
    ForeignKey {
        name: name.into(),
        columns: cols.iter().map(|c| (*c).into()).collect(),
        references: TableRef::new(Some(schema), table),
        ref_columns: refs.iter().map(|c| (*c).into()).collect(),
    }
}

fn table(name: &str, cols: &[(&str, &str)], fks: Vec<ForeignKey>) -> Table {
    Table {
        name: name.into(),
        columns: cols.iter().map(|(n, t)| col(n, t)).collect(),
        primary_key: vec!["id".into()],
        foreign_keys: fks,
        ..Default::default()
    }
}

fn catalog() -> Catalog {
    let mut users = table(
        "users",
        &[
            ("id", "int4"),
            ("name", "text"),
            ("email", "text"),
            ("created_at", "timestamptz"),
        ],
        vec![],
    );
    users.columns[0].generated = true;
    let mut active = table("active_users", &[("id", "int4"), ("name", "text")], vec![]);
    active.kind = TableKind::View;
    let public = Schema {
        name: "public".into(),
        tables: vec![
            users,
            table(
                "orders",
                &[
                    ("id", "int4"),
                    ("user_id", "int4"),
                    ("total", "numeric"),
                    ("status", "text"),
                ],
                vec![fk(
                    "orders_user_fk",
                    &["user_id"],
                    "public",
                    "users",
                    &["id"],
                )],
            ),
            table(
                "order_items",
                &[
                    ("id", "int4"),
                    ("order_id", "int4"),
                    ("product_id", "int4"),
                    ("qty", "int4"),
                ],
                vec![
                    fk("items_order_fk", &["order_id"], "public", "orders", &["id"]),
                    fk(
                        "items_product_fk",
                        &["product_id"],
                        "public",
                        "products",
                        &["id"],
                    ),
                ],
            ),
            table(
                "products",
                &[("id", "int4"), ("name", "text"), ("price", "numeric")],
                vec![],
            ),
            table(
                "UserProfiles",
                &[("id", "int4"), ("displayName", "text")],
                vec![],
            ),
            table("order", &[("id", "int4"), ("group", "text")], vec![]),
            active,
        ],
        routines: vec![Routine {
            name: "calc_tax".into(),
            kind: RoutineKind::Function,
            signature: "(amount numeric)".into(),
            returns: Some("numeric".into()),
        }],
        ..Default::default()
    };
    let sales = Schema {
        name: "sales".into(),
        tables: vec![
            table(
                "leads",
                &[("id", "int4"), ("user_id", "int4"), ("score", "int4")],
                vec![fk(
                    "leads_user_fk",
                    &["user_id"],
                    "public",
                    "users",
                    &["id"],
                )],
            ),
            table("users", &[("id", "int4"), ("region", "text")], vec![]),
        ],
        ..Default::default()
    };
    Catalog {
        database: "app".into(),
        search_path: vec!["public".into()],
        schemas: vec![public, sales],
    }
}

fn run(sql: &str, d: Dialect) -> CompletionResult {
    let offset = sql.find('|').expect("a caret");
    let text = sql.replacen('|', "", 1);
    complete(&text, offset, d, &catalog())
}

fn items(sql: &str) -> Vec<Completion> {
    run(sql, Dialect::Postgres).items
}

fn labels_in(sql: &str, d: Dialect) -> Vec<String> {
    run(sql, d).items.into_iter().map(|c| c.label).collect()
}

fn labels(sql: &str) -> Vec<String> {
    labels_in(sql, Dialect::Postgres)
}

fn kinds_of(sql: &str, kind: CompletionKind) -> Vec<String> {
    items(sql)
        .into_iter()
        .filter(|c| c.kind == kind)
        .map(|c| c.label)
        .collect()
}

fn find(sql: &str, label: &str) -> Completion {
    items(sql)
        .into_iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("{label} not offered for {sql}"))
}

fn has(sql: &str, label: &str) -> bool {
    labels(sql).iter().any(|l| l == label)
}

fn pos(list: &[String], label: &str) -> usize {
    list.iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("{label} missing from {list:?}"))
}

// ----- statement start and keywords -----

#[test]
fn statement_start_offers_statement_keywords() {
    let l = labels("|");
    assert_eq!(l[0], "SELECT");
    assert!(l.contains(&"INSERT INTO".into()) && l.contains(&"DELETE FROM".into()));
}

#[test]
fn keywords_follow_the_typed_case() {
    assert_eq!(labels("sel|")[0], "select");
    assert_eq!(labels("SEL|")[0], "SELECT");
    // No prefix: the statement's own keyword case.
    assert_eq!(labels("select * from users |")[0], "where");
    assert_eq!(labels("SELECT * FROM users |")[0], "WHERE");
}

#[test]
fn after_a_table_come_clause_keywords() {
    let l = labels("SELECT * FROM users |");
    assert_eq!(l[0], "WHERE");
    assert!(
        l.contains(&"LEFT JOIN".into())
            && l.contains(&"ORDER BY".into())
            && l.contains(&"LIMIT".into())
    );
    let m = labels_in("SELECT * FROM users |", Dialect::Mssql);
    assert!(!m.contains(&"LIMIT".into()) && m.contains(&"CROSS APPLY".into()));
    assert_eq!(labels("SELECT * FROM users u JOIN orders o |")[0], "ON");
    assert_eq!(labels("SELECT * FROM users WHE|"), vec!["WHERE"]);
}

#[test]
fn second_words_of_keyword_pairs() {
    assert_eq!(labels("SELECT * FROM users GROUP |"), vec!["BY"]);
    assert_eq!(labels("SELECT * FROM users ORDER |"), vec!["BY"]);
    assert_eq!(
        labels("SELECT * FROM users LEFT |"),
        vec!["JOIN", "OUTER JOIN"]
    );
    assert_eq!(labels("INSERT |"), vec!["INTO"]);
    assert_eq!(labels("DELETE |"), vec!["FROM"]);
    assert_eq!(labels("SELECT * FROM users WHERE name IS |")[0], "NULL");
    assert!(labels("CREATE |").contains(&"TABLE".into()));
}

#[test]
fn keywords_after_an_operand() {
    let l = labels("SELECT * FROM users u WHERE u.id = 1 |");
    assert_eq!(&l[..2], ["AND", "OR"]);
    assert!(l.contains(&"ORDER BY".into()) && l.contains(&"ILIKE".into()));
    assert!(
        !labels_in("SELECT * FROM users WHERE id = 1 |", Dialect::Mysql).contains(&"ILIKE".into())
    );
    assert_eq!(
        labels("SELECT * FROM users u WHERE u.id = 1 an|"),
        vec!["and"]
    );
    assert_eq!(
        &labels("SELECT * FROM users ORDER BY name |")[..2],
        ["ASC", "DESC"]
    );
    assert_eq!(&labels("SELECT name |")[..2], ["FROM", "AS"]);
    assert_eq!(labels("SELECT * |")[0], "FROM");
}

// ----- tables -----

#[test]
fn tables_after_from() {
    let l = labels("SELECT * FROM |");
    // Search-path tables (unqualified) before other schemas' (qualified) and schemas.
    assert!(pos(&l, "users") < pos(&l, "sales.leads"));
    assert!(pos(&l, "sales.leads") < pos(&l, "sales"));
    assert!(l.contains(&"sales.users".into()));
    assert_eq!(
        find("SELECT * FROM |", "active_users").kind,
        CompletionKind::View
    );
    assert_eq!(find("SELECT * FROM |", "sales.leads").insert, "sales.leads");
    for start in [
        "INSERT INTO |",
        "UPDATE |",
        "DELETE FROM |",
        "TRUNCATE |",
        "SELECT * FROM users, |",
    ] {
        assert!(has(start, "orders"), "{start}");
    }
}

#[test]
fn table_prefix_matching_and_quoting() {
    let l = labels("SELECT * FROM us|");
    assert_eq!(l[0], "users");
    assert!(l.contains(&"UserProfiles".into()) && l.contains(&"sales.users".into()));
    assert_eq!(
        find("SELECT * FROM us|", "UserProfiles").insert,
        "\"UserProfiles\""
    );
    assert_eq!(find("SELECT * FROM ord|", "order").insert, "\"order\"");
    assert_eq!(
        run("SELECT * FROM ord|", Dialect::Mssql)
            .items
            .iter()
            .find(|c| c.label == "order")
            .unwrap()
            .insert,
        "[order]"
    );
    // Humps: `oi` → order_items.
    assert_eq!(labels("SELECT * FROM oi|")[0], "order_items");
}

#[test]
fn schema_qualified_tables() {
    assert_eq!(labels("SELECT * FROM sales.|"), vec!["leads", "users"]);
    assert_eq!(labels("SELECT * FROM sales.le|"), vec!["leads"]);
    let r = run("SELECT * FROM sales.le|", Dialect::Postgres);
    assert_eq!(r.replace, 20..22);
    assert_eq!(r.items[0].insert, "leads");
}

#[test]
fn quoted_partial_identifiers() {
    let r = run("SELECT * FROM \"Us|", Dialect::Postgres);
    assert_eq!(r.replace, 14..17);
    assert_eq!(r.items[0].label, "UserProfiles");
    assert_eq!(r.items[0].insert, "\"UserProfiles\"");
    // A quote forces quoting even where it isn't needed.
    assert_eq!(
        r.items.iter().find(|c| c.label == "users").unwrap().insert,
        "\"users\""
    );
    // Editors close the quote as it is typed.
    let r = run(
        "SELECT p.\"disp|\" FROM \"UserProfiles\" p",
        Dialect::Postgres,
    );
    assert_eq!(r.items[0].insert, "\"displayName\"");
    let r = run("SELECT * FROM [us|", Dialect::Mssql);
    assert_eq!(r.items[0].insert, "[users]");
}

#[test]
fn ctes_are_tables() {
    let l = labels("WITH recent AS (SELECT id FROM orders) SELECT * FROM |");
    assert_eq!(l[0], "recent");
    assert_eq!(
        find(
            "WITH recent AS (SELECT id FROM orders) SELECT * FROM |",
            "recent"
        )
        .detail
        .as_deref(),
        Some("CTE")
    );
}

// ----- joins from foreign keys -----

#[test]
fn fk_joins_both_directions_come_first() {
    let l = labels("SELECT * FROM orders o JOIN |");
    assert_eq!(
        &l[..2],
        [
            "users u ON u.id = o.user_id",
            "order_items oi ON oi.order_id = o.id"
        ]
    );
    assert_eq!(
        find(
            "SELECT * FROM orders o JOIN |",
            "users u ON u.id = o.user_id"
        )
        .kind,
        CompletionKind::Join
    );
    // Incoming from another schema: qualified.
    let l = labels("SELECT * FROM users u LEFT JOIN |");
    assert!(l.contains(&"orders o ON o.user_id = u.id".into()));
    assert!(l.contains(&"sales.leads l ON l.user_id = u.id".into()));
    assert!(pos(&l, "sales.leads l ON l.user_id = u.id") < pos(&l, "users"));
}

#[test]
fn fk_join_aliases_avoid_names_in_scope() {
    assert!(has(
        "SELECT * FROM users o JOIN |",
        "orders o1 ON o1.user_id = o.id"
    ));
    // A table without an alias is referenced by its name.
    assert!(has(
        "SELECT * FROM order_items JOIN |",
        "products p ON p.id = order_items.product_id"
    ));
}

#[test]
fn fk_joins_filter_by_typed_table_name() {
    let l = labels("SELECT * FROM users u JOIN ord|");
    assert_eq!(l[0], "orders o ON o.user_id = u.id");
    assert!(l.contains(&"orders".into()));
    assert!(!l.iter().any(|x| x.starts_with("sales.leads")));
}

#[test]
fn fk_conditions_after_on() {
    let l = labels("SELECT * FROM users u JOIN orders o ON |");
    assert_eq!(l[0], "o.user_id = u.id");
    let l = labels("SELECT * FROM order_items i JOIN orders o ON |");
    assert_eq!(l[0], "o.id = i.order_id");
}

// ----- columns -----

#[test]
fn select_list_sees_tables_after_the_caret() {
    let l = labels("SELECT | FROM users");
    assert_eq!(&l[..4], ["created_at", "email", "id", "name"]);
    assert_eq!(
        find("SELECT | FROM users", "email").detail.as_deref(),
        Some("text")
    );
    assert!(l.contains(&"DISTINCT".into()) && l.contains(&"count".into()));
}

#[test]
fn ambiguous_columns_are_qualified() {
    let sql = "SELECT | FROM users u JOIN orders o ON o.user_id = u.id";
    let l = labels(sql);
    assert!(l.contains(&"u.id".into()) && l.contains(&"o.id".into()));
    assert!(!l.contains(&"id".into()));
    assert!(l.contains(&"email".into()) && l.contains(&"total".into()));
    assert_eq!(find(sql, "u.id").insert, "u.id");
    assert_eq!(find(sql, "u").kind, CompletionKind::Alias);
}

#[test]
fn alias_dot_offers_that_sources_columns() {
    let l = kinds_of("SELECT u.| FROM users u", CompletionKind::Column);
    assert_eq!(l, vec!["created_at", "email", "id", "name"]);
    let r = run("SELECT u.na| FROM users u", Dialect::Postgres);
    assert_eq!(r.items[0].label, "name");
    assert_eq!(r.replace, 9..11);
    // Mid-word: the whole word is replaced.
    assert_eq!(
        run("SELECT na|me FROM users", Dialect::Postgres).replace,
        7..11
    );
}

#[test]
fn table_and_schema_qualifiers() {
    assert_eq!(
        kinds_of("SELECT users.| FROM users", CompletionKind::Column).len(),
        4
    );
    assert_eq!(
        kinds_of(
            "SELECT public.users.| FROM public.users",
            CompletionKind::Column
        )
        .len(),
        4
    );
    assert_eq!(
        kinds_of("SELECT sales.users.|", CompletionKind::Column),
        vec!["id", "region"]
    );
    // Not in FROM: resolved through the catalog.
    assert_eq!(
        kinds_of("SELECT products.|", CompletionKind::Column),
        vec!["id", "name", "price"]
    );
}

#[test]
fn where_on_group_order_offer_columns() {
    for sql in [
        "SELECT * FROM users u WHERE |",
        "SELECT * FROM users u WHERE u.id = 1 AND |",
        "SELECT * FROM users u JOIN orders o ON o.user_id = |",
        "SELECT name FROM users GROUP BY |",
        "SELECT name FROM users ORDER BY |",
        "SELECT name FROM users GROUP BY name HAVING |",
        "SELECT * FROM users WHERE id IN (1, |)",
        "SELECT coalesce(|) FROM users",
    ] {
        assert!(has(sql, "email"), "{sql}");
    }
}

#[test]
fn humps_and_substring_matching() {
    assert_eq!(
        labels("SELECT * FROM order_items WHERE pid|")[0],
        "product_id"
    );
    assert_eq!(labels("SELECT * FROM users WHERE mail|")[0], "email");
    // Prefix matches of any kind rank before substring matches.
    let l = labels("SELECT * FROM users WHERE na|");
    assert_eq!(l[0], "name");
}

#[test]
fn order_by_offers_select_aliases() {
    let sql = "SELECT count(*) AS n FROM users GROUP BY name ORDER BY |";
    assert_eq!(find(sql, "n").kind, CompletionKind::Alias);
}

#[test]
fn star_expands_to_columns() {
    assert_eq!(
        find("SELECT | FROM users", "* → id, name, email, created_at").insert,
        "id, name, email, created_at"
    );
    let sql = "SELECT | FROM users u JOIN orders o ON true";
    let star = items(sql)
        .into_iter()
        .find(|c| c.kind == CompletionKind::Snippet)
        .unwrap();
    assert!(
        star.insert
            .starts_with("u.id, u.name, u.email, u.created_at, o.id, o.user_id")
    );
    let star = items("SELECT u.| FROM users u")
        .into_iter()
        .find(|c| c.kind == CompletionKind::Snippet)
        .unwrap();
    assert_eq!(star.insert, "id, u.name, u.email, u.created_at");
}

// ----- CTEs and subqueries -----

#[test]
fn cte_columns_are_inferred() {
    let sql = "WITH recent AS (SELECT id, total AS amount FROM orders) SELECT | FROM recent r";
    assert_eq!(kinds_of(sql, CompletionKind::Column), vec!["amount", "id"]);
    let sql = "WITH recent AS (SELECT o.* FROM orders o) SELECT r.| FROM recent r";
    assert_eq!(
        kinds_of(sql, CompletionKind::Column),
        vec!["id", "status", "total", "user_id"]
    );
    let sql = "WITH t(a, b) AS (SELECT 1, 2) SELECT t.| FROM t";
    assert_eq!(kinds_of(sql, CompletionKind::Column), vec!["a", "b"]);
    // Inside a CTE body the CTE's own sources apply.
    assert!(has(
        "WITH x AS (SELECT | FROM products) SELECT * FROM x",
        "price"
    ));
}

#[test]
fn inside_an_unfinished_cte_or_subquery() {
    assert_eq!(&labels("WITH x AS (|")[..2], ["SELECT", "VALUES"]);
    assert_eq!(labels("WITH x AS (SELECT 1) |")[0], "SELECT");
    assert_eq!(labels("SELECT * FROM (|")[0], "SELECT");
    assert_eq!(
        kinds_of(
            "SELECT * FROM users u WHERE u.id = (SELECT max(o.|) FROM orders o)",
            CompletionKind::Column
        ),
        vec!["id", "status", "total", "user_id"]
    );
}

#[test]
fn lateral_subqueries_are_correlated() {
    let sql = "SELECT * FROM users u CROSS JOIN LATERAL (SELECT | FROM orders o WHERE o.user_id = u.id) x";
    let l = labels(sql);
    assert!(pos(&l, "total") < pos(&l, "u.email"));
}

#[test]
fn subqueries_in_from() {
    let sql = "SELECT s.| FROM (SELECT id, total * 2 AS doubled FROM orders) s";
    assert_eq!(kinds_of(sql, CompletionKind::Column), vec!["doubled", "id"]);
    assert!(has("SELECT | FROM (SELECT price FROM products) p", "price"));
}

#[test]
fn subqueries_see_their_own_sources_then_outer_ones() {
    let sql = "SELECT * FROM users u WHERE u.id IN (SELECT | FROM orders)";
    let l = labels(sql);
    assert!(pos(&l, "user_id") < pos(&l, "u.email"));
    let sql = "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.|)";
    assert_eq!(
        kinds_of(sql, CompletionKind::Column),
        vec!["created_at", "email", "id", "name"]
    );
    // A FROM subquery is not correlated.
    assert!(!has(
        "SELECT * FROM users u, (SELECT | FROM orders) o",
        "u.email"
    ));
}

#[test]
fn union_parts_have_their_own_sources() {
    let sql = "SELECT | FROM users UNION SELECT id FROM orders";
    assert!(has(sql, "email") && !has(sql, "total"));
    assert_eq!(
        labels("SELECT id FROM users UNION |"),
        vec!["ALL", "SELECT"]
    );
}

// ----- DML -----

#[test]
fn insert_column_lists() {
    assert_eq!(
        kinds_of("INSERT INTO users (|", CompletionKind::Column),
        vec!["id", "name", "email", "created_at"]
    );
    // In table order, without the ones already listed.
    assert_eq!(
        kinds_of("INSERT INTO users (name, |)", CompletionKind::Column),
        vec!["id", "email", "created_at"]
    );
    let l = labels("INSERT INTO users |");
    assert_eq!(l[0], "(name, email, created_at) VALUES (…)");
    assert_eq!(
        find(
            "INSERT INTO users |",
            "(name, email, created_at) VALUES (…)"
        )
        .insert,
        "(name, email, created_at) VALUES ("
    );
    let l = labels("INSERT INTO users (name) |");
    assert_eq!(l[0], "VALUES (…)");
    assert!(l.contains(&"SELECT".into()));
    assert!(has("INSERT INTO users (name) VALUES (|", "now"));
}

#[test]
fn update_set_and_where() {
    assert_eq!(
        kinds_of("UPDATE users SET |", CompletionKind::Column),
        vec!["created_at", "email", "id", "name"]
    );
    assert!(has("UPDATE users SET name = 'x', |", "email"));
    assert!(has("UPDATE users u SET name = u.|", "email"));
    assert_eq!(labels("UPDATE users SET name = 'x' |")[0], "WHERE");
    assert_eq!(labels("UPDATE users |")[0], "SET");
    assert!(has("UPDATE users SET name = 'x' WHERE |", "email"));
    let m = run("UPDATE u SET | FROM users u", Dialect::Mssql);
    assert!(m.items.iter().any(|c| c.label == "email"));
}

#[test]
fn delete_where() {
    assert!(has("DELETE FROM orders WHERE |", "status"));
    assert_eq!(labels("DELETE FROM orders |")[0], "WHERE");
}

#[test]
fn on_conflict_targets_and_excluded() {
    assert!(has(
        "INSERT INTO users (name) VALUES ('x') ON CONFLICT (|",
        "email"
    ));
    assert_eq!(
        kinds_of(
            "INSERT INTO users (name) VALUES ('x') ON CONFLICT (name) DO UPDATE SET name = excluded.|",
            CompletionKind::Column
        ),
        vec!["created_at", "email", "id", "name"]
    );
}

// ----- functions -----

#[test]
fn functions_builtin_and_catalog() {
    let l = labels("SELECT co|");
    assert!(l.contains(&"count".into()) && l.contains(&"coalesce".into()));
    assert_eq!(find("SELECT co|", "count").insert, "count(");
    assert_eq!(find("SELECT calc|", "calc_tax").insert, "calc_tax(");
    assert_eq!(
        find("SELECT calc|", "calc_tax").detail.as_deref(),
        Some("(amount numeric) → numeric")
    );
    assert!(labels_in("SELECT getd|", Dialect::Mssql).contains(&"getdate".into()));
    assert_eq!(
        labels("SELECT * FROM users WHERE count(*) OVER (|"),
        vec!["PARTITION BY", "ORDER BY"]
    );
}

// ----- where nothing is offered, and statement boundaries -----

#[test]
fn nothing_inside_strings_comments_and_numbers() {
    for sql in [
        "SELECT 'abc|'",
        "SELECT 'abc|",
        "-- sel|",
        "SELECT 1 /* fr| */",
        "SELECT 12|",
        "SELECT * FROM users u AS |",
    ] {
        assert!(labels(sql).is_empty(), "{sql}");
    }
    assert!(labels_in("SELECT 1 # fr|", Dialect::Mysql).is_empty());
}

#[test]
fn completion_uses_the_statement_at_the_caret() {
    let sql = "SELECT * FROM orders;\nSELECT | FROM users";
    assert!(has(sql, "email") && !has(sql, "total"));
    // Unterminated, separated by a blank line: still separate statements.
    let sql = "SELECT * FROM orders o WHERE o.|\n\nSELECT * FROM users o";
    assert_eq!(
        kinds_of(sql, CompletionKind::Column),
        vec!["id", "status", "total", "user_id"]
    );
    // Right after `;`: a new statement.
    assert_eq!(labels("SELECT 1;|")[0], "SELECT");
}

#[test]
fn mssql_batches_and_top() {
    assert!(
        labels_in("DECLARE @x int; SELECT | FROM users", Dialect::Mssql).contains(&"email".into())
    );
    let l = labels_in("SELECT | FROM users", Dialect::Mssql);
    assert!(l.contains(&"TOP".into()));
    assert!(labels_in("SELECT TOP 10 | FROM users", Dialect::Mssql).contains(&"email".into()));
}

#[test]
fn mysql_delimiter_scripts() {
    let sql = "DELIMITER //\nSELECT | FROM users//\nDELIMITER ;";
    assert!(labels_in(sql, Dialect::Mysql).contains(&"email".into()));
}

#[test]
fn star_only_where_a_select_item_starts() {
    assert!(
        !items("SELECT CASE WHEN | FROM users")
            .iter()
            .any(|c| c.kind == CompletionKind::Snippet)
    );
    assert!(
        items("SELECT id, | FROM users")
            .iter()
            .any(|c| c.kind == CompletionKind::Snippet)
    );
}

#[test]
fn mysql_backticks() {
    let r = run("SELECT * FROM `us|", Dialect::Mysql);
    assert_eq!(r.items[0].insert, "`users`");
    let r = run("SELECT * FROM ord|", Dialect::Mysql);
    assert_eq!(
        r.items.iter().find(|c| c.label == "order").unwrap().insert,
        "`order`"
    );
}

// ----- performance -----

fn big_catalog() -> Catalog {
    let tables = (0..2000)
        .map(|i| Table {
            name: format!("table_{i:04}"),
            columns: (0..20)
                .map(|c| col(&format!("column_{c:02}"), "int4"))
                .collect(),
            foreign_keys: (i > 0)
                .then(|| {
                    fk(
                        "fk",
                        &["column_01"],
                        "public",
                        &format!("table_{:04}", i - 1),
                        &["column_00"],
                    )
                })
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .collect();
    Catalog {
        database: "big".into(),
        search_path: vec!["public".into()],
        schemas: vec![Schema {
            name: "public".into(),
            tables,
            ..Default::default()
        }],
    }
}

fn big_script() -> (String, Vec<usize>) {
    let mut sql = String::new();
    let mut carets = Vec::new();
    for i in 0..500 {
        if i == 250 {
            carets.push(sql.len() + "SELECT ".len());
            sql.push_str(
                "SELECT  FROM table_0001 a JOIN table_0002 b ON b.column_01 = a.column_00\n",
            );
            carets.push(sql.len() + "WHERE ".len());
            sql.push_str("WHERE  AND a.column_03 > 1\n");
            carets.push(sql.len() + "  JOIN ".len());
            sql.push_str("  JOIN  ;\n");
            carets.push(sql.len() + "SELECT * FROM ".len());
            sql.push_str("SELECT * FROM \n");
            continue;
        }
        sql.push_str(&format!("SELECT t.column_01, t.column_02 -- note ; here\nFROM table_{i:04} t\nWHERE t.column_03 = 'it''s'\n  AND t.column_04 IN (1, 2);\n"));
    }
    (sql, carets)
}

fn slowest(cat: &Catalog) -> (Duration, usize) {
    let (sql, carets) = big_script();
    assert!(sql.lines().count() >= 2000);
    let mut worst = Duration::ZERO;
    let mut most = 0;
    for &c in &carets {
        let t = Instant::now();
        let r = complete(&sql, c, Dialect::Postgres, cat);
        worst = worst.max(t.elapsed());
        most = most.max(r.items.len());
    }
    (worst, most)
}

#[test]
fn fast_on_a_big_script_and_catalog() {
    let cat = big_catalog();
    let (worst, most) = slowest(&cat);
    assert!(most >= 2000, "the FROM position lists every table");
    assert!(
        worst < Duration::from_millis(50),
        "slowest completion took {worst:?}"
    );
}

#[test]
#[ignore = "timing: cargo test -p sql-lang --release -- --ignored --nocapture"]
fn bench_completion() {
    let cat = big_catalog();
    let (sql, carets) = big_script();
    for &c in &carets {
        let mut times: Vec<Duration> = (0..20)
            .map(|_| {
                let t = Instant::now();
                complete(&sql, c, Dialect::Postgres, &cat);
                t.elapsed()
            })
            .collect();
        times.sort();
        println!("caret {c}: median {:?}, max {:?}", times[10], times[19]);
    }
}
