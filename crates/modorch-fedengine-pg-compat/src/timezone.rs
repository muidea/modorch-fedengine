//! PostgreSQL session/expression timezone parsing, DuckDB ICU mapping, and temporal formatting.

use chrono::{DateTime, Datelike, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Utc};
use modorch_fedengine_core::{EngineError, Result};
use sqlparser::ast::{DataType, Expr, Interval, TypedString, UnaryOperator, Value};

const MIN_DUCKDB_FIXED_OFFSET_HOURS: i32 = -12;
const MAX_DUCKDB_FIXED_OFFSET_HOURS: i32 = 14;
const MAX_PG_TIMEZONE_HOURS: i32 = 167;
const MAX_PG_TIMEZONE_SECONDS: f64 = 167.0 * 3600.0 + 59.0 * 60.0 + 59.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeZoneKind {
    Utc,
    Fixed(i32),
    Iana(chrono_tz::Tz),
}

/// Active PostgreSQL session timezone mapped consistently onto `chrono` / `chrono-tz`
/// and DuckDB's ICU `TimeZone` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimeZone {
    pub display: String,
    pub duckdb_zone: String,
    pub is_utc: bool,
    kind: TimeZoneKind,
}

impl Default for SessionTimeZone {
    fn default() -> Self {
        Self::utc()
    }
}

impl SessionTimeZone {
    /// Canonical UTC session timezone.
    pub fn utc() -> Self {
        Self {
            display: "UTC".into(),
            duckdb_zone: "UTC".into(),
            is_utc: true,
            kind: TimeZoneKind::Utc,
        }
    }

    /// Returns the UTC offset in seconds (east of UTC) at the given UTC microsecond instant.
    pub fn offset_seconds_at_utc(&self, utc_micros: i64) -> Result<i32> {
        let dt = DateTime::from_timestamp_micros(utc_micros)
            .ok_or_else(|| EngineError::new("22008", "timestamp out of range"))?;
        validate_utc_year(dt)?;
        Ok(match self.kind {
            TimeZoneKind::Utc => 0,
            TimeZoneKind::Fixed(east_seconds) => east_seconds,
            TimeZoneKind::Iana(tz) => {
                use chrono::Offset;
                tz.offset_from_utc_datetime(&dt.naive_utc())
                    .fix()
                    .local_minus_utc()
            }
        })
    }

    /// Formats a UTC microsecond instant as PostgreSQL `TIMESTAMPTZ` text in this session timezone.
    pub fn format_timestamptz(&self, utc_micros: i64) -> Result<String> {
        let dt = DateTime::from_timestamp_micros(utc_micros)
            .ok_or_else(|| EngineError::new("22008", "timestamp out of range"))?;
        validate_utc_year(dt)?;
        let east_seconds = self.offset_seconds_at_utc(utc_micros)?;
        let local = dt.naive_utc() + chrono::Duration::seconds(i64::from(east_seconds));
        Ok(format!(
            "{}{}",
            local.format("%Y-%m-%d %H:%M:%S%.f"),
            format_pg_offset_suffix(east_seconds)
        ))
    }

    /// Formats a PostgreSQL `TIMETZ` value (`micros` in `0..=86_400_000_000`, `west_offset` seconds).
    pub fn format_timetz(&self, micros: i64, west_offset: i32) -> Result<String> {
        if !(0..=86_400_000_000).contains(&micros) || west_offset.abs() > 15 * 3600 + 59 * 60 + 59 {
            return Err(EngineError::new("22P02", "invalid timetz value"));
        }
        let clock = format_time_micros(micros)?;
        Ok(format!("{clock}{}", format_pg_offset_suffix(-west_offset)))
    }

