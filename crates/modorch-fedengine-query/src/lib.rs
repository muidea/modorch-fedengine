//! SQL coordinator: analysis → binding → policy → source fences → DuckDB execution.
use modorch::{prelude::*, *};
use modorch_fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use modorch_fedengine_datasource::DataSourceRegistry;
use modorch_fedengine_ddl::{Analyzer, Kind, build_guarded_mutation_plan, index_ownership_target};
use modorch_fedengine_policy::{Authorize, POLICY_TOPIC};
use std::{
    num::NonZeroUsize,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

pub struct QueryFactory {
    pub config: Arc<Config>,
}
struct QueryModule {
    config: Arc<Config>,
    registry: Option<ServiceHandle<DataSourceRegistry>>,
    runtime: Option<ServiceHandle<ExecutionService>>,
}
fn descriptor(max_workers: usize) -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::module("query-coordinator"));
    d.execution = ExecutionPolicy::Concurrent {
        max_in_flight: NonZeroUsize::new(max_workers * 2).unwrap(),
    };
    d.requires.extend([
        ComponentRef::initiator("duckdb-runtime"),
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::module("catalog-manager"),
        ComponentRef::module("policy-manager"),
    ]);
    d
}
impl ModuleFactory for QueryFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.config.duckdb.max_workers)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(QueryModule {
            config: self.config.clone(),
            registry: None,
            runtime: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for QueryModule {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.registry = Some(
            ctx.services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
        );
        self.runtime = Some(
            ctx.services
                .get(&ComponentRef::initiator("duckdb-runtime"), "execution")?,
        );
        Ok(())
    }
}
impl Module for QueryModule {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.config.duckdb.max_workers)
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Concurrent {
            max_in_flight: NonZeroUsize::new(self.config.duckdb.max_workers * 2).unwrap(),
            handler: Arc::new(QueryHandler {
                config: self.config.clone(),
                registry: self.registry.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                30,
                descriptor(self.config.duckdb.max_workers).component(),
                QUERY_TOPIC,
            )],
        }))
    }
}
struct QueryHandler {
    config: Arc<Config>,
    registry: ServiceHandle<DataSourceRegistry>,
    runtime: ServiceHandle<ExecutionService>,
}
fn message_error(e: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("control request failed: {e}"))
}
#[async_trait]
impl ConcurrentHandler for QueryHandler {
    async fn handle(
        &self,
        ctx: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<QueryRequest>()?;
        let result = self.execute(ctx, &request).await;
        Ok(Arc::new(result))
    }
}
impl QueryHandler {
    async fn execute(&self, ctx: &HandlerContext, r: &QueryRequest) -> Result<QueryReply> {
        if r.cancel.is_cancelled() {
            return Err(EngineError::new("57014", "query canceled before admission"));
        }
        let admission_started = std::time::Instant::now();
        let registry = self.registry.acquire().map_err(message_error)?;
        // Instance lease precedes source fences and is transferred to the actual worker.
        let instance = tokio::time::timeout(remaining(r)?, registry.instance.clone().read_owned())
            .await
            .map_err(|_| EngineError::new("55P03", "instance execution barrier timed out"))?;
        let definitions = registry.definitions();
        let hub_payload = CatalogRequest::Headers;
        let hub_options = modorch::MessageOptions {
            payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                &hub_payload,
            )),
            ..Default::default()
        };
        let headers = ctx
            .event_hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                hub_payload,
                hub_options,
            )
            .map_err(message_error)?
            .wait(remaining(r)?)
            .await
            .map_err(message_error)?;
        if admission_started.elapsed() > Duration::from_millis(200) {
            // Includes waiting for the instance barrier and committed catalog heads.
            // Never include SQL or credentials in this diagnostic.
            tracing::debug!(
                elapsed_ms = admission_started.elapsed().as_secs_f64() * 1000.,
                "query catalog admission delayed"
            );
        }
        let headers = match headers.as_ref() {
            Ok(CatalogReply::Headers(headers)) => headers.clone(),
            Err(error) => return Err(error.clone()),
            _ => return Err(EngineError::new("XX000", "invalid catalog headers reply")),
        };
        let scope = modorch_fedengine_policy::session_metadata_scope(
            &self.config,
            &r.principal,
            &r.session,
            headers,
            &definitions,
        )?;
        let database_version = scope.virtual_database.as_ref().map(|d| DatabaseVersion {
            oid: d.oid,
            revision: d.revision,
        });
        if let QueryMode::Execute {
            expected_database: Some(expected),
            ..
        } = &r.mode
            && Some(*expected) != database_version
        {
            return Err(EngineError::new(
                "0A000",
                "virtual database definition changed; Parse/Bind the statement again",
            ));
        }
        let declared = match &r.mode {
            QueryMode::Prepare { parameter_types } => parameter_types
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(i, t)| t.map(|t| (i + 1, t)))
                .collect(),
            QueryMode::Execute {
                parameter_types, ..
            } => parameter_types
                .iter()
                .copied()
                .enumerate()
                .map(|(i, t)| (i + 1, t))
                .collect(),
        };
        let business_sql = match modorch_fedengine_pg_compat::plan_with_parameters(
            &r.sql,
            &r.session,
            &scope,
            r.session_id,
            &declared,
        )? {
            modorch_fedengine_pg_compat::CompatibilityPlan::Business(sql) => sql,
            modorch_fedengine_pg_compat::CompatibilityPlan::Metadata(plan) => {
                let generations = registry.generations(scope.catalogs.iter().map(|c| c.id.clone()));
                let (prepare_only, parameters, wire_types) =
                    query_parameters(r, &r.sql, &generations, &plan.parameter_types)?;
                // Compatibility lowering may erase arguments, but Parse/Bind still
                // describe and validate the complete original client contract.
                let wire_types = wire_types
                    .into_iter()
                    .map(|t| {
                        t.ok_or_else(|| {
                            EngineError::new("42P18", "parameter type cannot be determined")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let (sql, slots) = modorch_fedengine_ddl::compact_parameters(&plan.sql)?;
                let parameter_types = slots
                    .iter()
                    .map(|i| Some(wire_types[*i]))
                    .collect::<Vec<_>>();
                let parameters = if prepare_only {
                    vec![]
                } else {
                    slots.iter().map(|i| parameters[*i].clone()).collect()
                };
                let sql = typed_sql(
                    &sql,
                    &parameter_types,
                    (!prepare_only).then_some(parameters.as_slice()),
                )?;
                let notices=scope.catalogs.iter().filter(|c|c.stale).take(8).map(|c|(c.snapshot_id.clone(),format!("catalog {} uses a stale metadata snapshot; inspect the management API catalog status",c.catalog))).collect::<Vec<_>>();
                let runtime = self.runtime.acquire().map_err(message_error)?;
                let mut reply = runtime
                    .port
                    .submit(
                        ExecuteRequest {
                            metadata: Some(MetadataExecution {
                                search_path: modorch_fedengine_pg_compat::catalog_path(
                                    &r.session, &scope,
                                ),
                                scope,
                                columns: plan.columns,
                            }),
                            principal: r.principal.clone(),
                            session_id: r.session_id,
                            sql,
                            kind: StatementKind::Query,
                            parameters,
                            parameter_types,
                            prepare_only,
                            generations,
                            deadline: r.deadline,
                            cancel: r.cancel.clone(),
                            ddl_source: None,
                            command: plan.command,
                            returning: None,
                            guarded_mutation: None,
                            guarded_ddl: None,
                        },
                        Box::new((
                            self.runtime.acquire().map_err(message_error)?,
                            registry,
                            instance,
                        )),
                    )
                    .await?;
                if let RuntimeReply::Accepted(a) = &mut reply {
                    a.notices = notices;
                }
                if let RuntimeReply::Prepared(d) = &mut reply {
                    d.parameters = wire_types;
                }
                return Ok(query_reply(reply, r, database_version));
            }
        };
        let mut analyzer = Analyzer {
            namespace: None,
            sources: definitions.clone(),
            indexes: vec![],
        };
        let mut relations = modorch_fedengine_ddl::RelationNames::default();
        let mut bindings = std::collections::BTreeMap::new();
        let mut routes = std::collections::BTreeMap::new();
        let mut authorized_routes = std::collections::BTreeSet::new();
        for catalog in scope
            .catalogs
            .iter()
            .filter(|c| c.catalog == r.session.database)
        {
            let names_started = std::time::Instant::now();
            let hub_payload = CatalogRequest::Names(catalog.snapshot_id.clone());
            let hub_options = modorch::MessageOptions {
                payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                    &hub_payload,
                )),
                ..Default::default()
            };
            let names = ctx
                .event_hub
                .request::<_, Result<CatalogReply>>(
                    CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    hub_payload,
                    hub_options,
                )
                .map_err(message_error)?
                .wait(remaining(r)?)
                .await
                .map_err(message_error)?;
            if names_started.elapsed() > Duration::from_millis(200) {
                tracing::debug!(
                    elapsed_ms = names_started.elapsed().as_secs_f64() * 1000.,
                    "query relation name resolution delayed"
                );
            }
            match names.as_ref() {
                Ok(CatalogReply::Names(names)) => {
                    for (schema, object) in names.iter() {
                        let allowed = catalog.grants.iter().any(|g| {
                            g.actions.contains("SELECT")
                                && (g.schema == "*" || &g.schema == schema)
                                && (g.object == "*" || &g.object == object)
                        });
                        let remote = catalog
                            .mapping
                            .as_ref()
                            .map(|m| m.remote_schema.clone())
                            .unwrap_or_else(|| schema.clone());
                        let key = (schema.clone(), object.clone());
                        // Resolve an existing object even without SELECT, so policy
                        // returns 42501. Prefer the first authorized contribution.
                        if (allowed && authorized_routes.insert(key.clone()))
                            || !routes.contains_key(&key)
                        {
                            routes.insert(key, (catalog.id.clone(), remote));
                        }
                    }
                    relations.extend_shared(names.clone());
                }
                Err(e) => return Err(e.clone()),
                _ => return Err(EngineError::new("XX000", "invalid catalog names reply")),
            }
            let schemas = if catalog.mapping.is_none() {
                definitions
                    .iter()
                    .find(|d| d.id == catalog.id)
                    .map(|d| d.allowed_schemas.as_slice())
                    .unwrap_or(&[])
            } else {
                &catalog.schemas
            };
            for schema in schemas {
                let remote = catalog
                    .mapping
                    .as_ref()
                    .map(|m| m.remote_schema.clone())
                    .unwrap_or_else(|| schema.clone());
                bindings.insert(schema.clone(), (catalog.id.clone(), remote));
            }
        }
        analyzer.namespace = Some(modorch_fedengine_ddl::BindingNamespace {
            catalog: r.session.database.clone(),
            allow_cross_database: self.config.pgwire.allow_cross_database,
            search_path: modorch_fedengine_pg_compat::effective_path(&r.session, &scope),
            relations,
            bindings,
            routes: scope.virtual_database.as_ref().map(|_| routes),
        });
        // Inspect the actual AST to discover index ownership for DROP/ALTER INDEX; never trust textual prefixes.
        let statements = Analyzer::split(&business_sql)?;
        if statements.len() != 1 {
            return Err(EngineError::unsupported("one SQL statement is required"));
        }
        if let Some((name, required_action)) = index_ownership_target(&business_sql)? {
            let source = analyzer.bind(&name, false)?.source;
            if !self.config.grants.iter().any(|g| {
                g.principal == r.principal
                    && g.source == source
                    && g.actions.contains(required_action)
            }) {
                return Err(EngineError::denied("index operation is not granted"));
            }
            let hub_payload = CatalogRequest::Indexes(source);
            let hub_options = modorch::MessageOptions {
                payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                    &hub_payload,
                )),
                ..Default::default()
            };
            let response = ctx
                .event_hub
                .request::<_, Result<CatalogReply>>(
                    CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    hub_payload,
                    hub_options,
                )
                .map_err(message_error)?
                .wait(remaining(r)?)
                .await
                .map_err(message_error)?;
            match response.as_ref() {
                Ok(CatalogReply::Indexes(indexes)) => analyzer.indexes = indexes.clone(),
                Err(e) => return Err(e.clone()),
                _ => return Err(EngineError::new("XX000", "invalid catalog reply")),
            }
        }
        let analysis = analyzer.analyze(&business_sql)?;
        if scope.virtual_database.is_some() && analysis.kind != Kind::Query {
            return Err(EngineError::unsupported(
                "virtual database bindings support read-only queries",
            ));
        }
        let hub_payload = Authorize {
            principal: r.principal.clone(),
            analysis: analysis.clone(),
        };
        let hub_options = modorch::MessageOptions {
            payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                &hub_payload,
            )),
            ..Default::default()
        };
        let permission = ctx
            .event_hub
            .request::<_, Result<()>>(
                POLICY_TOPIC,
                Destination::Component(ComponentRef::module("policy-manager")),
                hub_payload,
                hub_options,
            )
            .map_err(message_error)?
            .wait(remaining(r)?)
            .await
            .map_err(message_error)?;
        permission.as_ref().clone()?;
        let analyzed_versions = registry.generations(analysis.sources.iter().cloned());
        let ddl = analysis.kind == Kind::Ddl;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        // BTreeSet provides a fixed source lock order for joins and DDL.
        for id in &analysis.sources {
            let source = registry.get(id)?;
            if source.stale.load(Ordering::SeqCst) {
                return Err(EngineError::new(
                    "55000",
                    "source metadata is stale; refresh it online",
                ));
            }
            let duration =
                Duration::from_millis(self.config.ddl.source_lock_timeout_ms).min(remaining(r)?);
            if ddl {
                writes.push(
                    tokio::time::timeout(duration, source.fence.clone().write_owned())
                        .await
                        .map_err(|_| EngineError::new("55P03", "source DDL fence timed out"))?,
                );
            } else {
                reads.push(
                    tokio::time::timeout(duration, source.fence.clone().read_owned())
                        .await
                        .map_err(|_| EngineError::new("55P03", "source fence timed out"))?,
                );
            }
        }
        registry.check(&analyzed_versions)?;
        let mut verified_index_owner: Option<String> = None;
        if let Some(index) = &analysis.index
            && !index.create
        {
            let runtime = self.runtime.acquire().map_err(message_error)?;
            let current = runtime
                .port
                .indexes(
                    index.table.source.clone(),
                    Box::new(self.runtime.acquire().map_err(message_error)?),
                )
                .await?;
            let owners: Vec<_> = current
                .iter()
                .filter(|i| i.schema == index.table.schema && i.index == index.name)
                .collect();
            if index.if_exists && owners.is_empty() {
                verified_index_owner = None;
            } else if owners.len() != 1
                || (!index.table.name.is_empty() && owners[0].table != index.table.name)
                || owners[0].protected
            {
                return Err(EngineError::denied(
                    "index ownership changed while awaiting the DDL fence",
                ));
            } else {
                verified_index_owner = Some(owners[0].table.clone());
            }
        } else if let Some(intent) = &analysis.ddl_intent
            && matches!(intent.action.as_str(), "rename_index" | "reindex")
            && let Some(primary) = &analysis.primary
        {
            let runtime = self.runtime.acquire().map_err(message_error)?;
            let current = runtime
                .port
                .indexes(
                    primary.source.clone(),
                    Box::new(self.runtime.acquire().map_err(message_error)?),
                )
                .await?;
            let owners: Vec<_> = current
                .iter()
                .filter(|i| i.schema == primary.schema && i.index == intent.table_name)
                .collect();
            if owners.len() != 1 || owners[0].table != primary.name || owners[0].protected {
                return Err(EngineError::denied(
                    "index ownership changed while awaiting the DDL fence",
                ));
            }
            verified_index_owner = Some(owners[0].table.clone());
        }
        let generations = registry.generations(analysis.sources.iter().cloned());
        let (prepare, parameters, wire_types) =
            query_parameters(r, &r.sql, &generations, &Default::default())?;
        let wire_types = wire_types
            .into_iter()
            .map(|t| {
                t.ok_or_else(|| EngineError::new("42P18", "parameter type cannot be determined"))
            })
            .collect::<Result<Vec<_>>>()?;
        let returning = analysis.returning.clone();
        let guarded_ddl = if analysis.kind == Kind::Ddl
            && let Some(intent) = analysis.ddl_intent
            && let Some(primary) = &analysis.primary
            && let Some(source) = definitions.iter().find(|s| s.id == primary.source)
        {
            let owner = if matches!(
                intent.action.as_str(),
                "drop_index" | "rename_index" | "reindex"
            ) && matches!(
                source.kind,
                DataSourceKind::Postgres
                    | DataSourceKind::Greenplum
                    | DataSourceKind::Kingbasees
                    | DataSourceKind::Mysql
                    | DataSourceKind::Mariadb
                    | DataSourceKind::Sqlserver
            ) {
                verified_index_owner.unwrap_or_else(|| intent.owner.clone())
            } else {
                String::new()
            };
            let schema = primary.schema.clone();
            intent.into_guarded_plan(source, schema.clone(), schema, owner)?
        } else {
            None
        };
        let sql = if guarded_ddl.is_none()
            && let Some(index) = &analysis.index
        {
            let source = definitions
                .iter()
                .find(|s| s.id == index.table.source)
                .unwrap();
            index.duckdb_adapter_sql(source.kind)?
        } else if analysis.kind == Kind::Query {
            modorch_fedengine_ddl::rewrite_pushdown(&analysis.sql, &analysis.sources, &definitions)?
        } else {
            analysis.sql
        };
        let (sql, slots) = modorch_fedengine_ddl::compact_parameters(&sql)?;
        let concrete_param_types: Vec<modorch_fedengine_contract::PgType> =
            slots.iter().map(|i| wire_types[*i]).collect();
        let parameter_types = concrete_param_types
            .iter()
            .copied()
            .map(Some)
            .collect::<Vec<_>>();
        let parameters = if prepare {
            vec![]
        } else {
            slots.iter().map(|i| parameters[*i].clone()).collect()
        };
        let guarded_mutation =
            if matches!(analysis.kind, Kind::Insert | Kind::Update | Kind::Delete)
                && let Some(primary) = &analysis.primary
                && let Some(source) = definitions.iter().find(|s| s.id == primary.source)
            {
                build_guarded_mutation_plan(
                    &sql,
                    source,
                    primary,
                    returning.as_ref(),
                    &concrete_param_types,
                )?
            } else {
                None
            };
        let sql = typed_sql(
            &sql,
            &parameter_types,
            (!prepare).then_some(parameters.as_slice()),
        )?;
        let kind = match analysis.kind {
            Kind::Query => StatementKind::Query,
            Kind::Insert => StatementKind::Insert,
            Kind::Update => StatementKind::Update,
            Kind::Delete => StatementKind::Delete,
            Kind::Ddl => StatementKind::Ddl,
        };
        let runtime = self.runtime.acquire().map_err(message_error)?;
        let ownership = Box::new((
            self.runtime.acquire().map_err(message_error)?,
            registry,
            instance,
            reads,
            writes,
        ));
        let reply = runtime
            .port
            .submit(
                ExecuteRequest {
                    metadata: None,
                    principal: r.principal.clone(),
                    session_id: r.session_id,
                    sql,
                    kind,
                    parameters,
                    parameter_types,
                    prepare_only: prepare,
                    generations,
                    deadline: r.deadline,
                    cancel: r.cancel.clone(),
                    ddl_source: if ddl {
                        analysis.sources.iter().next().cloned()
                    } else {
                        None
                    },
                    command: analysis.command,
                    returning,
                    guarded_mutation,
                    guarded_ddl,
                },
                ownership,
            )
            .await?;
        Ok(match reply {
            RuntimeReply::Prepared(mut descriptor) => {
                descriptor.parameters = wire_types;
                descriptor.sql = r.sql.clone();
                descriptor.namespace_version = r.session.namespace_version;
                descriptor.database_version = database_version;
                QueryReply::Prepared(descriptor)
            }
            RuntimeReply::Accepted(a) => QueryReply::Accepted(a),
        })
    }
}
fn remaining(r: &QueryRequest) -> Result<Duration> {
    let d = r
        .deadline
        .saturating_duration_since(std::time::Instant::now());
    if d.is_zero() {
        Err(EngineError::new("57014", "query deadline exceeded"))
    } else {
        Ok(d)
    }
}
fn hint_type(t: &str) -> Option<PgType> {
    let normalized = t.trim().to_ascii_uppercase();
    let normalized = normalized
        .strip_prefix("PG_CATALOG.")
        .unwrap_or(&normalized);
    if let Some(elem) = normalized.strip_suffix("[]") {
        let oid = match elem.trim() {
            "BOOLEAN" | "BOOL" => 1000,
            "BYTEA" | "BLOB" => 1001,
            "SMALLINT" | "INT2" => 1005,
            "INT" | "INTEGER" | "INT4" => 1007,
            "BIGINT" | "INT8" => 1016,
            "POINT" => 1017,
            "REAL" | "FLOAT4" => 1021,
            "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => 1022,
            "TEXT" | "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "BPCHAR" | "NAME" => 1009,
            "DATE" => 1182,
            "TIME" => 1183,
            "TIMESTAMP" => 1115,
            "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => 1185,
            "INTERVAL" => 1187,
            "UUID" => 2951,
            "JSON" => 199,
            v if v.starts_with("DECIMAL") || v.starts_with("NUMERIC") => 1231,
            _ => return None,
        };
        return Some(PgType::Catalog(oid));
    }
    let base = normalized.split('(').next().unwrap().trim();
    Some(match base {
        "INT" | "INTEGER" | "INT4" => PgType::Int4,
        "BIGINT" | "INT8" => PgType::Int8,
        "SMALLINT" | "INT2" => PgType::Int2,
        "BOOLEAN" | "BOOL" => PgType::Bool,
        "REAL" | "FLOAT" | "FLOAT4" => PgType::Float4,
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => PgType::Float8,
        "TEXT" | "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "CHARACTER" | "BPCHAR" => PgType::Text,
        "BLOB" | "BYTEA" => PgType::Bytea,
        "DATE" => PgType::Date,
        "TIME" | "TIME WITHOUT TIME ZONE" => PgType::Time,
        "TIMESTAMP" | "TIMESTAMP WITHOUT TIME ZONE" => PgType::Timestamp,
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => PgType::Timestamptz,
        "INTERVAL" => PgType::Interval,
        "POINT" => PgType::Point,
        "UUID" => PgType::Uuid,
        "JSON" => PgType::Json,
        "OID" => PgType::Catalog(26),
        "REGCLASS" => PgType::Catalog(2205),
        "NAME" => PgType::Catalog(19),
        "DECIMAL" | "NUMERIC" => PgType::Numeric,
        _ => return None,
    })
}

