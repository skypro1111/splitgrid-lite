//! The catalog from the sys.* views: one query per kind of object for the
//! whole database, each row read as text and assembled by `build`. The same
//! queries narrowed to one object (`{filter}`) feed the CREATE TABLE that
//! `ddl` reconstructs, since SQL Server has no SHOW CREATE.

use std::collections::HashMap;

use sql_core::{Catalog, ColumnInfo, ForeignKey, Index, Routine, RoutineKind, Schema, Table, TableKind, TableRef};

pub type Rows = Vec<Vec<Option<String>>>;

pub const CONTEXT: &str = "select schema_name(), db_name()";

/// Fixed database-role schemas (db_owner…) have ids 16384–16393.
pub const SCHEMAS: &str = "select s.name from sys.schemas s \
     where s.name not in ('sys', 'INFORMATION_SCHEMA', 'guest') and s.schema_id not between 16384 and 16399 \
     order by s.name";

pub const TABLES: &str = "select s.name, o.name, o.type, \
       (select sum(p.rows) from sys.partitions p where p.object_id = o.object_id and p.index_id in (0, 1)), \
       cast(ep.value as nvarchar(4000)) \
     from sys.objects o \
     join sys.schemas s on s.schema_id = o.schema_id \
     left join sys.extended_properties ep on ep.class = 1 and ep.major_id = o.object_id and ep.minor_id = 0 and ep.name = 'MS_Description' \
     where o.type in ('U', 'V') and o.is_ms_shipped = 0 {filter} \
     order by s.name, o.name";

pub const COLUMNS: &str = "select s.name, o.name, c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
       object_definition(c.default_object_id), c.is_identity, c.is_computed, cast(ep.value as nvarchar(4000)), \
       cast(ic.seed_value as nvarchar(40)), cast(ic.increment_value as nvarchar(40)), cc.definition \
     from sys.columns c \
     join sys.objects o on o.object_id = c.object_id \
     join sys.schemas s on s.schema_id = o.schema_id \
     join sys.types t on t.user_type_id = c.user_type_id \
     left join sys.identity_columns ic on ic.object_id = c.object_id and ic.column_id = c.column_id \
     left join sys.computed_columns cc on cc.object_id = c.object_id and cc.column_id = c.column_id \
     left join sys.extended_properties ep on ep.class = 1 and ep.major_id = c.object_id and ep.minor_id = c.column_id and ep.name = 'MS_Description' \
     where o.type in ('U', 'V') and o.is_ms_shipped = 0 {filter} \
     order by s.name, o.name, c.column_id";

pub const INDEXES: &str = "select s.name, o.name, i.name, i.is_unique, i.is_primary_key, c.name, i.type_desc, i.is_unique_constraint, ic.is_descending_key \
     from sys.indexes i \
     join sys.objects o on o.object_id = i.object_id \
     join sys.schemas s on s.schema_id = o.schema_id \
     join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id and ic.is_included_column = 0 \
     join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
     where i.name is not null and o.type in ('U', 'V') and o.is_ms_shipped = 0 {filter} \
     order by s.name, o.name, i.name, ic.key_ordinal";

pub const FOREIGN_KEYS: &str = "select s.name, o.name, fk.name, pc.name, rs.name, ro.name, rc.name, \
       fk.delete_referential_action_desc, fk.update_referential_action_desc \
     from sys.foreign_keys fk \
     join sys.objects o on o.object_id = fk.parent_object_id \
     join sys.schemas s on s.schema_id = o.schema_id \
     join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
     join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
     join sys.objects ro on ro.object_id = fk.referenced_object_id \
     join sys.schemas rs on rs.schema_id = ro.schema_id \
     join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
     where o.is_ms_shipped = 0 {filter} \
     order by s.name, o.name, fk.name, fkc.constraint_column_id";

pub const ROUTINES: &str = "select s.name, o.name, o.type, p.name, tp.name, p.max_length, p.precision, p.scale, p.is_output, p.parameter_id \
     from sys.objects o \
     join sys.schemas s on s.schema_id = o.schema_id \
     left join sys.parameters p on p.object_id = o.object_id \
     left join sys.types tp on tp.user_type_id = p.user_type_id \
     where o.type in ('P', 'PC', 'FN', 'FS', 'IF', 'TF', 'FT') and o.is_ms_shipped = 0 \
     order by s.name, o.name, p.parameter_id";

pub const SEQUENCES: &str = "select s.name, q.name from sys.sequences q join sys.schemas s on s.schema_id = q.schema_id order by s.name, q.name";

