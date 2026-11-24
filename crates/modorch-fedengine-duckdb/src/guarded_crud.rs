use crate::{
    codec::{cell, to_duckdb},
    error::{engine_error, map_sqlstate},
    worker::{Metadata, WorkerOutput, send_chunk},
};
use duckdb::{Connection, params_from_iter};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;

pub(super) fn crud_product(kind: DataSourceKind) -> Option<&'static str> {
    match kind {
        DataSourceKind::Postgres => Some("postgres"),
        DataSourceKind::Greenplum => Some("greenplum"),
        DataSourceKind::Kingbasees => Some("kingbasees"),
        DataSourceKind::Mysql | DataSourceKind::Mariadb => Some("mysql"),
        DataSourceKind::Oracle => Some("oracle"),
        DataSourceKind::Sqlserver => Some("sqlserver"),
        DataSourceKind::Dameng => Some("dameng"),
        DataSourceKind::Db2 => Some("db2"),
        DataSourceKind::Hana => Some("hana"),
        DataSourceKind::Starrocks => None,
    }
}

pub(super) fn is_native_scanner_catalog(
    conn: &Connection,
    catalog: &str,
    kind: DataSourceKind,
) -> bool {
    let expected = kind.attach_type();
    let expected_type = match expected {
        "oracle_scanner" => "oracle",
        "sqlserver_scanner" => "sqlserver",
        other => other,
    };
    conn.query_row(
        "SELECT lower(type) FROM duckdb_databases() WHERE database_name = ?",
        [catalog],
        |row| row.get::<_, String>(0),
    )
    .is_ok_and(|actual| actual == expected_type)
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(super) struct GuardedSourceError {
    #[serde(default)]
    pub(super) product: String,
    #[serde(default)]
    pub(super) state: String,
    #[serde(default)]
    pub(super) code: String,
    #[serde(default)]
    pub(super) message: String,
}

pub(super) fn decode_source_error(
    raw: &str,
) -> std::result::Result<Option<GuardedSourceError>, ()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed: GuardedSourceError = serde_json::from_str(trimmed).map_err(|_| ())?;
    if parsed.product.is_empty()
        && parsed.state.is_empty()
        && parsed.code.is_empty()
        && parsed.message.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(parsed))
}

pub(super) fn normalized_mutation_source_state(
    source: Option<&GuardedSourceError>,
    fallback: &str,
) -> &'static str {
    if let Some(err) = source
        && err.product.eq_ignore_ascii_case("mysql")
    {
        match err.code.as_str() {
            "1452" => return "23503",
            "3819" => return "23514",
            "1048" => return "23502",
            "1265" => return "22P02",
            "1406" => return "22001",
            _ => {}
        }
    }
    let candidate = source
        .map(|s| s.state.as_str())
        .filter(|s| s.len() == 5)
        .unwrap_or(fallback);
    map_sqlstate(candidate)
}