    /// Interprets a naive local timestamp (without explicit offset) in this session timezone
    /// and returns UTC microseconds, enforcing UTC year `[1..=9999]`.
    pub fn local_naive_to_utc_micros(&self, naive: NaiveDateTime) -> Result<i64> {
        let utc = match self.kind {
            TimeZoneKind::Utc => naive.and_utc(),
            TimeZoneKind::Fixed(east_seconds) => {
                let fixed = FixedOffset::east_opt(east_seconds)
                    .ok_or_else(|| EngineError::new("22023", "invalid fixed timezone offset"))?;
                fixed
                    .from_local_datetime(&naive)
                    .single()
                    .ok_or_else(|| EngineError::new("22007", "invalid timestamp for timezone"))?
                    .with_timezone(&Utc)
            }
            TimeZoneKind::Iana(tz) => match tz.from_local_datetime(&naive) {
                LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
                LocalResult::None => {
                    return Err(EngineError::new(
                        "22007",
                        "timestamp does not exist in active session timezone",
                    ));
                }
            },
        };
        validate_utc_year(utc)?;
        Ok(utc.timestamp_micros())
    }
}

/// Validates that the UTC timestamp falls within PostgreSQL's `[0001..9999]` year range.
pub fn validate_utc_year(dt: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let year = dt.year();
    if !(1..=9999).contains(&year) {
        return Err(EngineError::new(
            "22008",
            "timestamp out of PostgreSQL year range [1..9999]",
        ));
    }
    Ok(dt)
}

/// Formats a time-of-day microsecond count (`0..=86_400_000_000`) as PostgreSQL text.
pub fn format_time_micros(micros: i64) -> Result<String> {
    if !(0..=86_400_000_000).contains(&micros) {
        return Err(EngineError::new("22P02", "invalid time representation"));
    }
    if micros == 86_400_000_000 {
        return Ok("24:00:00".into());
    }
    let secs = (micros / 1_000_000) as u32;
    let nanos = ((micros % 1_000_000) * 1000) as u32;
    let t = chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)
        .ok_or_else(|| EngineError::new("22P02", "invalid time representation"))?;
    Ok(t.format("%H:%M:%S%.f").to_string())
}

/// Formats an east-of-UTC offset in seconds using PostgreSQL wire text rules
/// (`+00`, `+08`, `-04`, `+05:30`, `+05:30:15`).
pub fn format_pg_offset_suffix(east_seconds: i32) -> String {
    let sign = if east_seconds < 0 { '-' } else { '+' };
    let abs = east_seconds.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    let s = abs % 60;
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else if m != 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}")
    }
}

/// Formats an east-of-UTC offset in seconds as signed `±HH:MM[:SS]`.
pub fn format_signed_duration_offset(east_seconds: i32) -> String {
    let sign = if east_seconds < 0 { '-' } else { '+' };
    let abs = east_seconds.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    let s = abs % 60;
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

fn format_east_offset(hours: i32) -> String {
    if hours >= 0 {
        format!("+{hours:02}:00")
    } else {
        format!("-{:02}:00", -hours)
    }
}

fn fixed_hour_session_time_zone(
    east_seconds: i32,
    display_override: Option<String>,
) -> Result<SessionTimeZone> {
    if east_seconds == 0 {
        if let Some(disp) = display_override
            && !disp.eq_ignore_ascii_case("UTC")
            && !disp.eq_ignore_ascii_case("GMT")
        {
            return Ok(SessionTimeZone {
                display: disp,
                duckdb_zone: "UTC".into(),
                is_utc: true,
                kind: TimeZoneKind::Utc,
            });
        }
        return Ok(SessionTimeZone::utc());
    }
    let label = display_override
        .clone()
        .unwrap_or_else(|| format_signed_duration_offset(east_seconds));
    if east_seconds % 3600 != 0 {
        return Err(EngineError::unsupported(format!(
            "fixed sub-hour timezone offset {label:?} is not supported by DuckDB ICU; use an IANA timezone name such as 'Asia/Kolkata'"
        )));
    }
    let hours = east_seconds / 3600;
    if !(MIN_DUCKDB_FIXED_OFFSET_HOURS..=MAX_DUCKDB_FIXED_OFFSET_HOURS).contains(&hours) {
        return Err(EngineError::unsupported(format!(
            "fixed timezone offset {label:?} is outside DuckDB supported whole-hour range [-12:00, +14:00]"
        )));
    }
    let canonical = format_east_offset(hours);
    let display = display_override.unwrap_or(canonical);
    // DuckDB ICU follows IANA Etc/GMT sign convention (inverted relative to ISO-8601).
    let duckdb_zone = if hours < 0 {
        format!("Etc/GMT+{}", -hours)
    } else {
        format!("Etc/GMT-{hours}")
    };
    Ok(SessionTimeZone {
        display,
        duckdb_zone,
        is_utc: false,
        kind: TimeZoneKind::Fixed(east_seconds),
    })
}

fn is_canonical_utc_alias(trimmed: &str) -> bool {
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "DEFAULT"
            | "LOCAL"
            | "UTC"
            | "GMT"
            | "Z"
            | "ETC/UTC"
            | "ETC/GMT"
            | "ETC/GMT+0"
            | "ETC/GMT-0"
            | "UNIVERSAL"
            | "UCT"
            | "GREENWICH"
    )
}

