//! Identifier quoting and SQL literals per dialect.

use sql_core::{Dialect, TableRef, Value};

use crate::keywords::is_reserved;

/// Whether `name` must be quoted to be read back as exactly `name`:
/// reserved words, anything but `[A-Za-z_][A-Za-z0-9_]*` (`$` allowed after the
/// first character where the dialect allows it), and upper case in Postgres,
/// which folds bare names to lower case.
pub fn needs_quoting(name: &str, d: Dialect) -> bool {
    let b = name.as_bytes();
    let Some(&first) = b.first() else { return true };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return true;
    }
    let dollar_ok = matches!(d, Dialect::Postgres | Dialect::Mysql);
    if !b[1..]
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || (c == b'$' && dollar_ok))
    {
        return true;
    }
    if d == Dialect::Postgres && b.iter().any(u8::is_ascii_uppercase) {
        return true;
    }
    is_reserved(name, d)
}

/// `name` in the dialect's identifier quotes, the closing quote doubled inside.
pub fn force_quote(name: &str, d: Dialect) -> String {
    let (open, close) = match d {
        Dialect::Postgres | Dialect::Sqlite => ('"', '"'),
        Dialect::Mysql => ('`', '`'),
        Dialect::Mssql => ('[', ']'),
    };
    let mut s = String::with_capacity(name.len() + 2);
    s.push(open);
    for c in name.chars() {
        s.push(c);
        if c == close {
            s.push(c);
        }
    }
    s.push(close);
    s
}

pub fn quote_ident(name: &str, d: Dialect) -> String {
    if needs_quoting(name, d) {
        force_quote(name, d)
    } else {
        name.to_owned()
    }
}

pub fn qualified(t: &TableRef, d: Dialect) -> String {
    match &t.schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(s, d), quote_ident(&t.name, d)),
        _ => quote_ident(&t.name, d),
    }
}

/// The name a quoted or bare identifier token stands for (quotes removed and
/// doubled quotes unescaped; bare names as written).
pub fn unquote(text: &str) -> String {
    let b = text.as_bytes();
    let close = match b.first() {
        Some(b'"') => b'"',
        Some(b'`') => b'`',
        Some(b'[') => b']',
        _ => return text.to_owned(),
    };
    let inner = &text[1..];
    let inner = inner.strip_suffix(close as char).unwrap_or(inner);
    let c = close as char;
    inner.replace(&format!("{c}{c}"), c.encode_utf8(&mut [0; 4]))
}

