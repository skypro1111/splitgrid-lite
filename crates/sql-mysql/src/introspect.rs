//! The catalog from information_schema: six queries for the whole database
//! (not one per table), each row read as text and assembled by `build`.
//!
//! MySQL has no schemas below the database, so — like the Electron
//! introspector — the current database is the one schema. Without a current
//! database (`USE` never ran) every non-system database becomes a schema.

use std::collections::HashMap;

use sql_core::{Catalog, ColumnInfo, ForeignKey, Index, Routine, RoutineKind, Schema, Table, TableKind, TableRef};

pub const SYSTEM_DATABASES: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

pub type Rows = Vec<Vec<Option<String>>>;

/// The six queries; `{scope}` filters by database on the named column.
pub const TABLES: &str = "select TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT \
     from information_schema.TABLES where {scope:TABLE_SCHEMA} order by TABLE_SCHEMA, TABLE_NAME";
pub const COLUMNS: &str = "select TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT \
     from information_schema.COLUMNS where {scope:TABLE_SCHEMA} order by TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION";
pub const INDEXES: &str = "select TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME \
     from information_schema.STATISTICS where {scope:TABLE_SCHEMA} order by TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
pub const FOREIGN_KEYS: &str = "select TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, \
     REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
     from information_schema.KEY_COLUMN_USAGE where {scope:TABLE_SCHEMA} and REFERENCED_TABLE_NAME is not null \
     order by TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION";
pub const ROUTINES: &str = "select ROUTINE_SCHEMA, ROUTINE_NAME, ROUTINE_TYPE, DTD_IDENTIFIER \
     from information_schema.ROUTINES where {scope:ROUTINE_SCHEMA} order by ROUTINE_SCHEMA, ROUTINE_NAME";
pub const PARAMETERS: &str = "select SPECIFIC_SCHEMA, SPECIFIC_NAME, ROUTINE_TYPE, PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER \
     from information_schema.PARAMETERS where {scope:SPECIFIC_SCHEMA} and ORDINAL_POSITION > 0 \
     order by SPECIFIC_SCHEMA, SPECIFIC_NAME, ORDINAL_POSITION";

/// Expands `{scope:COL}` to a filter on one database (bound as `?`) or on
/// every non-system one.
pub fn scoped(sql: &str, database: Option<&str>) -> String {
    let start = sql.find("{scope:").expect("query has a scope placeholder");
    let end = start + sql[start..].find('}').expect("closed placeholder");
    let col = &sql[start + 7..end];
    let filter = match database {
        Some(_) => format!("{col} = ?"),
        None => format!("{col} not in ({})", SYSTEM_DATABASES.map(|d| format!("'{d}'")).join(", ")),
    };
    format!("{}{}{}", &sql[..start], filter, &sql[end + 1..])
}

pub struct Fetched {
    pub tables: Rows,
    pub columns: Rows,
    pub indexes: Rows,
    pub foreign_keys: Rows,
    pub routines: Rows,
    pub parameters: Rows,
}

fn s(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|v| !v.is_empty())
}

/// Tables of all schemas, found by (schema, table).
struct Builder {
    schemas: Vec<Schema>,
    by_name: HashMap<String, usize>,
    tables: HashMap<(String, String), (usize, usize)>,
}

impl Builder {
    fn schema(&mut self, name: &str) -> &mut Schema {
        let i = *self.by_name.entry(name.to_owned()).or_insert_with(|| {
            self.schemas.push(Schema { name: name.to_owned(), ..Default::default() });
            self.schemas.len() - 1
        });
        &mut self.schemas[i]
    }

    fn add_table(&mut self, schema: &str, table: Table) {
        let si = {
            self.schema(schema);
            self.by_name[schema]
        };
        let ti = self.schemas[si].tables.len();
        self.tables.insert((schema.to_owned(), table.name.clone()), (si, ti));
        self.schemas[si].tables.push(table);
    }

    fn table(&mut self, schema: &str, name: &str) -> Option<&mut Table> {
        let &(si, ti) = self.tables.get(&(schema.to_owned(), name.to_owned()))?;
        Some(&mut self.schemas[si].tables[ti])
    }
}

/// `generated` = leave it out of INSERTs: AUTO_INCREMENT and generated columns
/// (MySQL: `VIRTUAL GENERATED`/`STORED GENERATED`; MariaDB: `VIRTUAL`/`PERSISTENT`/`STORED`).
/// MySQL 8's `DEFAULT_GENERATED` only marks an expression default, so it is not one.
pub fn is_generated(extra: &str) -> bool {
    let e = extra.to_ascii_lowercase();
    ["auto_increment", "virtual", "stored", "persistent"].iter().any(|k| e.contains(k))
}