pub(super) fn map_status_failure(
    status: &str,
    sqlstate: &str,
    stage: &str,
    source_error_json: &str,
    default_msg: &str,
) -> EngineError {
    let unknown = !matches!(status, "CONFIRMED_NOT_EXECUTED" | "CONFIRMED_ROLLED_BACK");
    let source = decode_source_error(source_error_json).ok().flatten();
    let state = normalized_mutation_source_state(source.as_ref(), sqlstate);
    let detail = source
        .as_ref()
        .map(|s| s.message.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or(default_msg);
    let msg = if stage.is_empty() {
        detail.to_string()
    } else {
        format!("{detail} (stage: {stage})")
    };
    let err = EngineError::new(state, msg);
    if unknown { err.unknown() } else { err }
}

pub(super) fn returning_oid_to_pg_type(oid: i64, mysql: bool) -> Result<PgType> {
    match oid {
        20 => Ok(PgType::Int8),
        21 => Ok(PgType::Int2),
        23 => Ok(PgType::Int4),
        2950 if !mysql => Ok(PgType::Uuid),
        _ => Err(EngineError::unsupported(
            "returned identifier column type is unsupported",
        )),
    }
}

pub(super) fn parse_returning_descriptor(
    descriptor_json: &str,
    col_name: &str,
    mysql: bool,
) -> Result<(String, i64, PgType)> {
    let doc: serde_json::Value = serde_json::from_str(descriptor_json)
        .map_err(|_| EngineError::new("XX000", "guarded metadata descriptor is invalid JSON"))?;
    let cols = doc
        .get("columns")
        .and_then(|v| v.as_array())
        .ok_or_else(|| EngineError::new("0A000", "guarded metadata columns are missing"))?;
    let col = cols
        .iter()
        .find(|c| {
            !c.get("dropped").and_then(|v| v.as_bool()).unwrap_or(false)
                && c.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n == col_name)
        })
        .ok_or_else(|| EngineError::new("42703", format!("column {col_name} does not exist")))?;
    let oid = col
        .get("pg_type")
        .and_then(|t| t.get("oid"))
        .and_then(|v| v.as_i64())
        .or_else(|| col.get("type_oid").and_then(|v| v.as_i64()))
        .or_else(|| {
            col.get("data_type")
                .and_then(|v| v.as_str())
                .and_then(|dt| match dt.to_ascii_lowercase().as_str() {
                    "smallint" | "int2" => Some(21),
                    "int" | "integer" | "int4" => Some(23),
                    "bigint" | "int8" => Some(20),
                    "uniqueidentifier" | "uuid" => Some(2950),
                    _ => None,
                })
        })
        .ok_or_else(|| EngineError::unsupported("returned column OID is unavailable"))?;
    let pg = returning_oid_to_pg_type(oid, mysql)?;
    Ok((col_name.to_string(), oid, pg))
}

pub(super) fn decode_returning_rows(
    document: &str,
    col_type: PgType,
    affected: u64,
) -> Result<Vec<Vec<Cell>>> {
    if document.is_empty() || document.len() > 4 * 1024 * 1024 || affected > 4096 {
        return Err(EngineError::new(
            "54000",
            "returned rows exceed guarded contract",
        ));
    }
    let rows: Vec<Vec<Option<String>>> = serde_json::from_str(document).map_err(|_| {
        EngineError::new("XX000", "committed returned rows are invalid JSON").unknown()
    })?;
    if rows.len() as u64 != affected {
        return Err(EngineError::new(
            "XX000",
            "committed returned row count mismatch",
        ));
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != 1 {
            return Err(EngineError::new(
                "XX000",
                "committed returned row arity invalid",
            ));
        }
        let cell = match &row[0] {
            None => Cell::Null,
            Some(s) => {
                if s.len() > 64 {
                    return Err(EngineError::new(
                        "54000",
                        "returned identifier length exceeded",
                    ));
                }
                match col_type {
                    PgType::Int2 => Cell::I16(s.parse::<i16>().map_err(|_| {
                        EngineError::new("22P02", "invalid returned smallint value")
                    })?),
                    PgType::Int4 => Cell::I32(s.parse::<i32>().map_err(|_| {
                        EngineError::new("22P02", "invalid returned integer value")
                    })?),
                    PgType::Int8 => {
                        Cell::I64(s.parse::<i64>().map_err(|_| {
                            EngineError::new("22P02", "invalid returned bigint value")
                        })?)
                    }
                    PgType::Uuid => Cell::Uuid(
                        uuid::Uuid::parse_str(s)
                            .map_err(|_| EngineError::new("22P02", "invalid returned uuid value"))?
                            .into_bytes(),
                    ),
                    _ => {
                        return Err(EngineError::unsupported(
                            "unsupported returning column type",
                        ));
                    }
                }
            }
        };
        out.push(vec![cell]);
    }
    Ok(out)
}