fn split_hms_fields(rest: &str) -> Option<(&str, &str, &str)> {
    if rest.is_empty() {
        return None;
    }
    if rest.contains(':') {
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.as_slice() {
            [h, m]
                if (1..=2).contains(&h.len())
                    && m.len() == 2
                    && h.bytes().all(|b| b.is_ascii_digit())
                    && m.bytes().all(|b| b.is_ascii_digit()) =>
            {
                Some((h, m, ""))
            }
            [h, m, s]
                if (1..=2).contains(&h.len())
                    && m.len() == 2
                    && s.len() == 2
                    && h.bytes().all(|b| b.is_ascii_digit())
                    && m.bytes().all(|b| b.is_ascii_digit())
                    && s.bytes().all(|b| b.is_ascii_digit()) =>
            {
                Some((h, m, s))
            }
            _ => None,
        }
    } else {
        if !rest.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match rest.len() {
            1 | 2 => Some((rest, "", "")),
            4 => Some((&rest[..2], &rest[2..4], "")),
            6 => Some((&rest[..2], &rest[2..4], &rest[4..6])),
            _ => None,
        }
    }
}

fn split_interval_clock(trimmed: &str) -> Option<(char, &str, &str, &str)> {
    let (sign, rest) = if let Some(r) = trimmed.strip_prefix('+') {
        ('+', r)
    } else if let Some(r) = trimmed.strip_prefix('-') {
        ('-', r)
    } else {
        ('+', trimmed)
    };
    let parts: Vec<&str> = rest.split(':').collect();
    match parts.as_slice() {
        [h, m]
            if (1..=3).contains(&h.len())
                && m.len() == 2
                && h.bytes().all(|b| b.is_ascii_digit())
                && m.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some((sign, h, m, ""))
        }
        [h, m, s]
            if (1..=3).contains(&h.len())
                && m.len() == 2
                && s.len() == 2
                && h.bytes().all(|b| b.is_ascii_digit())
                && m.bytes().all(|b| b.is_ascii_digit())
                && s.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some((sign, h, m, s))
        }
        _ => None,
    }
}

fn parse_hms_parts(sign: char, h_str: &str, m_str: &str, s_str: &str) -> Option<i32> {
    let hours: i32 = h_str.parse().ok()?;
    let mins: i32 = if m_str.is_empty() {
        0
    } else {
        m_str.parse().ok()?
    };
    let secs: i32 = if s_str.is_empty() {
        0
    } else {
        s_str.parse().ok()?
    };
    if !(0..60).contains(&mins) || !(0..60).contains(&secs) {
        return None;
    }
    if hours > MAX_PG_TIMEZONE_HOURS || (hours == MAX_PG_TIMEZONE_HOURS && (mins > 59 || secs > 59))
    {
        return None;
    }
    let total = hours * 3600 + mins * 60 + secs;
    Some(if sign == '-' { -total } else { total })
}