/// For `ddl`: what kind of object, and a module's own source.
pub const OBJECT: &str = "select o.type, object_definition(o.object_id) from sys.objects o where o.object_id = object_id(@P1)";

pub const CHECKS: &str = "select name, definition from sys.check_constraints where parent_object_id = object_id(@P1) order by name";

/// The whole database, or (for `ddl`) the one object named by `@P1`.
pub fn filtered(sql: &str, one_object: bool) -> String {
    sql.replace("{filter}", if one_object { "and o.object_id = object_id(@P1)" } else { "" })
}

fn s(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|v| !v.is_empty())
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    matches!(s(row, i).as_str(), "1" | "true")
}

fn num(row: &[Option<String>], i: usize) -> i64 {
    s(row, i).parse().unwrap_or(0)
}

/// `nvarchar(50)`, `varbinary(max)`, `decimal(10,2)`, `datetime2(3)` from sys.columns.
pub fn full_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let len = |n: i64| if max_length == -1 { "max".to_owned() } else { n.to_string() };
    match name {
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", len(max_length)),
        "nvarchar" | "nchar" => format!("{name}({})", len(max_length / 2)),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_owned(),
    }
}

pub fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// A column with what CREATE TABLE needs beyond `ColumnInfo`.
pub struct Col {
    pub schema: String,
    pub table: String,
    pub info: ColumnInfo,
    /// (seed, increment) of an IDENTITY column.
    pub identity: Option<(String, String)>,
    /// The expression of a computed column.
    pub computed: Option<String>,
}

pub fn columns(rows: &Rows) -> Vec<Col> {
    rows.iter()
        .map(|r| {
            let type_name = full_type(&s(r, 3), num(r, 4), num(r, 5), num(r, 6));
            let identity = flag(r, 9).then(|| (opt(r, 12).unwrap_or_else(|| "1".into()), opt(r, 13).unwrap_or_else(|| "1".into())));
            let computed = flag(r, 10).then(|| opt(r, 14)).flatten();
            // rowversion (sys name `timestamp`) is filled by the server too.
            let generated = identity.is_some() || flag(r, 10) || s(r, 3) == "timestamp";
            Col {
                schema: s(r, 0),
                table: s(r, 1),
                info: ColumnInfo { name: s(r, 2), type_name, nullable: flag(r, 7), default: opt(r, 8), generated, comment: opt(r, 11) },
                identity,
                computed,
            }
        })
        .collect()
}

/// An index with what CREATE TABLE/INDEX needs beyond `Index`.
pub struct Ix {
    pub schema: String,
    pub table: String,
    pub index: Index,
    pub primary: bool,
    pub constraint: bool,
    pub kind: String,
    /// Column list with ASC/DESC, quoted.
    pub keys: Vec<String>,
}

pub fn indexes(rows: &Rows) -> Vec<Ix> {
    let mut out: Vec<Ix> = Vec::new();
    for r in rows {
        let key = format!("{}{}", quote(&s(r, 5)), if flag(r, 8) { " DESC" } else { "" });
        match out.last_mut() {
            Some(ix) if ix.schema == s(r, 0) && ix.table == s(r, 1) && ix.index.name == s(r, 2) => {
                ix.index.columns.push(s(r, 5));
                ix.keys.push(key);
            }
            _ => out.push(Ix {
                schema: s(r, 0),
                table: s(r, 1),
                index: Index { name: s(r, 2), columns: vec![s(r, 5)], unique: flag(r, 3) },
                primary: flag(r, 4),
                constraint: flag(r, 7),
                kind: s(r, 6),
                keys: vec![key],
            }),
        }
    }
    out
}

pub struct Fk {
    pub schema: String,
    pub table: String,
    pub fk: ForeignKey,
    pub on_delete: String,
    pub on_update: String,
}

pub fn foreign_keys(rows: &Rows) -> Vec<Fk> {
    let mut out: Vec<Fk> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(f) if f.schema == s(r, 0) && f.table == s(r, 1) && f.fk.name == s(r, 2) => {
                f.fk.columns.push(s(r, 3));
                f.fk.ref_columns.push(s(r, 6));
            }
            _ => out.push(Fk {
                schema: s(r, 0),
                table: s(r, 1),
                fk: ForeignKey {
                    name: s(r, 2),
                    columns: vec![s(r, 3)],
                    references: TableRef { schema: opt(r, 4), name: s(r, 5) },
                    ref_columns: vec![s(r, 6)],
                },
                on_delete: s(r, 7),
                on_update: s(r, 8),
            }),
        }
    }
    out
}

