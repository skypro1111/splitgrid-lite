//! The catalog and DDL, read from pg_catalog. A fixed handful of queries
//! covers the whole database (one per kind of object, never one per table),
//! so a schema with thousands of tables loads in a few round trips.

use std::collections::HashMap;

use sql_core::{
    Catalog, ColumnInfo, ForeignKey, Index, Routine, RoutineKind, Schema, Table, TableKind,
    TableRef,
};
use tokio_postgres::Client;

use crate::pg_err;

/// Schemas Postgres reserves for itself (`pg_` prefixes can't be user-created).
const USER_SCHEMA: &str = r"n.nspname <> 'information_schema' and n.nspname not like 'pg\_%'";

pub async fn catalog(client: &Client, major: u32) -> anyhow::Result<Catalog> {
    let (db, search_path) = {
        let row = client
            .query_one(
                "select current_database()::text, current_schemas(false)::text[]",
                &[],
            )
            .await
            .map_err(pg_err)?;
        (row.get::<_, String>(0), row.get::<_, Vec<String>>(1))
    };

    let mut schemas: Vec<Schema> = client
        .query(&format!("select n.nspname::text from pg_namespace n where {USER_SCHEMA} order by n.nspname = 'public' desc, n.nspname"), &[])
        .await
        .map_err(pg_err)?
        .iter()
        .map(|r| Schema { name: r.get(0), ..Default::default() })
        .collect();
    let schema_idx: HashMap<String, usize> = schemas
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), i))
        .collect();

    // Relations, keyed by OID for the per-column/key/index rows below.
    let mut tables: HashMap<u32, (usize, usize)> = HashMap::new();
    let rels = client
        .query(
            &format!(
                "select c.oid, n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::float8, d.description
                 from pg_class c join pg_namespace n on n.oid = c.relnamespace
                 left join pg_description d on d.objoid = c.oid and d.classoid = 'pg_class'::regclass and d.objsubid = 0
                 where c.relkind in ('r', 'p', 'v', 'm', 'f', 'S') and {USER_SCHEMA}
                 order by n.nspname, c.relname"
            ),
            &[],
        )
        .await
        .map_err(pg_err)?;
    for r in &rels {
        let Some(&si) = schema_idx.get(r.get::<_, &str>(1)) else {
            continue;
        };
        let name: String = r.get(2);
        let kind = match r.get::<_, &str>(3) {
            "S" => {
                schemas[si].sequences.push(name);
                continue;
            }
            "v" => TableKind::View,
            "m" => TableKind::MaterializedView,
            "f" => TableKind::Foreign,
            _ => TableKind::Table,
        };
        // reltuples is -1 (14+) or 0 (older) before the first ANALYZE; views have none.
        let tuples: f64 = r.get(4);
        let row_estimate = (kind != TableKind::View && tuples >= 0.0).then_some(tuples as u64);
        tables.insert(r.get(0), (si, schemas[si].tables.len()));
        schemas[si].tables.push(Table {
            name,
            kind,
            row_estimate,
            comment: r.get(5),
            ..Default::default()
        });
    }
    let table = |oid: u32| tables.get(&oid).copied();

    let identity = if major >= 10 {
        "a.attidentity <> ''"
    } else {
        "false"
    };
    let generated = if major >= 12 {
        "a.attgenerated <> ''"
    } else {
        "false"
    };
    let cols = client
        .query(
            &format!(
                "select a.attrelid, a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull,
                        pg_get_expr(ad.adbin, ad.adrelid), {identity}, {generated}, d.description
                 from pg_attribute a
                 join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
                 left join pg_attrdef ad on ad.adrelid = a.attrelid and ad.adnum = a.attnum
                 left join pg_description d on d.objoid = a.attrelid and d.classoid = 'pg_class'::regclass and d.objsubid = a.attnum
                 where c.relkind in ('r', 'p', 'v', 'm', 'f') and a.attnum > 0 and not a.attisdropped and {USER_SCHEMA}
                 order by a.attrelid, a.attnum"
            ),
            &[],
        )
        .await
        .map_err(pg_err)?;
    for r in &cols {
        let Some((si, ti)) = table(r.get(0)) else {
            continue;
        };
        let default: Option<String> = r.get(4);
        let (is_identity, is_generated): (bool, bool) = (r.get(5), r.get(6));
        let serial = default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval("));
        schemas[si].tables[ti].columns.push(ColumnInfo {
            name: r.get(1),
            type_name: r.get(2),
            nullable: !r.get::<_, bool>(3),
            // A stored generated column's expression is not a default.
            default: if is_generated { None } else { default },
            generated: is_identity || is_generated || serial,
            comment: r.get(7),
        });
    }

    let keys = client
        .query(
            &format!(
                "select con.conrelid, con.contype::text, con.conname::text,
                        array(select a.attname::text from unnest(con.conkey) with ordinality k(num, ord)
                              join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.num order by k.ord),
                        fn.nspname::text, fc.relname::text,
                        array(select a.attname::text from unnest(con.confkey) with ordinality k(num, ord)
                              join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.num order by k.ord)
                 from pg_constraint con
                 join pg_class c on c.oid = con.conrelid join pg_namespace n on n.oid = c.relnamespace
                 left join pg_class fc on fc.oid = con.confrelid left join pg_namespace fn on fn.oid = fc.relnamespace
                 where con.contype in ('p', 'f') and {USER_SCHEMA}
                 order by con.conrelid, con.conname"
            ),
            &[],
        )
        .await
        .map_err(pg_err)?;
    for r in &keys {
        let Some((si, ti)) = table(r.get(0)) else {
            continue;
        };
        let t = &mut schemas[si].tables[ti];
        let columns: Vec<String> = r.get(3);
        if r.get::<_, &str>(1) == "p" {
            t.primary_key = columns;
        } else {
            t.foreign_keys.push(ForeignKey {
                name: r.get(2),
                columns,
                references: TableRef {
                    schema: r.get(4),
                    name: r.get::<_, Option<String>>(5).unwrap_or_default(),
                },
                ref_columns: r.get(6),
            });
        }
    }

    // Expression index columns have attnum 0; show their expression instead.
    let key_atts = if major >= 11 {
        "i.indnkeyatts"
    } else {
        "i.indnatts"
    };
    let indexes = client
        .query(
            &format!(
                "select i.indrelid, ic.relname::text, i.indisunique,
                        array(select coalesce(a.attname::text, pg_get_indexdef(i.indexrelid, k.ord::int, true))
                              from unnest(i.indkey::int2[]) with ordinality k(num, ord)
                              left join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.num
                              where k.ord <= {key_atts} order by k.ord)
                 from pg_index i
                 join pg_class ic on ic.oid = i.indexrelid
                 join pg_class c on c.oid = i.indrelid join pg_namespace n on n.oid = c.relnamespace
                 where {USER_SCHEMA}
                 order by i.indrelid, ic.relname"
            ),
            &[],
        )
        .await
        .map_err(pg_err)?;
    for r in &indexes {
        let Some((si, ti)) = table(r.get(0)) else {
            continue;
        };
        schemas[si].tables[ti].indexes.push(Index {
            name: r.get(1),
            unique: r.get(2),
            columns: r.get(3),
        });
    }

    let (kind, is_routine) = if major >= 11 {
        ("p.prokind::text", "p.prokind in ('f', 'p')")
    } else {
        ("'f'", "not p.proisagg and not p.proiswindow")
    };
    let routines = client
        .query(
            &format!(
                "select n.nspname::text, p.proname::text, {kind}, '(' || pg_get_function_identity_arguments(p.oid) || ')',
                        case when {kind} = 'p' then null else pg_get_function_result(p.oid) end
                 from pg_proc p join pg_namespace n on n.oid = p.pronamespace
                 where {is_routine} and {USER_SCHEMA}
                 order by n.nspname, p.proname"
            ),
            &[],
        )
        .await
        .map_err(pg_err)?;
    for r in &routines {
        let Some(&si) = schema_idx.get(r.get::<_, &str>(0)) else {
            continue;
        };
        schemas[si].routines.push(Routine {
            name: r.get(1),
            kind: if r.get::<_, &str>(2) == "p" {
                RoutineKind::Procedure
            } else {
                RoutineKind::Function
            },
            signature: r.get(3),
            returns: r.get(4),
        });
    }

    Ok(Catalog {
        database: db,
        search_path,
        schemas,
    })
}

