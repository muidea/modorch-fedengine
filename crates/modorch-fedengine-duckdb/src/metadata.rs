use crate::{
    codec::type_name,
    error::{engine_error, metadata_text},
};
use duckdb::{Connection, params_from_iter, types::Value};
use modorch_fedengine_contract::{ColumnMeta, RemoteIndex};
use modorch_fedengine_core::*;
pub(super) fn describe(
    conn: &Connection,
    sql: &str,
    count: usize,
    overrides: Option<&[Option<ColumnMeta>]>,
) -> Result<Vec<ColumnMeta>> {
    let mut statement = conn
        .prepare(&format!("DESCRIBE {sql}"))
        .map_err(engine_error)?;
    let nulls = vec![Value::Null; count];
    let mut rows = statement
        .query(params_from_iter(nulls.iter()))
        .map_err(engine_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        let name: String = row.get(0).map_err(engine_error)?;
        let data_type: String = row.get(1).map_err(engine_error)?;
        let column = if let Some(Some(column)) = overrides.and_then(|o| o.get(result.len())) {
            column.clone()
        } else {
            let mut column = ColumnMeta::new(name.clone(), type_name(&data_type)?);
            column.type_modifier = crate::codec::type_modifier(&data_type);
            column
        };
        result.push(column);
    }
    Ok(result)
}
pub(super) fn discover_indexes(
    conn: &Connection,
    source: &DataSourceDefinition,
) -> Result<Vec<RemoteIndex>> {
    if source.kind == DataSourceKind::Starrocks {
        return Ok(vec![]);
    }
    if !source.kind.is_postgres_family() && !source.kind.is_mysql_family() {
        return discover_odbc_indexes(conn, source);
    }
    let remote = if source.kind.is_postgres_family() {
        "SELECT n.nspname AS schema_name, i.relname AS index_name, t.relname AS table_name, (x.indisprimary OR EXISTS(SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid=i.oid)) AS protected FROM pg_catalog.pg_index x JOIN pg_catalog.pg_class i ON i.oid=x.indexrelid JOIN pg_catalog.pg_class t ON t.oid=x.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=t.relnamespace"
    } else {
        "SELECT table_schema AS schema_name,index_name,table_name,(index_name='PRIMARY' OR non_unique=0) AS protected FROM information_schema.statistics GROUP BY table_schema,index_name,table_name,non_unique"
    };
    let sql = format!(
        "SELECT schema_name,index_name,table_name,protected FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(remote)
    );
    let mut stmt = conn.prepare(&sql).map_err(engine_error)?;
    let mut rows = stmt.query([]).map_err(engine_error)?;
    let mut indexes = Vec::new();
    while let Some(r) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(r, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            continue;
        }
        indexes.push(RemoteIndex {
            source: source.id.clone(),
            catalog: source.catalog_alias.clone(),
            schema,
            index: metadata_text(r, 1)?,
            table: metadata_text(r, 2)?,
            protected: r.get(3).map_err(engine_error)?,
        });
        if indexes.len() > 10000 {
            return Err(EngineError::new("54000", "index metadata limit exceeded"));
        }
    }
    Ok(indexes)
}

