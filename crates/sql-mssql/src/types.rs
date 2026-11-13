//! TDS values → `sql_core::Value`, and back for `@P1…` parameters.
//!
//! Dates are formatted here from TDS's raw day/tick counts (no chrono): the
//! grid gets the server's own text form (`2024-01-02 03:04:05.1234567`),
//! which also round-trips as a literal.

use sql_core::Value;
use tiberius::{ColumnData, ColumnType, Query};

/// The type as the grid header shows it. TDS sends some families as one
/// nullable wire type (`intn` covers tinyint…bigint), so those read generic.
pub fn type_name(ty: ColumnType) -> &'static str {
    use ColumnType::*;
    match ty {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Datetime4 => "smalldatetime",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money => "money",
        Money4 => "smallmoney",
        Datetime | Datetimen => "datetime",
        Guid => "uniqueidentifier",
        Decimaln => "decimal",
        Numericn => "numeric",
        Daten => "date",
        Timen => "time",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        BigVarBin => "varbinary",
        BigVarChar => "varchar",
        BigBinary => "binary",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Xml => "xml",
        Udt => "udt",
        Text => "text",
        Image => "image",
        NText => "ntext",
        SSVariant => "sql_variant",
    }
}

/// One cell, by its column's wire type (money arrives as f64 and is told
/// apart only by the column; sql_variant and CLR types are shown as text).
pub fn cell(ty: ColumnType, d: ColumnData<'static>) -> Value {
    let v = match d {
        ColumnData::U8(v) => v.map(|v| Value::Int(v.into())),
        ColumnData::I16(v) => v.map(|v| Value::Int(v.into())),
        ColumnData::I32(v) => v.map(|v| Value::Int(v.into())),
        ColumnData::I64(v) => v.map(Value::Int),
        // Through the shortest f32 text, so REAL 0.1 shows as 0.1.
        ColumnData::F32(v) => v.map(|f| Value::Float(f.to_string().parse().unwrap_or(f.into()))),
        ColumnData::F64(v) if matches!(ty, ColumnType::Money | ColumnType::Money4) => v.map(|f| Value::Decimal(format!("{f:.4}"))),
        ColumnData::F64(v) => v.map(Value::Float),
        ColumnData::Bit(v) => v.map(Value::Bool),
        ColumnData::String(v) => v.map(|s| Value::Text(s.into_owned())),
        ColumnData::Guid(v) => v.map(|g| Value::Uuid(g.to_string().to_uppercase())),
        ColumnData::Binary(v) if ty == ColumnType::Udt => v.map(|b| Value::Other(hex(&b))),
        ColumnData::Binary(v) => v.map(|b| Value::Bytes(b.into_owned())),
        ColumnData::Numeric(v) => v.map(|n| Value::Decimal(decimal(n.value(), n.scale()))),
        ColumnData::Xml(v) => v.map(|x| Value::Other(x.into_owned().into_string())),
        ColumnData::DateTime(v) => v.map(|dt| {
            // 1/300 s ticks, shown as milliseconds the way SQL Server rounds them.
            let ms = (dt.seconds_fragments() as u64 * 10 + 1) / 3;
            Value::Timestamp(format!("{} {}", date(DAYS_TO_1900 + dt.days() as i64), clock(ms as u128, 3)))
        }),
        ColumnData::SmallDateTime(v) => v.map(|dt| {
            Value::Timestamp(format!("{} {}", date(DAYS_TO_1900 + dt.days() as i64), clock(dt.seconds_fragments() as u128 * 60, 0)))
        }),
        ColumnData::Time(v) => v.map(|t| Value::Time(clock(t.increments() as u128, t.scale()))),
        ColumnData::Date(v) => v.map(|d| Value::Date(date(d.days() as i64))),
        ColumnData::DateTime2(v) => v.map(|dt| {
            Value::Timestamp(format!("{} {}", date(dt.date().days() as i64), clock(dt.time().increments() as u128, dt.time().scale())))
        }),
        ColumnData::DateTimeOffset(v) => v.map(|dto| {
            let (dt, offset) = (dto.datetime2(), dto.offset());
            Value::Timestamp(with_offset(dt.date().days() as i64, dt.time().increments(), dt.time().scale(), offset))
        }),
    };
    match v {
        None => Value::Null,
        Some(v) if ty == ColumnType::SSVariant => Value::Other(v.display()),
        Some(v) => v,
    }
}

