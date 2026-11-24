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

pub(super) fn ddl_product(kind: DataSourceKind) -> Option<&'static str> {
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

pub(super) fn resolve_remote_database(conn: &Connection, plan: &GuardedDdlPlan) -> String {
    let query = match plan.kind {
        DataSourceKind::Postgres | DataSourceKind::Greenplum | DataSourceKind::Kingbasees => {
            Some(format!(
                "SELECT current_database FROM postgres_query({}, 'SELECT current_database() AS current_database')",
                quote_literal(&plan.catalog)
            ))
        }
        DataSourceKind::Mysql | DataSourceKind::Mariadb => Some(format!(
            "SELECT db FROM mysql_query({}, 'SELECT DATABASE() AS db')",
            quote_literal(&plan.catalog)
        )),
        DataSourceKind::Sqlserver => Some(format!(
            "SELECT db FROM sqlserver_query({}, 'SELECT DB_NAME() AS db')",
            quote_literal(&plan.catalog)
        )),
        DataSourceKind::Oracle => Some(format!(
            "SELECT db FROM oracle_query({}, 'SELECT SYS_CONTEXT(''USERENV'',''DB_NAME'') AS db FROM DUAL')",
            quote_literal(&plan.catalog)
        )),
        _ => None,
    };
    if let Some(q) = query
        && let Ok(db) = conn.query_row(&q, [], |row| row.get::<_, String>(0))
        && !db.is_empty()
    {
        return db;
    }
    plan.database.clone()
}

