//! Project logical virtual bindings from typed source catalogs as ordinary
//! PostgreSQL relations, indexes, constraints, defaults, definitions and stats.
//! All rows remain in the caller's unpublished transaction.
use super::*;
use crate::identity::OidBatch;

pub(crate) struct ForeignProjection<'a> {
    pub snapshot: &'a CatalogSnapshot,
    pub source: &'a CatalogSnapshot,
    pub alias: &'a str,
    pub binding: &'a VirtualSchema,
    pub native_identity: bool,
}

impl ForeignProjection<'_> {
    pub fn publish(&self, conn: &Connection, oids: &mut OidBatch<'_>) -> Result<()> {
        let _memory = crate::memory::Stage::new("virtual_projection");
        let Self {
            snapshot,
            source,
            alias,
            binding,
            native_identity,
        } = self;
        super::publish(conn, snapshot, alias)?;
        let epoch = binding.object_epoch.clone();
        conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE __fed_foreign_relations(remote_oid UINTEGER,local_oid UINTEGER,object VARCHAR);
             CREATE OR REPLACE TEMP TABLE __fed_local_indexes(remote_oid UINTEGER,local_oid UINTEGER,table_remote_oid UINTEGER,table_local_oid UINTEGER,object VARCHAR,index_name VARCHAR);
             CREATE OR REPLACE TEMP TABLE __fed_local_constraints(remote_oid UINTEGER,local_oid UINTEGER,table_remote_oid UINTEGER,table_local_oid UINTEGER,object VARCHAR,conname VARCHAR);
             CREATE OR REPLACE TEMP TABLE __fed_local_attrdefs(remote_oid UINTEGER,local_oid UINTEGER,table_remote_oid UINTEGER,table_local_oid UINTEGER,object VARCHAR,adnum SMALLINT);",
        )
        .map_err(engine_error)?;

        {
            let mut statement = conn
                .prepare("SELECT p.oid,p.relname,o.native_id FROM _fedengine.meta_pg_class p LEFT JOIN _fedengine.catalog_objects o ON o.snapshot_id=p._snapshot_id AND o.schema_name=p._schema AND o.object_name=p.relname WHERE p._snapshot_id=? AND p._schema=? AND p.relkind IN ('r','v','m','p','f') ORDER BY p.relname")
                .map_err(engine_error)?;
            let mut rows = statement
                .query(params![source.id, binding.config.remote_schema])
                .map_err(engine_error)?;
            let mut app = conn
                .appender_to_db("__fed_foreign_relations", "main")
                .map_err(engine_error)?;
            while let Some(row) = rows.next().map_err(engine_error)? {
                let remote: u32 = row.get(0).map_err(engine_error)?;
                let name: String = row.get(1).map_err(engine_error)?;
                let native: Option<String> = row.get(2).map_err(engine_error)?;
                let local = if *native_identity {
                    oids.oid(&epoch, &["relation", native.as_deref().unwrap_or(&name)])?
                } else {
                    oids.oid(&epoch, &[&source.incarnation, &remote.to_string()])?
                };
                app.append_row(params![remote, local, name])
                    .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }

        {
            let mut statement = conn
                .prepare("SELECT i.indexrelid,m.remote_oid,m.local_oid,m.object,c.relname FROM _fedengine.meta_pg_index i JOIN _fedengine.meta_pg_class c ON c._snapshot_id=i._snapshot_id AND c._schema=i._schema AND c.oid=i.indexrelid JOIN __fed_foreign_relations m ON m.remote_oid=i.indrelid WHERE i._snapshot_id=? AND i._schema=? AND c.relkind IN ('i','I') ORDER BY m.object,c.relname")
                .map_err(engine_error)?;
            let mut rows = statement
                .query(params![source.id, binding.config.remote_schema])
                .map_err(engine_error)?;
            let mut app = conn
                .appender_to_db("__fed_local_indexes", "main")
                .map_err(engine_error)?;
            while let Some(row) = rows.next().map_err(engine_error)? {
                let remote: u32 = row.get(0).map_err(engine_error)?;
                let table_remote: u32 = row.get(1).map_err(engine_error)?;
                let table_local: u32 = row.get(2).map_err(engine_error)?;
                let object: String = row.get(3).map_err(engine_error)?;
                let index_name: String = row.get(4).map_err(engine_error)?;
                let local = oids.oid(&epoch, &["index", &object, &index_name])?;
                app.append_row(params![
                    remote,
                    local,
                    table_remote,
                    table_local,
                    object,
                    index_name
                ])
                .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }

        {
            let mut statement = conn
                .prepare("SELECT c.oid,m.remote_oid,m.local_oid,m.object,c.conname FROM _fedengine.meta_pg_constraint c JOIN __fed_foreign_relations m ON c.conrelid=m.remote_oid WHERE c._snapshot_id=? AND c._schema=? AND (c.contype<>'f' OR EXISTS(SELECT 1 FROM __fed_foreign_relations tf WHERE tf.remote_oid=c.confrelid)) ORDER BY m.object,c.conname")
                .map_err(engine_error)?;
            let mut rows = statement
                .query(params![source.id, binding.config.remote_schema])
                .map_err(engine_error)?;
            let mut app = conn
                .appender_to_db("__fed_local_constraints", "main")
                .map_err(engine_error)?;
            while let Some(row) = rows.next().map_err(engine_error)? {
                let remote: u32 = row.get(0).map_err(engine_error)?;
                let table_remote: u32 = row.get(1).map_err(engine_error)?;
                let table_local: u32 = row.get(2).map_err(engine_error)?;
                let object: String = row.get(3).map_err(engine_error)?;
                let conname: String = row.get(4).map_err(engine_error)?;
                let local = oids.oid(&epoch, &["constraint", &object, &conname])?;
                app.append_row(params![
                    remote,
                    local,
                    table_remote,
                    table_local,
                    object,
                    conname
                ])
                .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }

        {
            let mut statement = conn
                .prepare("SELECT a.oid,m.remote_oid,m.local_oid,m.object,a.adnum FROM _fedengine.meta_pg_attrdef a JOIN __fed_foreign_relations m ON a.adrelid=m.remote_oid WHERE a._snapshot_id=? AND a._schema=? ORDER BY m.object,a.adnum")
                .map_err(engine_error)?;
            let mut rows = statement
                .query(params![source.id, binding.config.remote_schema])
                .map_err(engine_error)?;
            let mut app = conn
                .appender_to_db("__fed_local_attrdefs", "main")
                .map_err(engine_error)?;
            while let Some(row) = rows.next().map_err(engine_error)? {
                let remote: u32 = row.get(0).map_err(engine_error)?;
                let table_remote: u32 = row.get(1).map_err(engine_error)?;
                let table_local: u32 = row.get(2).map_err(engine_error)?;
                let object: String = row.get(3).map_err(engine_error)?;
                let adnum: i16 = row.get(4).map_err(engine_error)?;
                let local = oids.oid(&epoch, &["attrdef", &object, &adnum.to_string()])?;
                app.append_row(params![
                    remote,
                    local,
                    table_remote,
                    table_local,
                    object,
                    adnum
                ])
                .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }

        let id = quote_literal(&snapshot.id);
        let alias = quote_literal(alias);
        let schema = quote_literal(&binding.config.name);

        // Project ordinary table, view, attribute, type, description and statistics rows.
        for relation in [
            "pg_class",
            "pg_tables",
            "pg_views",
            "pg_matviews",
            "pg_attribute",
            "pg_type",
            "pg_description",
            "pg_stats",
            "pg_stat_all_tables",
            "pg_stat_user_tables",
            "information_schema.tables",
            "information_schema.views",
            "information_schema.columns",
        ] {
            let fields = &catalog_schema()[relation];
            let expressions = fields
                .iter()
                .map(|field| {
                    let name = field.attname.as_str();
                    match (relation, name) {
                        ("pg_class", "oid")
                        | ("pg_attribute", "attrelid")
                        | ("pg_description", "objoid")
                        | ("pg_stat_all_tables" | "pg_stat_user_tables", "relid") => {
                            "m.local_oid".into()
                        }
                        ("pg_class", "relnamespace") | ("pg_type", "typnamespace") => {
                            binding.oid.to_string()
                        }
                        ("pg_class", "relkind") => {
                            "CASE WHEN p.relkind='f' THEN 'r' ELSE p.relkind END".into()
                        }
                        (
                            "pg_tables"
                            | "pg_views"
                            | "pg_matviews"
                            | "pg_stats"
                            | "pg_stat_all_tables"
                            | "pg_stat_user_tables",
                            "schemaname",
                        ) => schema.clone(),
                        (
                            "information_schema.tables" | "information_schema.views",
                            "table_catalog",
                        )
                        | ("information_schema.columns", "table_catalog" | "udt_catalog") => {
                            alias.clone()
                        }
                        (
                            "information_schema.tables"
                            | "information_schema.views"
                            | "information_schema.columns",
                            "table_schema",
                        ) => schema.clone(),
                        ("information_schema.tables", "table_type") => {
                            "CASE WHEN p.table_type='FOREIGN' THEN 'BASE TABLE' ELSE p.table_type END"
                                .into()
                        }
                        ("information_schema.columns", "udt_schema") => format!(
                            "CASE WHEN p.udt_schema='pg_catalog' THEN 'pg_catalog' ELSE {schema} END"
                        ),
                        _ => format!("p.{}", quote_identifier(name)),
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            let filter = match relation {
                "pg_class" => " AND p.oid=m.remote_oid",
                "pg_attribute" => " AND p.attrelid=m.remote_oid",
                "pg_description" => " AND p.objoid=m.remote_oid",
                "pg_stat_all_tables" | "pg_stat_user_tables" => " AND p.relid=m.remote_oid",
                _ => "",
            };
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN __fed_foreign_relations m ON p._object=m.object WHERE p._snapshot_id=? AND p._schema=?{filter}"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }

        // Project column defaults.
        {
            let relation = "pg_attrdef";
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match field.attname.as_str() {
                    "oid" => "d.local_oid".into(),
                    "adrelid" => "d.table_local_oid".into(),
                    other => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN __fed_local_attrdefs d ON p.oid=d.remote_oid AND p._object=d.object WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }

        // Project index relations, pg_index metadata, index attributes, descriptions and activity.
        for relation in [
            "pg_class",
            "pg_index",
            "pg_attribute",
            "pg_description",
            "pg_stat_all_indexes",
            "pg_stat_user_indexes",
        ] {
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match (relation, field.attname.as_str()) {
                    ("pg_class", "oid")
                    | ("pg_index", "indexrelid")
                    | ("pg_attribute", "attrelid")
                    | ("pg_description", "objoid")
                    | ("pg_stat_all_indexes" | "pg_stat_user_indexes", "indexrelid") => {
                        "i.local_oid".into()
                    }
                    ("pg_class", "relnamespace") => binding.oid.to_string(),
                    ("pg_index", "indrelid")
                    | ("pg_stat_all_indexes" | "pg_stat_user_indexes", "relid") => {
                        "i.table_local_oid".into()
                    }
                    ("pg_stat_all_indexes" | "pg_stat_user_indexes", "schemaname") => {
                        schema.clone()
                    }
                    (_, other) => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let key_col = match relation {
                "pg_class" => "oid",
                "pg_index" | "pg_stat_all_indexes" | "pg_stat_user_indexes" => "indexrelid",
                "pg_attribute" => "attrelid",
                "pg_description" => "objoid",
                _ => unreachable!(),
            };
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN __fed_local_indexes i ON p.{key_col}=i.remote_oid AND p._object=i.object WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }
        {
            let relation = "pg_indexes";
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match field.attname.as_str() {
                    "schemaname" => schema.clone(),
                    other => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN (SELECT DISTINCT object,index_name FROM __fed_local_indexes) i ON p._object=i.object AND p.indexname=i.index_name WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }

        // Project constraints, constraint descriptions and information_schema constraint views.
        {
            let relation = "pg_constraint";
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match field.attname.as_str() {
                    "oid" => "c.local_oid".into(),
                    "connamespace" => binding.oid.to_string(),
                    "conrelid" => "c.table_local_oid".into(),
                    "conindid" => {
                        "CASE WHEN p.conindid IS NULL THEN NULL ELSE COALESCE(li.local_oid, 0) END"
                            .into()
                    }
                    "confrelid" => {
                        "CASE WHEN COALESCE(p.confrelid, 0) = 0 THEN 0 ELSE COALESCE(tf.local_oid, 0) END"
                            .into()
                    }
                    other => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,CASE WHEN p._target_schema IS NOT NULL THEN {schema} ELSE NULL END,p._target_object,{expressions} FROM {table} p JOIN __fed_local_constraints c ON p.oid=c.remote_oid AND p._object=c.object LEFT JOIN (SELECT DISTINCT remote_oid,local_oid FROM __fed_local_indexes) li ON p.conindid=li.remote_oid LEFT JOIN __fed_foreign_relations tf ON p.confrelid=tf.remote_oid WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }
        {
            let relation = "pg_description";
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match field.attname.as_str() {
                    "objoid" => "c.local_oid".into(),
                    other => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN __fed_local_constraints c ON p.objoid=c.remote_oid AND p._object=c.object WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }
        for relation in [
            "information_schema.table_constraints",
            "information_schema.key_column_usage",
            "information_schema.constraint_column_usage",
            "information_schema.constraint_table_usage",
            "information_schema.referential_constraints",
            "information_schema.check_constraints",
        ] {
            let expressions = catalog_schema()[relation]
                .iter()
                .map(|field| match (relation, field.attname.as_str()) {
                    (_, "constraint_catalog" | "table_catalog") => alias.clone(),
                    (_, "constraint_schema" | "table_schema") => schema.clone(),
                    ("information_schema.referential_constraints", "unique_constraint_catalog") => {
                        format!(
                            "CASE WHEN p.unique_constraint_catalog IS NOT NULL THEN {alias} ELSE NULL END"
                        )
                    }
                    ("information_schema.referential_constraints", "unique_constraint_schema") => {
                        format!(
                            "CASE WHEN p.unique_constraint_schema IS NOT NULL THEN {schema} ELSE NULL END"
                        )
                    }
                    (_, other) => format!("p.{}", quote_identifier(other)),
                })
                .collect::<Vec<_>>()
                .join(",");
            let table = physical_catalog(relation)?;
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {id},{alias},{schema},p._object,CASE WHEN p._target_schema IS NOT NULL THEN {schema} ELSE NULL END,p._target_object,{expressions} FROM {table} p JOIN (SELECT DISTINCT object,conname FROM __fed_local_constraints) c ON p._object=c.object AND p.constraint_name=c.conname WHERE p._snapshot_id=? AND p._schema=?"
                ),
                params![source.id, binding.config.remote_schema],
            )
            .map_err(engine_error)?;
        }

        // Project definitions (views, indexes, constraints), view dependencies and statistics.
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_definitions(snapshot_id,oid,kind,definition,items,schema_name,object_name,target_schema,target_object,dependencies_known) SELECT {id},m.local_oid,d.kind,d.definition,d.items,{schema},d.object_name,NULL,NULL,d.dependencies_known FROM _fedengine.metadata_definitions d JOIN __fed_foreign_relations m ON d.oid=m.remote_oid WHERE d.snapshot_id=? AND d.schema_name=? AND d.kind='view'"
            ),
            params![source.id, binding.config.remote_schema],
        )
        .map_err(engine_error)?;
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_definitions(snapshot_id,oid,kind,definition,items,schema_name,object_name,target_schema,target_object,dependencies_known) SELECT {id},i.local_oid,d.kind,d.definition,d.items,{schema},d.object_name,NULL,NULL,d.dependencies_known FROM _fedengine.metadata_definitions d JOIN __fed_local_indexes i ON d.oid=i.remote_oid AND d.object_name=i.object WHERE d.snapshot_id=? AND d.schema_name=? AND d.kind='index'"
            ),
            params![source.id, binding.config.remote_schema],
        )
        .map_err(engine_error)?;
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_definitions(snapshot_id,oid,kind,definition,items,schema_name,object_name,target_schema,target_object,dependencies_known) SELECT {id},c.local_oid,d.kind,d.definition,d.items,{schema},d.object_name,CASE WHEN d.target_schema IS NOT NULL THEN {schema} ELSE NULL END,d.target_object,d.dependencies_known FROM _fedengine.metadata_definitions d JOIN __fed_local_constraints c ON d.oid=c.remote_oid AND d.object_name=c.object WHERE d.snapshot_id=? AND d.schema_name=? AND d.kind='constraint'"
            ),
            params![source.id, binding.config.remote_schema],
        )
        .map_err(engine_error)?;
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_view_dependencies(snapshot_id,oid,schema_name,object_name) SELECT {id},m.local_oid,CASE WHEN d.schema_name=? THEN {schema} ELSE '' END,d.object_name FROM _fedengine.metadata_view_dependencies d JOIN __fed_foreign_relations m ON d.oid=m.remote_oid WHERE d.snapshot_id=?"
            ),
            params![binding.config.remote_schema, source.id],
        )
        .map_err(engine_error)?;

        let storage = serde_json::to_string(&RelationStorage::default())
            .map_err(|_| EngineError::new("XX000", "invalid storage statistics"))?;
        let mut activity = serde_json::to_value(RelationActivity::default())
            .map_err(|_| EngineError::new("XX000", "invalid activity statistics"))?;
        activity["scan_count"] = Value::Null;
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_relation_statistics SELECT {id},m.local_oid,{schema},m.object,COALESCE(s.storage,?),COALESCE(s.activity,?) FROM __fed_foreign_relations m LEFT JOIN _fedengine.metadata_relation_statistics s ON s.snapshot_id=? AND s.oid=m.remote_oid"
            ),
            params![storage, activity.to_string(), source.id],
        )
        .map_err(engine_error)?;
        conn.execute(
            &format!(
                "INSERT INTO _fedengine.metadata_relation_statistics SELECT {id},i.local_oid,{schema},i.object,s.storage,s.activity FROM __fed_local_indexes i JOIN _fedengine.metadata_relation_statistics s ON s.snapshot_id=? AND s.oid=i.remote_oid"
            ),
            params![source.id],
        )
        .map_err(engine_error)?;

        conn.execute_batch(
            "DROP TABLE __fed_local_attrdefs;
             DROP TABLE __fed_local_constraints;
             DROP TABLE __fed_local_indexes;
             DROP TABLE __fed_foreign_relations;",
        )
        .map_err(engine_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_mapping_preserves_logical_facts_without_source_storage_or_definitions() {
        let config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        let page: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/metadata/published-pg16-directory.json"
        ))
        .unwrap();
        let mut source = CatalogSnapshot {
            id: "source".into(),
            incarnation: "source-epoch".into(),
            revision: 1,
            generation: 1,
            database_oid: 200000000,
            schema_oids: [("sales".into(), 200000001)].into(),
            refreshed_at: "now".into(),
            complete: true,
            stale: false,
            structure_digest: "v1".into(),
            schemas: vec!["sales".into()],
            objects: serde_json::from_value(page["objects"].clone()).unwrap(),
        };
        let object = source
            .objects
            .iter_mut()
            .find(|o| o.name == "customers")
            .unwrap();
        object.metadata.dropped_attnums.push(4);
        object.metadata.comment = Some("table comment".into());
        let properties = object.metadata.columns.get_mut("name").unwrap();
        properties.comment = Some("column comment".into());
        properties.identity = Some("a".into());
        properties.generated =
            MetadataExpression::source("'generated'".into(), DataSourceKind::Postgres);
        // Exercise a source type outside the built-in PG type set.
        properties.type_oid = 210000000;
        object.metadata.column_statistics.insert(
            "name".into(),
            ColumnStatistics {
                null_frac: Some(serde_json::Number::from_f64(0.25).unwrap()),
                avg_width: Some(12),
                n_distinct: Some(5.into()),
            },
        );
        object.metadata.storage.main_bytes = Some(8192);
        object.metadata.activity.seq_scan = Some(42);
        super::super::publish(&conn, &source, "remote").unwrap();
        let mut mapped = source.clone();
        mapped.id = "mapped".into();
        mapped.objects.clear();
        mapped.schema_oids = [("crm".into(), 200000002)].into();
        mapped.schemas = vec!["crm".into()];
        let binding = VirtualSchema {
            object_epoch: "virtual-schema:200000002".into(),
            config: VirtualSchemaConfig {
                name: "crm".into(),
                source: DataSourceId("remote".into()),
                remote_schema: "sales".into(),
            },
            oid: 200000002,
            snapshot_id: None,
        };
        let epoch = binding.object_epoch.clone();
        conn.execute_batch("BEGIN TRANSACTION").unwrap();
        let mut oids = OidBatch::load(&conn, &[epoch]).unwrap();
        ForeignProjection {
            snapshot: &mapped,
            source: &source,
            alias: "account",
            binding: &binding,
            native_identity: false,
        }
        .publish(&conn, &mut oids)
        .unwrap();
        oids.flush().unwrap();
        conn.execute_batch("COMMIT").unwrap();

        let (local, kind, has_index, checks): (u32, String, bool, i16) = conn
            .query_row(
                "SELECT oid,relkind,relhasindex,relchecks FROM _fedengine.meta_pg_class WHERE _snapshot_id='mapped' AND relname='customers'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_ne!(
            local,
            source
                .objects
                .iter()
                .find(|o| o.name == "customers")
                .unwrap()
                .metadata
                .oid
        );
        assert_eq!((kind.as_str(), has_index, checks), ("r", true, 0));
        let (orders_local, orders_kind, orders_has_index, orders_checks): (u32, String, bool, i16) = conn
            .query_row(
                "SELECT oid,relkind,relhasindex,relchecks FROM _fedengine.meta_pg_class WHERE _snapshot_id='mapped' AND relname='orders'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (orders_kind.as_str(), orders_has_index, orders_checks),
            ("r", true, 1)
        );
        let view_kind: String = conn
            .query_row(
                "SELECT relkind FROM _fedengine.meta_pg_class WHERE _snapshot_id='mapped' AND relname='order_totals'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(view_kind, "v");

        let (attributes, foreign_attributes): (i64, i64) = conn
            .query_row(
                "SELECT count(*),count(*) FILTER (WHERE attrelid<>?) FROM _fedengine.meta_pg_attribute WHERE _snapshot_id='mapped' AND _object='customers'",
                [local],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 4 table attributes (including 1 dropped) + 2 customers_pkey index attributes.
        assert_eq!((attributes, foreign_attributes), (6, 2));
        let (has_default, identity, generated): (bool, String, String) = conn
            .query_row(
                "SELECT atthasdef,attidentity,attgenerated FROM _fedengine.meta_pg_attribute WHERE _snapshot_id='mapped' AND _object='customers' AND attname='name'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (has_default, identity.as_str(), generated.as_str()),
            (true, "a", "s")
        );
        let table_type: String = conn
            .query_row(
                "SELECT table_type FROM _fedengine.meta_information_schema_tables WHERE _snapshot_id='mapped' AND table_name='customers'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_type, "BASE TABLE");
        let pg_tables_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_tables WHERE _snapshot_id='mapped' AND schemaname='crm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pg_tables_count, 2);
        let foreign_table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_foreign_table WHERE _snapshot_id='mapped'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(foreign_table_count, 0);
        let attrdef_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_attrdef WHERE _snapshot_id='mapped'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // 3 pg_attrdef rows: customers.name default, orders.amount default, orders.doubled generated expression.
        assert_eq!(attrdef_count, 3);
        let (index_count, pg_indexes_count, constraint_count): (i64, i64, i64) = (
            conn.query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_index WHERE _snapshot_id='mapped'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_indexes WHERE _snapshot_id='mapped' AND schemaname='crm'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_constraint WHERE _snapshot_id='mapped'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        );
        assert_eq!((index_count, pg_indexes_count, constraint_count), (3, 3, 4));
        let (fk_relid, fk_confrelid, fk_indid): (u32, u32, u32) = conn
            .query_row(
                "SELECT conrelid,confrelid,conindid FROM _fedengine.meta_pg_constraint WHERE _snapshot_id='mapped' AND conname='orders_customer_fk'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let cust_pk_idx: u32 = conn
            .query_row(
                "SELECT oid FROM _fedengine.meta_pg_class WHERE _snapshot_id='mapped' AND relname='customers_pkey' AND relkind='i'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            (fk_relid, fk_confrelid, fk_indid),
            (orders_local, local, cust_pk_idx)
        );
        let stat_tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_stat_all_tables WHERE _snapshot_id='mapped' AND schemaname='crm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stat_tables, 2);
        let info: (
            String,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT table_catalog,table_schema,column_default,is_identity,is_generated,generation_expression FROM _fedengine.meta_information_schema_columns WHERE _snapshot_id='mapped' AND table_name='customers' AND column_name='name'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            info,
            (
                "account".into(),
                "crm".into(),
                Some("'generated'".into()),
                "YES".into(),
                "ALWAYS".into(),
                Some("'generated'".into())
            )
        );
        {
            use modorch_fedengine_contract::{MetadataScope, SessionContext, VisibleCatalog};
            use modorch_fedengine_pg_compat::{CompatibilityPlan, plan_compatibility};
            let database = VirtualDatabase {
                managed: false,
                oid: 200000010,
                revision: 1,
                public_oid: 200000011,
                schemas: vec![binding.clone()],
                config: VirtualDatabaseConfig {
                    name: "account".into(),
                    owner: config.users[0].name.clone(),
                    connect: vec![],
                    allow_connections: true,
                    schemas: vec![binding.config.clone()],
                },
            };
            let mut scope = MetadataScope {
                database: "account".into(),
                principal: config.users[0].name.clone(),
                virtual_database: Some(database.clone()),
                databases: vec![database],
                catalogs: vec![VisibleCatalog {
                    internal: false,
                    id: binding.config.source.clone(),
                    catalog: "account".into(),
                    snapshot_id: mapped.id.clone(),
                    database_oid: 200000010,
                    schemas: vec!["crm".into()],
                    default_schema: String::new(),
                    grants: vec![],
                    stale: false,
                    mapping: Some(binding.config.clone()),
                }],
            };
            let session = SessionContext {
                database: scope.database.clone(),
                ..Default::default()
            };
            let plan = |sql: &str, scope: &MetadataScope| {
                let CompatibilityPlan::Metadata(p) =
                    plan_compatibility(sql, &session, scope, 1).unwrap()
                else {
                    panic!("not metadata")
                };
                p.sql
            };
            for sql in [
                "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='crm'",
                "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='customers'",
                "SELECT schema_owner FROM information_schema.schemata WHERE schema_name='crm'",
                "SELECT pg_get_userbyid(typowner) FROM pg_type WHERE oid=210000000",
            ] {
                let owner: String = conn
                    .query_row(&plan(sql, &scope), [], |r| r.get(0))
                    .unwrap();
                assert_eq!(owner, config.users[0].name, "{sql}");
            }
            let count: i64 = conn
                .query_row(
                    &plan(
                        "SELECT count(*) FROM information_schema.tables WHERE table_name='customers'",
                        &scope,
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1); // Local owner can describe the object without acquiring remote SELECT.
            let count: i64 = conn
                .query_row(
                    &plan(
                        "SELECT count(*) FROM pg_stats WHERE tablename='customers'",
                        &scope,
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
            scope.catalogs[0].grants.push(Grant {
                principal: scope.principal.clone(),
                source: binding.config.source.clone(),
                schema: "crm".into(),
                object: "customers".into(),
                actions: ["SELECT".into()].into(),
            });
            let count: i64 = conn
                .query_row(
                    &plan(
                        "SELECT count(*) FROM pg_stats WHERE tablename='customers'",
                        &scope,
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            let condef: String = conn
                .query_row(
                    &plan(
                        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='customers_pkey'",
                        &scope,
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(condef, "PRIMARY KEY (tenant_id, id)");
        }
        let comments: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_description WHERE _snapshot_id='mapped' AND objoid=? AND description IN ('table comment','column comment')",
                [local],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(comments, 2);
        let stats: (String, i32, f32) = conn
            .query_row(
                "SELECT schemaname,avg_width,null_frac FROM _fedengine.meta_pg_stats WHERE _snapshot_id='mapped' AND tablename='customers' AND attname='name'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(stats, ("crm".into(), 12, 0.25));
        let type_namespace: u32 = conn
            .query_row(
                "SELECT typnamespace FROM _fedengine.meta_pg_type WHERE _snapshot_id='mapped' AND oid=210000000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(type_namespace, binding.oid);
        let (storage, activity): (String, String) = conn
            .query_row(
                "SELECT storage,activity FROM _fedengine.metadata_relation_statistics WHERE snapshot_id='mapped' AND oid=?",
                [local],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RelationStorage>(&storage)
                .unwrap()
                .main_bytes,
            Some(8192)
        );
        assert_eq!(
            serde_json::from_str::<Value>(&activity).unwrap()["scan_count"].as_i64(),
            Some(42)
        );
    }
}
