use futures::channel::mpsc;
use sql_core::{ConnectConfig, Connection, Event, TableKind, TableRef, Value};

async fn open(dir: &tempfile::TempDir) -> Box<dyn Connection> {
    let cfg = ConnectConfig {
        file_path: Some(dir.path().join("t.db")),
        ..Default::default()
    };
    crate::connect(&cfg).await.unwrap()
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

#[tokio::test]
async fn creates_the_file_and_falls_back_to_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db");
    let cfg = ConnectConfig {
        database: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let c = crate::connect(&cfg).await.unwrap();
    assert!(path.exists());
    assert!(c.server_version().starts_with("SQLite 3."));
    assert!(crate::connect(&ConnectConfig::default()).await.is_err());
}

#[tokio::test]
async fn maps_values_by_storage_class_and_declared_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir).await;
    run(
        &mut c,
        "create table t (i integer, r real, s text, b blob, f boolean, j json, d date, n numeric)",
        &[],
    )
    .await
    .unwrap();
    let ev = run(
        &mut c,
        "insert into t values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        &[
            Value::Int(i64::MAX),
            Value::Float(1.5),
            Value::Text("hé".into()),
            Value::Bytes(vec![0, 255]),
            Value::Bool(true),
            Value::Json(serde_json::json!({"a": [1]})),
            Value::Date("2024-02-03".into()),
            Value::Decimal("12.50".into()),
        ],
    )
    .await
    .unwrap();
    assert_eq!(ev, vec![Event::ResultEnd { affected: Some(1) }]);
    run(&mut c, "insert into t (i) values (null)", &[])
        .await
        .unwrap();
    // DDL reports no rows, not the previous INSERT's count.
    assert_eq!(
        run(&mut c, "create table z (x)", &[]).await.unwrap(),
        vec![Event::ResultEnd { affected: Some(0) }]
    );

    let ev = run(&mut c, "select t.*, 1 + 1 as two from t", &[])
        .await
        .unwrap();
    let Event::ResultStart(cols) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(cols[0].type_name, "integer");
    assert_eq!(cols[0].origin, Some(TableRef::new(None, "t")));
    assert_eq!(cols[8].origin, None);
    let r = rows(&ev);
    assert_eq!(
        r[0],
        vec![
            Value::Int(i64::MAX),
            Value::Float(1.5),
            Value::Text("hé".into()),
            Value::Bytes(vec![0, 255]),
            Value::Bool(true),
            Value::Json(serde_json::json!({"a": [1]})),
            Value::Text("2024-02-03".into()),
            Value::Float(12.5),
            Value::Int(2),
        ]
    );
    assert_eq!(r[1][1], Value::Null);
    assert_eq!(ev.last(), Some(&Event::ResultEnd { affected: None }));
    assert!(run(&mut c, "select ?2", &[Value::Int(1)]).await.is_err());
}

#[tokio::test]
async fn streams_in_batches_and_runs_each_statement() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir).await;
    let ev = run(&mut c, "with recursive n(x) as (select 1 union all select x + 1 from n where x < 1234) select x from n", &[]).await.unwrap();
    let batches: Vec<usize> = ev
        .iter()
        .filter_map(|e| {
            if let Event::Rows(r) = e {
                Some(r.len())
            } else {
                None
            }
        })
        .collect();
    assert!(batches.iter().all(|&n| n <= 500), "{batches:?}");
    assert_eq!(batches.iter().sum::<usize>(), 1234);

    // A script yields one result per statement.
    let ev = run(
        &mut c,
        "create table a (x); insert into a values (1), (2); select x from a;",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(ev[0], Event::ResultEnd { affected: Some(0) });
    assert_eq!(ev[1], Event::ResultEnd { affected: Some(2) });
    assert!(matches!(&ev[2], Event::ResultStart(c) if c[0].name == "x"));
    assert_eq!(rows(&ev), vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // An empty result still describes its columns.
    let ev = run(&mut c, "select x from a where 0", &[]).await.unwrap();
    assert_eq!(ev.len(), 2);

    // A dropped receiver stops the statement without an error.
    let (tx, rx) = mpsc::unbounded();
    drop(rx);
    c.execute(
        "with recursive n(x) as (select 1 union all select x + 1 from n) select x from n",
        &[],
        tx,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn cancel_interrupts_a_running_statement() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir).await;
    let canceller = c.canceller();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        canceller.cancel().await
    });
    let started = std::time::Instant::now();
    let err = run(
        &mut c,
        "with recursive n(x) as (select 1 union all select x + 1 from n) select count(*) from n",
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("interrupt"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    cancel.await.unwrap().unwrap();
    // The connection is still good afterwards.
    assert_eq!(
        rows(&run(&mut c, "select 1", &[]).await.unwrap()),
        vec![vec![Value::Int(1)]]
    );
}

#[tokio::test]
async fn introspects_tables_keys_and_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir).await;
    run(
        &mut c,
        "create table parent (id integer primary key, code varchar(255) not null default 'x' unique);
         create table child (a int, b int, parent_id int references parent, note text,
                             total int generated always as (a + b), primary key (a, b));
         create index child_note on child (lower(note));
         create table kv (k text primary key, v) without rowid;
         create view v as select id from parent;
         attach database ':memory:' as aux;
         create table aux.other (id integer primary key);",
        &[],
    )
    .await
    .unwrap();

    let cat = c.introspect().await.unwrap();
    assert_eq!(cat.search_path, vec!["main", "aux"]);
    let main = cat.schema("main").unwrap();
    let names: Vec<&str> = main.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["child", "kv", "parent", "v"]);

    let parent = &main.tables[2];
    assert_eq!(parent.primary_key, vec!["id"]);
    assert!(parent.columns[0].generated, "rowid alias");
    assert_eq!(parent.columns[1].type_name, "varchar(255)");
    assert!(!parent.columns[1].nullable);
    assert_eq!(parent.columns[1].default.as_deref(), Some("'x'"));
    assert!(
        parent
            .indexes
            .iter()
            .any(|i| i.unique && i.columns == vec!["code"])
    );

    let child = &main.tables[0];
    assert_eq!(child.primary_key, vec!["a", "b"]);
    assert!(!child.columns[0].generated);
    assert!(
        child
            .columns
            .iter()
            .find(|c| c.name == "total")
            .unwrap()
            .generated
    );
    assert_eq!(child.foreign_keys.len(), 1);
    assert_eq!(
        child.foreign_keys[0].references,
        TableRef::new(None, "parent")
    );
    assert_eq!(child.foreign_keys[0].columns, vec!["parent_id"]);
    assert_eq!(child.foreign_keys[0].ref_columns, vec!["id"]);
    assert!(
        child
            .indexes
            .iter()
            .any(|i| i.name == "child_note" && i.columns == vec!["(expression)"])
    );

    assert!(
        !main.tables[1].columns[0].generated,
        "WITHOUT ROWID has no rowid alias"
    );
    assert_eq!(main.tables[3].kind, TableKind::View);
    assert_eq!(cat.schema("aux").unwrap().tables[0].name, "other");

    let ddl = c.ddl(&TableRef::new(None, "child")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE child"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX child_note"), "{ddl}");
    assert!(
        c.ddl(&TableRef::new(Some("aux"), "other"))
            .await
            .unwrap()
            .contains("other")
    );
    assert!(c.ddl(&TableRef::new(None, "missing")).await.is_err());
    assert_eq!(c.databases().await.unwrap().len(), 1);
}