pub(super) fn build_guarded_ddl_sql(
    plan: &GuardedDdlPlan,
    diagnostics: bool,
) -> (String, Vec<duckdb::types::Value>) {
    let product = ddl_product(plan.kind).unwrap_or("postgres");
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
    if v == 6 {
        let q = push_param(
            duckdb::types::Value::Text(plan.query_sql.clone().unwrap_or_default()),
            "VARCHAR",
        );
        extra.push_str(&format!("query_sql := {q}, "));
        let constraints = plan
            .constraints
            .iter()
            .map(|c| {
                let name = push_param(duckdb::types::Value::Text(c.name.clone()), "VARCHAR");
                let kind = push_param(duckdb::types::Value::Text(c.kind.clone()), "VARCHAR");
                let cols = c
                    .columns
                    .iter()
                    .map(|col| push_param(duckdb::types::Value::Text(col.clone()), "VARCHAR"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let rs = push_param(
                    duckdb::types::Value::Text(c.reference_schema.clone()),
                    "VARCHAR",
                );
                let rt = push_param(
                    duckdb::types::Value::Text(c.reference_table.clone()),
                    "VARCHAR",
                );
                let rcols = c
                    .reference_columns
                    .iter()
                    .map(|col| push_param(duckdb::types::Value::Text(col.clone()), "VARCHAR"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let cc = push_param(
                    duckdb::types::Value::Text(c.check_column.clone()),
                    "VARCHAR",
                );
                let co = push_param(
                    duckdb::types::Value::Text(c.check_operator.clone()),
                    "VARCHAR",
                );
                let cv = push_param(
                    duckdb::types::Value::Text(c.check_value.clone()),
                    "VARCHAR",
                );
                format!(
                    "struct_pack(name := {name}, kind := {kind}, columns := CAST([{cols}] AS VARCHAR[]), reference_schema := {rs}, reference_table := {rt}, reference_columns := CAST([{rcols}] AS VARCHAR[]), check_column := {cc}, check_operator := {co}, check_value := {cv})"
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        extra.push_str(&format!(
            "constraints := CAST([{constraints}] AS STRUCT(name VARCHAR, kind VARCHAR, columns VARCHAR[], reference_schema VARCHAR, reference_table VARCHAR, reference_columns VARCHAR[], check_column VARCHAR, check_operator VARCHAR, check_value VARCHAR)[]), "
        ));
    }
    let columns = plan
        .columns
        .iter()
        .map(|col| {
            let n = push_param(duckdb::types::Value::Text(col.name.clone()), "VARCHAR");
            let t = push_param(duckdb::types::Value::Text(col.type_name.clone()), "VARCHAR");
            let l = push_param(duckdb::types::Value::Int(col.length), "INTEGER");
            if v == 6 {
                let p = push_param(duckdb::types::Value::Int(col.precision), "INTEGER");
                let s = push_param(duckdb::types::Value::Int(col.scale), "INTEGER");
                let nn = push_param(duckdb::types::Value::Boolean(col.not_null), "BOOLEAN");
                let dk = push_param(
                    duckdb::types::Value::Text(col.default_kind.clone().unwrap_or_default()),
                    "VARCHAR",
                );
                let dv = push_param(
                    duckdb::types::Value::Text(col.default_value.clone().unwrap_or_default()),
                    "VARCHAR",
                );
                let pk = push_param(duckdb::types::Value::Boolean(col.primary_key), "BOOLEAN");
                let uq = push_param(duckdb::types::Value::Boolean(col.unique), "BOOLEAN");
                format!(
                    "struct_pack(name := {n}, type := {t}, length := {l}, \"precision\" := {p}, scale := {s}, not_null := {nn}, default_kind := {dk}, default_value := {dv}, primary_key := {pk}, \"unique\" := {uq})"
                )
            } else if v == 5 {
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
    let col_ty = if v == 6 {
        "STRUCT(name VARCHAR, type VARCHAR, length INTEGER, \"precision\" INTEGER, scale INTEGER, not_null BOOLEAN, default_kind VARCHAR, default_value VARCHAR, primary_key BOOLEAN, \"unique\" BOOLEAN)[]"
    } else if v == 5 {
        "STRUCT(name VARCHAR, type VARCHAR, length INTEGER, \"precision\" INTEGER, scale INTEGER)[]"
    } else {
        "STRUCT(name VARCHAR, type VARCHAR, length INTEGER)[]"
    };
    let diag_arg = if diagnostics {
        ", source_diagnostics := true"
    } else {
        ""
    };
    let cond_arg = if plan.conditional {
        ", conditional := true"
    } else {
        ""
    };
    let sql = format!(
        "SELECT * FROM system.main.{product}_guarded_ddl(CAST(? AS VARCHAR), CAST(? AS VARCHAR), CAST(? AS VARCHAR), CAST(? AS VARCHAR), struct_pack(\
         contract_version := CAST({v} AS INTEGER), target_contract := 'EXECUTION_NAME', \
         action := CAST(? AS VARCHAR), name := CAST(? AS VARCHAR), index_name := CAST(? AS VARCHAR), owner := CAST(? AS VARCHAR), \
         {extra}columns := CAST([{columns}] AS {col_ty}), keys := CAST([{keys}] AS VARCHAR[])){diag_arg}{cond_arg})"
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

fn validate_capabilities_document(
    kind: DataSourceKind,
    state: &str,
    stage: &str,
    document: &str,
) -> Result<()> {
    if !state.is_empty() || document.is_empty() {
        let code = crate::error::map_sqlstate(if state.is_empty() { "0A000" } else { state });
        return Err(EngineError::new(
            code,
            format!("DDL capabilities check failed at stage {stage}"),
        ));
    }
    if !kind.is_postgres_family()
        && !kind.is_mysql_family()
        && serde_json::from_str::<serde_json::Value>(document)
            .ok()
            .and_then(|v| v.get("executor_available").and_then(|b| b.as_bool()))
            == Some(false)
    {
        return Err(EngineError::unsupported(
            "DDL executor is not available for this database version",
        ));
    }
    Ok(())
}

fn check_ddl_capabilities(conn: &Connection, plan: &GuardedDdlPlan) -> Result<()> {
    let product = ddl_product(plan.kind).ok_or_else(|| {
        EngineError::unsupported("guarded DDL is not enabled for this data source kind")
    })?;
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
    let cap_schema = if plan.kind.is_mysql_family()
        && matches!(plan.action.as_str(), "create_schema" | "drop_schema")
        && !plan.database.is_empty()
    {
        plan.database.as_str()
    } else {
        plan.schema.as_str()
    };
    let cap_sql = format!("SELECT sqlstate, stage, document FROM system.main.{cap_fn}(?, ?)");
    let (state, stage, document): (String, String, String) = conn
        .query_row(&cap_sql, [plan.catalog.as_str(), cap_schema], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(engine_error)?;
    validate_capabilities_document(plan.kind, &state, &stage, &document)
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

    let product = ddl_product(effective.kind).ok_or_else(|| {
        EngineError::unsupported("guarded DDL is not enabled for this data source kind")
    })?;
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
            columns: if version >= 5 {
                vec![DdlColumnSpec {
                    name: "amount".into(),
                    type_name: "numeric".into(),
                    length: 0,
                    precision: 18,
                    scale: 4,
                    not_null: false,
                    default_kind: None,
                    default_value: None,
                    primary_key: false,
                    unique: false,
                }]
            } else {
                vec![]
            },
            constraints: vec![],
            keys: vec![],
            conditional: false,
            query_sql: None,
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

        for (kind, expected_prefix) in [
            (DataSourceKind::Postgres, "postgres_guarded_ddl("),
            (DataSourceKind::Greenplum, "greenplum_guarded_ddl("),
            (DataSourceKind::Kingbasees, "kingbasees_guarded_ddl("),
            (DataSourceKind::Mysql, "mysql_guarded_ddl("),
            (DataSourceKind::Mariadb, "mysql_guarded_ddl("),
            (DataSourceKind::Oracle, "oracle_guarded_ddl("),
            (DataSourceKind::Sqlserver, "sqlserver_guarded_ddl("),
            (DataSourceKind::Dameng, "dameng_guarded_ddl("),
            (DataSourceKind::Db2, "db2_guarded_ddl("),
            (DataSourceKind::Hana, "hana_guarded_ddl("),
        ] {
            for action in [
                "drop_column",
                "alter_column_type",
                "create_schema",
                "drop_schema",
            ] {
                let mut p = sample_plan(5, action, kind);
                if action != "alter_column_type" {
                    p.columns.clear();
                } else {
                    p.column_name = "amount".into();
                }
                let (sql, _) = build_guarded_ddl_sql(&p, false);
                assert!(sql.contains(expected_prefix), "{sql}");
                assert!(sql.contains("contract_version := CAST(5 AS INTEGER)"));
            }
        }
        assert_eq!(ddl_product(DataSourceKind::Starrocks), None);

        // v6 CREATE TABLE + conditional := true
        let mut p6 = sample_plan(6, "create_table", DataSourceKind::Postgres);
        p6.conditional = true;
        p6.columns = vec![DdlColumnSpec {
            name: "id".into(),
            type_name: "bigint".into(),
            length: 0,
            precision: 0,
            scale: 0,
            not_null: true,
            default_kind: Some("int".into()),
            default_value: Some("0".into()),
            primary_key: true,
            unique: true,
        }];
        p6.constraints = vec![DdlConstraintSpec {
            name: "chk_id".into(),
            kind: "check".into(),
            columns: vec![],
            reference_schema: String::new(),
            reference_table: String::new(),
            reference_columns: vec![],
            check_column: "id".into(),
            check_operator: ">=".into(),
            check_value: "0".into(),
        }];
        let (sql_v6, _) = build_guarded_ddl_sql(&p6, false);
        assert!(
            sql_v6.contains("contract_version := CAST(6 AS INTEGER)"),
            "{sql_v6}"
        );
        assert!(
            sql_v6.contains("query_sql := CAST(? AS VARCHAR)"),
            "{sql_v6}"
        );
        assert!(
            sql_v6.contains("not_null := CAST(? AS BOOLEAN)"),
            "{sql_v6}"
        );
        assert!(
            sql_v6.contains("\"unique\" := CAST(? AS BOOLEAN)"),
            "{sql_v6}"
        );
        assert!(sql_v6.contains(", conditional := true)"), "{sql_v6}");

        // validate_capabilities_document rejects Sqlserver executor_available:false while allowing PG/MySQL legacy JSON
        assert!(
            validate_capabilities_document(
                DataSourceKind::Sqlserver,
                "",
                "",
                r#"{"contract_version":1,"executor_available":false}"#
            )
            .is_err()
        );
        assert!(
            validate_capabilities_document(
                DataSourceKind::Postgres,
                "",
                "",
                r#"{"contract_version":1,"executor_available":false}"#
            )
            .is_ok()
        );
        assert!(
            validate_capabilities_document(
                DataSourceKind::Mysql,
                "",
                "",
                r#"{"contract_version":1,"executor_available":false}"#
            )
            .is_ok()
        );
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
