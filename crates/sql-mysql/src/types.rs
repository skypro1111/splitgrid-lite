//! MySQL/MariaDB wire values → `sql_core::Value`, and back for parameters.
//!
//! The text protocol (plain queries) sends every cell as bytes and the binary
//! protocol (statements with parameters) sends numbers and dates typed, so both
//! funnel through the column's declared type. Nothing here fails: a type we do
//! not know becomes `Other` with whatever text the server sent.

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value as My};
use sql_core::Value;

/// MySQL's `binary` collation id: BLOB/BINARY columns, as opposed to TEXT/CHAR.
const BINARY_CHARSET: u16 = 63;

/// The type as the grid header shows it (`int unsigned`, `varchar`, `json`…).
pub fn type_name(col: &Column) -> String {
    use ColumnType::*;
    let flags = col.flags();
    let binary = col.character_set() == BINARY_CHARSET;
    let text_or = |text: &'static str, bin: &'static str| {
        if flags.contains(ColumnFlags::ENUM_FLAG) {
            "enum"
        } else if flags.contains(ColumnFlags::SET_FLAG) {
            "set"
        } else if binary {
            bin
        } else {
            text
        }
    };
    let base = match col.column_type() {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_VECTOR => "vector",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => text_or("varchar", "varbinary"),
        MYSQL_TYPE_STRING => text_or("char", "binary"),
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => text_or("text", "blob"),
        _ => "unknown",
    };
    if flags.contains(ColumnFlags::UNSIGNED_FLAG) && is_numeric(col.column_type()) {
        format!("{base} unsigned")
    } else {
        base.to_owned()
    }
}

fn is_numeric(ty: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        ty,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
    )
}

/// One cell, by its column's type.
pub fn cell(col: &Column, v: My) -> Value {
    use ColumnType::*;
    match v {
        My::NULL => Value::Null,
        My::Int(i) => Value::Int(i),
        // BIGINT UNSIGNED beyond i64 keeps its digits as a decimal.
        My::UInt(u) => i64::try_from(u).map(Value::Int).unwrap_or_else(|_| Value::Decimal(u.to_string())),
        // Through the shortest f32 text, so FLOAT 0.1 shows as 0.1, not 0.10000000149.
        My::Float(f) => Value::Float(f.to_string().parse().unwrap_or(f as f64)),
        My::Double(d) => Value::Float(d),
        My::Date(y, mo, d, h, mi, s, us) => match col.column_type() {
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Value::Date(format!("{y:04}-{mo:02}-{d:02}")),
            _ => Value::Timestamp(format!("{y:04}-{mo:02}-{d:02} {}", clock(h as u32, mi, s, us))),
        },
        My::Time(neg, days, h, mi, s, us) => {
            Value::Time(format!("{}{}", if neg { "-" } else { "" }, clock(days * 24 + h as u32, mi, s, us)))
        }
        My::Bytes(b) => from_bytes(col, b),
    }
}

/// `HH:MM:SS[.ffffff]`, the way the server prints it.
fn clock(h: u32, mi: u8, s: u8, us: u32) -> String {
    if us == 0 { format!("{h:02}:{mi:02}:{s:02}") } else { format!("{h:02}:{mi:02}:{s:02}.{us:06}") }
}

fn from_bytes(col: &Column, b: Vec<u8>) -> Value {
    use ColumnType::*;
    let text = |b: Vec<u8>| String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            let s = text(b);
            match s.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) => Value::Decimal(s),
                Err(_) => Value::Other(s),
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text(b);
            s.parse().map(Value::Float).unwrap_or(Value::Other(s))
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Value::Decimal(text(b)),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Value::Date(text(b)),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Value::Time(text(b)),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => Value::Timestamp(text(b)),
        MYSQL_TYPE_BIT => bits(&b, col.column_length()),
        MYSQL_TYPE_JSON => serde_json::from_slice(&b).map(Value::Json).unwrap_or_else(|_| Value::Text(text(b))),
        MYSQL_TYPE_NULL => Value::Null,
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            let flags = col.flags();
            let labelled = flags.intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG);
            if col.character_set() == BINARY_CHARSET && !labelled { Value::Bytes(b) } else { Value::Text(text(b)) }
        }
        // GEOMETRY (WKB with an SRID prefix), VECTOR and anything newer.
        _ => match String::from_utf8(b) {
            Ok(s) => Value::Other(s),
            Err(e) => Value::Other(hex(e.as_bytes())),
        },
    }
}