/// CREATE TABLE (columns, constraints, indexes, comments) rebuilt from the
/// catalog, or CREATE VIEW with the server's own view definition.
pub async fn ddl(client: &Client, major: u32, t: &TableRef) -> anyhow::Result<String> {
    let rel = client
        .query_opt(
            "select c.oid, c.relkind::text, quote_ident(n.nspname) || '.' || quote_ident(c.relname),
                    quote_literal(obj_description(c.oid, 'pg_class'))
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where c.relname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f')
               and case when $2::text is null then pg_table_is_visible(c.oid) else n.nspname = $2 end",
            &[&t.name, &t.schema],
        )
        .await
        .map_err(pg_err)?
        .ok_or_else(|| anyhow::anyhow!("{} not found", t.name))?;
    let (oid, kind, qname, comment): (u32, &str, String, Option<String>) =
        (rel.get(0), rel.get(1), rel.get(2), rel.get(3));

    let mut out = if matches!(kind, "v" | "m") {
        let def: String = client
            .query_one("select pg_get_viewdef($1::oid, true)", &[&oid])
            .await
            .map_err(pg_err)?
            .get(0);
        let what = if kind == "m" {
            "MATERIALIZED VIEW"
        } else {
            "VIEW"
        };
        format!(
            "CREATE {what} {qname} AS\n{};\n",
            def.trim_end().trim_end_matches(';')
        )
    } else {
        table_ddl(client, major, oid, kind, &qname).await?
    };

    if let Some(c) = comment {
        let what = match kind {
            "v" => "VIEW",
            "m" => "MATERIALIZED VIEW",
            "f" => "FOREIGN TABLE",
            _ => "TABLE",
        };
        out.push_str(&format!("\nCOMMENT ON {what} {qname} IS {c};\n"));
    }
    let col_comments = client
        .query(
            "select quote_ident(a.attname), quote_literal(d.description)
             from pg_description d join pg_attribute a on a.attrelid = d.objoid and a.attnum = d.objsubid
             where d.objoid = $1 and d.classoid = 'pg_class'::regclass and d.objsubid > 0
             order by a.attnum",
            &[&oid],
        )
        .await
        .map_err(pg_err)?;
    for r in &col_comments {
        out.push_str(&format!(
            "COMMENT ON COLUMN {qname}.{} IS {};\n",
            r.get::<_, &str>(0),
            r.get::<_, &str>(1)
        ));
    }
    Ok(out)
}

