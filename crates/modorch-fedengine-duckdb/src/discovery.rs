//! Bounded instance enumeration on an isolated DuckDB connection.
use crate::{
    control, credentials,
    error::{engine_error, metadata_text},
};
use duckdb::Connection;
use modorch_fedengine_core::*;
use tokio_util::sync::CancellationToken;

pub(super) fn discover(
    conn: &Connection,
    config: &Config,
    connection: &SourceConnection,
    cancel: &CancellationToken,
) -> Result<Vec<DatabaseCandidate>> {
    connection.validate()?;
    control::load_extension(conn, config, connection.kind)?;
    let alias = "__discovery";
    let attach = |database: Option<&str>| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(EngineError::new("57014", "database discovery canceled"));
        }
        let target_db = database.unwrap_or("information_schema");
        let org = SourceOrganization {
            connection_id: connection.id.clone(),
            instance: connection.instance.clone(),
            database: target_db.to_string(),
            virtual_database: "discovery".into(),
            owner: "discovery".into(),
            connect: vec![],
        };
        let plan = credentials::attach_plan(
            &connection.secret_ref,
            &config.management.credentials_directory,
            connection.kind,
            alias,
            true,
            "",
            Some(&org),
        )?;
        if let Some(secret_sql) = &plan.secret_sql {
            conn.execute_batch(secret_sql).map_err(engine_error)?;
        }
        conn.execute_batch(&plan.attach_sql).map_err(engine_error)
    };
    let query = |sql: &str| {
        format!(
            "SELECT * FROM {}_query({}, {})",
            connection.kind.extension(),
            quote_literal(alias),
            quote_literal(sql)
        )
    };
    let databases = if let Some(database) = &connection.database {
        vec![(database.clone(), true)]
    } else if connection.kind.is_odbc_catalog_family() || connection.kind == DataSourceKind::Oracle
    {
        return Err(EngineError::config(
            "database name is required for Oracle/ODBC catalog discovery",
        ));
    } else {
        attach(if connection.kind.is_postgres_family() {
            Some(
                connection
                    .bootstrap_database
                    .as_deref()
                    .unwrap_or("postgres"),
            )
        } else if connection.kind == DataSourceKind::Sqlserver {
            Some(connection.bootstrap_database.as_deref().unwrap_or("master"))
        } else {
            Some("information_schema")
        })?;
        let sql = if connection.kind.is_postgres_family() {
            "SELECT datname::text, (datallowconn AND NOT datistemplate AND has_database_privilege(current_user, oid, 'CONNECT')) AS connectable FROM pg_catalog.pg_database ORDER BY datname LIMIT 129"
        } else if connection.kind == DataSourceKind::Sqlserver {
            let modern: i64 = conn
                .query_row(
                    &query("SELECT CASE WHEN OBJECT_ID('sys.objects') IS NULL THEN 0 ELSE 1 END"),
                    [],
                    |r| r.get(0),
                )
                .map_err(engine_error)?;
            if modern == 1 {
                "SELECT TOP 129 name, CASE WHEN state = 0 THEN 1 ELSE 0 END AS connectable FROM sys.databases ORDER BY name"
            } else {
                "SELECT TOP 129 name, 1 AS connectable FROM master.dbo.sysdatabases ORDER BY name"
            }
        } else {
            "SELECT SCHEMA_NAME, TRUE AS connectable FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME LIMIT 129"
        };
        let mut stmt = conn.prepare(&query(sql)).map_err(engine_error)?;
        let mut rows = stmt.query([]).map_err(engine_error)?;
        let mut databases = Vec::new();
        while let Some(row) = rows.next().map_err(engine_error)? {
            databases.push((
                metadata_text(row, 0)?,
                if connection.kind.is_postgres_family() {
                    row.get::<_, bool>(1).map_err(engine_error)?
                } else if connection.kind == DataSourceKind::Sqlserver {
                    row.get::<_, i32>(1).map_err(engine_error)? == 1
                } else {
                    true
                },
            ));
        }
        drop(rows);
        drop(stmt);
        control::detach(conn, alias)?;
        if databases.len() > 128 {
            return Err(EngineError::new(
                "54000",
                "instance exceeds 128 database discovery limit; specify a database",
            ));
        }
        databases
    };
    let mut result = Vec::new();
    for (database, connectable) in databases {
        if cancel.is_cancelled() {
            return Err(EngineError::new("57014", "database discovery canceled"));
        }
        let mut candidate = DatabaseCandidate {
            database: database.clone(),
            schemas: vec![],
            status: DatabaseDiscoveryStatus::Available,
            error: None,
            virtual_database: None,
            merged: false,
        };
        if system_database(connection.kind, &database) {
            candidate.status = DatabaseDiscoveryStatus::Excluded;
        } else if !connectable {
            candidate.status = DatabaseDiscoveryStatus::Forbidden;
            candidate.error = Some("database does not allow this account to connect".into());
        } else if !allowed_namespace(&database) || database.len() > 128 {
            candidate.status = DatabaseDiscoveryStatus::Unavailable;
            candidate.error = Some(
                "database name cannot be represented by the current identifier contract".into(),
            );
        } else {
            let inspection = (|| {
                attach(Some(&database))?;
                let sql = if connection.kind.is_postgres_family() {
                    query(
                        "SELECT nspname::text FROM pg_catalog.pg_namespace WHERE has_schema_privilege(current_user, oid, 'USAGE') ORDER BY nspname LIMIT 1025",
                    )
                } else if connection.kind == DataSourceKind::Sqlserver {
                    let modern: i64 = conn
                        .query_row(
                            &query("SELECT CASE WHEN OBJECT_ID('sys.objects') IS NULL THEN 0 ELSE 1 END"),
                            [],
                            |r| r.get(0),
                        )
                        .map_err(engine_error)?;
                    if modern == 1 {
                        query("SELECT TOP 1025 name FROM sys.schemas ORDER BY name")
                    } else {
                        query(
                            "SELECT DISTINCT TOP 1025 TABLE_SCHEMA FROM INFORMATION_SCHEMA.TABLES ORDER BY TABLE_SCHEMA",
                        )
                    }
                } else if connection.kind == DataSourceKind::Oracle {
                    query(
                        "SELECT username FROM (SELECT username FROM all_users ORDER BY username) WHERE ROWNUM <= 1025",
                    )
                } else if connection.kind.is_odbc_catalog_family() {
                    format!(
                        "SELECT DISTINCT table_schema FROM odbc_catalog_metadata({}) ORDER BY table_schema LIMIT 1025",
                        quote_literal(alias)
                    )
                } else {
                    query("SELECT DATABASE() AS schema_name")
                };
                let mut stmt = conn.prepare(&sql).map_err(engine_error)?;
                let mut rows = stmt.query([]).map_err(engine_error)?;
                let mut schemas = Vec::new();
                while let Some(row) = rows.next().map_err(engine_error)? {
                    schemas.push(metadata_text(row, 0)?);
                }
                if schemas.len() > 1024 {
                    return Err(EngineError::new(
                        "54000",
                        "database exceeds schema discovery budget",
                    ));
                }
                Ok(schemas
                    .into_iter()
                    .filter(|s| allowed_namespace(s))
                    .collect())
            })();
            let _ = control::detach(conn, alias);
            match inspection {
                Ok(schemas) => candidate.schemas = schemas,
                Err(e) if e.sqlstate == "57014" => return Err(e),
                Err(e) => {
                    candidate.status = if e.sqlstate == "42501" || e.sqlstate == "28P01" {
                        DatabaseDiscoveryStatus::Forbidden
                    } else {
                        DatabaseDiscoveryStatus::Unavailable
                    };
                    candidate.error = Some(e.to_string());
                }
            }
        }
        result.push(candidate);
    }
    Ok(result)
}