pub fn build(database: Option<&str>, mariadb: bool, f: Fetched) -> Catalog {
    let mut b = Builder { schemas: Vec::new(), by_name: HashMap::new(), tables: HashMap::new() };
    if let Some(db) = database {
        b.schema(db);
    }

    for r in &f.tables {
        let (schema, name, ty) = (s(r, 0), s(r, 1), s(r, 2));
        if ty == "SEQUENCE" {
            b.schema(&schema).sequences.push(name);
            continue;
        }
        let kind = if ty.contains("VIEW") { TableKind::View } else { TableKind::Table };
        let table = Table {
            name,
            kind,
            row_estimate: if kind == TableKind::View { None } else { opt(r, 3).and_then(|v| v.parse().ok()) },
            // MySQL reports a view's comment as the word "VIEW".
            comment: if kind == TableKind::View { None } else { opt(r, 4) },
            ..Default::default()
        };
        b.add_table(&schema, table);
    }

    for r in &f.columns {
        let default = opt(r, 5).filter(|d| !(mariadb && d == "NULL"));
        let column = ColumnInfo {
            name: s(r, 2),
            type_name: s(r, 3),
            nullable: s(r, 4) == "YES",
            default,
            generated: is_generated(&s(r, 6)),
            comment: opt(r, 7),
        };
        if let Some(t) = b.table(&s(r, 0), &s(r, 1)) {
            t.columns.push(column);
        }
    }

    for r in &f.indexes {
        let Some(t) = b.table(&s(r, 0), &s(r, 1)) else { continue };
        let name = s(r, 2);
        // A functional index part (MySQL 8) has no column name.
        let column = opt(r, 4).unwrap_or_else(|| "(expression)".into());
        if name == "PRIMARY" {
            t.primary_key.push(column.clone());
        }
        match t.indexes.last_mut() {
            Some(ix) if ix.name == name => ix.columns.push(column),
            _ => t.indexes.push(Index { name, columns: vec![column], unique: s(r, 3) == "0" }),
        }
    }

    for r in &f.foreign_keys {
        let Some(t) = b.table(&s(r, 0), &s(r, 1)) else { continue };
        let name = s(r, 2);
        match t.foreign_keys.last_mut() {
            Some(fk) if fk.name == name => {
                fk.columns.push(s(r, 3));
                fk.ref_columns.push(s(r, 6));
            }
            _ => t.foreign_keys.push(ForeignKey {
                name,
                columns: vec![s(r, 3)],
                references: TableRef { schema: opt(r, 4), name: s(r, 5) },
                ref_columns: vec![s(r, 6)],
            }),
        }
    }

    let mut params: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for r in &f.parameters {
        let mode = opt(r, 3).map(|m| format!("{m} ")).unwrap_or_default();
        params.entry((s(r, 0), s(r, 1), s(r, 2))).or_default().push(format!("{mode}{} {}", s(r, 4), s(r, 5)));
    }
    for r in &f.routines {
        let (schema, name, ty) = (s(r, 0), s(r, 1), s(r, 2));
        let args = params.remove(&(schema.clone(), name.clone(), ty.clone())).unwrap_or_default();
        let kind = if ty == "PROCEDURE" { RoutineKind::Procedure } else { RoutineKind::Function };
        let routine = Routine {
            name,
            kind,
            signature: format!("({})", args.join(", ")),
            returns: if kind == RoutineKind::Function { opt(r, 3) } else { None },
        };
        b.schema(&schema).routines.push(routine);
    }

    Catalog {
        database: database.unwrap_or_default().to_owned(),
        search_path: database.map(|d| vec![d.to_owned()]).unwrap_or_default(),
        schemas: b.schemas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[Option<&str>]]) -> Rows {
        data.iter().map(|r| r.iter().map(|c| c.map(str::to_owned)).collect()).collect()
    }

    #[test]
    fn scope_filters_one_database_or_all_user_ones() {
        assert_eq!(scoped("x where {scope:TABLE_SCHEMA} order", Some("app")), "x where TABLE_SCHEMA = ? order");
        let all = scoped("where {scope:ROUTINE_SCHEMA}", None);
        assert!(all.starts_with("where ROUTINE_SCHEMA not in ('information_schema', 'mysql'"), "{all}");
    }

    #[test]
    fn generated_columns_but_not_expression_defaults() {
        assert!(is_generated("auto_increment"));
        assert!(is_generated("VIRTUAL GENERATED"));
        assert!(is_generated("PERSISTENT"));
        assert!(!is_generated("DEFAULT_GENERATED on update CURRENT_TIMESTAMP"));
        assert!(!is_generated(""));
    }

    #[test]
    fn builds_tables_keys_indexes_and_routines() {
        let f = Fetched {
            tables: rows(&[
                &[Some("app"), Some("orders"), Some("BASE TABLE"), Some("120"), Some("All orders")],
                &[Some("app"), Some("users"), Some("BASE TABLE"), Some("3"), Some("")],
                &[Some("app"), Some("v_users"), Some("VIEW"), None, Some("VIEW")],
                &[Some("app"), Some("seq1"), Some("SEQUENCE"), Some("1"), Some("")],
            ]),
            columns: rows(&[
                &[Some("app"), Some("orders"), Some("id"), Some("bigint unsigned"), Some("NO"), None, Some("auto_increment"), Some("")],
                &[Some("app"), Some("orders"), Some("user_id"), Some("int"), Some("NO"), None, Some(""), Some("buyer")],
                &[Some("app"), Some("orders"), Some("note"), Some("varchar(50)"), Some("YES"), Some("NULL"), Some(""), Some("")],
                &[Some("app"), Some("users"), Some("id"), Some("int"), Some("NO"), None, Some("auto_increment"), Some("")],
            ]),
            indexes: rows(&[
                &[Some("app"), Some("orders"), Some("PRIMARY"), Some("0"), Some("id")],
                &[Some("app"), Some("orders"), Some("ix_user"), Some("1"), Some("user_id")],
                &[Some("app"), Some("orders"), Some("ix_user"), Some("1"), Some("note")],
                &[Some("app"), Some("orders"), Some("ix_fn"), Some("1"), None],
            ]),
            foreign_keys: rows(&[&[Some("app"), Some("orders"), Some("fk_user"), Some("user_id"), Some("app"), Some("users"), Some("id")]]),
            routines: rows(&[
                &[Some("app"), Some("add"), Some("FUNCTION"), Some("int")],
                &[Some("app"), Some("add"), Some("PROCEDURE"), None],
            ]),
            parameters: rows(&[
                &[Some("app"), Some("add"), Some("FUNCTION"), None, Some("a"), Some("int")],
                &[Some("app"), Some("add"), Some("FUNCTION"), None, Some("b"), Some("int")],
                &[Some("app"), Some("add"), Some("PROCEDURE"), Some("OUT"), Some("total"), Some("bigint")],
            ]),
        };
        let cat = build(Some("app"), true, f);
        assert_eq!(cat.search_path, vec!["app"]);
        let app = cat.schema("app").unwrap();
        assert_eq!(app.sequences, vec!["seq1"]);
        let (_, orders) = cat.table(&TableRef::new(None, "orders")).unwrap();
        assert_eq!(orders.row_estimate, Some(120));
        assert_eq!(orders.comment.as_deref(), Some("All orders"));
        assert_eq!(orders.primary_key, vec!["id"]);
        assert!(orders.columns[0].generated);
        assert_eq!(orders.columns[1].comment.as_deref(), Some("buyer"));
        // MariaDB spells "no default" as the bare word NULL.
        assert_eq!(orders.columns[2].default, None);
        assert!(orders.columns[2].nullable);
        assert_eq!(orders.indexes.len(), 3);
        assert_eq!(orders.indexes[1].columns, vec!["user_id", "note"]);
        assert!(orders.indexes[0].unique && !orders.indexes[1].unique);
        assert_eq!(orders.indexes[2].columns, vec!["(expression)"]);
        assert_eq!(orders.foreign_keys[0].references, TableRef::new(Some("app"), "users"));
        let (_, view) = cat.table(&TableRef::new(None, "v_users")).unwrap();
        assert_eq!((view.kind, view.comment.clone(), view.row_estimate), (TableKind::View, None, None));
        assert_eq!(app.routines[0].signature, "(a int, b int)");
        assert_eq!(app.routines[0].returns.as_deref(), Some("int"));
        assert_eq!(app.routines[1].kind, RoutineKind::Procedure);
        assert_eq!(app.routines[1].signature, "(OUT total bigint)");
    }

    #[test]
    fn without_a_current_database_every_user_database_is_a_schema() {
        let f = Fetched {
            tables: rows(&[&[Some("a"), Some("t"), Some("BASE TABLE"), None, None], &[Some("b"), Some("t"), Some("BASE TABLE"), None, None]]),
            columns: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            routines: vec![],
            parameters: vec![],
        };
        let cat = build(None, false, f);
        assert!(cat.search_path.is_empty());
        assert_eq!(cat.schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }
}
