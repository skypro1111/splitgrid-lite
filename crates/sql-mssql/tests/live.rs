//! Against a real server, when `SQL_TEST_MSSQL_URL` is set
//! (`mssql://user:pass@host:1433/db[?ssl=true]`); skipped otherwise.
//! The login must be able to create tables/procedures; for the cancel check
//! it also needs ALTER ANY CONNECTION (sa has it). Objects are named `sgtest_*`.

use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use sql_core::{ConnectConfig, Connection, Dialect, Event, TableRef, Value};

fn config() -> Option<ConnectConfig> {
    let url = std::env::var("SQL_TEST_MSSQL_URL").ok()?;
    let rest = url.strip_prefix("mssql://").expect("mssql:// URL");
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, rest) = rest.rsplit_once('@').expect("user@host");
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "1433"));
    Some(ConnectConfig {
        dialect: Some(Dialect::Mssql),
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
async fn live_mssql() -> anyhow::Result<()> {
    let Some(cfg) = config() else {
        eprintln!("SQL_TEST_MSSQL_URL not set; skipping");
        return Ok(());
    };
    let mut conn = sql_mssql::connect(&cfg).await?;
    assert!(conn.server_version().starts_with("SQL Server "), "{}", conn.server_version());

    // Types.
    let ev = run(
        &mut conn,
        "select cast(1 as int) as i, cast(12.34 as decimal(10,2)) as d, cast(5.5 as money) as m, N'héllo' as t, \
         cast('6F9619FF-8B86-D011-B42D-00C04FC964FF' as uniqueidentifier) as g, 0x00FF as b, \
         cast('2024-02-29T13:05:09.1234567' as datetime2) as ts, cast('2024-02-29' as date) as dt, \
         cast(1 as bit) as f, null as n",
        &[],
    )
    .await?;
    let r = &rows(&ev)[0];
    assert_eq!(r[0], Value::Int(1));
    assert_eq!(r[1], Value::Decimal("12.34".into()));
    assert_eq!(r[2], Value::Decimal("5.5000".into()));
    assert_eq!(r[3], Value::Text("héllo".into()));
    assert_eq!(r[4], Value::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()));
    assert_eq!(r[5], Value::Bytes(vec![0, 255]));
    assert_eq!(r[6], Value::Timestamp("2024-02-29 13:05:09.1234567".into()));
    assert_eq!(r[7], Value::Date("2024-02-29".into()));
    assert_eq!(r[8], Value::Bool(true));
    assert_eq!(r[9], Value::Null);

    // Parameters, several result sets, PRINT.
    let ev = run(&mut conn, "select @P1 + 1 as a", &[Value::Int(41)]).await?;
    assert_eq!(rows(&ev)[0], vec![Value::Int(42)]);
    let ev = run(&mut conn, "print 'hello'; select 1 as a; select 2 as b", &[]).await?;
    assert_eq!(ev.iter().filter(|e| matches!(e, Event::ResultStart(_))).count(), 2);
    assert!(ev.contains(&Event::Notice("hello".into())), "{ev:?}");

    // Streaming in batches.
    let ev = run(&mut conn, "select top 1200 row_number() over (order by (select null)) as i from sys.all_columns a cross join sys.all_columns b", &[]).await?;
    let batches: Vec<usize> = ev.iter().filter_map(|e| if let Event::Rows(r) = e { Some(r.len()) } else { None }).collect();
    assert_eq!(batches.iter().sum::<usize>(), 1200);
    assert!(batches.iter().all(|&n| n <= 500));

    // Temp tables persist across statements (plain batches) and DML counts rows.
    run(&mut conn, "create table #sgtest_tmp (id int)", &[]).await?;
    let ev = run(&mut conn, "insert into #sgtest_tmp values (1), (2), (3)", &[]).await?;
    assert_eq!(ev, vec![Event::ResultEnd { affected: Some(3) }]);

    // Introspection and DDL.
    run(&mut conn, "if object_id('dbo.sgtest_child') is not null drop table dbo.sgtest_child", &[]).await?;
    run(&mut conn, "if object_id('dbo.sgtest_parent') is not null drop table dbo.sgtest_parent", &[]).await?;
    run(
        &mut conn,
        "create table dbo.sgtest_parent (id int identity(1,1) constraint pk_sgtest_parent primary key, \
         name nvarchar(50) not null default N'x', total decimal(10,2), doubled as (total * 2))",
        &[],
    )
    .await?;
    run(
        &mut conn,
        "create table dbo.sgtest_child (id int primary key, parent_id int constraint fk_sgtest foreign key references dbo.sgtest_parent (id) on delete cascade)",
        &[],
    )
    .await?;
    run(&mut conn, "create index ix_sgtest_parent on dbo.sgtest_child (parent_id)", &[]).await?;
    run(
        &mut conn,
        "exec sp_addextendedproperty 'MS_Description', 'parents', 'SCHEMA', 'dbo', 'TABLE', 'sgtest_parent'",
        &[],
    )
    .await?;
    let cat = conn.introspect().await?;
    assert_eq!(cat.search_path, vec!["dbo"]);
    let (_, parent) = cat.table(&TableRef::new(None, "sgtest_parent")).expect("parent table");
    assert_eq!(parent.primary_key, vec!["id"]);
    assert_eq!(parent.comment.as_deref(), Some("parents"));
    assert_eq!(parent.columns[1].type_name, "nvarchar(50)");
    assert!(parent.columns[0].generated && parent.columns[3].generated && !parent.columns[1].generated);
    let (_, child) = cat.table(&TableRef::new(Some("dbo"), "sgtest_child")).expect("child table");
    assert_eq!(child.foreign_keys[0].references, TableRef::new(Some("dbo"), "sgtest_parent"));
    let ddl = conn.ddl(&TableRef::new(Some("dbo"), "sgtest_child")).await?;
    assert!(ddl.contains("ON DELETE CASCADE") && ddl.contains("CREATE NONCLUSTERED INDEX [ix_sgtest_parent]"), "{ddl}");
    assert!(!conn.databases().await?.iter().any(|d| d == "master"));

    // Cancel: the statement stops and the next one runs on a fresh session.
    let canceller = conn.canceller();
    let started = Instant::now();
    let stop = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        canceller.cancel().await
    });
    let err = run(&mut conn, "waitfor delay '00:00:20'", &[]).await.expect_err("cancelled");
    assert!(err.to_string().contains("cancelled"), "{err}");
    stop.await??;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(rows(&run(&mut conn, "select 7", &[]).await?)[0], vec![Value::Int(7)]);

    run(&mut conn, "drop table dbo.sgtest_child; drop table dbo.sgtest_parent", &[]).await?;
    conn.close().await
}
