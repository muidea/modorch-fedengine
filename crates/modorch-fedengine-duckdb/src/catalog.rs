use crate::error::{engine_error, metadata_text};
use duckdb::Connection;
use modorch_fedengine_core::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Hash serialized facts without retaining a second full JSON payload.
pub(super) fn fact_digest(value: &impl serde::Serialize) -> String {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).expect("serializable metadata facts");
    writer
        .0
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Comparable source definitions, independent of local OIDs, counters and scan timestamps.
/// Only borrowed per-object fields are staged; do not clone a catalog graph.
pub(super) fn object_definition_digest(object: &CatalogObject) -> String {
    let metadata = &object.metadata;
    let columns = metadata
        .columns
        .iter()
        .map(|(name, p)| {
            (
                name,
                &p.source_id,
                p.type_modifier,
                &p.default,
                &p.identity,
                &p.generated,
                &p.comment,
            )
        })
        .collect::<Vec<_>>();
    let constraints = metadata
        .constraints
        .iter()
        .map(|c| {
            (
                &c.source_id,
                &c.name,
                &c.kind,
                &c.columns,
                &c.backing_index,
                c.foreign_key.as_ref().map(|f| {
                    (
                        &f.schema,
                        &f.table,
                        &f.columns,
                        &f.on_update,
                        &f.on_delete,
                        &f.match_type,
                    )
                }),
                &c.definition,
                c.enforced,
                c.validated,
                c.deferrable,
                c.initially_deferred,
            )
        })
        .collect::<Vec<_>>();
    let indexes = metadata
        .indexes
        .iter()
        .map(|i| {
            (
                &i.source_id,
                &i.name,
                &i.method,
                i.unique,
                i.primary,
                i.valid,
                i.enabled,
                i.nulls_not_distinct,
                &i.keys,
                &i.include,
                &i.predicate,
                &i.definition,
            )
        })
        .collect::<Vec<_>>();
    let statuses = metadata
        .status
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "statistics" | "storage" | "activity" | "column_statistics"
            )
        })
        .map(|(name, s)| {
            (
                name,
                s.support,
                s.scan_state,
                s.complete,
                s.stale,
                &s.error_code,
                &s.unsupported_fields,
            )
        })
        .collect::<Vec<_>>();
    fact_digest(&(
        &object.schema,
        &object.name,
        &object.object_type,
        &metadata.source_id,
        &object.columns,
        &metadata.comment,
        columns,
        constraints,
        indexes,
        &metadata.view_definition,
        &metadata.view_dependencies,
        statuses,
    ))
}