fn text_literal(s: &str, d: Dialect) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    if d == Dialect::Mssql && !s.is_ascii() {
        out.push('N');
    }
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            // MySQL reads backslash escapes inside strings by default.
            '\\' if d == Dialect::Mysql => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn literal(v: &Value, d: Dialect) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match d {
            Dialect::Postgres | Dialect::Mysql => if *b { "TRUE" } else { "FALSE" }.into(),
            Dialect::Sqlite | Dialect::Mssql => if *b { "1" } else { "0" }.into(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => {
            let s = f.to_string();
            // Keep it a float literal so integer-looking values don't change type.
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Float(f) => text_literal(&f.to_string().replace("inf", "Infinity"), d),
        Value::Decimal(s) if is_number(s) => s.clone(),
        Value::Bytes(b) => match d {
            Dialect::Postgres => format!("'\\x{}'", hex(b)),
            Dialect::Mysql | Dialect::Sqlite => format!("X'{}'", hex(b)),
            Dialect::Mssql => format!("0x{}", hex(b)),
        },
        Value::Json(j) => text_literal(&j.to_string(), d),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::Other(s) => text_literal(s, d),
    }
}

fn is_number(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty()
        && t.bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        && t.as_bytes()[0].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;

    #[test]
    fn quoting_only_when_needed() {
        let cases: &[(&str, Dialect, &str)] = &[
            ("users", Postgres, "users"),
            ("Users", Postgres, "\"Users\""),
            ("Users", Mysql, "Users"),
            ("Users", Mssql, "Users"),
            ("Users", Sqlite, "Users"),
            ("order", Postgres, "\"order\""),
            ("order", Mysql, "`order`"),
            ("order", Mssql, "[order]"),
            ("user", Postgres, "\"user\""),
            ("key", Mysql, "`key`"),
            ("first name", Sqlite, "\"first name\""),
            ("a\"b", Postgres, "\"a\"\"b\""),
            ("a`b", Mysql, "`a``b`"),
            ("a]b", Mssql, "[a]]b]"),
            ("1st", Postgres, "\"1st\""),
            ("_x1", Postgres, "_x1"),
            ("a$b", Postgres, "a$b"),
            ("a$b", Mssql, "[a$b]"),
            ("naïve", Postgres, "\"naïve\""),
            ("", Postgres, "\"\""),
        ];
        for (name, d, want) in cases {
            assert_eq!(quote_ident(name, *d), *want, "{name} {d:?}");
        }
    }

    #[test]
    fn qualified_names() {
        assert_eq!(
            qualified(&TableRef::new(Some("public"), "users"), Postgres),
            "public.users"
        );
        assert_eq!(
            qualified(&TableRef::new(Some("Sales"), "Order"), Postgres),
            "\"Sales\".\"Order\""
        );
        assert_eq!(
            qualified(&TableRef::new(Some("dbo"), "order"), Mssql),
            "dbo.[order]"
        );
        assert_eq!(qualified(&TableRef::new(None, "t"), Sqlite), "t");
    }

    #[test]
    fn unquoting() {
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[a]]b]"), "a]b");
        assert_eq!(unquote("`x"), "x");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn literals_per_dialect() {
        let cases: Vec<(Value, Dialect, &str)> = vec![
            (Value::Null, Postgres, "NULL"),
            (Value::Bool(true), Postgres, "TRUE"),
            (Value::Bool(false), Mysql, "FALSE"),
            (Value::Bool(true), Sqlite, "1"),
            (Value::Bool(false), Mssql, "0"),
            (Value::Int(-42), Postgres, "-42"),
            (Value::Float(1.5), Postgres, "1.5"),
            (Value::Float(2.0), Postgres, "2.0"),
            (Value::Float(f64::NAN), Postgres, "'NaN'"),
            (Value::Float(f64::NEG_INFINITY), Postgres, "'-Infinity'"),
            (
                Value::Decimal("12345678901234567890.01".into()),
                Postgres,
                "12345678901234567890.01",
            ),
            (Value::Decimal("oops".into()), Postgres, "'oops'"),
            (Value::Text("it's".into()), Postgres, "'it''s'"),
            (Value::Text("a\\b".into()), Postgres, "'a\\b'"),
            (Value::Text("a\\b".into()), Mysql, "'a\\\\b'"),
            (Value::Text("é".into()), Mssql, "N'é'"),
            (Value::Text("e".into()), Mssql, "'e'"),
            (Value::Bytes(vec![0xde, 0xad]), Postgres, "'\\xdead'"),
            (Value::Bytes(vec![0xde, 0xad]), Mysql, "X'dead'"),
            (Value::Bytes(vec![0xde, 0xad]), Sqlite, "X'dead'"),
            (Value::Bytes(vec![0xde, 0xad]), Mssql, "0xdead"),
            (
                Value::Json(serde_json::json!({"a": "it's"})),
                Postgres,
                "'{\"a\":\"it''s\"}'",
            ),
            (Value::Uuid("6f1c…".into()), Postgres, "'6f1c…'"),
            (
                Value::Timestamp("2024-01-02 03:04:05".into()),
                Mssql,
                "'2024-01-02 03:04:05'",
            ),
        ];
        for (v, d, want) in cases {
            assert_eq!(literal(&v, d), want, "{v:?} {d:?}");
        }
    }
}
