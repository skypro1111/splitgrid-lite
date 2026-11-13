//! Text-protocol values: the simple query protocol returns every cell as the
//! server prints it, which never fails on an unknown type; the column's type
//! OID (from a Describe of the same statement) picks the `Value` variant.
//! Parameters go the other way, spliced in as escaped literals.

use sql_core::Value;
use tokio_postgres::types::{Kind, Type};

/// How a column's text is turned into a `Value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    Bool,
    Int,
    Float,
    Decimal,
    Text,
    Bytes,
    Json,
    Uuid,
    Date,
    Time,
    Timestamp,
    Other,
}

impl Decode {
    pub fn for_type(ty: &Type) -> Decode {
        match ty.oid() {
            16 => Decode::Bool,
            20 | 21 | 23 | 26 => Decode::Int,
            700 | 701 => Decode::Float,
            1700 => Decode::Decimal,
            18 | 19 | 25 | 705 | 1042 | 1043 => Decode::Text,
            17 => Decode::Bytes,
            114 | 3802 => Decode::Json,
            2950 => Decode::Uuid,
            1082 => Decode::Date,
            1083 | 1266 => Decode::Time,
            1114 | 1184 => Decode::Timestamp,
            _ if ty.name() == "citext" => Decode::Text,
            _ => Decode::Other,
        }
    }

    pub fn value(self, text: Option<&str>) -> Value {
        let Some(s) = text else { return Value::Null };
        let other = || Value::Other(s.to_owned());
        match self {
            Decode::Bool => match s {
                "t" => Value::Bool(true),
                "f" => Value::Bool(false),
                _ => other(),
            },
            Decode::Int => s.parse().map(Value::Int).unwrap_or_else(|_| other()),
            Decode::Float => s.parse().map(Value::Float).unwrap_or_else(|_| other()),
            Decode::Decimal => Value::Decimal(s.to_owned()),
            Decode::Text => Value::Text(s.to_owned()),
            Decode::Bytes => s
                .strip_prefix("\\x")
                .and_then(unhex)
                .map(Value::Bytes)
                .unwrap_or_else(other),
            Decode::Json => serde_json::from_str(s)
                .map(Value::Json)
                .unwrap_or_else(|_| other()),
            Decode::Uuid => Value::Uuid(s.to_owned()),
            Decode::Date => Value::Date(s.to_owned()),
            Decode::Time => Value::Time(s.to_owned()),
            Decode::Timestamp => Value::Timestamp(s.to_owned()),
            Decode::Other => other(),
        }
    }
}

