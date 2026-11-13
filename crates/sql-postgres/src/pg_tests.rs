//! Against a real server, only when `SQL_TEST_PG_URL` is set
//! (`postgres://user:pass@host:port/db`); skipped otherwise.

use futures::channel::mpsc;
use sql_core::{ConnectConfig, Connection, Event, TableKind, TableRef, Value};

fn config() -> Option<ConnectConfig> {
    let url = std::env::var("SQL_TEST_PG_URL").ok()?;
    let pg: tokio_postgres::Config = url.parse().expect("SQL_TEST_PG_URL");
    let host = match pg.get_hosts().first()? {
        tokio_postgres::config::Host::Tcp(h) => h.clone(),
        tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
    };
    Some(ConnectConfig {
        host,
        port: pg.get_ports().first().copied().unwrap_or(5432),
        user: pg.get_user().unwrap_or("postgres").into(),
        password: pg
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default(),
        database: pg.get_dbname().unwrap_or("postgres").into(),
        ..Default::default()
    })
}

async fn run(
    c: &mut Box<dyn Connection>,
    sql: &str,
    params: &[Value],
) -> anyhow::Result<Vec<Event>> {
    let (tx, rx) = mpsc::unbounded();
    c.execute(sql, params, tx).await?;
    Ok(futures::StreamExt::collect(rx).await)
}

fn rows(events: &[Event]) -> Vec<Vec<Value>> {
    events
        .iter()
        .filter_map(|e| {
            if let Event::Rows(r) = e {
                Some(r.clone())
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

macro_rules! connect_or_skip {
    () => {{
        let Some(cfg) = config() else {
            eprintln!("SQL_TEST_PG_URL unset; skipping");
            return;
        };
        crate::connect(&cfg).await.expect("connect")
    }};
}

#[tokio::test]
async fn types_streaming_notices_and_cancel() {
    let mut c = connect_or_skip!();
    assert!(c.server_version().starts_with("PostgreSQL "));

    let ev = run(
        &mut c,
        "select 1::int2, 2::int8, 1.5::float8, 12345678901234567890.123456789::numeric, true, 'x'::varchar(3),
                '\\xdead'::bytea, '{\"a\":1}'::jsonb, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '2024-02-03'::date,
                '04:05:06'::time, '2024-02-03 04:05:06'::timestamp, array[1,2], interval '1 day', '10.0.0.1'::inet, null::text, $1::int",
        &[Value::Int(42)],
    )
    .await
    .unwrap();
    let Event::ResultStart(cols) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(cols[0].type_name, "int2");
    assert_eq!(cols[12].type_name, "int4[]");
    assert_eq!(
        rows(&ev)[0],
        vec![
            Value::Int(1),
            Value::Int(2),
            Value::Float(1.5),
            Value::Decimal("12345678901234567890.123456789".into()),
            Value::Bool(true),
            Value::Text("x".into()),
            Value::Bytes(vec![0xde, 0xad]),
            Value::Json(serde_json::json!({"a": 1})),
            Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            Value::Date("2024-02-03".into()),
            Value::Time("04:05:06".into()),
            Value::Timestamp("2024-02-03 04:05:06".into()),
            Value::Other("{1,2}".into()),
            Value::Other("1 day".into()),
            Value::Other("10.0.0.1".into()),
            Value::Null,
            Value::Int(42),
        ]
    );

    // Custom and geometric types come back as text, inside a transaction that
    // survives both a script and a described statement.
    run(&mut c, "begin", &[]).await.unwrap();
    run(
        &mut c,
        "create type pg_temp.mood as enum ('ok'); create temp table m (x pg_temp.mood, p point)",
        &[],
    )
    .await
    .unwrap();
    run(&mut c, "insert into m values ('ok', '(1,2)')", &[])
        .await
        .unwrap();
    let ev = run(&mut c, "select x, p from m", &[]).await.unwrap();
    assert_eq!(
        rows(&ev),
        vec![vec![
            Value::Other("ok".into()),
            Value::Other("(1,2)".into())
        ]]
    );
    run(&mut c, "commit", &[]).await.unwrap();

    let ev = run(&mut c, "select g from generate_series(1, 1200) g", &[])
        .await
        .unwrap();
    assert!(
        ev.iter()
            .all(|e| !matches!(e, Event::Rows(r) if r.len() > 500))
    );
    assert_eq!(rows(&ev).len(), 1200);

    let ev = run(&mut c, "select 1 a; select 2 b, 3 c", &[])
        .await
        .unwrap();
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(e, Event::ResultStart(_)))
            .count(),
        2
    );

    let ev = run(&mut c, "do $$ begin raise notice 'hello'; end $$", &[])
        .await
        .unwrap();
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Notice(n) if n.contains("hello"))),
        "{ev:?}"
    );

    let canceller = c.canceller();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        canceller.cancel().await
    });
    let err = run(&mut c, "select pg_sleep(30)", &[]).await.unwrap_err();
    assert!(err.to_string().contains("cancel"), "{err}");
    cancel.await.unwrap().unwrap();

    // A dropped receiver ends the statement quietly and leaves the connection usable.
    let (tx, rx) = mpsc::unbounded();
    drop(rx);
    c.execute("select g from generate_series(1, 1000000) g", &[], tx)
        .await
        .unwrap();
    assert_eq!(
        rows(&run(&mut c, "select 7", &[]).await.unwrap()),
        vec![vec![Value::Int(7)]]
    );
}