/// BIT(n) arrives as n bits big-endian; BIT(1) is a flag.
fn bits(b: &[u8], width: u32) -> Value {
    let n = b.iter().fold(0u64, |acc, &byte| (acc << 8) | byte as u64);
    if width == 1 {
        Value::Bool(n != 0)
    } else {
        i64::try_from(n).map(Value::Int).unwrap_or_else(|_| Value::Decimal(n.to_string()))
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// A bound `?` parameter. Text-like values go as strings and the server casts them.
pub fn param(v: &Value) -> My {
    match v {
        Value::Null => My::NULL,
        Value::Bool(b) => My::Int(*b as i64),
        Value::Int(i) => My::Int(*i),
        Value::Float(f) => My::Double(*f),
        Value::Bytes(b) => My::Bytes(b.clone()),
        Value::Json(j) => My::Bytes(j.to_string().into_bytes()),
        Value::Decimal(s) | Value::Text(s) | Value::Uuid(s) | Value::Date(s) | Value::Time(s) | Value::Timestamp(s) | Value::Other(s) => {
            My::Bytes(s.clone().into_bytes())
        }
    }
}

/// A metadata cell as text (introspection reads everything as strings).
pub fn text(v: My) -> Option<String> {
    match v {
        My::NULL => None,
        My::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        My::Int(i) => Some(i.to_string()),
        My::UInt(u) => Some(u.to_string()),
        My::Float(f) => Some(f.to_string()),
        My::Double(d) => Some(d.to_string()),
        My::Date(y, mo, d, h, mi, s, us) => Some(format!("{y:04}-{mo:02}-{d:02} {}", clock(h as u32, mi, s, us))),
        My::Time(neg, days, h, mi, s, us) => Some(format!("{}{}", if neg { "-" } else { "" }, clock(days * 24 + h as u32, mi, s, us))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ty: ColumnType) -> Column {
        Column::new(ty).with_character_set(45)
    }

    fn bytes(s: &str) -> My {
        My::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn integers_including_unsigned_bigint_beyond_i64() {
        let big = col(ColumnType::MYSQL_TYPE_LONGLONG).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(cell(&big, bytes("18446744073709551615")), Value::Decimal("18446744073709551615".into()));
        assert_eq!(cell(&big, My::UInt(u64::MAX)), Value::Decimal(u64::MAX.to_string()));
        assert_eq!(cell(&big, My::UInt(7)), Value::Int(7));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_LONG), bytes("-42")), Value::Int(-42));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_YEAR), bytes("2024")), Value::Int(2024));
        assert_eq!(type_name(&big), "bigint unsigned");
    }

    #[test]
    fn floats_and_decimals() {
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_DOUBLE), bytes("1.5")), Value::Float(1.5));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_FLOAT), My::Float(0.1)), Value::Float(0.1));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_NEWDECIMAL), bytes("12345678901234567890.123")), Value::Decimal("12345678901234567890.123".into()));
    }

    #[test]
    fn temporal_values_in_both_protocols() {
        let dt = col(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(cell(&dt, bytes("2024-02-29 13:05:09")), Value::Timestamp("2024-02-29 13:05:09".into()));
        assert_eq!(cell(&dt, My::Date(2024, 2, 29, 13, 5, 9, 120)), Value::Timestamp("2024-02-29 13:05:09.000120".into()));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_DATE), My::Date(2024, 1, 2, 0, 0, 0, 0)), Value::Date("2024-01-02".into()));
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_TIME), My::Time(true, 1, 2, 3, 4, 0)), Value::Time("-26:03:04".into()));
        // Zero dates stay as the server prints them.
        assert_eq!(cell(&dt, bytes("0000-00-00 00:00:00")), Value::Timestamp("0000-00-00 00:00:00".into()));
    }

    #[test]
    fn strings_blobs_enums_and_json() {
        let blob = Column::new(ColumnType::MYSQL_TYPE_BLOB).with_character_set(BINARY_CHARSET);
        assert_eq!(cell(&blob, My::Bytes(vec![0, 255])), Value::Bytes(vec![0, 255]));
        assert_eq!(type_name(&blob), "blob");
        let text = col(ColumnType::MYSQL_TYPE_BLOB);
        assert_eq!(cell(&text, bytes("héllo")), Value::Text("héllo".into()));
        assert_eq!(type_name(&text), "text");
        let en = Column::new(ColumnType::MYSQL_TYPE_STRING).with_flags(ColumnFlags::ENUM_FLAG).with_character_set(BINARY_CHARSET);
        assert_eq!(cell(&en, bytes("red")), Value::Text("red".into()));
        assert_eq!(type_name(&en), "enum");
        let json = Column::new(ColumnType::MYSQL_TYPE_JSON).with_character_set(BINARY_CHARSET);
        assert_eq!(cell(&json, bytes(r#"{"a":[1,2]}"#)), Value::Json(serde_json::json!({"a": [1, 2]})));
        assert_eq!(cell(&json, bytes("not json")), Value::Text("not json".into()));
    }

    #[test]
    fn bits_geometry_and_unknown_types_never_fail() {
        let flag = Column::new(ColumnType::MYSQL_TYPE_BIT).with_column_length(1);
        assert_eq!(cell(&flag, My::Bytes(vec![1])), Value::Bool(true));
        let wide = Column::new(ColumnType::MYSQL_TYPE_BIT).with_column_length(16);
        assert_eq!(cell(&wide, My::Bytes(vec![1, 2])), Value::Int(258));
        let geo = Column::new(ColumnType::MYSQL_TYPE_GEOMETRY);
        assert_eq!(cell(&geo, My::Bytes(vec![0, 0, 0, 0, 1, 0xff])), Value::Other("0x0000000001ff".into()));
        assert_eq!(type_name(&geo), "geometry");
        assert_eq!(cell(&col(ColumnType::MYSQL_TYPE_LONG), My::NULL), Value::Null);
    }

    #[test]
    fn params_bind_as_mysql_values() {
        assert_eq!(param(&Value::Bool(true)), My::Int(1));
        assert_eq!(param(&Value::Decimal("1.50".into())), bytes("1.50"));
        assert_eq!(param(&Value::Json(serde_json::json!([1]))), bytes("[1]"));
        assert_eq!(param(&Value::Null), My::NULL);
    }
}