fn invalid_tz_err(raw: &str) -> EngineError {
    EngineError::config(format!("invalid value for parameter \"TimeZone\": {raw:?}"))
}

fn parse_iana_tz(trimmed: &str) -> Option<chrono_tz::Tz> {
    if let Ok(tz) = trimmed.parse::<chrono_tz::Tz>() {
        return Some(tz);
    }
    chrono_tz::TZ_VARIANTS
        .iter()
        .find(|tz| tz.name().eq_ignore_ascii_case(trimmed))
        .copied()
}

/// Parses a PostgreSQL timezone specification string (IANA name, DEFAULT/LOCAL,
/// ISO-8601 fixed offset `±HH[:MM[:SS]]`, or PG POSIX `UTC±H`/`GMT±H`).
pub fn parse_session_timezone(raw: &str) -> Result<SessionTimeZone> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(EngineError::config(
            "invalid value for parameter \"TimeZone\": \"\"",
        ));
    }
    if is_canonical_utc_alias(trimmed) {
        return Ok(SessionTimeZone::utc());
    }
    if let Some(first) = trimmed.as_bytes().first().copied()
        && (first == b'+' || first == b'-')
        && let Some((h, m, s)) = split_hms_fields(&trimmed[1..])
    {
        let total = parse_hms_parts(first as char, h, m, s).ok_or_else(|| invalid_tz_err(raw))?;
        if total == 0 {
            return Ok(SessionTimeZone::utc());
        }
        return fixed_hour_session_time_zone(total, None);
    }
    if trimmed.len() > 4 {
        let prefix = &trimmed[..3];
        if prefix.eq_ignore_ascii_case("UTC") || prefix.eq_ignore_ascii_case("GMT") {
            let sign = trimmed.as_bytes()[3] as char;
            if (sign == '+' || sign == '-')
                && let Some((h, m, s)) = split_hms_fields(&trimmed[4..])
            {
                let total = parse_hms_parts('+', h, m, s).ok_or_else(|| invalid_tz_err(raw))?;
                // POSIX UTC+N means N hours WEST of UTC (-N*3600); UTC-N means +N*3600.
                let east_seconds = if sign == '-' { total } else { -total };
                if east_seconds == 0 {
                    return Ok(SessionTimeZone::utc());
                }
                let mut canonical = format!("{}{sign}{h}", prefix.to_ascii_uppercase());
                if !m.is_empty() {
                    canonical.push(':');
                    canonical.push_str(m);
                }
                if !s.is_empty() {
                    canonical.push(':');
                    canonical.push_str(s);
                }
                return fixed_hour_session_time_zone(east_seconds, Some(canonical));
            }
        }
    }
    let tz = parse_iana_tz(trimmed).ok_or_else(|| invalid_tz_err(raw))?;
    Ok(SessionTimeZone {
        display: tz.name().into(),
        duckdb_zone: tz.name().into(),
        is_utc: tz == chrono_tz::Tz::UTC,
        kind: TimeZoneKind::Iana(tz),
    })
}