/// `int4`, `varchar`, `int4[]` (Postgres calls the array type `_int4`).
pub fn type_name(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(member) => format!("{}[]", member.name()),
        _ => ty.name().to_owned(),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A parameter as an untyped string literal, so the server infers its type
/// from context exactly as it would for a text-format bind parameter.
/// `E''` escaping is correct whatever `standard_conforming_strings` says.
fn literal(v: &Value) -> String {
    let text = match v {
        Value::Null => return "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Value::Float(f) => f.to_string(),
        Value::Bytes(b) => {
            let mut s = String::from("\\x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s
        }
        Value::Json(j) => j.to_string(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::Other(s) => s.clone(),
    };
    format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

/// What `scan` found in a statement.
#[derive(Debug, PartialEq)]
pub struct Scanned {
    /// The SQL with `$n` replaced by the parameters' literals.
    pub sql: String,
    /// More than one statement (`select 1; select 2`): can't be described up front.
    pub multi: bool,
}

/// Walks the SQL the way the server's lexer does (strings, quoted identifiers,
/// dollar quotes, nested comments) to splice parameters and spot a second statement.
pub fn scan(sql: &str, params: &[Value]) -> anyhow::Result<Scanned> {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut multi = false;
    let mut after_semicolon = false;
    let mut i = 0;
    let mut copied = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' => {
                let escapes =
                    i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !ident(b[i - 2]));
                i += 1;
                while i < b.len() {
                    if escapes && b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'$' if i == 0 || !ident(b[i - 1]) => {
                let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 {
                    let n: usize = sql[i + 1..i + 1 + digits].parse()?;
                    let v = n
                        .checked_sub(1)
                        .and_then(|k| params.get(k))
                        .ok_or_else(|| anyhow::anyhow!("no value for parameter ${n}"))?;
                    out.push_str(&sql[copied..i]);
                    out.push_str(&literal(v));
                    i += 1 + digits;
                    copied = i;
                } else {
                    let tag_len = b[i + 1..]
                        .iter()
                        .take_while(|&&c| c != b'$' && ident(c))
                        .count();
                    if b.get(i + 1 + tag_len) == Some(&b'$') {
                        let tag = &sql[i..i + tag_len + 2];
                        i += tag.len();
                        i = sql[i..].find(tag).map_or(b.len(), |p| i + p + tag.len());
                    } else {
                        i += 1;
                    }
                }
            }
            b';' => {
                after_semicolon = true;
                i += 1;
                continue;
            }
            _ => i += 1,
        }
        if after_semicolon && !c.is_ascii_whitespace() {
            multi = true;
        }
    }
    out.push_str(&sql[copied..]);
    Ok(Scanned { sql: out, multi })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_by_type() {
        assert_eq!(
            Decode::for_type(&Type::INT8).value(Some("9007199254740993")),
            Value::Int(9007199254740993)
        );
        assert_eq!(
            Decode::for_type(&Type::BOOL).value(Some("t")),
            Value::Bool(true)
        );
        assert_eq!(
            Decode::for_type(&Type::FLOAT8).value(Some("-Infinity")),
            Value::Float(f64::NEG_INFINITY)
        );
        assert_eq!(
            Decode::for_type(&Type::NUMERIC).value(Some("12345678901234567890.000000000001")),
            Value::Decimal("12345678901234567890.000000000001".into())
        );
        assert_eq!(
            Decode::for_type(&Type::BYTEA).value(Some("\\xdead")),
            Value::Bytes(vec![0xde, 0xad])
        );
        assert_eq!(
            Decode::for_type(&Type::JSONB).value(Some(r#"{"a": [1]}"#)),
            Value::Json(serde_json::json!({"a": [1]}))
        );
        assert_eq!(
            Decode::for_type(&Type::TIMESTAMPTZ).value(Some("2024-01-02 03:04:05+00")),
            Value::Timestamp("2024-01-02 03:04:05+00".into())
        );
        assert_eq!(
            Decode::for_type(&Type::INT4_ARRAY).value(Some("{1,2}")),
            Value::Other("{1,2}".into())
        );
        assert_eq!(
            Decode::for_type(&Type::INTERVAL).value(Some("1 day")),
            Value::Other("1 day".into())
        );
        assert_eq!(Decode::for_type(&Type::INT4).value(None), Value::Null);
        // A value that doesn't parse as its type is shown, not dropped.
        assert_eq!(
            Decode::for_type(&Type::BYTEA).value(Some("abc")),
            Value::Other("abc".into())
        );
    }

    #[test]
    fn names_array_types_with_brackets() {
        assert_eq!(type_name(&Type::INT4_ARRAY), "int4[]");
        assert_eq!(type_name(&Type::VARCHAR), "varchar");
    }

    #[test]
    fn splices_params_outside_strings_and_comments() {
        let p = [Value::Int(7), Value::Text("it's \\ ok".into()), Value::Null];
        let s = scan(
            "select $1, '$1', \"$2\", $$ $1 $$, $f$x$1$f$ -- $1\n, /* $1 /* $2 */ */ $2, $3, a$1",
            &p,
        )
        .unwrap();
        assert_eq!(
            s.sql,
            "select E'7', '$1', \"$2\", $$ $1 $$, $f$x$1$f$ -- $1\n, /* $1 /* $2 */ */ E'it''s \\\\ ok', NULL, a$1"
        );
        assert!(!s.multi);
        assert!(scan("select $2", &p[..1]).is_err());
        assert_eq!(scan("select E'\\'$1'", &[]).unwrap().sql, "select E'\\'$1'");
        assert_eq!(literal(&Value::Bytes(vec![1, 255])), "E'\\\\x01ff'");
    }

    #[test]
    fn spots_a_second_statement() {
        assert!(!scan("select 1;", &[]).unwrap().multi);
        assert!(!scan("select 1; -- done\n", &[]).unwrap().multi);
        assert!(!scan("select ';' ; /* x */ ", &[]).unwrap().multi);
        assert!(scan("select 1; select 2", &[]).unwrap().multi);
        assert!(
            !scan(
                "create function f() returns int as $$ select 1; $$ language sql",
                &[]
            )
            .unwrap()
            .multi
        );
    }
}
