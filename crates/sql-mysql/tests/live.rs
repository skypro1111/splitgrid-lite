//! Against a real server, when `SQL_TEST_MYSQL_URL` is set
//! (`mysql://user:pass@host:3306/db[?ssl=true]`); skipped otherwise.
//! The database must allow CREATE TABLE/PROCEDURE; objects are named `sgtest_*`.

use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use sql_core::{ConnectConfig, Connection, Dialect, Event, TableRef, Value};

fn config() -> Option<ConnectConfig> {
    let url = std::env::var("SQL_TEST_MYSQL_URL").ok()?;
    let rest = url.strip_prefix("mysql://").expect("mysql:// URL");
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, rest) = rest.rsplit_once('@').expect("user@host");
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "3306"));
    Some(ConnectConfig {
        dialect: Some(Dialect::Mysql),
        host: host.into(),
        port: port.parse().expect("port"),
        user: user.into(),
        password: password.into(),
        database: database.into(),
        ssl: query.contains("ssl=true"),
        file_path: None,
    })
}

async fn run(conn: &mut Box<dyn Connection>, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Event>> {
    let (tx, rx) = mpsc::unbounded();
    conn.execute(sql, params, tx).await?;
    Ok(rx.collect().await)
}

fn rows(events: &[Event]) -> Vec<Vec<Value>> {
    events.iter().filter_map(|e| if let Event::Rows(r) = e { Some(r.clone()) } else { None }).flatten().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mysql() -> anyhow::Result<()> {
    let Some(cfg) = config() else {
        eprintln!("SQL_TEST_MYSQL_URL not set; skipping");
        return Ok(());
    };
    let mut conn = sql_mysql::connect(&cfg).await?;
    let version = conn.server_version();
    assert!(version.starts_with("MySQL ") || version.starts_with("MariaDB "), "{version}");

    // Types.
    let ev = run(
        &mut conn,
        "select 1 as i, cast(18446744073709551615 as unsigned) as big, 1.50 as d, 'héllo' as t, \
         x'00ff' as b, cast('2024-02-29 13:05:09' as datetime) as ts, cast('2024-02-29' as date) as dt, null as n",
        &[],
    )
    .await?;
    assert!(matches!(&ev[0], Event::ResultStart(cols) if cols.len() == 8));
    let r = &rows(&ev)[0];
    assert_eq!(r[0], Value::Int(1));
    assert_eq!(r[1], Value::Decimal("18446744073709551615".into()));
    assert_eq!(r[2], Value::Decimal("1.50".into()));
    assert_eq!(r[3], Value::Text("héllo".into()));
    assert_eq!(r[4], Value::Bytes(vec![0, 255]));
    assert_eq!(r[5], Value::Timestamp("2024-02-29 13:05:09".into()));
    assert_eq!(r[6], Value::Date("2024-02-29".into()));
    assert_eq!(r[7], Value::Null);
    assert_eq!(ev.last(), Some(&Event::ResultEnd { affected: None }));

    // Parameters (binary protocol).
    let ev = run(&mut conn, "select ? + 1, ?", &[Value::Int(41), Value::Text("x".into())]).await?;
    assert_eq!(rows(&ev)[0], vec![Value::Int(42), Value::Text("x".into())]);

    // Streaming in batches.
    let ev = run(
        &mut conn,
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < 1200) select i from n",
        &[],
    )
    .await?;
    let batches: Vec<usize> = ev.iter().filter_map(|e| if let Event::Rows(r) = e { Some(r.len()) } else { None }).collect();
    assert_eq!(batches.iter().sum::<usize>(), 1200);
    assert!(batches.iter().all(|&n| n <= 500));

    // DML, warnings, JSON.
    run(&mut conn, "drop table if exists sgtest_child, sgtest_parent", &[]).await?;
    run(
        &mut conn,
        "create table sgtest_parent (id int unsigned auto_increment primary key, name varchar(20) not null default 'x' comment 'who', \
         doc json, flag bit(1)) comment 'parents'",
        &[],
    )
    .await?;
    run(
        &mut conn,
        "create table sgtest_child (id int primary key, parent_id int unsigned, key ix_parent (parent_id), \
         constraint fk_parent foreign key (parent_id) references sgtest_parent (id))",
        &[],
    )
    .await?;
    let ev = run(&mut conn, "insert into sgtest_parent (name, doc, flag) values ('a', '{\"k\": 1}', b'1'), ('b', null, b'0')", &[]).await?;
    assert_eq!(ev, vec![Event::ResultEnd { affected: Some(2) }]);
    let ev = run(&mut conn, "select doc, flag from sgtest_parent order by id", &[]).await?;
    assert_eq!(rows(&ev)[0], vec![Value::Json(serde_json::json!({"k": 1})), Value::Bool(true)]);
    let ev = run(&mut conn, "select cast('12abc' as signed)", &[]).await?;
    assert!(ev.iter().any(|e| matches!(e, Event::Notice(m) if m.contains("Truncated"))), "{ev:?}");

    // Procedures with several result sets.
    run(&mut conn, "drop procedure if exists sgtest_proc", &[]).await?;
    run(&mut conn, "create procedure sgtest_proc() begin select 1 as a; select 2 as b, 3 as c; end", &[]).await?;
    let ev = run(&mut conn, "call sgtest_proc()", &[]).await?;
    let starts = ev.iter().filter(|e| matches!(e, Event::ResultStart(_))).count();
    assert_eq!(starts, 2, "{ev:?}");

    // Introspection and DDL.
    let cat = conn.introspect().await?;
    let (_, parent) = cat.table(&TableRef::new(None, "sgtest_parent")).expect("parent table");
    assert_eq!(parent.primary_key, vec!["id"]);
    assert!(parent.columns[0].generated);
    assert_eq!(parent.columns[0].type_name, "int unsigned");
    assert_eq!(parent.comment.as_deref(), Some("parents"));
    assert_eq!(parent.columns[1].comment.as_deref(), Some("who"));
    let (_, child) = cat.table(&TableRef::new(None, "sgtest_child")).expect("child table");
    assert_eq!(child.foreign_keys[0].references.name, "sgtest_parent");
    assert!(child.indexes.iter().any(|i| i.name == "ix_parent"));
    assert!(cat.schemas[0].routines.iter().any(|r| r.name == "sgtest_proc"));
    let ddl = conn.ddl(&TableRef::new(None, "sgtest_child")).await?;
    assert!(ddl.starts_with("CREATE TABLE `sgtest_child`"), "{ddl}");
    assert!(!conn.databases().await?.iter().any(|d| d == "mysql"));

    // Cancel.
    let canceller = conn.canceller();
    let started = Instant::now();
    let stop = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        canceller.cancel().await
    });
    let _ = run(&mut conn, "select sleep(20)", &[]).await;
    stop.await??;
    assert!(started.elapsed() < Duration::from_secs(10));
    // The session survives KILL QUERY.
    assert_eq!(rows(&run(&mut conn, "select 7", &[]).await?)[0], vec![Value::Int(7)]);

    // A dropped receiver stops the stream without an error.
    let (tx, rx) = mpsc::unbounded();
    drop(rx);
    conn.execute("select * from information_schema.columns", &[], tx).await?;

    run(&mut conn, "drop procedure sgtest_proc", &[]).await?;
    run(&mut conn, "drop table sgtest_child, sgtest_parent", &[]).await?;
    conn.close().await
}