pub struct Fetched {
    pub schemas: Rows,
    pub tables: Rows,
    pub columns: Rows,
    pub indexes: Rows,
    pub foreign_keys: Rows,
    pub routines: Rows,
    pub sequences: Rows,
}

pub fn build(database: &str, default_schema: &str, f: Fetched) -> Catalog {
    let mut schemas: Vec<Schema> = f.schemas.iter().map(|r| Schema { name: s(r, 0), ..Default::default() }).collect();
    let mut by_name: HashMap<String, usize> = schemas.iter().enumerate().map(|(i, s)| (s.name.clone(), i)).collect();
    let mut schema_ix = |name: &str, schemas: &mut Vec<Schema>| {
        *by_name.entry(name.to_owned()).or_insert_with(|| {
            schemas.push(Schema { name: name.to_owned(), ..Default::default() });
            schemas.len() - 1
        })
    };
    let mut tables: HashMap<(String, String), (usize, usize)> = HashMap::new();

    for r in &f.tables {
        let si = schema_ix(&s(r, 0), &mut schemas);
        let view = s(r, 2).trim() == "V";
        let table = Table {
            name: s(r, 1),
            kind: if view { TableKind::View } else { TableKind::Table },
            row_estimate: if view { None } else { opt(r, 3).and_then(|n| n.parse().ok()) },
            comment: opt(r, 4),
            ..Default::default()
        };
        tables.insert((s(r, 0), table.name.clone()), (si, schemas[si].tables.len()));
        schemas[si].tables.push(table);
    }
    let table = |schema: &str, name: &str| tables.get(&(schema.to_owned(), name.to_owned())).copied();

    for c in columns(&f.columns) {
        if let Some((si, ti)) = table(&c.schema, &c.table) {
            schemas[si].tables[ti].columns.push(c.info);
        }
    }
    for ix in indexes(&f.indexes) {
        if let Some((si, ti)) = table(&ix.schema, &ix.table) {
            let t = &mut schemas[si].tables[ti];
            if ix.primary {
                t.primary_key = ix.index.columns.clone();
            }
            t.indexes.push(ix.index);
        }
    }
    for fk in foreign_keys(&f.foreign_keys) {
        if let Some((si, ti)) = table(&fk.schema, &fk.table) {
            schemas[si].tables[ti].foreign_keys.push(fk.fk);
        }
    }

    // Parameter 0 of a scalar function is its return type.
    let mut routines: Vec<(String, Routine, Vec<String>)> = Vec::new();
    for r in &f.routines {
        let (schema, name, ty) = (s(r, 0), s(r, 1), s(r, 2).trim().to_owned());
        if !matches!(routines.last(), Some((sc, rt, _)) if *sc == schema && rt.name == name) {
            let kind = if matches!(ty.as_str(), "P" | "PC") { RoutineKind::Procedure } else { RoutineKind::Function };
            let returns = matches!(ty.as_str(), "IF" | "TF" | "FT").then(|| "TABLE".to_owned());
            routines.push((schema, Routine { name, kind, signature: String::new(), returns }, Vec::new()));
        }
        let Some(param) = opt(r, 3).or_else(|| (opt(r, 9).as_deref() == Some("0")).then(String::new)) else { continue };
        let ty = full_type(&s(r, 4), num(r, 5), num(r, 6), num(r, 7));
        let (_, routine, params) = routines.last_mut().expect("pushed above");
        if param.is_empty() {
            routine.returns = Some(ty);
        } else {
            params.push(format!("{param} {ty}{}", if flag(r, 8) { " OUTPUT" } else { "" }));
        }
    }
    for (schema, mut routine, params) in routines {
        routine.signature = format!("({})", params.join(", "));
        let si = schema_ix(&schema, &mut schemas);
        schemas[si].routines.push(routine);
    }
    for r in &f.sequences {
        let si = schema_ix(&s(r, 0), &mut schemas);
        schemas[si].sequences.push(s(r, 1));
    }

    Catalog { database: database.to_owned(), search_path: vec![default_schema.to_owned()], schemas }
}