pub(super) fn duckdb_cast_type(ty: PgType) -> Result<&'static str> {
    match ty {
        PgType::Bool => Ok("BOOLEAN"),
        PgType::Int2 => Ok("SMALLINT"),
        PgType::Int4 => Ok("INTEGER"),
        PgType::Int8 => Ok("BIGINT"),
        PgType::Float4 => Ok("FLOAT"),
        PgType::Float8 => Ok("DOUBLE"),
        PgType::Text
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::Timestamptz
        | PgType::Numeric
        | PgType::Uuid
        | PgType::Bytea
        | PgType::Interval
        | PgType::Point
        | PgType::Json => Ok("VARCHAR"),
        _ => Err(EngineError::unsupported(
            "parameter type is not enabled for guarded mutation",
        )),
    }
}

pub(super) fn build_guarded_mutation_sql(
    product: &str,
    returning: Option<(&str, i64)>,
    param_types: &[PgType],
    diagnostics: bool,
) -> Result<String> {
    let fn_name = if returning.is_some() {
        format!("{product}_guarded_insert_returning")
    } else {
        format!("{product}_guarded_mutation")
    };
    let mut query = format!("SELECT * FROM system.main.{fn_name}(?, ?, ?, ?, ?, ?");
    if !param_types.is_empty() {
        let oracle = product == "oracle";
        let pg_family = matches!(product, "postgres" | "greenplum" | "kingbasees");
        let parts = param_types
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                let cast = if *ty == PgType::Bytea && !pg_family {
                    "BLOB"
                } else if oracle {
                    let _ = duckdb_cast_type(*ty)?;
                    "VARCHAR"
                } else {
                    duckdb_cast_type(*ty)?
                };
                Ok(format!("p{} := CAST(? AS {cast})", i + 1))
            })
            .collect::<Result<Vec<_>>>()?;
        query.push_str(&format!(", params := struct_pack({})", parts.join(", ")));
    }
    if let Some((name, _oid)) = returning {
        query.push_str(&format!(
            ", returning_name := {}, returning_oid := CAST(? AS BIGINT)",
            quote_literal(name)
        ));
    }
    if diagnostics {
        query.push_str(", source_diagnostics := true");
    }
    query.push(')');
    Ok(query)
}

