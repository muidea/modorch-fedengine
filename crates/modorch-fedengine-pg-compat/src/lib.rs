//! PostgreSQL scalar codecs and scrubbed SQLSTATE responses.
mod catalog;
pub use catalog::*;
mod catalog_codec;
pub use catalog_codec::array_element_type;
mod session;
pub use session::*;
mod planner;
pub use planner::*;
mod execution;
mod result_types;
use bytes::{BufMut, BytesMut};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
pub use execution::{EXECUTION_SETTINGS, execution_sql};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use pgwire::{
    api::{
        Type,
        portal::Format,
        results::{FieldFormat, FieldInfo},
    },
    error::{ErrorInfo, PgWireError},
    messages::data::DataRow,
};
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
const PG_EPOCH_DAYS: i32 = 10957;
pub fn pg_error(e: EngineError) -> PgWireError {
    let message = if e.outcome_unknown {
        format!(
            "{}; write outcome is unknown, do not retry automatically",
            e.message
        )
    } else {
        e.message
    };
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".into(),
        e.sqlstate.into(),
        message,
    )))
}
pub fn fields(columns: &[ColumnMeta], format: &Format) -> Vec<FieldInfo> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            FieldInfo::new(
                c.name.clone(),
                c.table_oid.map(|id| id as i32),
                c.column_attnum,
                Type::from_oid(c.data_type.oid()).unwrap(),
                format.format_for(i),
            )
            .with_type_modifier(c.type_modifier)
        })
        .collect()
}
pub fn encode_row(row: &[Cell], columns: &[ColumnMeta], format: &Format) -> Result<DataRow> {
    if row.len() != columns.len() || row.len() > i16::MAX as usize {
        return Err(EngineError::new("08P01", "row/column count mismatch"));
    }
    let mut data = BytesMut::new();
    for (i, (cell, col)) in row.iter().zip(columns).enumerate() {
        if matches!(cell, Cell::Null) {
            data.put_i32(-1);
            continue;
        }
        let value = if format.format_for(i) == FieldFormat::Binary {
            binary(cell, col.data_type)?
        } else {
            text_value(cell, col.data_type)?.unwrap().into_bytes()
        };
        if value.len() > i32::MAX as usize {
            return Err(EngineError::new(
                "54000",
                "field exceeds protocol size limit",
            ));
        }
        data.put_i32(value.len() as i32);
        data.extend_from_slice(&value);
    }
    Ok(DataRow::new(data, row.len() as i16))
}
/// PostgreSQL text representation shared by wire and HTTP consumers. NULL remains distinct.
pub fn text_value(cell: &Cell, data_type: PgType) -> Result<Option<String>> {
    if matches!(cell, Cell::Null) {
        return Ok(None);
    }
    if let Some(value) = catalog_codec::text(cell, data_type)? {
        return Ok(Some(value));
    }
    let mut value = text(cell)?;
    if data_type == PgType::Timestamptz {
        value.push_str("+00");
    }
    Ok(Some(value))
}
fn invalid() -> EngineError {
    EngineError::new("22P02", "invalid parameter representation")
}
fn binary(c: &Cell, t: PgType) -> Result<Vec<u8>> {
    if let PgType::Catalog(oid) = t {
        return catalog_codec::binary(c, oid);
    }
    Ok(match (t, c) {
        (PgType::Bool, Cell::Bool(v)) => vec![u8::from(*v)],
        (PgType::Int2, Cell::I16(v)) => v.to_be_bytes().to_vec(),
        (PgType::Int4, Cell::I32(v)) => v.to_be_bytes().to_vec(),
        (PgType::Int8, Cell::I64(v)) => v.to_be_bytes().to_vec(),
        (PgType::Float4, Cell::F32(v)) => v.to_be_bytes().to_vec(),
        (PgType::Float8, Cell::F64(v)) => v.to_be_bytes().to_vec(),
        (PgType::Text | PgType::Json, Cell::Text(v)) => v.as_bytes().to_vec(),
        (PgType::Bytea, Cell::Bytes(v)) => v.clone(),
        (PgType::Numeric, Cell::Numeric(v) | Cell::Text(v)) => encode_numeric(v)?,
        (PgType::Date, Cell::Date(v)) => v
            .checked_sub(PG_EPOCH_DAYS)
            .ok_or_else(invalid)?
            .to_be_bytes()
            .to_vec(),
        (PgType::Time, Cell::Time(v)) => v.to_be_bytes().to_vec(),
        (PgType::Timestamp | PgType::Timestamptz, Cell::Timestamp(v)) => v
            .checked_sub(PG_EPOCH_MICROS)
            .ok_or_else(invalid)?
            .to_be_bytes()
            .to_vec(),
        (
            PgType::Interval,
            Cell::Interval {
                months,
                days,
                micros,
            },
        ) => {
            let mut b = Vec::with_capacity(16);
            b.extend_from_slice(&micros.to_be_bytes());
            b.extend_from_slice(&days.to_be_bytes());
            b.extend_from_slice(&months.to_be_bytes());
            b
        }
        (PgType::Point, Cell::Point { x, y }) => {
            let mut b = Vec::with_capacity(16);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            b
        }
        (PgType::Uuid, Cell::Uuid(v)) => v.to_vec(),
        (PgType::Uuid, Cell::Text(v)) => uuid::Uuid::parse_str(v)
            .map_err(|_| invalid())?
            .as_bytes()
            .to_vec(),
        // Numeric result schemas may promote an integer, without truncating it.
        (PgType::Numeric, Cell::I64(v)) => encode_numeric(&v.to_string())?,
        _ => return Err(EngineError::unsupported("result type/value codec mismatch")),
    })
}
pub fn format_interval(months: i32, days: i32, micros: i64) -> String {
    let mut parts = Vec::with_capacity(4);
    let mut before_negative = false;
    let mut add = |value: i32, unit: &str| {
        if value == 0 {
            return;
        }
        let sign = if before_negative && value > 0 {
            "+"
        } else {
            ""
        };
        let plural = if value == 1 { "" } else { "s" };
        parts.push(format!("{sign}{value} {unit}{plural}"));
        before_negative = value < 0;
    };
    add(months / 12, "year");
    add(months % 12, "mon");
    add(days, "day");
    if micros != 0 || parts.is_empty() {
        let abs_micros = micros.unsigned_abs();
        let sign = if micros < 0 {
            "-"
        } else if before_negative {
            "+"
        } else {
            ""
        };
        let hours = abs_micros / 3_600_000_000;
        let minutes = (abs_micros / 60_000_000) % 60;
        let seconds = (abs_micros / 1_000_000) % 60;
        let mut s = format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
        let frac = abs_micros % 1_000_000;
        if frac != 0 {
            let frac_str = format!("{frac:06}");
            s.push('.');
            s.push_str(frac_str.trim_end_matches('0'));
        }
        parts.push(s);
    }
    parts.join(" ")
}
fn text(c: &Cell) -> Result<String> {
    Ok(match c {
        Cell::Oid(v) => v.to_string(),
        Cell::NamedOid { name, .. } => name.clone(),
        Cell::Array { .. } => {
            return Err(EngineError::unsupported(
                "array requires its declared catalog type",
            ));
        }
        Cell::Null => String::new(),
        Cell::Bool(v) => if *v { "t" } else { "f" }.into(),
        Cell::I16(v) => v.to_string(),
        Cell::I32(v) => v.to_string(),
        Cell::I64(v) => v.to_string(),
        Cell::F32(v) => float_text(*v as f64),
        Cell::F64(v) => float_text(*v),
        Cell::Text(v) | Cell::Numeric(v) => v.clone(),
        Cell::Bytes(v) => format!(
            "\\x{}",
            v.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        Cell::Date(d) => NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .checked_add_signed(chrono::Duration::days(*d as i64))
            .ok_or_else(invalid)?
            .to_string(),
        Cell::Time(t) => {
            if !(0..86_400_000_000).contains(t) {
                return Err(invalid());
            }
            NaiveTime::from_num_seconds_from_midnight_opt(
                (t / 1_000_000) as u32,
                ((t % 1_000_000) * 1000) as u32,
            )
            .ok_or_else(invalid)?
            .format("%H:%M:%S%.f")
            .to_string()
        }
        Cell::Timestamp(v) => chrono::DateTime::from_timestamp_micros(*v)
            .ok_or_else(invalid)?
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string(),
        Cell::Interval {
            months,
            days,
            micros,
        } => format_interval(*months, *days, *micros),
        Cell::Point { x, y } => format!("({},{})", float_text(*x), float_text(*y)),
        Cell::Uuid(v) => uuid::Uuid::from_bytes(*v).to_string(),
    })
}
fn float_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "Infinity".into()
    } else if v == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        v.to_string()
    }
}
pub fn decode_parameter(bytes: Option<&[u8]>, t: PgType, binary_format: bool) -> Result<Cell> {
    let Some(b) = bytes else {
        return Ok(Cell::Null);
    };
    if b.len() > 1024 * 1024 {
        return Err(EngineError::new("54000", "parameter exceeds 1 MiB limit"));
    }
    if let PgType::Catalog(oid) = t {
        return catalog_codec::decode(b, oid, binary_format);
    }
    if !binary_format {
        return decode_text(std::str::from_utf8(b).map_err(|_| invalid())?, t);
    }
    macro_rules! number {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(<$ty>::from_be_bytes(b.try_into().map_err(|_| invalid())?))
        };
    }
    Ok(match t {
        PgType::Catalog(_) => unreachable!("catalog parameters dispatched above"),
        PgType::Bool => {
            if b.len() != 1 || b[0] > 1 {
                return Err(invalid());
            }
            Cell::Bool(b[0] == 1)
        }
        PgType::Int2 => number!(i16, I16),
        PgType::Int4 => number!(i32, I32),
        PgType::Int8 => number!(i64, I64),
        PgType::Float4 => number!(f32, F32),
        PgType::Float8 => number!(f64, F64),
        PgType::Text | PgType::Json => {
            Cell::Text(std::str::from_utf8(b).map_err(|_| invalid())?.into())
        }
        PgType::Bytea => Cell::Bytes(b.into()),
        PgType::Numeric => Cell::Numeric(decode_numeric(b)?),
        PgType::Date => Cell::Date(
            i32::from_be_bytes(b.try_into().map_err(|_| invalid())?)
                .checked_add(PG_EPOCH_DAYS)
                .ok_or_else(invalid)?,
        ),
        PgType::Time => {
            let v = i64::from_be_bytes(b.try_into().map_err(|_| invalid())?);
            if !(0..86_400_000_000).contains(&v) {
                return Err(invalid());
            }
            Cell::Time(v)
        }
        PgType::Timestamp | PgType::Timestamptz => Cell::Timestamp(
            i64::from_be_bytes(b.try_into().map_err(|_| invalid())?)
                .checked_add(PG_EPOCH_MICROS)
                .ok_or_else(invalid)?,
        ),
        PgType::Interval => {
            if b.len() != 16 {
                return Err(invalid());
            }
            Cell::Interval {
                micros: i64::from_be_bytes(b[0..8].try_into().unwrap()),
                days: i32::from_be_bytes(b[8..12].try_into().unwrap()),
                months: i32::from_be_bytes(b[12..16].try_into().unwrap()),
            }
        }
        PgType::Point => {
            if b.len() != 16 {
                return Err(invalid());
            }
            Cell::Point {
                x: f64::from_be_bytes(b[0..8].try_into().unwrap()),
                y: f64::from_be_bytes(b[8..16].try_into().unwrap()),
            }
        }
        PgType::Uuid => Cell::Uuid(b.try_into().map_err(|_| invalid())?),
    })
}
fn parse_point_coord(s: &str) -> Result<f64> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("nan") {
        Ok(f64::NAN)
    } else if trimmed.eq_ignore_ascii_case("infinity") || trimmed.eq_ignore_ascii_case("inf") {
        Ok(f64::INFINITY)
    } else if trimmed.eq_ignore_ascii_case("-infinity") || trimmed.eq_ignore_ascii_case("-inf") {
        Ok(f64::NEG_INFINITY)
    } else {
        trimmed.parse::<f64>().map_err(|_| invalid())
    }
}
fn decode_text(v: &str, t: PgType) -> Result<Cell> {
    macro_rules! parse {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(v.parse::<$ty>().map_err(|_| invalid())?)
        };
    }
    Ok(match t {
        PgType::Catalog(oid) => return catalog_codec::decode(v.as_bytes(), oid, false),
        PgType::Bool => Cell::Bool(match v.to_lowercase().as_str() {
            "t" | "true" | "1" => true,
            "f" | "false" | "0" => false,
            _ => return Err(invalid()),
        }),
        PgType::Int2 => parse!(i16, I16),
        PgType::Int4 => parse!(i32, I32),
        PgType::Int8 => parse!(i64, I64),
        PgType::Float4 => parse!(f32, F32),
        PgType::Float8 => parse!(f64, F64),
        PgType::Text | PgType::Json => Cell::Text(v.into()),
        PgType::Numeric => Cell::Numeric(normalize_numeric_text(v)?),
        PgType::Bytea => {
            let h = v.strip_prefix("\\x").ok_or_else(invalid)?;
            if h.len() % 2 != 0 {
                return Err(invalid());
            }
            Cell::Bytes(
                (0..h.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| invalid()))
                    .collect::<Result<_>>()?,
            )
        }
        PgType::Date => {
            let d = NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| invalid())?;
            Cell::Date(
                d.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
                    .num_days()
                    .try_into()
                    .map_err(|_| invalid())?,
            )
        }
        PgType::Time => {
            let t = NaiveTime::parse_from_str(v, "%H:%M:%S%.f").map_err(|_| invalid())?;
            Cell::Time(
                t.num_seconds_from_midnight() as i64 * 1_000_000 + t.nanosecond() as i64 / 1000,
            )
        }
        PgType::Timestamp => Cell::Timestamp(
            NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S%.f")
                .map_err(|_| invalid())?
                .and_utc()
                .timestamp_micros(),
        ),
        PgType::Timestamptz => Cell::Timestamp(
            chrono::DateTime::parse_from_rfc3339(v)
                .or_else(|_| chrono::DateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S%.f%#z"))
                .map_err(|_| invalid())?
                .timestamp_micros(),
        ),
        PgType::Interval => {
            if v.trim().is_empty() {
                return Err(invalid());
            }
            Cell::Text(v.into())
        }
        PgType::Point => {
            let inner = v
                .trim()
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(invalid)?;
            let (xs, ys) = inner.split_once(',').ok_or_else(invalid)?;
            Cell::Point {
                x: parse_point_coord(xs)?,
                y: parse_point_coord(ys)?,
            }
        }
        PgType::Uuid => Cell::Uuid(*uuid::Uuid::parse_str(v).map_err(|_| invalid())?.as_bytes()),
    })
}
fn numeric_range() -> EngineError {
    EngineError::new("22003", "numeric value exceeds DuckDB DECIMAL(38) range")
}
fn binary_numeric_invalid(msg: &'static str) -> EngineError {
    EngineError::new("22P03", msg)
}
pub fn normalize_numeric_text(input: &str) -> Result<String> {
    let mut s = input.trim();
    if s.is_empty() {
        return Err(invalid());
    }
    let negative = if let Some(rest) = s.strip_prefix('-') {
        s = rest;
        true
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
        false
    } else {
        false
    };
    if s.is_empty() {
        return Err(invalid());
    }
    if matches!(s.to_ascii_lowercase().as_str(), "nan" | "infinity" | "inf") {
        return Err(EngineError::new(
            "22003",
            "numeric special value is outside DECIMAL range",
        ));
    }
    let (mantissa, exp) = if let Some((m, e)) = s.split_once(['e', 'E']) {
        if m.is_empty() || e.is_empty() || e.contains(['e', 'E']) {
            return Err(invalid());
        }
        let e_digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e_digits.is_empty() || !e_digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let parsed = e.parse::<i32>().map_err(|_| numeric_range())?;
        (m, parsed as i64)
    } else {
        (s, 0i64)
    };
    let (digits_before_point, raw_digits) = if let Some((int_s, frac_s)) = mantissa.split_once('.')
    {
        if frac_s.contains('.') || (int_s.is_empty() && frac_s.is_empty()) {
            return Err(invalid());
        }
        (int_s.len(), format!("{int_s}{frac_s}"))
    } else {
        (mantissa.len(), mantissa.to_string())
    };
    if raw_digits.is_empty() || !raw_digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let all_zero = raw_digits.bytes().all(|b| b == b'0');
    if all_zero {
        let scale = (raw_digits.len() as i64 - (digits_before_point as i64 + exp)).max(0);
        if scale > 1000 {
            return Err(EngineError::new(
                "22003",
                "numeric scale exceeds protocol limit",
            ));
        }
        return Ok(if scale == 0 {
            "0".into()
        } else {
            format!("0.{}", "0".repeat(scale as usize))
        });
    }
    let trimmed = raw_digits.trim_start_matches('0');
    let leading_zeros = (raw_digits.len() - trimmed.len()) as i64;
    let decimal_pos = digits_before_point as i64 + exp - leading_zeros;
    let scale = (trimmed.len() as i64 - decimal_pos).max(0);
    if scale > 1000 || decimal_pos > 1000 {
        return Err(EngineError::new(
            "22003",
            "numeric value exceeds protocol limit",
        ));
    }
    let (int_part, frac_part) = if decimal_pos <= 0 {
        (
            "0".to_string(),
            format!("{}{}", "0".repeat((-decimal_pos) as usize), trimmed),
        )
    } else if decimal_pos >= trimmed.len() as i64 {
        (
            format!(
                "{}{}",
                trimmed,
                "0".repeat(decimal_pos as usize - trimmed.len())
            ),
            String::new(),
        )
    } else {
        let split = decimal_pos as usize;
        (trimmed[..split].to_string(), trimmed[split..].to_string())
    };
    let mut out = if frac_part.is_empty() {
        int_part
    } else {
        format!("{int_part}.{frac_part}")
    };
    if negative {
        out.insert(0, '-');
    }
    Ok(out)
}
pub fn numeric_precision_scale(value: &str) -> Result<(u8, u8)> {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (int_part, scale) = match unsigned.split_once('.') {
        Some((i, f)) if !f.contains('.') => (i, f.len()),
        Some(_) => return Err(invalid()),
        None => (unsigned, 0),
    };
    let trimmed_int = int_part.trim_start_matches('0');
    let precision = (trimmed_int.len() + scale).max(1);
    if precision > 38 || scale > 38 {
        return Err(numeric_range());
    }
    Ok((precision as u8, scale as u8))
}
fn encode_numeric(v: &str) -> Result<Vec<u8>> {
    let neg = v.starts_with('-');
    let v = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    if int.is_empty()
        || int.len() + frac.len() > 1000
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    let mut digits = format!(
        "{}{}{}{}",
        "0".repeat((4 - int.len() % 4) % 4),
        int,
        frac,
        "0".repeat((4 - frac.len() % 4) % 4)
    );
    let mut weight = int.len().div_ceil(4) as i16 - 1;
    let mut groups = Vec::new();
    for d in digits.as_bytes().chunks(4) {
        groups.push(
            std::str::from_utf8(d)
                .unwrap()
                .parse::<i16>()
                .map_err(|_| invalid())?,
        );
    }
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    digits.clear();
    let mut b = BytesMut::new();
    b.put_i16(groups.len() as i16);
    b.put_i16(if groups.is_empty() { 0 } else { weight });
    b.put_i16(if neg && !groups.is_empty() { 0x4000 } else { 0 });
    b.put_i16(frac.len() as i16);
    for g in groups {
        b.put_i16(g);
    }
    Ok(b.to_vec())
}
fn decode_numeric(b: &[u8]) -> Result<String> {
    if b.len() < 8 || !b.len().is_multiple_of(2) {
        return Err(binary_numeric_invalid("invalid binary numeric length"));
    }
    let get = |i| i16::from_be_bytes([b[i], b[i + 1]]);
    let n = get(0);
    let weight = get(2);
    let sign = get(4) as u16;
    let scale = get(6);
    if n < 0 || b.len() != 8 + n as usize * 2 {
        return Err(binary_numeric_invalid("invalid binary numeric length"));
    }
    match sign {
        0x0000 | 0x4000 => {}
        0xC000 | 0xD000 | 0xF000 => {
            return Err(EngineError::new(
                "22003",
                "numeric special value is outside DECIMAL range",
            ));
        }
        _ => return Err(binary_numeric_invalid("invalid binary numeric sign")),
    }
    if !(0..=1000).contains(&scale) || !(-250..=250).contains(&weight) {
        return Err(EngineError::new(
            "22003",
            "numeric value exceeds protocol range",
        ));
    }
    let mut groups = Vec::new();
    let mut all_zero = true;
    for i in 0..n as usize {
        let d = get(8 + i * 2);
        if !(0..10000).contains(&d) {
            return Err(binary_numeric_invalid("invalid binary numeric digit"));
        }
        if d != 0 {
            all_zero = false;
        }
        groups.push(d);
    }
    for (index, &digit) in groups.iter().enumerate() {
        let exponent = i32::from(weight) - index as i32;
        if digit == 0 || exponent >= 0 {
            continue;
        }
        let group_start = (-exponent - 1) * 4 + 1;
        let kept_digits = i32::from(scale) - group_start + 1;
        if kept_digits >= 4 {
            continue;
        }
        let divisor = 10_i16.pow((4 - kept_digits.max(0)) as u32);
        if digit % divisor != 0 {
            return Err(binary_numeric_invalid(
                "binary numeric contains digits beyond its declared scale",
            ));
        }
    }
    if all_zero {
        return Ok(if scale == 0 {
            "0".into()
        } else {
            format!("0.{}", "0".repeat(scale as usize))
        });
    }
    let digit = |position: i16| -> i16 {
        let offset = weight - position;
        if offset < 0 {
            0
        } else {
            groups.get(offset as usize).copied().unwrap_or(0)
        }
    };
    let mut int = if weight < 0 {
        "0".into()
    } else {
        (0..=weight)
            .rev()
            .map(|p| {
                if p == weight {
                    digit(p).to_string()
                } else {
                    format!("{:04}", digit(p))
                }
            })
            .collect::<String>()
    };
    let trimmed_int = int.trim_start_matches('0');
    int = if trimmed_int.is_empty() {
        "0".into()
    } else {
        trimmed_int.into()
    };
    if scale > 0 {
        int.push('.');
        let frac = (1..=((scale + 3) / 4))
            .map(|p| format!("{:04}", digit(-p)))
            .collect::<String>();
        int.push_str(&frac[..scale as usize]);
    }
    if sign == 0x4000 {
        int.insert(0, '-');
    }
    Ok(int)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_binary_preserves_precision_scale_and_sign() {
        for value in [
            "0",
            "-0.0001",
            "12345678901234567890123456789012345678.25",
            "10000",
            "0.00100",
            "-123.4500",
        ] {
            let encoded = encode_numeric(value).unwrap();
            let decoded = decode_numeric(&encoded).unwrap();
            assert_eq!(decoded, value);
        }
    }
    #[test]
    fn binary_scalar_codecs_and_null_round_trip() {
        for (c, t) in [
            (Cell::I32(123), PgType::Int4),
            (Cell::Date(0), PgType::Date),
            (Cell::Timestamp(0), PgType::Timestamp),
            (Cell::Time(123456), PgType::Time),
            (Cell::Bool(true), PgType::Bool),
        ] {
            let b = binary(&c, t).unwrap();
            assert_eq!(
                text(&decode_parameter(Some(&b), t, true).unwrap()).unwrap(),
                text(&c).unwrap()
            );
        }
        assert!(matches!(
            decode_parameter(None, PgType::Text, true).unwrap(),
            Cell::Null
        ));
    }
    #[test]
    fn malformed_binary_parameters_fail_without_panics() {
        for t in [
            PgType::Int4,
            PgType::Bool,
            PgType::Date,
            PgType::Timestamp,
            PgType::Numeric,
            PgType::Uuid,
        ] {
            assert!(decode_parameter(Some(&[42, 43, 44]), t, true).is_err());
        }
    }
    #[test]
    fn numeric_text_normalization_preserves_scale_and_rejects_overflow() {
        for (input, normalized, precision, scale) in [
            ("123.4500", "123.4500", 7, 4),
            ("-0.001200", "-0.001200", 6, 6),
            ("+1.23e3", "1230", 4, 0),
            ("1.230e-2", "0.01230", 5, 5),
            (&format!("{}1.20", "0".repeat(100)), "1.20", 3, 2),
            ("-0", "0", 1, 0),
            ("-0.00", "0.00", 2, 2),
            (&"9".repeat(38), &"9".repeat(38), 38, 0),
        ] {
            let got = normalize_numeric_text(input).unwrap();
            assert_eq!(got, normalized, "{input}");
            assert_eq!(numeric_precision_scale(&got).unwrap(), (precision, scale));
        }
        for (bad, state) in [
            ("12x", "22P02"),
            ("1e+", "22P02"),
            ("1e12x", "22P02"),
            ("NaN", "22003"),
            ("-Infinity", "22003"),
            ("1e9999999999", "22003"),
        ] {
            assert_eq!(
                normalize_numeric_text(bad).unwrap_err().sqlstate,
                state,
                "{bad}"
            );
        }
        for overflow in [&"9".repeat(39), "1e-39"] {
            let normalized = normalize_numeric_text(overflow).unwrap();
            assert_eq!(
                numeric_precision_scale(&normalized).unwrap_err().sqlstate,
                "22003",
                "{overflow}"
            );
        }
    }
    #[test]
    fn numeric_binary_rejects_special_values_and_corrupt_digits() {
        let packet = |weight: i16, sign: u16, scale: i16, digits: &[i16]| -> Vec<u8> {
            let mut b = Vec::with_capacity(8 + digits.len() * 2);
            b.extend_from_slice(&(digits.len() as i16).to_be_bytes());
            b.extend_from_slice(&weight.to_be_bytes());
            b.extend_from_slice(&(sign as i16).to_be_bytes());
            b.extend_from_slice(&scale.to_be_bytes());
            for d in digits {
                b.extend_from_slice(&d.to_be_bytes());
            }
            b
        };
        assert_eq!(
            decode_numeric(&packet(0, 0x0000, 4, &[123, 4500])).unwrap(),
            "123.4500"
        );
        assert_eq!(
            decode_numeric(&packet(-1, 0x4000, 4, &[12])).unwrap(),
            "-0.0012"
        );
        assert_eq!(decode_numeric(&packet(0, 0x4000, 2, &[0])).unwrap(), "0.00");
        for (bad, state) in [
            (vec![0, 1, 0, 0, 0, 0, 0, 0], "22P03"),
            (packet(0, 0x0000, 0, &[10000]), "22P03"),
            (packet(0, 0x2000, 0, &[1]), "22P03"),
            (packet(-1, 0x0000, 2, &[1201]), "22P03"),
            (packet(0, 0xC000, 0, &[]), "22003"),
            (packet(0, 0xF000, 0, &[]), "22003"),
        ] {
            assert_eq!(decode_numeric(&bad).unwrap_err().sqlstate, state);
        }
    }
    #[test]
    fn interval_and_point_codecs_round_trip() {
        for (months, days, micros, expected) in [
            (0, 0, 0, "00:00:00"),
            (
                14,
                3,
                14_706_123_456,
                "1 year 2 mons 3 days 04:05:06.123456",
            ),
            (-14, 3, 14_706_000_000, "-1 years -2 mons +3 days 04:05:06"),
            (0, 0, i64::MIN, "-2562047788:00:54.775808"),
        ] {
            let cell = Cell::Interval {
                months,
                days,
                micros,
            };
            assert_eq!(text(&cell).unwrap(), expected);
            let wire = binary(&cell, PgType::Interval).unwrap();
            let decoded = decode_parameter(Some(&wire), PgType::Interval, true).unwrap();
            assert_eq!(text(&decoded).unwrap(), expected);
        }
        let point = Cell::Point { x: 1.5, y: -2.25 };
        assert_eq!(text(&point).unwrap(), "(1.5,-2.25)");
        let wire = binary(&point, PgType::Point).unwrap();
        let decoded = decode_parameter(Some(&wire), PgType::Point, true).unwrap();
        assert_eq!(text(&decoded).unwrap(), "(1.5,-2.25)");
        let decoded_text = decode_parameter(Some(b"(1.5, -2.25)"), PgType::Point, false).unwrap();
        assert_eq!(text(&decoded_text).unwrap(), "(1.5,-2.25)");
    }
}

mod privileges;
pub use privileges::parse_privileges;