/// Days from 0001-01-01 (the `date` epoch) to 1900-01-01 (the `datetime` one).
const DAYS_TO_1900: i64 = 693_595;

/// `YYYY-MM-DD` of a day count from 0001-01-01 (Hinnant's civil-from-days).
fn date(days: i64) -> String {
    let z = days - 719_162 + 719_468; // → days since 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM:SS[.f…]` of a tick count at 10^-scale seconds.
fn clock(ticks: u128, scale: u8) -> String {
    let unit = 10u128.pow(scale as u32);
    let secs = ticks / unit;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if scale == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{:0width$}", ticks % unit, width = scale as usize)
    }
}

/// datetimeoffset travels as UTC plus the offset; shown in its own zone.
fn with_offset(days: i64, ticks: u64, scale: u8, offset_min: i16) -> String {
    let unit = 10i128.pow(scale as u32);
    let per_day = 86_400 * unit;
    let total = days as i128 * per_day + ticks as i128 + offset_min as i128 * 60 * unit;
    let sign = if offset_min < 0 { '-' } else { '+' };
    let off = offset_min.unsigned_abs();
    format!(
        "{} {} {sign}{:02}:{:02}",
        date(total.div_euclid(per_day) as i64),
        clock(total.rem_euclid(per_day) as u128, scale),
        off / 60,
        off % 60
    )
}

/// A scaled integer as decimal text without float rounding.
fn decimal(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (int, frac) = digits.split_at(digits.len() - scale);
    format!("{sign}{int}.{frac}")
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for byte in b {
        s.push_str(&format!("{byte:02X}"));
    }
    s
}

/// A metadata cell as text (introspection reads everything as strings).
pub fn text(v: Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(if b { "1" } else { "0" }.into()),
        v => Some(v.display()),
    }
}