fn has_source_diagnostics(conn: &Connection, fn_name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = ? AND list_contains(parameters, 'source_diagnostics')",
        [fn_name],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

fn cell_as_guarded_param(
    value: &Cell,
    ty: PgType,
    kind: DataSourceKind,
) -> Result<duckdb::types::Value> {
    if let (Cell::Bytes(b), PgType::Bytea) = (value, ty)
        && !kind.is_postgres_family()
    {
        return Ok(duckdb::types::Value::Blob(b.clone()));
    }
    if let (Cell::Timestamp(v), PgType::Timestamptz) = (value, ty)
        && matches!(
            kind,
            DataSourceKind::Oracle | DataSourceKind::Sqlserver | DataSourceKind::Dameng
        )
    {
        let dt = chrono::DateTime::from_timestamp_micros(*v)
            .ok_or_else(|| EngineError::new("22007", "invalid timestamptz value"))?;
        return Ok(duckdb::types::Value::Text(
            dt.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string(),
        ));
    }
    if kind == DataSourceKind::Oracle {
        return Ok(match value {
            Cell::Null => duckdb::types::Value::Null,
            Cell::Bool(v) => duckdb::types::Value::Text(if *v { "1" } else { "0" }.into()),
            Cell::I16(v) => duckdb::types::Value::Text(v.to_string()),
            Cell::I32(v) => duckdb::types::Value::Text(v.to_string()),
            Cell::I64(v) => duckdb::types::Value::Text(v.to_string()),
            Cell::F32(v) => duckdb::types::Value::Text(v.to_string()),
            Cell::F64(v) => duckdb::types::Value::Text(v.to_string()),
            Cell::Numeric(s) | Cell::Text(s) => duckdb::types::Value::Text(s.clone()),
            _ => match modorch_fedengine_pg_compat::text_value(value, ty)? {
                Some(text) => duckdb::types::Value::Text(text),
                None => duckdb::types::Value::Null,
            },
        });
    }
    Ok(match value {
        Cell::Null => duckdb::types::Value::Null,
        Cell::Bool(v) => duckdb::types::Value::Boolean(*v),
        Cell::I16(v) => duckdb::types::Value::SmallInt(*v),
        Cell::I32(v) => duckdb::types::Value::Int(*v),
        Cell::I64(v) => duckdb::types::Value::BigInt(*v),
        Cell::F32(v) => duckdb::types::Value::Float(*v),
        Cell::F64(v) => duckdb::types::Value::Double(*v),
        Cell::Numeric(s) | Cell::Text(s) => duckdb::types::Value::Text(s.clone()),
        Cell::Date(_)
        | Cell::Time(_)
        | Cell::Timestamp(_)
        | Cell::Interval { .. }
        | Cell::Point { .. }
        | Cell::Uuid(_)
        | Cell::Bytes(_) => match modorch_fedengine_pg_compat::text_value(value, ty)? {
            Some(text) => duckdb::types::Value::Text(text),
            None => duckdb::types::Value::Null,
        },
        _ => to_duckdb(value),
    })
}

pub(super) fn execute(
    conn: &Connection,
    r: &ExecuteRequest,
    plan: &GuardedMutationPlan,
    param_types: &[PgType],
    output: WorkerOutput<'_>,
) -> Result<()> {
    let product = crud_product(plan.kind).ok_or_else(|| {
        EngineError::unsupported("guarded mutation is not available for this data source kind")
    })?;
    let meta_fn = format!("{product}_crud_metadata");
    let meta_diag = has_source_diagnostics(conn, &meta_fn);
    let meta_sql = if meta_diag {
        format!("SELECT * FROM system.main.{meta_fn}(?, ?, ?, source_diagnostics := true)")
    } else {
        format!("SELECT * FROM system.main.{meta_fn}(?, ?, ?)")
    };
    let mut meta_stmt = conn.prepare(&meta_sql).map_err(engine_error)?;
    let (m_status, _m_rows, m_state, m_stage, version, descriptor, m_source_err): (
        String,
        i64,
        String,
        String,
        String,
        String,
        String,
    ) = meta_stmt
        .query_row([&plan.catalog, &plan.schema, &plan.table], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                if meta_diag {
                    row.get(6)?
                } else {
                    String::new()
                },
            ))
        })
        .map_err(engine_error)?;
    if !m_state.is_empty()
        || m_status != "CONFIRMED_NOT_EXECUTED"
        || version.is_empty()
        || descriptor.is_empty()
    {
        return Err(map_status_failure(
            &m_status,
            if m_state.is_empty() {
                "0A000"
            } else {
                &m_state
            },
            &m_stage,
            &m_source_err,
            "guarded metadata verification failed",
        ));
    }

    let returning_info = if let Some(ret) = &plan.returning {
        let info = parse_returning_descriptor(
            &descriptor,
            &ret.column,
            matches!(plan.kind, DataSourceKind::Mysql | DataSourceKind::Mariadb),
        )?;
        Some(info)
    } else {
        None
    };

    let columns = returning_info
        .as_ref()
        .map(|(name, _, ty)| vec![ColumnMeta::new(name.clone(), *ty)])
        .unwrap_or_default();
    let _ = output.metadata.take().unwrap().send(Ok(Metadata {
        columns,
        parameters: param_types.to_vec(),
    }));
    if r.prepare_only {
        return Ok(());
    }
    if r.cancel.is_cancelled() {
        return Err(EngineError::new("57014", "write canceled before execute"));
    }

    let mut_fn = if returning_info.is_some() {
        format!("{product}_guarded_insert_returning")
    } else {
        format!("{product}_guarded_mutation")
    };
    let mut_diag = has_source_diagnostics(conn, &mut_fn);
    let ret_tuple = returning_info
        .as_ref()
        .map(|(n, oid, _)| (n.as_str(), *oid));
    let expanded_param_types: Vec<PgType>;
    let effective_param_types: &[PgType] = if plan.param_map.is_empty() {
        param_types
    } else {
        expanded_param_types = plan
            .param_map
            .iter()
            .map(|&idx| {
                param_types.get(idx).copied().ok_or_else(|| {
                    EngineError::new("XX000", "guarded mutation parameter index out of bounds")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        &expanded_param_types
    };
    let mut_sql = build_guarded_mutation_sql(product, ret_tuple, effective_param_types, mut_diag)?;

    let mut args: Vec<duckdb::types::Value> =
        Vec::with_capacity(6 + effective_param_types.len() + 1);
    args.push(duckdb::types::Value::Text(plan.catalog.clone()));
    args.push(duckdb::types::Value::Text(plan.schema.clone()));
    args.push(duckdb::types::Value::Text(plan.table.clone()));
    args.push(duckdb::types::Value::Text(descriptor));
    args.push(duckdb::types::Value::Text(version));
    args.push(duckdb::types::Value::Text(plan.native_sql.clone()));
    if plan.param_map.is_empty() {
        for (p, ty) in r.parameters.iter().zip(param_types.iter()) {
            args.push(cell_as_guarded_param(p, *ty, plan.kind)?);
        }
    } else {
        for &idx in &plan.param_map {
            let p = r.parameters.get(idx).ok_or_else(|| {
                EngineError::new("XX000", "guarded mutation parameter value out of bounds")
            })?;
            let ty = param_types.get(idx).ok_or_else(|| {
                EngineError::new("XX000", "guarded mutation parameter type out of bounds")
            })?;
            args.push(cell_as_guarded_param(p, *ty, plan.kind)?);
        }
    }
    if let Some((_, oid, _)) = &returning_info {
        args.push(duckdb::types::Value::BigInt(*oid));
    }

    let mut stmt = conn.prepare(&mut_sql).map_err(engine_error)?;
    let returning_present = returning_info.is_some();
    let (status, affected, state, stage, _ver, _desc, returned_json, source_err): (
        String,
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = stmt
        .query_row(params_from_iter(args.iter()), |row| {
            let ret_col = if returning_present {
                row.get(6)?
            } else {
                String::new()
            };
            let diag_col = match (returning_present, mut_diag) {
                (true, true) => row.get(7)?,
                (false, true) => row.get(6)?,
                _ => String::new(),
            };
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                ret_col,
                diag_col,
            ))
        })
        .map_err(|error| engine_error(error).unknown())?;

    if status != "COMMITTED" || !state.is_empty() {
        return Err(map_status_failure(
            &status,
            if state.is_empty() { "XX000" } else { &state },
            &stage,
            &source_err,
            "guarded mutation did not confirm commit",
        ));
    }
    let affected_u64 = u64::try_from(affected)
        .map_err(|_| EngineError::new("XX000", "invalid affected row count").unknown())?;
    if let Some((_, _, pg_ty)) = returning_info {
        let rows = decode_returning_rows(&returned_json, pg_ty, affected_u64)?;
        if !rows.is_empty() {
            send_chunk(output.sender, ResultChunk::Rows(rows), r)?;
        }
    }
    let completion = Completion {
        notices: vec![],
        command: r.command.clone(),
        affected_rows: Some(affected_u64),
        outcome: ExecutionOutcome::CommitConfirmed,
    };
    send_chunk(output.sender, ResultChunk::Complete(completion), r)?;
    Ok(())
}

pub(super) fn execute_local_returning(
    conn: &Connection,
    r: &ExecuteRequest,
    ret: &ReturningPlan,
    sql: &str,
    param_types: &[PgType],
    output: WorkerOutput<'_>,
) -> Result<()> {
    let values = r.parameters.iter().map(to_duckdb).collect::<Vec<_>>();
    let mut stmt = conn.prepare(sql).map_err(engine_error)?;
    let mut stream = stmt
        .stream_arrow(params_from_iter(values.iter()))
        .map_err(|e| engine_error(e).unknown())?;
    let mut meta_sent = false;
    let mut row_types: Vec<PgType> = Vec::new();
    let mut total = 0u64;
    for batch in &mut stream {
        let batch = batch.map_err(|e| engine_error(e).unknown())?;
        if !meta_sent {
            let schema = batch.schema();
            let columns: Vec<ColumnMeta> = schema
                .fields()
                .iter()
                .map(|field| {
                    let pg = match field.data_type() {
                        duckdb::arrow::datatypes::DataType::Int16 => PgType::Int2,
                        duckdb::arrow::datatypes::DataType::Int32 => PgType::Int4,
                        duckdb::arrow::datatypes::DataType::Int64 => PgType::Int8,
                        duckdb::arrow::datatypes::DataType::FixedSizeBinary(16) => PgType::Uuid,
                        _ => PgType::Text,
                    };
                    ColumnMeta::new(field.name().clone(), pg)
                })
                .collect();
            row_types = columns.iter().map(|c| c.data_type).collect();
            let _ = output.metadata.take().unwrap().send(Ok(Metadata {
                columns,
                parameters: param_types.to_vec(),
            }));
            meta_sent = true;
        }
        let mut rows = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .zip(&row_types)
                    .map(|(a, t)| cell(a.as_ref(), row).and_then(|c| crate::codec::coerce(c, *t)))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        total += rows.len() as u64;
        if !rows.is_empty() {
            send_chunk(output.sender, ResultChunk::Rows(rows), r)?;
        }
    }
    if !meta_sent {
        let _ = output.metadata.take().unwrap().send(Ok(Metadata {
            columns: vec![ColumnMeta::new(ret.column.clone(), PgType::Int8)],
            parameters: param_types.to_vec(),
        }));
    }
    let completion = Completion {
        notices: vec![],
        command: r.command.clone(),
        affected_rows: Some(total),
        outcome: ExecutionOutcome::CommitConfirmed,
    };
    send_chunk(output.sender, ResultChunk::Complete(completion), r)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_guarded_mutation_and_insert_returning_calls() {
        let sql = build_guarded_mutation_sql(
            "postgres",
            Some(("id", 20)),
            &[PgType::Int4, PgType::Text],
            true,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM system.main.postgres_guarded_insert_returning(?, ?, ?, ?, ?, ?, params := struct_pack(p1 := CAST(? AS INTEGER), p2 := CAST(? AS VARCHAR)), returning_name := 'id', returning_oid := CAST(? AS BIGINT), source_diagnostics := true)"
        );

        let sql_mut = build_guarded_mutation_sql("mysql", None, &[PgType::Int8], false).unwrap();
        assert_eq!(
            sql_mut,
            "SELECT * FROM system.main.mysql_guarded_mutation(?, ?, ?, ?, ?, ?, params := struct_pack(p1 := CAST(? AS BIGINT)))"
        );
    }

    #[test]
    fn maps_mysql_source_errors_and_indeterminate_outcomes() {
        let src = r#"{"product":"mysql","code":"1452","state":"23000","message":"fk fail"}"#;
        let rolled_back = map_status_failure(
            "CONFIRMED_ROLLED_BACK",
            "HY000",
            "source_execute",
            src,
            "fallback",
        );
        assert_eq!(rolled_back.sqlstate, "23503");
        assert!(!rolled_back.outcome_unknown);

        let unknown = map_status_failure(
            "COMMIT_STATUS_UNKNOWN",
            "08006",
            "source_commit",
            "",
            "lost socket",
        );
        assert_eq!(unknown.sqlstate, "08006");
        assert!(unknown.outcome_unknown);
    }

    #[test]
    fn parses_descriptor_and_decodes_returning_rows() {
        let desc = r#"{"columns":[{"name":"id","pg_type":{"oid":20}},{"name":"u","pg_type":{"oid":2950}}]}"#;
        let (name, oid, pg) = parse_returning_descriptor(desc, "id", false).unwrap();
        assert_eq!((name.as_str(), oid, pg), ("id", 20, PgType::Int8));
        assert!(parse_returning_descriptor(desc, "u", true).is_err());

        let pg_desc = r#"{"columns":[{"name":"id","type_oid":23,"dropped":true},{"name":"id","type_oid":20,"dropped":false}]}"#;
        let (name, oid, pg) = parse_returning_descriptor(pg_desc, "id", false).unwrap();
        assert_eq!((name.as_str(), oid, pg), ("id", 20, PgType::Int8));
        let dropped_only = r#"{"columns":[{"name":"id","type_oid":23,"dropped":true}]}"#;
        assert!(parse_returning_descriptor(dropped_only, "id", false).is_err());

        let rows = decode_returning_rows(r#"[["42"],["100"]]"#, PgType::Int8, 2).unwrap();
        assert!(matches!(rows[0].as_slice(), [Cell::I64(42)]));
        assert!(matches!(rows[1].as_slice(), [Cell::I64(100)]));
        assert!(decode_returning_rows(r#"[["42"]]"#, PgType::Int8, 2).is_err());
    }

    #[test]
    fn local_returning_streams_metadata_rows_and_commit_confirmed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE items(id BIGINT, name VARCHAR);")
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (meta_tx, mut meta_rx) = tokio::sync::oneshot::channel();
        let mut meta_opt = Some(meta_tx);
        let req = ExecuteRequest {
            metadata: None,
            principal: "fedengine".into(),
            session_id: 1,
            sql: "INSERT INTO items(id, name) VALUES ($1, $2) RETURNING id".into(),
            kind: StatementKind::Insert,
            parameters: vec![Cell::I64(77), Cell::Text("ok".into())],
            parameter_types: vec![Some(PgType::Int8), Some(PgType::Text)],
            prepare_only: false,
            generations: Default::default(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            cancel: Default::default(),
            ddl_source: None,
            command: "INSERT".into(),
            returning: Some(ReturningPlan {
                column: "id".into(),
            }),
            guarded_mutation: None,
            guarded_ddl: None,
        };
        execute_local_returning(
            &conn,
            &req,
            &ReturningPlan {
                column: "id".into(),
            },
            "INSERT INTO items(id, name) VALUES ($1, $2) RETURNING id",
            &[PgType::Int8, PgType::Text],
            WorkerOutput {
                sender: &tx,
                completion: None,
                metadata: &mut meta_opt,
            },
        )
        .unwrap();
        let meta = meta_rx.try_recv().unwrap().unwrap();
        assert_eq!(meta.columns[0].name, "id");
        assert!(
            matches!(rx.try_recv().unwrap(), ResultChunk::Rows(r) if matches!(r[0].as_slice(), [Cell::I64(77)]))
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            ResultChunk::Complete(c) if c.affected_rows == Some(1) && c.outcome == ExecutionOutcome::CommitConfirmed
        ));
    }

    #[test]
    fn kingbasees_hana_and_oracle_varchar_param_packing() {
        assert_eq!(crud_product(DataSourceKind::Kingbasees), Some("kingbasees"));
        assert_eq!(crud_product(DataSourceKind::Hana), Some("hana"));

        let oracle_sql =
            build_guarded_mutation_sql("oracle", None, &[PgType::Bool, PgType::Int8], false)
                .unwrap();
        assert_eq!(
            oracle_sql,
            "SELECT * FROM system.main.oracle_guarded_mutation(?, ?, ?, ?, ?, ?, params := struct_pack(p1 := CAST(? AS VARCHAR), p2 := CAST(? AS VARCHAR)))"
        );

        assert_eq!(
            cell_as_guarded_param(&Cell::Bool(true), PgType::Bool, DataSourceKind::Oracle).unwrap(),
            duckdb::types::Value::Text("1".into())
        );
        assert_eq!(
            cell_as_guarded_param(&Cell::Bool(false), PgType::Bool, DataSourceKind::Oracle)
                .unwrap(),
            duckdb::types::Value::Text("0".into())
        );
        assert_eq!(
            cell_as_guarded_param(&Cell::I64(42), PgType::Int8, DataSourceKind::Oracle).unwrap(),
            duckdb::types::Value::Text("42".into())
        );
        assert_eq!(
            cell_as_guarded_param(&Cell::F64(1.5), PgType::Float8, DataSourceKind::Oracle).unwrap(),
            duckdb::types::Value::Text("1.5".into())
        );

        // Non-PG Bytea casts to BLOB (including Oracle), whereas PG uses VARCHAR
        for prod in ["mysql", "oracle", "sqlserver", "dameng", "db2", "hana"] {
            let blob_sql = build_guarded_mutation_sql(prod, None, &[PgType::Bytea], false).unwrap();
            assert!(
                blob_sql.contains("p1 := CAST(? AS BLOB)"),
                "{prod}: {blob_sql}"
            );
        }
        let pg_blob_sql =
            build_guarded_mutation_sql("postgres", None, &[PgType::Bytea], false).unwrap();
        assert!(
            pg_blob_sql.contains("p1 := CAST(? AS VARCHAR)"),
            "{pg_blob_sql}"
        );

        assert_eq!(
            cell_as_guarded_param(
                &Cell::Bytes(vec![1, 2, 3]),
                PgType::Bytea,
                DataSourceKind::Oracle
            )
            .unwrap(),
            duckdb::types::Value::Blob(vec![1, 2, 3])
        );
        assert_eq!(
            cell_as_guarded_param(
                &Cell::Timestamp(0),
                PgType::Timestamptz,
                DataSourceKind::Oracle
            )
            .unwrap(),
            duckdb::types::Value::Text("1970-01-01 00:00:00.000000+00:00".into())
        );
        assert_eq!(
            cell_as_guarded_param(
                &Cell::Timestamp(0),
                PgType::Timestamptz,
                DataSourceKind::Sqlserver
            )
            .unwrap(),
            duckdb::types::Value::Text("1970-01-01 00:00:00.000000+00:00".into())
        );
        assert_eq!(
            cell_as_guarded_param(
                &Cell::Timestamp(0),
                PgType::Timestamptz,
                DataSourceKind::Dameng
            )
            .unwrap(),
            duckdb::types::Value::Text("1970-01-01 00:00:00.000000+00:00".into())
        );

        // parse_returning_descriptor data_type fallback
        for (dt, expected_oid, expected_pg) in [
            ("smallint", 21, PgType::Int2),
            ("int4", 23, PgType::Int4),
            ("bigint", 20, PgType::Int8),
            ("uniqueidentifier", 2950, PgType::Uuid),
        ] {
            let desc = format!(r#"{{"columns":[{{"name":"id","data_type":"{dt}"}}]}}"#);
            let (n, oid, pg) = parse_returning_descriptor(&desc, "id", false).unwrap();
            assert_eq!((n.as_str(), oid, pg), ("id", expected_oid, expected_pg));
        }

        // param_map expansion helper check via build_guarded_mutation_sql
        let param_map = [1usize, 0, 0];
        let base_types = [PgType::Int8, PgType::Text];
        let expanded: Vec<PgType> = param_map.iter().map(|&i| base_types[i]).collect();
        let reordered_sql = build_guarded_mutation_sql("mysql", None, &expanded, false).unwrap();
        assert!(
            reordered_sql.contains(
                "p1 := CAST(? AS VARCHAR), p2 := CAST(? AS BIGINT), p3 := CAST(? AS BIGINT)"
            ),
            "{reordered_sql}"
        );
    }
}