async fn table_ddl(
    client: &Client,
    major: u32,
    oid: u32,
    kind: &str,
    qname: &str,
) -> anyhow::Result<String> {
    let identity = if major >= 10 {
        "a.attidentity::text"
    } else {
        "''"
    };
    let generated = if major >= 12 {
        "a.attgenerated::text"
    } else {
        "''"
    };
    let cols = client
        .query(
            &format!(
                "select quote_ident(a.attname), format_type(a.atttypid, a.atttypmod), a.attnotnull,
                        pg_get_expr(ad.adbin, ad.adrelid), {identity}, {generated}
                 from pg_attribute a left join pg_attrdef ad on ad.adrelid = a.attrelid and ad.adnum = a.attnum
                 where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped
                 order by a.attnum"
            ),
            &[&oid],
        )
        .await
        .map_err(pg_err)?;
    let mut lines: Vec<String> = cols
        .iter()
        .map(|r| {
            let mut line = format!("    {} {}", r.get::<_, &str>(0), r.get::<_, &str>(1));
            let default: Option<&str> = r.get(3);
            match (r.get::<_, &str>(4), r.get::<_, &str>(5), default) {
                (_, "s", Some(expr)) => {
                    line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"))
                }
                ("a", _, _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                ("d", _, _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                (_, _, Some(expr)) => line.push_str(&format!(" DEFAULT {expr}")),
                _ => {}
            }
            if r.get::<_, bool>(2) {
                line.push_str(" NOT NULL");
            }
            line
        })
        .collect();

    let constraints = client
        .query(
            "select quote_ident(conname), pg_get_constraintdef(oid, true) from pg_constraint
             where conrelid = $1 and contype in ('p', 'u', 'c', 'f', 'x')
             order by array_position(array['p', 'u', 'c', 'f', 'x']::\"char\"[], contype), conname",
            &[&oid],
        )
        .await
        .map_err(pg_err)?;
    lines.extend(constraints.iter().map(|r| {
        format!(
            "    CONSTRAINT {} {}",
            r.get::<_, &str>(0),
            r.get::<_, &str>(1)
        )
    }));

    let what = if kind == "f" {
        "FOREIGN TABLE"
    } else {
        "TABLE"
    };
    let mut out = format!("CREATE {what} {qname} (\n{}\n)", lines.join(",\n"));
    if kind == "p" {
        let key: String = client
            .query_one("select pg_get_partkeydef($1::oid)", &[&oid])
            .await
            .map_err(pg_err)?
            .get(0);
        out.push_str(&format!("\nPARTITION BY {key}"));
    }
    if kind == "f" {
        let server = client
            .query_opt("select quote_ident(s.srvname) from pg_foreign_table f join pg_foreign_server s on s.oid = f.ftserver where f.ftrelid = $1", &[&oid])
            .await
            .map_err(pg_err)?;
        if let Some(s) = server {
            out.push_str(&format!("\nSERVER {}", s.get::<_, &str>(0)));
        }
    }
    out.push_str(";\n");

    // Indexes that back a constraint are already in the CREATE TABLE.
    let indexes = client
        .query(
            "select pg_get_indexdef(i.indexrelid) from pg_index i join pg_class ic on ic.oid = i.indexrelid
             where i.indrelid = $1
               and not exists (select 1 from pg_constraint c where c.conindid = i.indexrelid and c.contype in ('p', 'u', 'x'))
             order by ic.relname",
            &[&oid],
        )
        .await
        .map_err(pg_err)?;
    if !indexes.is_empty() {
        out.push('\n');
        for r in &indexes {
            out.push_str(&format!("{};\n", r.get::<_, &str>(0)));
        }
    }
    Ok(out)
}
