use modorch::FrameworkError;
use modorch_fedengine_core::EngineError;
pub(super) fn fw(e: EngineError) -> FrameworkError {
    FrameworkError::InvalidConfig(e.to_string())
}
pub(super) fn engine_error(e: duckdb::Error) -> EngineError {
    // DuckDB errors can embed SQL, ATTACH credentials or remote diagnostics. Never expose them verbatim.
    match &e {
        duckdb::Error::IntegralValueOutOfRange(_, _)
        | duckdb::Error::UnsignedIntegralValueOutOfRange(_, _) => {
            return EngineError::new("22003", "numeric value out of range");
        }
        duckdb::Error::InvalidParameterCount(_, _) => {
            return EngineError::new("08P01", "Bind parameter count mismatch");
        }
        duckdb::Error::Utf8Error(_) => {
            return EngineError::new("22021", "result text is not valid UTF-8");
        }
        _ => {}
    }
    let text = e.to_string().to_lowercase();
    let (code, message) = if text.starts_with("invalid input error: __fed_pg_division_by_zero")
        || text.starts_with("divide by zero error:")
    {
        ("22012", "division by zero")
    } else if text.starts_with("invalid input error: __fed_pg_relation_missing") {
        (
            "42P01",
            "referenced relation does not exist in the visible database",
        )
    } else if text.starts_with("invalid input error: __fed_pg_oid_range") {
        ("22003", "value is out of range for type oid")
    } else if text.starts_with("invalid input error: __fed_pg_database_missing") {
        ("3D000", "database does not exist")
    } else if text.starts_with("invalid input error: __fed_pg_schema_missing") {
        ("3F000", "schema does not exist")
    } else if text.starts_with("invalid input error: __fed_pg_invalid_privilege") {
        ("22023", "unrecognized privilege type")
    } else if text.starts_with("invalid input error: __fed_pg_invalid_fork") {
        ("22023", "invalid fork name")
    } else if text.starts_with("invalid input error: __fed_pg_relation_name") {
        ("42602", "invalid relation identifier")
    } else if text.starts_with("invalid input error: __fed_pg_cross_database") {
        ("0A000", "cross-database references are not implemented")
    } else if text.starts_with(
        "binder error: no function matches the given name and argument types '__fed_pg_decode",
    ) {
        (
            "42883",
            "function decode(text, text) does not match the supplied arguments",
        )
    } else if text.starts_with("invalid input error: __fed_pg_decode_escape") {
        ("22P02", "invalid input syntax for type bytea")
    } else if text.starts_with("invalid input error: __fed_pg_decode") {
        ("22023", "invalid data or encoding name for decode")
    } else if text.starts_with("invalid input error: __fed_pg_numeric_division") {
        (
            "0A000",
            "exact PostgreSQL numeric division is not implemented",
        )
    } else if (text.contains("conversion error") && text.contains("out of range"))
        || text.contains("out of range error")
    {
        ("22003", "numeric value out of range")
    } else if text.contains("conversion error") && text.contains("could not convert string") {
        ("22P02", "invalid input syntax for the requested type")
    } else if text.starts_with("invalid input error:")
        && (text.contains("could not convert") || text.contains("unimplemented type for cast"))
    {
        ("22000", "data conversion failed during execution")
    } else if text.contains("interrupt") {
        ("57014", "query canceled")
    } else if text.contains("password authentication failed")
        || text.contains("authentication failed")
    {
        ("28P01", "source authentication failed")
    } else if text.starts_with("connection error:")
        || text.starts_with("network error:")
        || (text.starts_with("io error:")
            && (text.contains("connect")
                || text.contains("connection")
                || text.contains("network")
                || text.contains("timed out")
                || text.contains("timeout")
                || text.contains("refused")
                || text.contains("reset")
                || text.contains("broken pipe")
                || text.contains("closed")))
    {
        ("08006", "source connection or network communication failed")
    } else if text.contains("permission denied")
        || text.contains("access denied")
        || text.contains("command denied")
    {
        ("42501", "source denied the requested metadata or operation")
    } else if text.contains("parser error") {
        ("42601", "DuckDB rejected SQL syntax")
    } else if text.contains("does not exist") {
        ("42P01", "referenced object does not exist")
    } else if text.contains("constraint error:")
        || text.contains("violates")
        || text.contains("constraint failed")
        || text.contains("duplicate key")
        || text.contains("unique constraint")
        || text.contains("foreign key constraint")
        || text.contains("check constraint")
        || text.contains("not null constraint")
    {
        ("23000", "remote or DuckDB constraint violation")
    } else if text.contains("memory") {
        ("53200", "DuckDB memory limit exceeded")
    } else if text.contains("libodbc.so.2") {
        (
            "0A000",
            "required ODBC runtime library (libodbc.so.2) is unavailable on this host",
        )
    } else if text.contains("not implemented") || text.contains("not supported") {
        (
            "0A000",
            "operation not supported by the locked engine/extension",
        )
    } else {
        (
            "XX000",
            "DuckDB execution failed; inspect the source state before retrying writes",
        )
    };
    EngineError::new(code, message)
}
pub(super) fn framework_error(e: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("execution admission failed: {e}"))
}