fn discover_odbc_indexes(
    conn: &Connection,
    source: &DataSourceDefinition,
) -> Result<Vec<RemoteIndex>> {
    use std::collections::{BTreeMap, BTreeSet};

    if !crate::guarded_crud::is_native_scanner_catalog(conn, &source.catalog_alias, source.kind) {
        return Ok(vec![]);
    }
    let fn_name = match source.kind {
        DataSourceKind::Oracle => "oracle_relational_metadata",
        DataSourceKind::Sqlserver => "sqlserver_relational_metadata",
        _ => "odbc_relational_metadata",
    };
    let present: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = ?",
            [fn_name],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !present {
        return Ok(vec![]);
    }

    let mut indexes = Vec::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();

    for schema in &source.allowed_schemas {
        match source.kind {
            DataSourceKind::Oracle => {
                let mut protected_names: BTreeSet<(String, String)> = BTreeSet::new();
                let cons_sql = format!(
                    "SELECT table_name, index_name FROM oracle_relational_metadata({}, 'constraints', {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(schema)
                );
                if let Ok(mut stmt) = conn.prepare(&cons_sql)
                    && let Ok(mut rows) = stmt.query([])
                {
                    while let Some(r) = rows.next().map_err(engine_error)? {
                        let table = metadata_text(r, 0)?;
                        let idx_name = metadata_text(r, 1)?;
                        if !idx_name.is_empty() {
                            protected_names.insert((table, idx_name));
                        }
                    }
                }
                let idx_sql = format!(
                    "SELECT table_name, index_name, uniqueness FROM oracle_relational_metadata({}, 'indexes', {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(schema)
                );
                let mut stmt = conn.prepare(&idx_sql).map_err(engine_error)?;
                let mut rows = stmt.query([]).map_err(engine_error)?;
                while let Some(r) = rows.next().map_err(engine_error)? {
                    let table = metadata_text(r, 0)?;
                    let index = metadata_text(r, 1)?;
                    if index.is_empty()
                        || !seen.insert((schema.clone(), table.clone(), index.clone()))
                    {
                        continue;
                    }
                    let protected = protected_names.contains(&(table.clone(), index.clone()))
                        || metadata_text(r, 2)?.eq_ignore_ascii_case("UNIQUE");
                    indexes.push(RemoteIndex {
                        source: source.id.clone(),
                        catalog: source.catalog_alias.clone(),
                        schema: schema.clone(),
                        index,
                        table,
                        protected,
                    });
                    if indexes.len() > 10000 {
                        return Err(EngineError::new("54000", "index metadata limit exceeded"));
                    }
                }
            }
            DataSourceKind::Sqlserver => {
                let mut protected_ids: BTreeSet<(String, String)> = BTreeSet::new();
                let cons_sql = format!(
                    "SELECT table_name, backing_index_id FROM sqlserver_relational_metadata({}, 'constraints', {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(schema)
                );
                if let Ok(mut stmt) = conn.prepare(&cons_sql)
                    && let Ok(mut rows) = stmt.query([])
                {
                    while let Some(r) = rows.next().map_err(engine_error)? {
                        let table = metadata_text(r, 0)?;
                        let idx_id = metadata_text(r, 1)?;
                        if !idx_id.is_empty() && idx_id != "0" {
                            protected_ids.insert((table, idx_id));
                        }
                    }
                }
                let idx_sql = format!(
                    "SELECT table_name, index_id, index_name, is_unique, is_primary_key FROM sqlserver_relational_metadata({}, 'indexes', {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(schema)
                );
                let mut stmt = conn.prepare(&idx_sql).map_err(engine_error)?;
                let mut rows = stmt.query([]).map_err(engine_error)?;
                while let Some(r) = rows.next().map_err(engine_error)? {
                    let table = metadata_text(r, 0)?;
                    let index_id = metadata_text(r, 1)?;
                    let index = metadata_text(r, 2)?;
                    if index.is_empty()
                        || !seen.insert((schema.clone(), table.clone(), index.clone()))
                    {
                        continue;
                    }
                    let is_unique = metadata_text(r, 3)? == "1";
                    let is_pk = metadata_text(r, 4)? == "1";
                    let protected =
                        is_pk || is_unique || protected_ids.contains(&(table.clone(), index_id));
                    indexes.push(RemoteIndex {
                        source: source.id.clone(),
                        catalog: source.catalog_alias.clone(),
                        schema: schema.clone(),
                        index,
                        table,
                        protected,
                    });
                    if indexes.len() > 10000 {
                        return Err(EngineError::new("54000", "index metadata limit exceeded"));
                    }
                }
            }
            _ => {
                let mut tstmt = conn
                    .prepare(
                        "SELECT table_name FROM duckdb_tables() WHERE database_name = ? AND schema_name = ? ORDER BY table_name",
                    )
                    .map_err(engine_error)?;
                let mut trows = tstmt
                    .query([&source.catalog_alias, schema])
                    .map_err(engine_error)?;
                let mut tables = Vec::new();
                while let Some(r) = trows.next().map_err(engine_error)? {
                    tables.push(metadata_text(r, 0)?);
                }
                for chunk in tables.chunks(128) {
                    let list_expr = format!(
                        "CAST([{}] AS VARCHAR[])",
                        chunk
                            .iter()
                            .map(|t| quote_literal(t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    let mut backed: BTreeSet<(String, String)> = BTreeSet::new();
                    let cons_sql = format!(
                        "SELECT table_name, backing_index FROM odbc_relational_metadata({}, 'constraintDetails', {}, {})",
                        quote_literal(&source.catalog_alias),
                        quote_literal(schema),
                        list_expr
                    );
                    if let Ok(mut stmt) = conn.prepare(&cons_sql)
                        && let Ok(mut rows) = stmt.query([])
                    {
                        while let Some(r) = rows.next().map_err(engine_error)? {
                            let table = metadata_text(r, 0)?;
                            let bidx = metadata_text(r, 1)?;
                            if !bidx.is_empty() {
                                backed.insert((table, bidx));
                            }
                        }
                    }
                    let idx_sql = format!(
                        "SELECT table_name, index_name, is_unique, is_primary FROM odbc_relational_metadata({}, 'indexDetails', {}, {})",
                        quote_literal(&source.catalog_alias),
                        quote_literal(schema),
                        list_expr
                    );
                    if let Ok(mut stmt) = conn.prepare(&idx_sql)
                        && let Ok(mut rows) = stmt.query([])
                    {
                        while let Some(r) = rows.next().map_err(engine_error)? {
                            let table = metadata_text(r, 0)?;
                            let index = metadata_text(r, 1)?;
                            if index.is_empty()
                                || !seen.insert((schema.clone(), table.clone(), index.clone()))
                            {
                                continue;
                            }
                            let is_unique = metadata_text(r, 2)?.eq_ignore_ascii_case("true");
                            let is_pk = metadata_text(r, 3)?.eq_ignore_ascii_case("true");
                            let protected = is_pk
                                || is_unique
                                || backed.contains(&(table.clone(), index.clone()));
                            indexes.push(RemoteIndex {
                                source: source.id.clone(),
                                catalog: source.catalog_alias.clone(),
                                schema: schema.clone(),
                                index,
                                table,
                                protected,
                            });
                            if indexes.len() > 10000 {
                                return Err(EngineError::new(
                                    "54000",
                                    "index metadata limit exceeded",
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = BTreeMap::<String, String>::new();
    Ok(indexes)
}
