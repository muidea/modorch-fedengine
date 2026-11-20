use duckdb::{
    arrow::{
        array::*,
        datatypes::{DataType, IntervalUnit, TimeUnit},
    },
    types::{OrderedMap, Value},
};
use modorch_fedengine_contract::{Cell, PgType};
use modorch_fedengine_core::*;
pub(super) fn type_modifier(t: &str) -> i32 {
    let args = t
        .strip_prefix("DECIMAL(")
        .or_else(|| t.strip_prefix("MYSQL_DECIMAL("))
        .and_then(|s| s.strip_suffix(')'));
    if let Some(args) = args
        && let Some((p, s)) = args.split_once(',')
        && let (Ok(p), Ok(s)) = (p.trim().parse::<i32>(), s.trim().parse::<i32>())
        && (1..=1000).contains(&p)
        && (0..=p).contains(&s)
    {
        return ((p << 16) | s) + 4;
    }
    -1
}
fn is_point_struct_type(t: &str) -> bool {
    let trimmed = t.trim();
    if trimmed.eq_ignore_ascii_case("POINT") {
        return true;
    }
    let Some(inner) = trimmed
        .strip_prefix("STRUCT(")
        .or_else(|| trimmed.strip_prefix("struct("))
        .and_then(|s| s.strip_suffix(')'))
    else {
        return false;
    };
    let mut parts = inner.split(',');
    let (Some(first), Some(second), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let parse_field = |part: &str| -> Option<(String, String)> {
        let mut tokens = part.split_whitespace();
        let name = tokens.next()?.trim_matches('"').to_string();
        let ty = tokens.next()?.to_ascii_uppercase();
        if tokens.next().is_some() {
            return None;
        }
        Some((name, ty))
    };
    matches!(
        (parse_field(first), parse_field(second)),
        (Some((n1, t1)), Some((n2, t2))) if n1 == "x" && t1 == "DOUBLE" && n2 == "y" && t2 == "DOUBLE"
    )
}
pub(super) fn type_name(t: &str) -> Result<PgType> {
    if let Some(elem) = t.strip_suffix("[]") {
        if elem.contains("[]") {
            return Err(EngineError::unsupported(
                "multi-dimensional arrays are not implemented",
            ));
        }
        let oid = match elem {
            "BOOLEAN" => 1000,
            "BLOB" => 1001,
            "TINYINT" | "SMALLINT" => 1005,
            "INTEGER" | "UTINYINT" | "USMALLINT" => 1007,
            "BIGINT" | "UINTEGER" => 1016,
            "FLOAT" => 1021,
            "DOUBLE" => 1022,
            "VARCHAR" => 1009,
            "DATE" => 1182,
            "TIME" => 1183,
            "TIMESTAMP" | "TIMESTAMP_NS" | "TIMESTAMP_MS" | "TIMESTAMP_S" => 1115,
            "TIMESTAMP WITH TIME ZONE" => 1185,
            "INTERVAL" => 1187,
            "UUID" => 2951,
            "JSON" => 199,
            v if is_point_struct_type(v) => 1017,
            v if v.starts_with("DECIMAL")
                || v.starts_with("MYSQL_DECIMAL(")
                || ["HUGEINT", "UHUGEINT", "UBIGINT"].contains(&v) =>
            {
                1231
            }
            _ => {
                return Err(EngineError::unsupported(
                    "result array type has no verified PG encoding",
                ));
            }
        };
        return Ok(PgType::Catalog(oid));
    }
    Ok(match t {
        "BOOLEAN" => PgType::Bool,
        "TINYINT" | "SMALLINT" => PgType::Int2,
        "INTEGER" | "UTINYINT" | "USMALLINT" => PgType::Int4,
        "BIGINT" | "UINTEGER" => PgType::Int8,
        "FLOAT" => PgType::Float4,
        "DOUBLE" => PgType::Float8,
        "VARCHAR" | "NULL" | "\"NULL\"" => PgType::Text,
        "BLOB" => PgType::Bytea,
        "DATE" => PgType::Date,
        "TIME" => PgType::Time,
        "TIMESTAMP" | "TIMESTAMP_NS" | "TIMESTAMP_MS" | "TIMESTAMP_S" => PgType::Timestamp,
        "TIMESTAMP WITH TIME ZONE" => PgType::Timestamptz,
        "INTERVAL" => PgType::Interval,
        "UUID" => PgType::Uuid,
        "JSON" => PgType::Json,
        v if is_point_struct_type(v) => PgType::Point,
        v if v.starts_with("DECIMAL")
            || v.starts_with("MYSQL_DECIMAL(")
            || ["HUGEINT", "UHUGEINT", "UBIGINT"].contains(&v) =>
        {
            PgType::Numeric
        }
        _ => {
            return Err(EngineError::unsupported(
                "result type has no verified PG encoding",
            ));
        }
    })
}
pub(super) fn cell(a: &dyn Array, i: usize) -> Result<Cell> {
    if matches!(a.data_type(), DataType::Null) || a.is_null(i) {
        return Ok(Cell::Null);
    }
    macro_rules! value {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(a.as_any().downcast_ref::<$ty>().unwrap().value(i).into())
        };
    }
    Ok(match a.data_type() {
        DataType::Boolean => value!(BooleanArray, Bool),
        DataType::Int8 => value!(Int8Array, I16),
        DataType::Int16 => value!(Int16Array, I16),
        DataType::UInt8 => value!(UInt8Array, I32),
        DataType::UInt16 => value!(UInt16Array, I32),
        DataType::Int32 => value!(Int32Array, I32),
        DataType::UInt32 => value!(UInt32Array, I64),
        DataType::Int64 => value!(Int64Array, I64),
        DataType::Float32 => value!(Float32Array, F32),
        DataType::Float64 => value!(Float64Array, F64),
        DataType::Utf8 => value!(StringArray, Text),
        DataType::LargeUtf8 => value!(LargeStringArray, Text),
        DataType::Binary => value!(BinaryArray, Bytes),
        DataType::LargeBinary => value!(LargeBinaryArray, Bytes),
        DataType::Date32 => value!(Date32Array, Date),
        DataType::UInt64 => Cell::Numeric(
            a.as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(i)
                .to_string(),
        ),
        DataType::Decimal128(_, scale) => Cell::Numeric(decimal(
            a.as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(i)
                .to_string(),
            *scale,
        )),
        DataType::Decimal256(_, scale) => Cell::Numeric(decimal(
            a.as_any()
                .downcast_ref::<Decimal256Array>()
                .unwrap()
                .value(i)
                .to_string(),
            *scale,
        )),
        DataType::Time64(TimeUnit::Microsecond) => value!(Time64MicrosecondArray, Time),
        DataType::Time64(TimeUnit::Nanosecond) => Cell::Time(
            a.as_any()
                .downcast_ref::<Time64NanosecondArray>()
                .unwrap()
                .value(i)
                / 1000,
        ),
        DataType::Timestamp(unit, _) => Cell::Timestamp(
            match unit {
                TimeUnit::Second => a
                    .as_any()
                    .downcast_ref::<TimestampSecondArray>()
                    .unwrap()
                    .value(i)
                    .checked_mul(1_000_000),
                TimeUnit::Millisecond => a
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .unwrap()
                    .value(i)
                    .checked_mul(1000),
                TimeUnit::Microsecond => Some(
                    a.as_any()
                        .downcast_ref::<TimestampMicrosecondArray>()
                        .unwrap()
                        .value(i),
                ),
                TimeUnit::Nanosecond => Some(
                    a.as_any()
                        .downcast_ref::<TimestampNanosecondArray>()
                        .unwrap()
                        .value(i)
                        / 1000,
                ),
            }
            .ok_or_else(|| EngineError::new("22008", "timestamp out of range"))?,
        ),
        DataType::FixedSizeBinary(16) => {
            let bytes: [u8; 16] = a
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .unwrap()
                .value(i)
                .try_into()
                .map_err(|_| EngineError::new("22P02", "invalid fixed-width binary UUID"))?;
            Cell::Uuid(bytes)
        }
        DataType::Interval(unit) => match unit {
            IntervalUnit::MonthDayNano => {
                let v = a
                    .as_any()
                    .downcast_ref::<IntervalMonthDayNanoArray>()
                    .unwrap()
                    .value(i);
                Cell::Interval {
                    months: v.months,
                    days: v.days,
                    micros: v.nanoseconds / 1000,
                }
            }
            IntervalUnit::DayTime => {
                let v = a
                    .as_any()
                    .downcast_ref::<IntervalDayTimeArray>()
                    .unwrap()
                    .value(i);
                Cell::Interval {
                    months: 0,
                    days: v.days,
                    micros: i64::from(v.milliseconds) * 1000,
                }
            }
            IntervalUnit::YearMonth => {
                let months = a
                    .as_any()
                    .downcast_ref::<IntervalYearMonthArray>()
                    .unwrap()
                    .value(i);
                Cell::Interval {
                    months,
                    days: 0,
                    micros: 0,
                }
            }
        },
        DataType::Struct(fields) => {
            if fields.len() == 2
                && fields[0].name() == "x"
                && fields[1].name() == "y"
                && fields[0].data_type() == &DataType::Float64
                && fields[1].data_type() == &DataType::Float64
            {
                let s = a.as_any().downcast_ref::<StructArray>().unwrap();
                let xs = s.column(0).as_any().downcast_ref::<Float64Array>().unwrap();
                let ys = s.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
                if xs.is_null(i) || ys.is_null(i) {
                    return Err(EngineError::new(
                        "22P02",
                        "POINT coordinate components must be non-null",
                    ));
                }
                Cell::Point {
                    x: xs.value(i),
                    y: ys.value(i),
                }
            } else {
                return Err(EngineError::unsupported(
                    "result value encoding is unsupported",
                ));
            }
        }
        DataType::List(field) => {
            if matches!(
                field.data_type(),
                DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _)
            ) {
                return Err(EngineError::unsupported(
                    "multi-dimensional arrays are not implemented",
                ));
            }
            let values = a.as_any().downcast_ref::<ListArray>().unwrap().value(i);
            Cell::Array {
                lower_bound: 1,
                values: (0..values.len())
                    .map(|i| cell(values.as_ref(), i))
                    .collect::<Result<_>>()?,
            }
        }
        _ => {
            return Err(EngineError::unsupported(
                "result value encoding is unsupported",
            ));
        }
    })
}
fn decimal(mut s: String, scale: i8) -> String {
    if scale <= 0 {
        s.push_str(&"0".repeat((-scale) as usize));
        return s;
    }
    let neg = s.starts_with('-');
    if neg {
        s.remove(0);
    }
    while s.len() <= scale as usize {
        s.insert(0, '0');
    }
    s.insert(s.len() - scale as usize, '.');
    if neg {
        s.insert(0, '-');
    }
    s
}
pub(super) fn to_duckdb(c: &Cell) -> Value {
    match c {
        Cell::Oid(v) => Value::UInt(*v),
        Cell::NamedOid { oid, .. } => Value::UInt(*oid),
        Cell::Array { values, .. } => Value::List(values.iter().map(to_duckdb).collect()),
        Cell::Null => Value::Null,
        Cell::Bool(v) => Value::Boolean(*v),
        Cell::I16(v) => Value::SmallInt(*v),
        Cell::I32(v) => Value::Int(*v),
        Cell::I64(v) => Value::BigInt(*v),
        Cell::F32(v) => Value::Float(*v),
        Cell::F64(v) => Value::Double(*v),
        Cell::Text(v) | Cell::Numeric(v) => Value::Text(v.clone()),
        Cell::Bytes(v) => Value::Blob(v.clone()),
        Cell::Date(v) => Value::Date32(*v),
        Cell::Time(v) => Value::Time64(duckdb::types::TimeUnit::Microsecond, *v),
        Cell::Timestamp(v) => Value::Timestamp(duckdb::types::TimeUnit::Microsecond, *v),
        Cell::Interval {
            months,
            days,
            micros,
        } => match micros.checked_mul(1000) {
            Some(nanos) => Value::Interval {
                months: *months,
                days: *days,
                nanos,
            },
            None => Value::Text(modorch_fedengine_pg_compat::format_interval(
                *months, *days, *micros,
            )),
        },
        Cell::Point { x, y } => Value::Struct(OrderedMap::from(vec![
            ("x".to_string(), Value::Double(*x)),
            ("y".to_string(), Value::Double(*y)),
        ])),
        Cell::Uuid(v) => Value::Text(uuid_text(v)),
    }
}
fn uuid_text(v: &[u8; 16]) -> String {
    let h = v.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Normalize physical Arrow values to the proven PostgreSQL result type, checking narrowing.
pub(super) fn coerce(c: Cell, t: PgType) -> Result<Cell> {
    if matches!(c, Cell::Null) {
        return Ok(c);
    }
    let number = match &c {
        Cell::I16(v) => Some(i64::from(*v)),
        Cell::I32(v) => Some(i64::from(*v)),
        Cell::I64(v) => Some(*v),
        _ => None,
    };
    let range = || {
        EngineError::new(
            "22003",
            "metadata integer is outside its PostgreSQL type range",
        )
    };
    Ok(match (t, number, c) {
        (PgType::Int2, Some(n), _) => Cell::I16(n.try_into().map_err(|_| range())?),
        (PgType::Int4, Some(n), _) => Cell::I32(n.try_into().map_err(|_| range())?),
        (PgType::Int8, Some(n), _) => Cell::I64(n),
        (PgType::Catalog(24 | 26 | 28 | 29 | 2205 | 2206), Some(n), _) => {
            Cell::Oid(n.try_into().map_err(|_| range())?)
        }
        (PgType::Numeric, _, Cell::Text(v)) => Cell::Numeric(v),
        (PgType::Catalog(oid), _, Cell::Array { values, .. }) => {
            let element =
                modorch_fedengine_pg_compat::array_element_type(oid).unwrap_or(PgType::Text);
            Cell::Array {
                lower_bound: if matches!(oid, 22 | 30) { 0 } else { 1 },
                values: values
                    .into_iter()
                    .map(|c| coerce(c, element))
                    .collect::<Result<_>>()?,
            }
        }
        (_, _, c) => c,
    })
}
