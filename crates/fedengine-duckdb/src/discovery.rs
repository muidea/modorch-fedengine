//! Bounded instance enumeration on an isolated DuckDB connection.
use crate::{
    control, credentials,
    error::{engine_error, metadata_text},
};
use duckdb::Connection;
use fedengine_core::*;
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
        let dsn = credentials::target_dsn(
            &connection.secret_ref,
            &config.management.credentials_directory,
            connection.kind,
            &connection.instance,
            database,
        )?;
        conn.execute_batch(&format!(
            "ATTACH {} AS {} (TYPE {}, READ_ONLY)",
            quote_literal(&dsn),
            quote_identifier(alias),
            connection.kind.extension()
        ))
        .map_err(engine_error)
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
    } else {
        attach(if connection.kind == DataSourceKind::Postgres {
            Some(
                connection
                    .bootstrap_database
                    .as_deref()
                    .unwrap_or("postgres"),
            )
        } else {
            Some("information_schema")
        })?;
        let sql = if connection.kind == DataSourceKind::Postgres {
            "SELECT datname::text, (datallowconn AND NOT datistemplate AND has_database_privilege(current_user, oid, 'CONNECT')) AS connectable FROM pg_catalog.pg_database ORDER BY datname LIMIT 129"
        } else {
            "SELECT SCHEMA_NAME, TRUE AS connectable FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME LIMIT 129"
        };
        let mut stmt = conn.prepare(&query(sql)).map_err(engine_error)?;
        let mut rows = stmt.query([]).map_err(engine_error)?;
        let mut databases = Vec::new();
        while let Some(row) = rows.next().map_err(engine_error)? {
            databases.push((
                metadata_text(row, 0)?,
                if connection.kind == DataSourceKind::Postgres {
                    row.get::<_, bool>(1).map_err(engine_error)?
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
                let sql = if connection.kind == DataSourceKind::Postgres {
                    "SELECT nspname::text FROM pg_catalog.pg_namespace WHERE has_schema_privilege(current_user, oid, 'USAGE') ORDER BY nspname LIMIT 1025"
                } else {
                    "SELECT DATABASE() AS schema_name"
                };
                let mut stmt = conn.prepare(&query(sql)).map_err(engine_error)?;
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