fn is_numeric_hours_str(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    match body.split_once('.') {
        Some((int_part, frac_part)) => {
            !int_part.is_empty()
                && !frac_part.is_empty()
                && int_part.bytes().all(|b| b.is_ascii_digit())
                && frac_part.bytes().all(|b| b.is_ascii_digit())
        }
        None => body.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// Parses an unquoted numeric offset as used by PostgreSQL `SET TIME ZONE <number>` (east-of-UTC hours).
pub fn parse_numeric_timezone_hours(raw: &str) -> Result<SessionTimeZone> {
    let trimmed = raw.trim();
    if !is_numeric_hours_str(trimmed) {
        return Err(invalid_tz_err(raw));
    }
    let hours_float: f64 = trimmed.parse().map_err(|_| invalid_tz_err(raw))?;
    if !hours_float.is_finite() || !(-167.999..=167.999).contains(&hours_float) {
        return Err(EngineError::config(format!(
            "time zone displacement out of range: {raw:?}"
        )));
    }
    if let Ok(whole) = trimmed.strip_prefix('+').unwrap_or(trimmed).parse::<i32>() {
        if whole == 0 {
            return Ok(SessionTimeZone::utc());
        }
        return fixed_hour_session_time_zone(whole * 3600, None);
    }
    if hours_float == 0.0 {
        return Ok(SessionTimeZone::utc());
    }
    if hours_float.fract() == 0.0 {
        return fixed_hour_session_time_zone((hours_float as i32) * 3600, None);
    }
    Err(EngineError::unsupported(format!(
        "fractional fixed timezone offset {raw:?} is not supported by DuckDB ICU; use a whole-hour offset or IANA timezone name"
    )))
}

fn invalid_interval_err(raw: &str) -> EngineError {
    EngineError::config(format!("invalid input syntax for type interval: {raw:?}"))
}

/// Parses a PostgreSQL `INTERVAL` timezone literal such as `'+08:00'`, `'8 hours'`, or `'-5 hours 30 minutes'`.
pub fn parse_timezone_interval_literal(raw: &str) -> Result<SessionTimeZone> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid_interval_err(raw));
    }
    if let Some((sign, h, m, s)) = split_interval_clock(trimmed) {
        let total = parse_hms_parts(sign, h, m, s).ok_or_else(|| invalid_interval_err(raw))?;
        if total == 0 {
            return Ok(SessionTimeZone::utc());
        }
        return fixed_hour_session_time_zone(total, None);
    }
    if is_numeric_hours_str(trimmed) {
        let val: f64 = trimmed.parse().map_err(|_| invalid_interval_err(raw))?;
        if val == 0.0 {
            return Ok(SessionTimeZone::utc());
        }
        let secs = val as i64;
        if (secs as f64) != val
            || secs % 3600 != 0
            || !(-MAX_PG_TIMEZONE_SECONDS..=MAX_PG_TIMEZONE_SECONDS).contains(&val)
        {
            if !(-MAX_PG_TIMEZONE_SECONDS..=MAX_PG_TIMEZONE_SECONDS).contains(&val) {
                return Err(EngineError::config(format!(
                    "time zone displacement out of range: {raw:?}"
                )));
            }
            return Err(EngineError::unsupported(format!(
                "fixed sub-hour timezone interval {raw:?} is not supported by DuckDB ICU; use an IANA timezone name"
            )));
        }
        return fixed_hour_session_time_zone(secs as i32, None);
    }

    // Parse PostgreSQL verbose interval units (e.g. '8 hours', '-5 hours 30 minutes').
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut total_seconds = 0.0f64;
    let mut matched_any = false;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let num_start = i;
        if bytes[i] == b'+' || bytes[i] == b'-' {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return Err(invalid_interval_err(raw));
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            let frac_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == frac_start {
                return Err(invalid_interval_err(raw));
            }
        }
        let val: f64 = trimmed[num_start..i]
            .parse()
            .map_err(|_| invalid_interval_err(raw))?;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i == unit_start {
            return Err(invalid_interval_err(raw));
        }
        let unit = trimmed[unit_start..i].to_ascii_lowercase();
        match unit.as_str() {
            "hour" | "hours" | "hr" | "hrs" | "h" => total_seconds += val * 3600.0,
            "minute" | "minutes" | "min" | "mins" | "m" => total_seconds += val * 60.0,
            "second" | "seconds" | "sec" | "secs" | "s" => total_seconds += val,
            "day" | "days" | "d" | "week" | "weeks" | "wk" | "wks" | "month" | "months" | "mon"
            | "mons" | "year" | "years" | "yr" | "yrs" => {
                return Err(EngineError::config(format!(
                    "interval time zone displacement cannot contain {unit}: {raw:?}"
                )));
            }
            _ => return Err(invalid_interval_err(raw)),
        }
        matched_any = true;
    }
    if !matched_any {
        return Err(invalid_interval_err(raw));
    }
    if !(-MAX_PG_TIMEZONE_SECONDS..=MAX_PG_TIMEZONE_SECONDS).contains(&total_seconds) {
        return Err(EngineError::config(format!(
            "time zone displacement out of range: {raw:?}"
        )));
    }
    if total_seconds == 0.0 {
        return Ok(SessionTimeZone::utc());
    }
    let rounded = total_seconds as i32;
    if (rounded as f64) != total_seconds || rounded == 0 {
        return Err(EngineError::unsupported(format!(
            "fixed sub-hour timezone interval {raw:?} is not supported by DuckDB ICU; use an IANA timezone name"
        )));
    }
    fixed_hour_session_time_zone(rounded, None)
}