fn query_reply(
    reply: RuntimeReply,
    request: &QueryRequest,
    database_version: Option<DatabaseVersion>,
) -> QueryReply {
    match reply {
        RuntimeReply::Prepared(mut descriptor) => {
            descriptor.sql = request.sql.clone();
            descriptor.namespace_version = request.session.namespace_version;
            descriptor.database_version = database_version;
            QueryReply::Prepared(descriptor)
        }
        RuntimeReply::Accepted(a) => QueryReply::Accepted(a),
    }
}
fn query_parameters(
    r: &QueryRequest,
    sql: &str,
    generations: &std::collections::BTreeMap<DataSourceId, SourceVersion>,
    signatures: &std::collections::BTreeMap<usize, PgType>,
) -> Result<(bool, Vec<Cell>, Vec<Option<PgType>>)> {
    match &r.mode {
        QueryMode::Prepare { parameter_types } => {
            let mut hints = modorch_fedengine_ddl::parameter_hints(sql)?;
            if parameter_types.len() > 1024 {
                return Err(EngineError::new("54000", "parameter limit exceeded"));
            }
            hints.resize(hints.len().max(parameter_types.len()), None);
            Ok((
                true,
                vec![],
                hints
                    .into_iter()
                    .enumerate()
                    .map(|(i, h)| {
                        parameter_types
                            .get(i)
                            .copied()
                            .flatten()
                            .or_else(|| signatures.get(&(i + 1)).copied())
                            .or_else(|| h.and_then(|t| hint_type(&t)))
                    })
                    .collect(),
            ))
        }
        QueryMode::Execute {
            parameters,
            parameter_types,
            expected,
            ..
        } => {
            if parameters.len() != parameter_types.len()
                || parameters.len() < modorch_fedengine_ddl::parameter_hints(sql)?.len()
            {
                return Err(EngineError::new("08P01", "Bind parameter count mismatch"));
            }
            for (value, typ) in parameters.iter().zip(parameter_types) {
                if let Cell::Array { lower_bound, .. } = value
                    && *lower_bound != 1
                    && !matches!(typ, PgType::Catalog(22 | 30))
                {
                    return Err(EngineError::unsupported(
                        "execution of arrays with non-unit lower bounds is not implemented",
                    ));
                }
            }
            if !expected.is_empty() && expected != generations {
                return Err(EngineError::new(
                    "0A000",
                    "catalog generation changed; Parse/Bind the statement again",
                ));
            }
            Ok((
                false,
                parameters.clone(),
                parameter_types.iter().copied().map(Some).collect(),
            ))
        }
    }
}
fn numeric_duckdb_cast(value: Option<&Cell>) -> Result<String> {
    let Some(value) = value else {
        return Ok("DECIMAL(38,10)".into());
    };
    match value {
        Cell::Numeric(s) | Cell::Text(s) => {
            let (p, scale) = modorch_fedengine_pg_compat::numeric_precision_scale(s)?;
            Ok(format!("DECIMAL({p},{scale})"))
        }
        Cell::Null => Ok("DECIMAL(38,0)".into()),
        _ => Err(EngineError::new("22P02", "invalid numeric parameter value")),
    }
}
fn typed_sql(sql: &str, types: &[Option<PgType>], parameters: Option<&[Cell]>) -> Result<String> {
    if types.is_empty() {
        return Ok(sql.into());
    }
    let hints = modorch_fedengine_ddl::parameter_hints(sql)?;
    let types = types
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if let Some(Some(hint)) = hints.get(i) {
                let normalized = hint.trim().to_ascii_uppercase();
                let bare_numeric = matches!(
                    normalized
                        .strip_prefix("PG_CATALOG.")
                        .unwrap_or(&normalized),
                    "NUMERIC" | "DECIMAL"
                );
                if bare_numeric && *p == Some(PgType::Numeric) {
                    return numeric_duckdb_cast(parameters.and_then(|vals| vals.get(i)));
                }
            }
            if *p == Some(PgType::Numeric)
                && hints
                    .get(i)
                    .is_none_or(|h| h.as_ref().is_none_or(|s| !s.contains('(')))
            {
                return numeric_duckdb_cast(parameters.and_then(|vals| vals.get(i)));
            }
            p.map(|t| t.duckdb_name().into()).ok_or_else(|| {
                EngineError::new(
                    "42P18",
                    "parameter type requires an OID or explicit SQL cast",
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    modorch_fedengine_ddl::cast_parameters(sql, &types)
}