/// Bound accounted metadata payload without allocating another serialized copy.
pub(super) fn enforce_budget(value: &impl serde::Serialize, limit: usize) -> Result<()> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(std::io::Error::other("metadata budget exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { bytes: 0, limit }, value)
        .map_err(|_| EngineError::new("54000", "metadata operation payload budget exceeded"))
}
fn remote_sql(source: &DataSourceDefinition, query: &str) -> String {
    format!(
        "SELECT * FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(query)
    )
}
fn sqlserver_has_sys_objects(conn: &Connection, source: &DataSourceDefinition) -> Result<bool> {
    let probe = remote_sql(
        source,
        "SELECT CASE WHEN OBJECT_ID('sys.objects') IS NULL THEN 0 ELSE 1 END",
    );
    let modern: i64 = conn
        .query_row(&probe, [], |r| r.get(0))
        .map_err(engine_error)?;
    Ok(modern == 1)
}
pub(super) fn discover(conn: &Connection, source: &DataSourceDefinition) -> Result<Vec<String>> {
    let sql = if source.kind.is_postgres_family() {
        remote_sql(
            source,
            "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name",
        )
    } else if source.kind.is_mysql_family() {
        remote_sql(source, "SELECT DATABASE() AS schema_name")
    } else if source.kind == DataSourceKind::Oracle {
        remote_sql(
            source,
            "SELECT username AS schema_name FROM all_users ORDER BY username",
        )
    } else if source.kind == DataSourceKind::Sqlserver {
        let query = if sqlserver_has_sys_objects(conn, source)? {
            "SELECT name AS schema_name FROM sys.schemas ORDER BY name"
        } else {
            "SELECT DISTINCT TABLE_SCHEMA AS schema_name FROM INFORMATION_SCHEMA.TABLES ORDER BY TABLE_SCHEMA"
        };
        remote_sql(source, query)
    } else {
        format!(
            "SELECT DISTINCT table_schema FROM odbc_catalog_metadata({}) ORDER BY table_schema",
            quote_literal(&source.catalog_alias)
        )
    };
    let mut statement = conn.prepare(&sql).map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut schemas = Vec::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if allowed_namespace(&schema) {
            schemas.push(schema);
        }
        if schemas.len() > 10_000 {
            return Err(EngineError::new("54000", "schema discovery limit exceeded"));
        }
    }
    Ok(schemas)
}
pub(super) fn capture(conn: &Connection, revision: &SourceRevision) -> Result<CatalogSnapshot> {
    capture_bounded(conn, revision, 64 * 1024 * 1024)
}
pub(super) fn capture_bounded(
    conn: &Connection,
    revision: &SourceRevision,
    budget: usize,
) -> Result<CatalogSnapshot> {
    for attempt in 0..3 {
        if attempt > 0 {
            conn.execute_batch(&crate::extensions::clear_cache_sql(
                revision.definition.kind.extension(),
                &revision.definition.catalog_alias,
            ))
            .map_err(engine_error)?;
        }
        match capture_once(conn, revision, budget) {
            Err(error) if error.sqlstate == "40001" && attempt < 2 => continue,
            result => return result,
        }
    }
    unreachable!()
}
fn capture_once(
    conn: &Connection,
    revision: &SourceRevision,
    budget: usize,
) -> Result<CatalogSnapshot> {
    let started = std::time::Instant::now();
    let source = revision.effective_definition();
    let available = discover(conn, &source)?;
    if source
        .allowed_schemas
        .iter()
        .any(|s| !available.contains(s))
    {
        return Err(EngineError::config(
            "selected schema does not exist in the target database",
        ));
    }
    let _memory = crate::memory::Stage::new("remote_capture");
    let mut objects = read_core(conn, &source, budget)?;
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog core enumeration finished");
    enforce_budget(&objects.values().collect::<Vec<_>>(), budget)?;
    let before = core_identity(&objects);
    let mapping_started = std::time::Instant::now();
    // Ask the pinned extension for its actual mapping; do not guess remote types.
    for object in objects.values_mut() {
        // Zero-column relations (e.g. `CREATE TABLE t ()` in PostgreSQL) have no columns
        // to map and cannot be bound via `SELECT *` in extension scanners.
        if object.columns.is_empty() {
            continue;
        }
        let sql = format!(
            "DESCRIBE SELECT * FROM {}.{}.{} LIMIT 0",
            quote_identifier(&source.catalog_alias),
            quote_identifier(&object.schema),
            quote_identifier(&object.name)
        );
        let mappings = (|| -> Result<BTreeMap<String, String>> {
            let mut statement = conn.prepare(&sql).map_err(engine_error)?;
            let mut rows = statement.query([]).map_err(engine_error)?;
            let mut mappings = BTreeMap::new();
            while let Some(row) = rows.next().map_err(engine_error)? {
                mappings.insert(
                    row.get(0).map_err(engine_error)?,
                    row.get(1).map_err(engine_error)?,
                );
            }
            Ok(mappings)
        })();
        match mappings {
            Ok(mappings) => {
                for column in &mut object.columns {
                    column.mapped_type = mappings.get(&column.name).cloned();
                    column.supported = column
                        .mapped_type
                        .as_ref()
                        .is_some_and(|t| crate::codec::type_name(t).is_ok());
                }
            }
            Err(error) if error.sqlstate == "0A000" => {}
            Err(error) => return Err(error),
        }
    }
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=mapping_started.elapsed().as_secs_f64()*1000., "catalog type mapping finished");
    let facts_started = std::time::Instant::now();
    crate::relational::capture(conn, &source, &mut objects, budget)?;
    tracing::debug!(source=%source.id.0, elapsed_ms=facts_started.elapsed().as_secs_f64()*1000., "catalog relational facts finished");
    // Extension queries may use different remote transactions. A second bounded
    // enumeration detects observed relation/column drift before any publication.
    if core_identity(&read_core(conn, &source, budget)?) != before {
        return Err(EngineError::new(
            "40001",
            "remote DDL changed the scanned catalog; retry refresh",
        ));
    }
    let objects: Vec<_> = objects.into_values().collect();
    let structure_digest = structure_digest(&source.allowed_schemas, &objects);
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog remote capture finished");
    Ok(CatalogSnapshot {
        database_oid: 0,
        schema_oids: BTreeMap::new(),
        id: uuid::Uuid::new_v4().to_string(),
        incarnation: revision.incarnation.clone(),
        revision: revision.revision,
        generation: 0,
        refreshed_at: chrono::Utc::now().to_rfc3339(),
        complete: true,
        stale: false,
        structure_digest,
        schemas: source.allowed_schemas,
        objects,
    })
}
fn structure_digest(schemas: &[String], objects: &[CatalogObject]) -> String {
    // Hash one object's definition at a time. Scanner bindings depend on
    // defaults/constraints/indexes too; changing only statistics must not evict
    // every attached source's cache. Computed before local identity allocation;
    // omit scan timestamps and physical statistics.
    let definitions: Vec<_> = objects
        .iter()
        .map(|o| {
            let indexes: Vec<_> = o
                .metadata
                .indexes
                .iter()
                .map(|i| {
                    (
                        &i.source_id,
                        &i.name,
                        &i.method,
                        i.unique,
                        i.primary,
                        i.valid,
                        i.enabled,
                        i.nulls_not_distinct,
                        &i.keys,
                        &i.include,
                        &i.predicate,
                        &i.definition,
                    )
                })
                .collect();
            fact_digest(&(
                &o.schema,
                &o.name,
                &o.object_type,
                &o.metadata.source_id,
                &o.columns,
                &o.metadata.columns,
                &o.metadata.dropped_attnums,
                &o.metadata.constraints,
                indexes,
                &o.metadata.view_definition,
                &o.metadata.view_dependencies,
            ))
        })
        .collect();
    fact_digest(&(schemas, definitions))
}
type CoreObjects = BTreeMap<(String, String), CatalogObject>;
fn core_identity(objects: &CoreObjects) -> String {
    // Only source identities and original core facts; mapped types and scan times
    // are deliberately excluded from the remote consistency comparison.
    let facts = objects
        .values()
        .map(|o| {
            (
                &o.schema,
                &o.name,
                &o.object_type,
                &o.metadata.source_id,
                o.columns
                    .iter()
                    .map(|c| (&c.name, c.ordinal, &c.remote_type, c.nullable))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    fact_digest(&facts)
}
fn read_core(
    conn: &Connection,
    source: &DataSourceDefinition,
    budget: usize,
) -> Result<CoreObjects> {
    if source.kind.is_odbc_catalog_family() {
        return read_odbc_catalog_core(conn, source, budget);
    }
    let mut objects = BTreeMap::new();
    let schemas_in = source
        .allowed_schemas
        .iter()
        .map(|s| quote_literal(s))
        .collect::<Vec<_>>()
        .join(",");
    let schemas_in = if schemas_in.is_empty() {
        "NULL".to_string()
    } else {
        schemas_in
    };
    let modern_sqlserver = if source.kind == DataSourceKind::Sqlserver {
        sqlserver_has_sys_objects(conn, source)?
    } else {
        true
    };
    let query = if source.kind.is_postgres_family() {
        scoped_query(
            source,
            "SELECT n.nspname AS table_schema,c.relname AS table_name,CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'BASE TABLE' END AS table_type,c.oid::text AS source_id FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') AND has_schema_privilege(current_user,n.oid,'USAGE') AND has_table_privilege(current_user,c.oid,'SELECT') ORDER BY n.nspname,c.relname",
        )
    } else if source.kind == DataSourceKind::Oracle {
        format!(
            "SELECT owner AS table_schema, object_name AS table_name, CASE object_type WHEN 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type, TO_CHAR(object_id) AS source_id FROM all_objects WHERE object_type IN ('TABLE','VIEW') AND owner IN ({schemas_in}) ORDER BY owner, object_name"
        )
    } else if source.kind == DataSourceKind::Sqlserver {
        if modern_sqlserver {
            format!(
                "SELECT s.name AS table_schema, t.name AS table_name, CASE t.type WHEN 'V' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type, CAST(t.object_id AS varchar(30)) AS source_id FROM sys.objects t JOIN sys.schemas s ON s.schema_id=t.schema_id WHERE t.is_ms_shipped=0 AND t.type IN ('U','V') AND s.name IN ({schemas_in}) ORDER BY s.name, t.name"
            )
        } else {
            format!(
                "SELECT TABLE_SCHEMA AS table_schema, TABLE_NAME AS table_name, CASE TABLE_TYPE WHEN 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type, CAST(OBJECT_ID(QUOTENAME(TABLE_SCHEMA)+'.'+QUOTENAME(TABLE_NAME)) AS varchar(30)) AS source_id FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA IN ({schemas_in}) ORDER BY TABLE_SCHEMA, TABLE_NAME"
            )
        }
    } else {
        "SELECT table_schema, table_name, table_type,NULL AS source_id FROM information_schema.tables WHERE table_schema=DATABASE() ORDER BY table_schema, table_name".into()
    };
    let mut statement = conn
        .prepare(&remote_sql(source, &query))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            continue;
        }
        let name = metadata_text(row, 1)?;
        objects.insert(
            (schema.clone(), name.clone()),
            CatalogObject {
                metadata: ObjectMetadata {
                    source_id: row.get(3).map_err(engine_error)?,
                    ..Default::default()
                },
                schema,
                name,
                object_type: metadata_text(row, 2)?,
                columns: vec![],
                indexes: vec![],
            },
        );
        if objects.len() > 10_000 {
            return Err(EngineError::new("54000", "catalog object limit exceeded"));
        }
    }
    let query = if source.kind.is_postgres_family() {
        scoped_query(
            source,
            "SELECT n.nspname AS table_schema,c.relname AS table_name,a.attname AS column_name,a.attnum AS ordinal_position,pg_catalog.format_type(a.atttypid,a.atttypmod) AS data_type,CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN ('r','v','m','f','p') AND has_schema_privilege(current_user,n.oid,'USAGE') AND has_table_privilege(current_user,c.oid,'SELECT') ORDER BY n.nspname,c.relname,a.attnum",
        )
    } else if source.kind == DataSourceKind::Oracle {
        format!(
            "SELECT owner AS table_schema, table_name, column_name, column_id AS ordinal_position, data_type, CASE WHEN nullable = 'Y' THEN 'YES' ELSE 'NO' END AS is_nullable FROM all_tab_columns WHERE owner IN ({schemas_in}) ORDER BY owner, table_name, column_id"
        )
    } else if source.kind == DataSourceKind::Sqlserver {
        if modern_sqlserver {
            format!(
                "SELECT s.name AS table_schema, t.name AS table_name, c.name AS column_name, c.column_id AS ordinal_position, ty.name AS data_type, CASE WHEN c.is_nullable = 1 THEN 'YES' ELSE 'NO' END AS is_nullable FROM sys.objects t JOIN sys.schemas s ON s.schema_id=t.schema_id JOIN sys.columns c ON c.object_id=t.object_id JOIN sys.types ty ON ty.user_type_id=c.user_type_id WHERE t.is_ms_shipped=0 AND t.type IN ('U','V') AND s.name IN ({schemas_in}) ORDER BY s.name, t.name, c.column_id"
            )
        } else {
            format!(
                "SELECT TABLE_SCHEMA AS table_schema, TABLE_NAME AS table_name, COLUMN_NAME AS column_name, ORDINAL_POSITION AS ordinal_position, DATA_TYPE AS data_type, IS_NULLABLE AS is_nullable FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA IN ({schemas_in}) ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"
            )
        }
    } else {
        "SELECT table_schema,table_name,column_name,ordinal_position,column_type AS data_type,is_nullable FROM information_schema.columns WHERE table_schema=DATABASE() ORDER BY table_schema,table_name,ordinal_position".into()
    };
    let mut statement = conn
        .prepare(&remote_sql(source, &query))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut count = 0;
    let mut bytes = 0usize;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let key: (String, String) = (metadata_text(row, 0)?, metadata_text(row, 1)?);
        let object = objects.get_mut(&key).ok_or_else(|| {
            EngineError::new("40001", "remote DDL changed the scanned relation set")
        })?;
        let remote_type = metadata_text(row, 4)?;

        object.columns.push(CatalogColumn {
            name: metadata_text(row, 2)?,
            ordinal: row.get::<_, u32>(3).map_err(engine_error)?,
            remote_type,
            supported: false,
            mapped_type: None,
            nullable: metadata_text(row, 5)? == "YES",
        });
        count += 1;
        let col = object.columns.last().unwrap();
        bytes = bytes.saturating_add(
            col.name.len() + col.remote_type.len() + std::mem::size_of::<CatalogColumn>() + 128,
        );
        if count > 100_000 || bytes > budget {
            return Err(EngineError::new("54000", "catalog column limit exceeded"));
        }
    }
    Ok(objects)
}
fn read_odbc_catalog_core(
    conn: &Connection,
    source: &DataSourceDefinition,
    budget: usize,
) -> Result<CoreObjects> {
    let sql = format!(
        "SELECT table_schema, table_name, table_type, column_name, data_type, ordinal_position FROM odbc_catalog_metadata({}) ORDER BY table_schema, table_name, ordinal_position",
        quote_literal(&source.catalog_alias)
    );
    let mut statement = conn.prepare(&sql).map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut objects = BTreeMap::new();
    let mut count = 0usize;
    let mut bytes = 0usize;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            continue;
        }
        let table = metadata_text(row, 1)?;
        let table_type = metadata_text(row, 2)?;
        let column_name = metadata_text(row, 3)?;
        let remote_type = metadata_text(row, 4)?;
        let ordinal = row.get::<_, u32>(5).map_err(engine_error)?;
        let key = (schema.clone(), table.clone());
        if !objects.contains_key(&key) && objects.len() >= 10_000 {
            return Err(EngineError::new("54000", "catalog object limit exceeded"));
        }
        let object = objects.entry(key).or_insert_with(|| CatalogObject {
            metadata: ObjectMetadata::default(),
            schema,
            name: table,
            object_type: table_type,
            columns: vec![],
            indexes: vec![],
        });
        object.columns.push(CatalogColumn {
            name: column_name,
            ordinal,
            remote_type,
            supported: false,
            mapped_type: None,
            nullable: true,
        });
        count += 1;
        let col = object.columns.last().unwrap();
        bytes = bytes.saturating_add(
            col.name.len() + col.remote_type.len() + std::mem::size_of::<CatalogColumn>() + 128,
        );
        if count > 100_000 || bytes > budget {
            return Err(EngineError::new("54000", "catalog column limit exceeded"));
        }
    }
    Ok(objects)
}
fn scoped_query(source: &DataSourceDefinition, query: &str) -> String {
    if source.kind.is_postgres_family() {
        let schemas = source
            .allowed_schemas
            .iter()
            .map(|s| quote_literal(s))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = format!(
            "n.nspname IN ({})",
            if schemas.is_empty() { "NULL" } else { &schemas }
        );
        let (sql, order) = query.split_once(" ORDER BY ").unwrap_or((query, ""));
        format!(
            "{sql} {} {predicate} ORDER BY {order}",
            if sql.contains(" WHERE ") {
                "AND"
            } else {
                "WHERE"
            }
        )
    } else {
        query.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merged_object_digest_ignores_local_identity_and_scan_counters() {
        let page: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/metadata/published-pg16-directory.json"
        ))
        .unwrap();
        let mut objects: Vec<CatalogObject> =
            serde_json::from_value(page["objects"].clone()).unwrap();
        let original = objects.pop().unwrap();
        let expected = object_definition_digest(&original);
        let mut local = original.clone();
        local.metadata.oid += 1;
        local.metadata.storage.main_bytes = Some(12345);
        local.metadata.activity.seq_scan = Some(99);
        local.metadata.statistics.pages = Some(100);
        local
            .metadata
            .status
            .insert("statistics".into(), MetadataStatus::success());
        for status in local.metadata.status.values_mut() {
            status.last_attempt_at = None;
            status.last_success_at = None;
        }
        for column in local.metadata.columns.values_mut() {
            column.default_oid += 1;
            column.type_oid += 1;
            column.attnum += 1;
        }
        for constraint in &mut local.metadata.constraints {
            constraint.oid += 1;
            if let Some(foreign_key) = &mut constraint.foreign_key {
                foreign_key.reference_state = MetadataReferenceState::OutsideScope;
            }
        }
        for index in &mut local.metadata.indexes {
            index.oid += 1;
            index.storage.main_bytes = Some(23456);
            index.activity.idx_scan = Some(88);
        }
        assert_eq!(object_definition_digest(&local), expected);
        let mut changed = original.clone();
        changed.metadata.columns.get_mut("amount").unwrap().default =
            MetadataExpression::source("42".into(), DataSourceKind::Postgres);
        assert_ne!(object_definition_digest(&changed), expected);
        let mut changed = original.clone();
        changed.metadata.constraints[0].validated = Some(false);
        assert_ne!(object_definition_digest(&changed), expected);
        let mut changed = original;
        changed.metadata.indexes[0].valid = Some(false);
        assert_ne!(object_definition_digest(&changed), expected);
    }
    #[test]
    fn definition_digest_tracks_binding_changes_but_not_statistics() {
        let page: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/metadata/published-pg16-directory.json"
        ))
        .unwrap();
        let objects: Vec<CatalogObject> = serde_json::from_value(page["objects"].clone()).unwrap();
        let schemas = vec!["sales".into()];
        let expected = structure_digest(&schemas, &objects);
        let mut stats = objects.clone();
        for object in &mut stats {
            object.metadata.storage.main_bytes = Some(12345);
            object.metadata.activity.seq_scan = Some(999);
            object.metadata.statistics.pages = Some(100);
            object
                .metadata
                .status
                .insert("statistics".into(), MetadataStatus::success());
            for index in &mut object.metadata.indexes {
                index.storage.main_bytes = Some(54321);
                index.activity.idx_scan = Some(88);
            }
        }
        assert_eq!(structure_digest(&schemas, &stats), expected);
        let mut defaults = objects.clone();
        defaults[0]
            .metadata
            .columns
            .values_mut()
            .next()
            .unwrap()
            .default = MetadataExpression::source("42".into(), DataSourceKind::Postgres);
        assert_ne!(structure_digest(&schemas, &defaults), expected);
        let mut constraints = objects.clone();
        constraints[0].metadata.constraints.clear();
        assert_ne!(structure_digest(&schemas, &constraints), expected);
        let mut indexes = objects.clone();
        indexes[0].metadata.indexes[0].valid = Some(false);
        assert_ne!(structure_digest(&schemas, &indexes), expected);
        let mut views = objects;
        views[0].metadata.view_definition =
            MetadataExpression::source("SELECT 2".into(), DataSourceKind::Postgres);
        assert_ne!(structure_digest(&schemas, &views), expected);
    }
}
