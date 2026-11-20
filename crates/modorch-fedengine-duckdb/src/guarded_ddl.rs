use crate::{
    error::engine_error,
    guarded_crud::{decode_source_error, normalized_mutation_source_state},
    worker::{Metadata, WorkerOutput, send_chunk},
};
use duckdb::{Connection, params, params_from_iter};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use modorch_fedengine_datasource::DataSourceRegistry;
use std::sync::atomic::Ordering;

pub(super) fn resolve_remote_database(conn: &Connection, plan: &GuardedDdlPlan) -> String {
    if plan.kind == DataSourceKind::Postgres {
        let q = format!(
            "SELECT current_database FROM postgres_query({}, 'SELECT current_database() AS current_database')",
            quote_literal(&plan.catalog)
        );
        if let Ok(db) = conn.query_row(&q, [], |row| row.get::<_, String>(0))
            && !db.is_empty()
        {
            return db;
        }
    }
    plan.database.clone()
}

pub(super) fn build_guarded_ddl_sql(
    plan: &GuardedDdlPlan,
    diagnostics: bool,
) -> (String, Vec<duckdb::types::Value>) {
    let product = match plan.kind {
        DataSourceKind::Mysql => "mysql",
        _ => "postgres",
    };
    let mut args: Vec<duckdb::types::Value> = vec![
        duckdb::types::Value::Text(plan.catalog.clone()),
        duckdb::types::Value::Text(plan.database.clone()),
        duckdb::types::Value::Text(plan.schema.clone()),
        duckdb::types::Value::Text(uuid::Uuid::new_v4().to_string()),
        duckdb::types::Value::Text(plan.action.clone()),
        duckdb::types::Value::Text(plan.table_name.clone()),
        duckdb::types::Value::Text(plan.index_name.clone()),
        duckdb::types::Value::Text(plan.owner.clone()),
    ];
    let mut extra = String::new();
    let v = plan.contract_version;
    if v >= 3 {
        args.push(duckdb::types::Value::Text(plan.new_name.clone()));
        args.push(duckdb::types::Value::Text(plan.column_name.clone()));
        extra.push_str("new_name := CAST(? AS VARCHAR), column_name := CAST(? AS VARCHAR), ");
    }
    let mut push_param = |val: duckdb::types::Value, ty: &str| -> String {
        args.push(val);
        format!("CAST(? AS {ty})")
    };
    if v == 4 {
        let c = plan.constraint.as_ref();
        let fields: [(&str, String); 10] = [
            ("default_kind", plan.default_kind.clone()),
            ("default_value", plan.default_value.clone()),
            (
                "constraint_name",
                c.map(|v| v.name.clone())
                    .unwrap_or_else(|| plan.constraint_name.clone()),
            ),
            (
                "constraint_kind",
                c.map(|v| v.kind.clone()).unwrap_or_default(),
            ),
            (
                "check_column",
                c.map(|v| v.check_column.clone()).unwrap_or_default(),
            ),
            (
                "check_operator",
                c.map(|v| v.check_operator.clone()).unwrap_or_default(),
            ),
            (
                "check_value",
                c.map(|v| v.check_value.clone()).unwrap_or_default(),
            ),
            (
                "reference_schema",
                c.map(|v| v.reference_schema.clone()).unwrap_or_default(),
            ),
            (
                "reference_table",
                c.map(|v| v.reference_table.clone()).unwrap_or_default(),
            ),
            ("identity_mode", plan.identity_mode.clone()),
        ];
        for (name, val) in fields {
            let expr = push_param(duckdb::types::Value::Text(val), "VARCHAR");
            extra.push_str(&format!("{name} := {expr}, "));
        }
        let ckeys = c.map(|v| v.columns.as_slice()).unwrap_or(&[]);
        let rkeys = c.map(|v| v.reference_columns.as_slice()).unwrap_or(&[]);
        for (name, list) in [("constraint_keys", ckeys), ("reference_keys", rkeys)] {
            let items = list
                .iter()
                .map(|s| push_param(duckdb::types::Value::Text(s.clone()), "VARCHAR"))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push_str(&format!("{name} := CAST([{items}] AS VARCHAR[]), "));
        }
    }
    let columns = plan
        .columns
        .iter()
        .map(|col| {
            let n = push_param(duckdb::types::Value::Text(col.name.clone()), "VARCHAR");
            let t = push_param(duckdb::types::Value::Text(col.type_name.clone()), "VARCHAR");
            let l = push_param(duckdb::types::Value::Int(col.length), "INTEGER");
            if v == 5 {
                let p = push_param(duckdb::types::Value::Int(col.precision), "INTEGER");
                let s = push_param(duckdb::types::Value::Int(col.scale), "INTEGER");
                format!(
                    "struct_pack(name := {n}, type := {t}, length := {l}, \"precision\" := {p}, scale := {s})"
                )
            } else {
                format!("struct_pack(name := {n}, type := {t}, length := {l})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let keys = plan
        .keys
        .iter()
        .map(|k| push_param(duckdb::types::Value::Text(k.clone()), "VARCHAR"))
        .collect::<Vec<_>>()
        .join(", ");
    let col_ty = if v == 5 {
        "STRUCT(name VARCHAR, type VARCHAR, length INTEGER, \"precision\" INTEGER, scale INTEGER)[]"
    } else {
        "STRUCT(name VARCHAR, type VARCHAR, length INTEGER)[]"
    };
    let diag_arg = if diagnostics {
        ", source_diagnostics := true"
    } else {
        ""
    };
    let sql = format!(
        "SELECT * FROM system.main.{product}_guarded_ddl(CAST(? AS VARCHAR), CAST(? AS VARCHAR), CAST(? AS VARCHAR), CAST(? AS VARCHAR), struct_pack(\
         contract_version := CAST({v} AS INTEGER), target_contract := 'EXECUTION_NAME', \
         action := CAST(? AS VARCHAR), name := CAST(? AS VARCHAR), index_name := CAST(? AS VARCHAR), owner := CAST(? AS VARCHAR), \
         {extra}columns := CAST([{columns}] AS {col_ty}), keys := CAST([{keys}] AS VARCHAR[])){diag_arg})"
    );
    (sql, args)
}

fn record_ddl_start(conn: &Connection, operation_id: &str, plan: &GuardedDdlPlan) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let target = format!("{}.{}", plan.schema, plan.table_name);
    conn.execute(
        "INSERT INTO _fedengine.ddl_operations(operation_id, source_id, action, target, status, sqlstate, stage, warning_count, source_error, created_at, updated_at) VALUES (?, ?, ?, ?, 'PENDING', '', 'queued', 0, NULL, ?, ?)",
        params![operation_id, plan.source.0, plan.action, target, now, now],
    )
    .map_err(engine_error)?;
    Ok(())
}

fn record_ddl_finish(
    conn: &Connection,
    operation_id: &str,
    status: &str,
    sqlstate: &str,
    stage: &str,
    warning_count: i64,
    source_error: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let _ = conn.execute(
        "UPDATE _fedengine.ddl_operations SET status=?, sqlstate=?, stage=?, warning_count=?, source_error=?, updated_at=? WHERE operation_id=?",
        params![
            status,
            sqlstate,
            stage,
            warning_count,
            (!source_error.is_empty()).then_some(source_error),
            now,
            operation_id
        ],
    );
}

fn has_source_diagnostics(conn: &Connection, fn_name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = ? AND list_contains(parameters, 'source_diagnostics')",
        [fn_name],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

fn check_ddl_capabilities(conn: &Connection, plan: &GuardedDdlPlan) -> Result<()> {
    let product = match plan.kind {
        DataSourceKind::Mysql => "mysql",
        _ => "postgres",
    };
    let cap_fn = format!("{product}_ddl_capabilities");
    let present: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = ?",
            [&cap_fn],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !present {
        return Ok(());
    }
    let cap_sql = format!("SELECT sqlstate, stage, document FROM system.main.{cap_fn}(?, ?)");
    let (state, stage, document): (String, String, String) = conn
        .query_row(&cap_sql, [&plan.catalog, &plan.schema], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(engine_error)?;
    if !state.is_empty() || document.is_empty() {
        let code = crate::error::map_sqlstate(if state.is_empty() { "0A000" } else { &state });
        return Err(EngineError::new(
            code,
            format!("DDL capabilities check failed at stage {stage}"),
        ));
    }
    Ok(())
}

pub(super) struct DdlExecEnv<'a> {
    pub(super) config: &'a Config,
    pub(super) registry: &'a DataSourceRegistry,
    pub(super) metrics: &'a EngineMetrics,
}

pub(super) fn execute(
    conn: &Connection,
    r: &ExecuteRequest,
    plan: &GuardedDdlPlan,
    param_types: &[PgType],
    output: WorkerOutput<'_>,
    env: DdlExecEnv<'_>,
) -> Result<()> {
    let c = env.config;
    let registry = env.registry;
    let metrics = env.metrics;
    let _ = output.metadata.take().unwrap().send(Ok(Metadata {
        columns: vec![],
        parameters: param_types.to_vec(),
    }));
    if r.cancel.is_cancelled() {
        return Err(EngineError::new("57014", "write canceled before execute"));
    }

    let mut effective = plan.clone();
    effective.database = resolve_remote_database(conn, &effective);
    check_ddl_capabilities(conn, &effective)?;

    let product = match effective.kind {
        DataSourceKind::Mysql => "mysql",
        _ => "postgres",
    };
    let fn_name = format!("{product}_guarded_ddl");
    let diag = has_source_diagnostics(conn, &fn_name);
    let (sql, mut args) = build_guarded_ddl_sql(&effective, diag);
    let op_id = match &args[3] {
        duckdb::types::Value::Text(s) => s.clone(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    args[3] = duckdb::types::Value::Text(op_id.clone());
    record_ddl_start(conn, &op_id, &effective)?;

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        record_ddl_finish(
            conn,
            &op_id,
            "CONFIRMED_NOT_EXECUTED",
            "0A000",
            "prepare",
            0,
            "",
        );
        engine_error(e)
    })?;

    let row_res: std::result::Result<(String, String, String, String, i64, String), duckdb::Error> =
        stmt.query_row(params_from_iter(args.iter()), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                if diag { row.get(5)? } else { String::new() },
            ))
        });

    let (_ret_op, status, state, stage, warnings, source_err) = match row_res {
        Ok(tuple) => tuple,
        Err(err) => {
            let mapped = engine_error(err);
            record_ddl_finish(
                conn,
                &op_id,
                "COMMIT_STATUS_UNKNOWN",
                mapped.sqlstate,
                "source_execute",
                0,
                "",
            );
            return Err(mapped.unknown());
        }
    };

    record_ddl_finish(conn, &op_id, &status, &state, &stage, warnings, &source_err);

    if status != "COMMITTED" {
        let unknown = !matches!(
            status.as_str(),
            "CONFIRMED_NOT_EXECUTED" | "CONFIRMED_ROLLED_BACK"
        );
        let parsed = decode_source_error(&source_err).ok().flatten();
        let sqlstate = normalized_mutation_source_state(
            parsed.as_ref(),
            if state.is_empty() { "XX000" } else { &state },
        );
        let detail = parsed
            .as_ref()
            .map(|s| s.message.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or("guarded DDL did not confirm commit");
        let err = EngineError::new(sqlstate, format!("{detail} (stage: {stage})"));
        return Err(if unknown { err.unknown() } else { err });
    }

    if let Some(source) = &r.ddl_source {
        metrics.ddl_confirmed.fetch_add(1, Ordering::Relaxed);
        if c.management.enabled {
            registry.invalidate(source, true)?;
        } else {
            registry.invalidate(source, false)?;
            let src = registry.get(source)?;
            if conn
                .execute_batch(&crate::extensions::clear_cache_sql(
                    src.definition.kind.extension(),
                    &src.definition.catalog_alias,
                ))
                .is_err()
            {
                src.stale.store(true, Ordering::SeqCst);
                metrics.metadata_stale.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let completion = Completion {
        notices: vec![],
        command: r.command.clone(),
        affected_rows: None,
        outcome: ExecutionOutcome::CommitConfirmed,
    };
    if let Some(deferred) = output.completion {
        let _ = deferred.send(completion);
        return Ok(());
    }
    send_chunk(output.sender, ResultChunk::Complete(completion), r)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(version: i32, action: &str, kind: DataSourceKind) -> GuardedDdlPlan {
        GuardedDdlPlan {
            source: DataSourceId("crm".into()),
            kind,
            catalog: "pg_crm".into(),
            database: "crmdb".into(),
            schema: "public".into(),
            action: action.into(),
            table_name: "items".into(),
            index_name: String::new(),
            owner: String::new(),
            new_name: if version == 3 {
                "items2".into()
            } else {
                String::new()
            },
            column_name: if action == "drop_default" {
                "note".into()
            } else {
                String::new()
            },
            default_kind: String::new(),
            default_value: String::new(),
            constraint_name: String::new(),
            constraint: None,
            identity_mode: String::new(),
            columns: if version == 5 {
                vec![DdlColumnSpec {
                    name: "amount".into(),
                    type_name: "numeric".into(),
                    length: 0,
                    precision: 18,
                    scale: 4,
                }]
            } else {
                vec![]
            },
            keys: vec![],
            contract_version: version,
        }
    }

    #[test]
    fn builds_guarded_ddl_v3_v4_v5_struct_contracts() {
        let (sql_v3, args_v3) = build_guarded_ddl_sql(
            &sample_plan(3, "rename_table", DataSourceKind::Postgres),
            true,
        );
        assert!(sql_v3.contains("postgres_guarded_ddl("), "{sql_v3}");
        assert!(sql_v3.contains("contract_version := CAST(3 AS INTEGER)"));
        assert!(sql_v3.contains("new_name := CAST(? AS VARCHAR)"));
        assert!(sql_v3.contains("source_diagnostics := true"));
        assert_eq!(args_v3.len(), 10);

        let (sql_v4, args_v4) = build_guarded_ddl_sql(
            &sample_plan(4, "drop_default", DataSourceKind::Mysql),
            false,
        );
        assert!(sql_v4.contains("mysql_guarded_ddl("), "{sql_v4}");
        assert!(sql_v4.contains("default_kind := CAST(? AS VARCHAR)"));
        assert!(sql_v4.contains("constraint_keys := CAST([] AS VARCHAR[])"));
        assert_eq!(args_v4.len(), 20);

        let (sql_v5, args_v5) = build_guarded_ddl_sql(
            &sample_plan(5, "create_table", DataSourceKind::Postgres),
            true,
        );
        assert!(sql_v5.contains("\"precision\" := CAST(? AS INTEGER)"));
        assert!(sql_v5.contains("STRUCT(name VARCHAR, type VARCHAR, length INTEGER, \"precision\" INTEGER, scale INTEGER)[]"));
        assert_eq!(args_v5.len(), 15);
    }

    #[test]
    fn records_ddl_operation_ledger_inside_fedengine() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = Config::load("../../config/application.toml").unwrap();
        cfg.datasources.clear();
        crate::storage::initialize(&conn, &cfg).unwrap();
        let plan = sample_plan(2, "drop_table", DataSourceKind::Postgres);
        record_ddl_start(&conn, "11111111-1111-1111-1111-111111111111", &plan).unwrap();
        record_ddl_finish(
            &conn,
            "11111111-1111-1111-1111-111111111111",
            "COMMITTED",
            "",
            "",
            0,
            "",
        );
        let (status, target): (String, String) = conn
            .query_row(
                "SELECT status, target FROM _fedengine.ddl_operations WHERE operation_id=?",
                ["11111111-1111-1111-1111-111111111111"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "COMMITTED");
        assert_eq!(target, "public.items");
    }
}