fn is_interval_data_type(ty: &DataType) -> bool {
    match ty {
        DataType::Interval { .. } => true,
        DataType::Custom(name, mods) if mods.is_empty() => {
            let lower = name.to_string().to_ascii_lowercase();
            lower == "interval" || lower == "pg_catalog.interval"
        }
        _ => false,
    }
}

/// Extracts a `(literal, is_interval)` pair from a constant timezone expression if possible.
pub fn extract_interval_or_string_literal(expr: &Expr) -> Option<(String, bool)> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => Some((s.clone(), false)),
            _ => None,
        },
        Expr::Interval(Interval {
            value,
            leading_field,
            last_field,
            ..
        }) => {
            let Expr::Value(v) = value.as_ref() else {
                return None;
            };
            let Value::SingleQuotedString(s) = &v.value else {
                return None;
            };
            let text = if let Some(field) = leading_field
                && last_field.is_none()
                && !s.bytes().any(|b| b.is_ascii_alphabetic())
                && !s.contains(':')
            {
                format!("{s} {field}")
            } else {
                s.clone()
            };
            Some((text, true))
        }
        Expr::TypedString(TypedString {
            data_type, value, ..
        }) if is_interval_data_type(data_type) => match &value.value {
            Value::SingleQuotedString(s) => Some((s.clone(), true)),
            _ => None,
        },
        Expr::Cast {
            expr: inner,
            data_type,
            ..
        } if is_interval_data_type(data_type) => {
            if let Expr::Value(v) = inner.as_ref()
                && let Value::SingleQuotedString(s) = &v.value
            {
                Some((s.clone(), true))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parses a SQL expression argument passed to `SET TIME ZONE <expr>` or `SET timezone = <expr>`.
pub fn parse_timezone_expr(expr: &Expr) -> Result<SessionTimeZone> {
    if let Some((literal, is_interval)) = extract_interval_or_string_literal(expr) {
        return if is_interval {
            parse_timezone_interval_literal(&literal)
        } else {
            parse_session_timezone(&literal)
        };
    }
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => parse_numeric_timezone_hours(n),
            _ => Err(EngineError::unsupported("setting requires a literal value")),
        },
        Expr::UnaryOp { op, expr: inner }
            if matches!(op, UnaryOperator::Plus | UnaryOperator::Minus) =>
        {
            let Expr::Value(v) = inner.as_ref() else {
                return Err(EngineError::unsupported("setting requires a literal value"));
            };
            let Value::Number(n, _) = &v.value else {
                return Err(EngineError::unsupported("setting requires a literal value"));
            };
            let prefix = if *op == UnaryOperator::Minus {
                "-"
            } else {
                "+"
            };
            parse_numeric_timezone_hours(&format!("{prefix}{n}"))
        }
        Expr::Identifier(i) => parse_session_timezone(&i.value),
        _ => Err(EngineError::unsupported("setting requires a literal value")),
    }
}