pub(super) fn map_sqlstate(code: &str) -> &'static str {
    match code {
        "01000" => "01000",
        "08006" => "08006",
        "08P01" => "08P01",
        "0A000" => "0A000",
        "22000" => "22000",
        "22001" => "22001",
        "22003" => "22003",
        "22007" => "22007",
        "22008" => "22008",
        "22012" => "22012",
        "22021" => "22021",
        "22023" => "22023",
        "22P02" => "22P02",
        "22P03" => "22P03",
        "23000" => "23000",
        "23502" => "23502",
        "23503" => "23503",
        "23505" => "23505",
        "23514" => "23514",
        "3D000" => "3D000",
        "3F000" => "3F000",
        "40001" => "40001",
        "42501" => "42501",
        "42601" => "42601",
        "42602" => "42602",
        "42701" => "42701",
        "42702" => "42702",
        "42703" => "42703",
        "42704" => "42704",
        "42804" => "42804",
        "42883" => "42883",
        "42P01" => "42P01",
        "42P07" => "42P07",
        "42P18" => "42P18",
        "53300" => "53300",
        "54000" => "54000",
        "55000" => "55000",
        "55P03" => "55P03",
        "57014" => "57014",
        "57P01" => "57P01",
        _ => "XX000",
    }
}

/// MySQL 8 information_schema strings can arrive with a binary collation.
/// Decode UTF-8 bytes explicitly instead of DuckDB's escaped BLOB-to-VARCHAR cast.
pub(super) fn metadata_text(
    row: &duckdb::Row<'_>,
    index: usize,
) -> modorch_fedengine_core::Result<String> {
    use duckdb::types::ValueRef;
    match row.get_ref(index).map_err(engine_error)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(|_| EngineError::new("22021", "metadata text is not valid UTF-8")),
        _ => Err(EngineError::new(
            "55000",
            "metadata text has an unexpected source type",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_errors_map_sqlstates_and_redact_credentials() {
        assert_eq!(
            engine_error(duckdb::Error::IntegralValueOutOfRange(0, 999)).sqlstate,
            "22003"
        );
        assert_eq!(
            engine_error(duckdb::Error::InvalidParameterCount(1, 2)).sqlstate,
            "08P01"
        );
        let secret = "postgres://admin:supersecret@db.internal:5432/prod";
        let conn_err = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(1),
            Some(format!("Connection Error: failed connecting to {secret}")),
        );
        let mapped = engine_error(conn_err);
        assert_eq!(mapped.sqlstate, "08006");
        assert!(!mapped.message.contains("supersecret"));
        assert!(!mapped.message.contains("db.internal"));

        let fqe_syntax = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(1),
            Some(
                r#"IO Error: FQE_SOURCE_ERROR {"source_error":{"version":1,"product":"mysql","state":"HY000","code":"1064","message":"Getting syntax error","constraint":""},"stage":"source_execute"}"#
                    .into(),
            ),
        );
        assert_ne!(engine_error(fqe_syntax).sqlstate, "23000");
    }
}