#[tokio::test]
async fn introspection_and_ddl() {
    let mut c = connect_or_skip!();
    run(
        &mut c,
        "drop schema if exists sgtest cascade;
         create schema sgtest;
         create table sgtest.parent (id int generated always as identity primary key, code varchar(255) not null default 'x' unique);
         comment on table sgtest.parent is 'the parent';
         create table sgtest.child (id serial primary key, parent_id int references sgtest.parent(id), note text);
         create index child_note on sgtest.child (lower(note));
         create view sgtest.v as select id from sgtest.parent;
         create function sgtest.f(a integer, b text) returns int language sql as $$ select a $$;",
        &[],
    )
    .await
    .unwrap();

    let cat = c.introspect().await.unwrap();
    let s = cat.schema("sgtest").unwrap();
    let parent = s.tables.iter().find(|t| t.name == "parent").unwrap();
    assert_eq!(parent.primary_key, vec!["id"]);
    assert_eq!(parent.comment.as_deref(), Some("the parent"));
    assert!(parent.columns[0].generated);
    assert_eq!(parent.columns[1].type_name, "character varying(255)");
    assert!(!parent.columns[1].nullable);
    let child = s.tables.iter().find(|t| t.name == "child").unwrap();
    assert!(child.columns[0].generated);
    assert_eq!(
        child.foreign_keys[0].references,
        TableRef::new(Some("sgtest"), "parent")
    );
    assert!(
        child
            .indexes
            .iter()
            .any(|i| i.name == "child_note" && i.columns == vec!["lower(note)"])
    );
    assert_eq!(
        s.tables.iter().find(|t| t.name == "v").unwrap().kind,
        TableKind::View
    );
    assert_eq!(s.routines[0].signature, "(a integer, b text)");
    assert!(s.sequences.contains(&"child_id_seq".to_string()));
    assert!(!cat.search_path.is_empty());

    let ddl = c
        .ddl(&TableRef::new(Some("sgtest"), "parent"))
        .await
        .unwrap();
    assert!(ddl.contains("CREATE TABLE sgtest.parent"), "{ddl}");
    assert!(ddl.contains("GENERATED ALWAYS AS IDENTITY"), "{ddl}");
    assert!(ddl.contains("PRIMARY KEY (id)"), "{ddl}");
    assert!(
        ddl.contains("COMMENT ON TABLE sgtest.parent IS 'the parent'"),
        "{ddl}"
    );
    let ddl = c
        .ddl(&TableRef::new(Some("sgtest"), "child"))
        .await
        .unwrap();
    assert!(ddl.contains("CREATE INDEX child_note"), "{ddl}");
    assert!(
        ddl.contains("FOREIGN KEY (parent_id) REFERENCES sgtest.parent(id)"),
        "{ddl}"
    );
    let ddl = c.ddl(&TableRef::new(Some("sgtest"), "v")).await.unwrap();
    assert!(ddl.starts_with("CREATE VIEW sgtest.v AS"), "{ddl}");

    let ev = run(&mut c, "select p.code, 1 one from sgtest.parent p", &[])
        .await
        .unwrap();
    let Event::ResultStart(cols) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(
        cols[0].origin,
        Some(TableRef::new(Some("sgtest"), "parent"))
    );
    assert_eq!(cols[1].origin, None);

    assert!(c.databases().await.unwrap().iter().any(|d| d == "postgres"));
    run(&mut c, "drop schema sgtest cascade", &[])
        .await
        .unwrap();
}

/// With `SQL_TEST_PG_SSL=1` the server must accept TLS (a self-signed
/// certificate is fine on loopback).
#[tokio::test]
async fn tls_on_loopback() {
    let Some(cfg) = config().filter(|_| std::env::var("SQL_TEST_PG_SSL").is_ok()) else {
        return;
    };
    let mut c = crate::connect(&ConnectConfig { ssl: true, ..cfg })
        .await
        .expect("connect");
    let ev = run(
        &mut c,
        "select ssl from pg_stat_ssl where pid = pg_backend_pid()",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(rows(&ev), vec![vec![Value::Bool(true)]]);
}