/// Binds one `@Pn`. NULL and text-like values go as nvarchar, which SQL
/// Server converts implicitly to nearly every column type.
pub fn bind(q: &mut Query<'_>, v: &Value) {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::Float(f) => q.bind(*f),
        Value::Bytes(b) => q.bind(b.clone()),
        Value::Json(j) => q.bind(j.to_string()),
        Value::Uuid(s) => match tiberius::Uuid::parse_str(s) {
            Ok(u) => q.bind(u),
            Err(_) => q.bind(s.clone()),
        },
        Value::Decimal(s) | Value::Text(s) | Value::Date(s) | Value::Time(s) | Value::Timestamp(s) | Value::Other(s) => q.bind(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use tiberius::numeric::Numeric;
    use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};

    #[test]
    fn integers_bits_and_floats() {
        assert_eq!(cell(ColumnType::Intn, ColumnData::I64(Some(-5))), Value::Int(-5));
        assert_eq!(cell(ColumnType::Int1, ColumnData::U8(Some(255))), Value::Int(255));
        assert_eq!(cell(ColumnType::Bitn, ColumnData::Bit(Some(true))), Value::Bool(true));
        assert_eq!(cell(ColumnType::Float4, ColumnData::F32(Some(0.1))), Value::Float(0.1));
        assert_eq!(cell(ColumnType::Intn, ColumnData::I32(None)), Value::Null);
    }

    #[test]
    fn decimals_and_money_keep_their_digits() {
        let n = |v, s| ColumnData::Numeric(Some(Numeric::new_with_scale(v, s)));
        assert_eq!(cell(ColumnType::Decimaln, n(12345, 2)), Value::Decimal("123.45".into()));
        assert_eq!(cell(ColumnType::Decimaln, n(-5, 3)), Value::Decimal("-0.005".into()));
        assert_eq!(cell(ColumnType::Numericn, n(42, 0)), Value::Decimal("42".into()));
        assert_eq!(cell(ColumnType::Money, ColumnData::F64(Some(12.5))), Value::Decimal("12.5000".into()));
        assert_eq!(cell(ColumnType::Floatn, ColumnData::F64(Some(12.5))), Value::Float(12.5));
    }

    #[test]
    fn text_guid_binary_xml_and_variants() {
        assert_eq!(cell(ColumnType::NVarchar, ColumnData::String(Some(Cow::Borrowed("héllo")))), Value::Text("héllo".into()));
        let g = tiberius::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(cell(ColumnType::Guid, ColumnData::Guid(Some(g))), Value::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()));
        assert_eq!(cell(ColumnType::BigVarBin, ColumnData::Binary(Some(Cow::Owned(vec![1, 2])))), Value::Bytes(vec![1, 2]));
        assert_eq!(cell(ColumnType::Udt, ColumnData::Binary(Some(Cow::Owned(vec![0xab])))), Value::Other("0xAB".into()));
        let xml = tiberius::xml::XmlData::new("<a/>");
        assert_eq!(cell(ColumnType::Xml, ColumnData::Xml(Some(Cow::Owned(xml)))), Value::Other("<a/>".into()));
        assert_eq!(cell(ColumnType::SSVariant, ColumnData::I32(Some(7))), Value::Other("7".into()));
        assert_eq!(type_name(ColumnType::Guid), "uniqueidentifier");
    }

    #[test]
    fn calendar_math() {
        assert_eq!(date(0), "0001-01-01");
        assert_eq!(date(DAYS_TO_1900), "1900-01-01");
        assert_eq!(date(DAYS_TO_1900 + 45_350), "2024-03-01");
        assert_eq!(date(3_652_058), "9999-12-31");
        assert_eq!(decimal(1, 4), "0.0001");
    }

    #[test]
    fn temporal_values_print_like_the_server() {
        // datetime: 2024-02-29 13:05:09.997 (days since 1900, 1/300 s ticks).
        let ticks = (13 * 3600 + 5 * 60 + 9) * 300 + 299;
        assert_eq!(
            cell(ColumnType::Datetimen, ColumnData::DateTime(Some(DateTime::new(45_349, ticks)))),
            Value::Timestamp("2024-02-29 13:05:09.997".into())
        );
        assert_eq!(
            cell(ColumnType::Datetime4, ColumnData::SmallDateTime(Some(SmallDateTime::new(45_349, 61)))),
            Value::Timestamp("2024-02-29 01:01:00".into())
        );
        let d = Date::new((DAYS_TO_1900 + 45_349) as u32);
        assert_eq!(cell(ColumnType::Daten, ColumnData::Date(Some(d))), Value::Date("2024-02-29".into()));
        let t = Time::new(12_345_678_901, 7); // 1234.5678901 s
        assert_eq!(cell(ColumnType::Timen, ColumnData::Time(Some(t))), Value::Time("00:20:34.5678901".into()));
        assert_eq!(
            cell(ColumnType::Datetime2, ColumnData::DateTime2(Some(DateTime2::new(d, Time::new(5, 0))))),
            Value::Timestamp("2024-02-29 00:00:05".into())
        );
        // 23:30 UTC at +02:00 is the next day locally.
        let utc = DateTime2::new(d, Time::new((23 * 3600 + 30 * 60) * 1000, 3));
        assert_eq!(
            cell(ColumnType::DatetimeOffsetn, ColumnData::DateTimeOffset(Some(DateTimeOffset::new(utc, 120)))),
            Value::Timestamp("2024-03-01 01:30:00.000 +02:00".into())
        );
        let west = DateTime2::new(d, Time::new(3600, 0));
        assert_eq!(
            cell(ColumnType::DatetimeOffsetn, ColumnData::DateTimeOffset(Some(DateTimeOffset::new(west, -330)))),
            Value::Timestamp("2024-02-28 19:30:00 -05:30".into())
        );
    }

    #[test]
    fn metadata_text() {
        assert_eq!(text(Value::Bool(true)).as_deref(), Some("1"));
        assert_eq!(text(Value::Null), None);
        assert_eq!(text(Value::Int(3)).as_deref(), Some("3"));
    }
}