/// A CREATE TABLE as SSMS would script it: columns (identity, computed,
/// defaults), key/unique/check/foreign-key constraints, then the other indexes.
pub fn create_table(target: &TableRef, cols: &[Col], ixs: &[Ix], fks: &[Fk], checks: &Rows) -> String {
    let name = format!("{}.{}", quote(target.schema.as_deref().unwrap_or("dbo")), quote(&target.name));
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let col = quote(&c.info.name);
            if let Some(expr) = &c.computed {
                return format!("{col} AS {expr}");
            }
            let mut line = format!("{col} {}", c.info.type_name);
            if let Some((seed, step)) = &c.identity {
                line.push_str(&format!(" IDENTITY({seed},{step})"));
            }
            line.push_str(if c.info.nullable { " NULL" } else { " NOT NULL" });
            if let Some(d) = &c.info.default {
                line.push_str(&format!(" DEFAULT {d}"));
            }
            line
        })
        .collect();
    for ix in ixs.iter().filter(|ix| ix.primary || ix.constraint) {
        let what = if ix.primary { "PRIMARY KEY" } else { "UNIQUE" };
        lines.push(format!("CONSTRAINT {} {what} {} ({})", quote(&ix.index.name), ix.kind, ix.keys.join(", ")));
    }
    for r in checks {
        lines.push(format!("CONSTRAINT {} CHECK {}", quote(&s(r, 0)), s(r, 1)));
    }
    for f in fks {
        let refs = &f.fk.references;
        let mut line = format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{} ({})",
            quote(&f.fk.name),
            f.fk.columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "),
            quote(refs.schema.as_deref().unwrap_or("dbo")),
            quote(&refs.name),
            f.fk.ref_columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "),
        );
        for (verb, action) in [("DELETE", &f.on_delete), ("UPDATE", &f.on_update)] {
            if !action.is_empty() && action != "NO_ACTION" {
                line.push_str(&format!(" ON {verb} {}", action.replace('_', " ")));
            }
        }
        lines.push(line);
    }
    let mut ddl = format!("CREATE TABLE {name} (\n    {}\n);", lines.join(",\n    "));
    for ix in ixs.iter().filter(|ix| !ix.primary && !ix.constraint) {
        ddl.push_str(&format!(
            "\nCREATE {}{} INDEX {} ON {name} ({});",
            if ix.index.unique { "UNIQUE " } else { "" },
            ix.kind,
            quote(&ix.index.name),
            ix.keys.join(", ")
        ));
    }
    ddl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[Option<&str>]]) -> Rows {
        data.iter().map(|r| r.iter().map(|c| c.map(str::to_owned)).collect()).collect()
    }

    #[test]
    fn full_types_read_like_ssms() {
        assert_eq!(full_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(full_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(full_type("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(full_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(full_type("datetime2", 8, 27, 3), "datetime2(3)");
        assert_eq!(full_type("int", 4, 10, 0), "int");
        assert_eq!(quote("a]b"), "[a]]b]");
    }

    fn sample_columns() -> Rows {
        rows(&[
            &[Some("dbo"), Some("orders"), Some("id"), Some("int"), Some("4"), Some("10"), Some("0"), Some("0"), None, Some("1"), Some("0"), None, Some("100"), Some("5"), None],
            &[Some("dbo"), Some("orders"), Some("name"), Some("nvarchar"), Some("100"), Some("0"), Some("0"), Some("1"), Some("(N'x')"), Some("0"), Some("0"), Some("Who"), None, None, None],
            &[Some("dbo"), Some("orders"), Some("total"), Some("decimal"), Some("9"), Some("10"), Some("2"), Some("1"), None, Some("0"), Some("1"), None, None, None, Some("([qty]*[price])")],
            &[Some("dbo"), Some("orders"), Some("ver"), Some("timestamp"), Some("8"), Some("0"), Some("0"), Some("0"), None, Some("0"), Some("0"), None, None, None, None],
        ])
    }

    fn sample_indexes() -> Rows {
        rows(&[
            &[Some("dbo"), Some("orders"), Some("ix_name"), Some("0"), Some("0"), Some("name"), Some("NONCLUSTERED"), Some("0"), Some("1")],
            &[Some("dbo"), Some("orders"), Some("PK_orders"), Some("1"), Some("1"), Some("id"), Some("CLUSTERED"), Some("0"), Some("0")],
        ])
    }

    fn sample_fks() -> Rows {
        rows(&[&[Some("dbo"), Some("orders"), Some("FK_o_u"), Some("name"), Some("app"), Some("users"), Some("login"), Some("CASCADE"), Some("NO_ACTION")]])
    }

    #[test]
    fn builds_the_catalog() {
        let f = Fetched {
            schemas: rows(&[&[Some("app")], &[Some("dbo")]]),
            tables: rows(&[
                &[Some("dbo"), Some("orders"), Some("U "), Some("1200"), Some("All orders")],
                &[Some("dbo"), Some("v_orders"), Some("V "), None, None],
            ]),
            columns: sample_columns(),
            indexes: sample_indexes(),
            foreign_keys: sample_fks(),
            routines: rows(&[
                &[Some("dbo"), Some("add"), Some("FN"), Some(""), Some("int"), Some("4"), Some("10"), Some("0"), Some("0"), Some("0")],
                &[Some("dbo"), Some("add"), Some("FN"), Some("@a"), Some("int"), Some("4"), Some("10"), Some("0"), Some("0"), Some("1")],
                &[Some("dbo"), Some("proc1"), Some("P "), Some("@out"), Some("nvarchar"), Some("20"), Some("0"), Some("0"), Some("1"), Some("1")],
                &[Some("dbo"), Some("noargs"), Some("P "), None, None, None, None, None, None, None],
                &[Some("dbo"), Some("tvf"), Some("IF"), Some("@d"), Some("date"), Some("3"), Some("10"), Some("0"), Some("0"), Some("1")],
            ]),
            sequences: rows(&[&[Some("dbo"), Some("seq")]]),
        };
        let cat = build("shop", "dbo", f);
        assert_eq!(cat.search_path, vec!["dbo"]);
        assert_eq!(cat.schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["app", "dbo"]);
        let (_, t) = cat.table(&TableRef::new(None, "orders")).unwrap();
        assert_eq!((t.row_estimate, t.comment.as_deref()), (Some(1200), Some("All orders")));
        assert_eq!(t.primary_key, vec!["id"]);
        assert_eq!(t.columns.iter().map(|c| c.generated).collect::<Vec<_>>(), vec![true, false, true, true]);
        assert_eq!(t.columns[1].type_name, "nvarchar(50)");
        assert_eq!(t.columns[1].comment.as_deref(), Some("Who"));
        assert_eq!(t.indexes.len(), 2);
        assert_eq!(t.foreign_keys[0].references, TableRef::new(Some("app"), "users"));
        let (_, v) = cat.table(&TableRef::new(Some("dbo"), "v_orders")).unwrap();
        assert_eq!(v.kind, TableKind::View);
        let dbo = cat.schema("dbo").unwrap();
        let sig = |n: &str| dbo.routines.iter().find(|r| r.name == n).map(|r| (r.signature.clone(), r.returns.clone())).unwrap();
        assert_eq!(sig("add"), ("(@a int)".into(), Some("int".into())));
        assert_eq!(sig("proc1"), ("(@out nvarchar(10) OUTPUT)".into(), None));
        assert_eq!(sig("noargs"), ("()".into(), None));
        assert_eq!(sig("tvf"), ("(@d date)".into(), Some("TABLE".into())));
        assert_eq!(dbo.sequences, vec!["seq"]);
    }

    #[test]
    fn reconstructs_create_table() {
        let checks = rows(&[&[Some("CK_total"), Some("([total]>=(0))")]]);
        let ddl = create_table(
            &TableRef::new(Some("dbo"), "orders"),
            &columns(&sample_columns()),
            &indexes(&sample_indexes()),
            &foreign_keys(&sample_fks()),
            &checks,
        );
        assert_eq!(
            ddl,
            "CREATE TABLE [dbo].[orders] (\n    \
             [id] int IDENTITY(100,5) NOT NULL,\n    \
             [name] nvarchar(50) NULL DEFAULT (N'x'),\n    \
             [total] AS ([qty]*[price]),\n    \
             [ver] timestamp NOT NULL,\n    \
             CONSTRAINT [PK_orders] PRIMARY KEY CLUSTERED ([id]),\n    \
             CONSTRAINT [CK_total] CHECK ([total]>=(0)),\n    \
             CONSTRAINT [FK_o_u] FOREIGN KEY ([name]) REFERENCES [app].[users] ([login]) ON DELETE CASCADE\n);\n\
             CREATE NONCLUSTERED INDEX [ix_name] ON [dbo].[orders] ([name] DESC);"
        );
    }

    #[test]
    fn filters_to_one_object_for_ddl() {
        assert!(filtered(TABLES, true).contains("and o.object_id = object_id(@P1)"));
        assert!(!filtered(TABLES, false).contains("{filter}"));
    }
}