/// Translates constant PostgreSQL `AT TIME ZONE` / `timezone(tz, expr)` offset or interval
/// arguments into DuckDB ICU `Etc/GMT∓H` identifiers when DuckDB would otherwise reject ISO
/// offsets or invert POSIX `UTC±H` signs. Returns `Ok(None)` when `raw` does not require rewrite.
pub fn parse_timezone_literal_for_at_time_zone(
    raw: &str,
    is_interval: bool,
) -> Result<Option<String>> {
    if is_interval {
        let tz = parse_timezone_interval_literal(raw)?;
        return Ok(Some(tz.duckdb_zone));
    }
    let trimmed = raw.trim();
    let looks_iso = trimmed
        .as_bytes()
        .first()
        .is_some_and(|b| *b == b'+' || *b == b'-')
        && split_hms_fields(&trimmed[1..]).is_some();
    let looks_posix = trimmed.len() > 4
        && (trimmed[..3].eq_ignore_ascii_case("UTC") || trimmed[..3].eq_ignore_ascii_case("GMT"))
        && matches!(trimmed.as_bytes()[3], b'+' | b'-')
        && split_hms_fields(&trimmed[4..]).is_some();
    if looks_iso || looks_posix {
        let tz = parse_session_timezone(trimmed)?;
        return Ok(Some(tz.duckdb_zone));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn parse_session_timezone_mappings_and_errors() {
        let ref_utc = NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        for (input, disp, duckdb, is_utc, offset) in [
            ("UTC", "UTC", "UTC", true, 0),
            ("utc", "UTC", "UTC", true, 0),
            ("Local", "UTC", "UTC", true, 0),
            ("DEFAULT", "UTC", "UTC", true, 0),
            ("GMT", "UTC", "UTC", true, 0),
            ("Z", "UTC", "UTC", true, 0),
            ("Etc/UTC", "UTC", "UTC", true, 0),
            (
                "Asia/Shanghai",
                "Asia/Shanghai",
                "Asia/Shanghai",
                false,
                8 * 3600,
            ),
            (
                "asia/shanghai",
                "Asia/Shanghai",
                "Asia/Shanghai",
                false,
                8 * 3600,
            ),
            (
                "Asia/Kolkata",
                "Asia/Kolkata",
                "Asia/Kolkata",
                false,
                5 * 3600 + 1800,
            ),
            ("US/Eastern", "US/Eastern", "US/Eastern", false, -4 * 3600),
            ("+08:00", "+08:00", "Etc/GMT-8", false, 8 * 3600),
            ("+0800", "+08:00", "Etc/GMT-8", false, 8 * 3600),
            ("-05", "-05:00", "Etc/GMT+5", false, -5 * 3600),
            ("+14:00", "+14:00", "Etc/GMT-14", false, 14 * 3600),
            ("-12:00", "-12:00", "Etc/GMT+12", false, -12 * 3600),
            ("+00:00", "UTC", "UTC", true, 0),
            ("UTC+8", "UTC+8", "Etc/GMT+8", false, -8 * 3600),
            ("UTC-8", "UTC-8", "Etc/GMT-8", false, 8 * 3600),
            ("GMT+5", "GMT+5", "Etc/GMT+5", false, -5 * 3600),
            ("GMT-0", "UTC", "UTC", true, 0),
        ] {
            let tz = parse_session_timezone(input).unwrap();
            assert_eq!(tz.display, disp, "{input}");
            assert_eq!(tz.duckdb_zone, duckdb, "{input}");
            assert_eq!(tz.is_utc, is_utc, "{input}");
            assert_eq!(
                tz.offset_seconds_at_utc(ref_utc).unwrap(),
                offset,
                "{input}"
            );
        }
        for (bad, state) in [
            ("+05:30", "0A000"),
            ("UTC-5:30", "0A000"),
            ("+15:00", "0A000"),
            ("-13:00", "0A000"),
            ("", "22023"),
            ("Mars/Olympus_Mons", "22023"),
            ("+08:99", "22023"),
            ("+168:00", "22023"),
        ] {
            assert_eq!(
                parse_session_timezone(bad).unwrap_err().sqlstate,
                state,
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_numeric_and_interval_and_at_time_zone_literals() {
        assert_eq!(
            parse_numeric_timezone_hours("8").unwrap().duckdb_zone,
            "Etc/GMT-8"
        );
        assert_eq!(
            parse_numeric_timezone_hours("-5").unwrap().duckdb_zone,
            "Etc/GMT+5"
        );
        assert_eq!(
            parse_numeric_timezone_hours("0").unwrap().duckdb_zone,
            "UTC"
        );
        assert_eq!(
            parse_numeric_timezone_hours("5.5").unwrap_err().sqlstate,
            "0A000"
        );
        assert_eq!(
            parse_numeric_timezone_hours("15").unwrap_err().sqlstate,
            "0A000"
        );
        assert_eq!(
            parse_numeric_timezone_hours("200").unwrap_err().sqlstate,
            "22023"
        );

        assert_eq!(
            parse_timezone_interval_literal("+08:00")
                .unwrap()
                .duckdb_zone,
            "Etc/GMT-8"
        );
        assert_eq!(
            parse_timezone_interval_literal("-5 hours")
                .unwrap()
                .duckdb_zone,
            "Etc/GMT+5"
        );
        assert_eq!(
            parse_timezone_interval_literal("5 hours 30 minutes")
                .unwrap_err()
                .sqlstate,
            "0A000"
        );
        assert_eq!(
            parse_timezone_interval_literal("1 day")
                .unwrap_err()
                .sqlstate,
            "22023"
        );

        assert_eq!(
            parse_timezone_literal_for_at_time_zone("+08:00", false)
                .unwrap()
                .as_deref(),
            Some("Etc/GMT-8")
        );
        assert_eq!(
            parse_timezone_literal_for_at_time_zone("UTC+8", false)
                .unwrap()
                .as_deref(),
            Some("Etc/GMT+8")
        );
        assert_eq!(
            parse_timezone_literal_for_at_time_zone("8 hours", true)
                .unwrap()
                .as_deref(),
            Some("Etc/GMT-8")
        );
        assert_eq!(
            parse_timezone_literal_for_at_time_zone("Asia/Kolkata", false).unwrap(),
            None
        );
        assert_eq!(
            parse_timezone_literal_for_at_time_zone("+05:30", false)
                .unwrap_err()
                .sqlstate,
            "0A000"
        );
        assert_eq!(
            parse_timezone_literal_for_at_time_zone("UTC+8:99", false)
                .unwrap_err()
                .sqlstate,
            "22023"
        );
    }

    #[test]
    fn dst_formatting_and_year_bounds() {
        let ny = parse_session_timezone("America/New_York").unwrap();
        let winter = NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        let summer = NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        assert_eq!(
            ny.format_timestamptz(winter).unwrap(),
            "2026-01-15 07:00:00-05"
        );
        assert_eq!(
            ny.format_timestamptz(summer).unwrap(),
            "2026-06-15 08:00:00-04"
        );
        let kolkata = parse_session_timezone("Asia/Kolkata").unwrap();
        assert_eq!(
            kolkata.format_timestamptz(summer).unwrap(),
            "2026-06-15 17:30:00+05:30"
        );
        let y0 = NaiveDate::from_ymd_opt(0, 12, 31)
            .unwrap()
            .and_hms_opt(23, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        assert_eq!(
            SessionTimeZone::utc()
                .format_timestamptz(y0)
                .unwrap_err()
                .sqlstate,
            "22008"
        );
    }
}
