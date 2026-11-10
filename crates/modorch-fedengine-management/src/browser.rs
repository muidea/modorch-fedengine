//! Virtual database browser. All directory SQL uses the same PG projection and policy as clients.
use crate::browser_query::{self, QueryResult};
use axum::{
    body::Bytes,
    http::{Method, Uri},
};
use modorch::{prelude::*, *};
use modorch_fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use modorch_fedengine_datasource::DataSourceRegistry;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const RETENTION_SECONDS: i64 = 300;
const MAX_JOBS: usize = 16;
const MAX_RUNNING: usize = 4;
#[derive(Clone)]
pub(crate) struct Browser {
    pub config: Arc<Config>,
    pub hub: ExternalEventHub,
    pub registry: ServiceHandle<DataSourceRegistry>,
    pub tasks: TaskScheduler,
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
}
struct Context {
    database: VirtualDatabase,
    scope: MetadataScope,
    stamp: String,
}
struct Job {
    digest: Vec<u8>,
    expires: i64,
    cancel: CancellationToken,
    view: Value,
    running: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    database: String,
    oid: u32,
    revision: u64,
    #[serde(default)]
    stamp: Option<String>,
    #[serde(default)]
    schema: String,
    #[serde(default)]
    object: String,
    #[serde(default)]
    search: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    system: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    database: String,
    oid: u32,
    revision: u64,
    sql: String,
    issued_at: i64,
}
impl Browser {
    pub fn new(
        config: Arc<Config>,
        hub: ExternalEventHub,
        registry: ServiceHandle<DataSourceRegistry>,
        tasks: TaskScheduler,
    ) -> Self {
        Self {
            config,
            hub,
            registry,
            tasks,
            jobs: Default::default(),
        }
    }
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config.pgwire.query_timeout_ms.min(60_000))
    }
    async fn context(
        &self,
        name: &str,
        oid: u32,
        revision: u64,
        stamp: Option<&str>,
    ) -> Result<Context> {
        let hub_payload = CatalogRequest::Headers;
        let hub_options = modorch::MessageOptions {
            payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                &hub_payload,
            )),
            ..Default::default()
        };
        let reply = self
            .hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                hub_payload,
                hub_options,
            )
            .map_err(control_error)?
            .wait(Duration::from_millis(
                self.config.management.request_timeout_ms,
            ))
            .await
            .map_err(control_error)?;
        let heads = match reply.as_ref() {
            Ok(CatalogReply::Headers(h)) => h.clone(),
            Err(e) => return Err(e.clone()),
            _ => return Err(EngineError::new("XX000", "invalid catalog reply")),
        };
        let database = heads
            .databases
            .iter()
            .find(|d| d.config.name == name && name != "_fedengine")
            .cloned()
            .ok_or_else(|| EngineError::new("3D000", "virtual database does not exist"))?;
        if database.oid != oid || database.revision != revision {
            return Err(changed());
        }
        let definitions = self
            .registry
            .acquire()
            .map_err(control_error)?
            .definitions();
        // This is a new management admission; never inherit a PG session's CONNECT exception.
        let scope = modorch_fedengine_policy::metadata_scope(
            &self.config,
            &database.config.owner,
            name,
            heads,
            &definitions,
        )?;
        let visible = scope.catalogs.iter().filter(|c| c.catalog == name).map(|c| json!({"id":c.id,"snapshot":c.snapshot_id,"stale":c.stale,"mapping":c.mapping,"grants":c.grants})).collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&json!({"database":database,"visible":visible}))
            .map_err(|_| EngineError::new("XX000", "catalog encoding failed"))?;
        let current = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if stamp.is_some_and(|s| s != current) {
            return Err(changed());
        }
        Ok(Context {
            database,
            scope,
            stamp: current,
        })
    }
    pub async fn route(&self, method: &Method, uri: &Uri, body: &Bytes) -> Result<Value> {
        let route: Vec<_> = uri
            .path()
            .trim_end_matches('/')
            .split('/')
            .skip(4)
            .collect();
        match route.as_slice() {
            ["queries", id] if *method == Method::POST => {
                let input: Start = serde_json::from_slice(body)
                    .map_err(|_| EngineError::config("invalid query request"))?;
                self.start(id, input).await
            }
            ["queries", id] if *method == Method::GET => self.job(id, false),
            ["queries", id, "cancel"] if *method == Method::POST => self.job(id, true),
            [kind @ ("schemas" | "objects" | "object")] if *method == Method::GET => {
                let p = axum::extract::Query::<Parameters>::try_from_uri(uri)
                    .map_err(|_| EngineError::config("invalid browser parameters"))?
                    .0;
                if p.offset > 1_000_000
                    || p.search.len() > 256
                    || p.schema.len() > 256
                    || p.object.len() > 256
                    || (p.offset > 0 && p.stamp.is_none())
                {
                    return Err(EngineError::config(
                        "invalid pagination or missing snapshot stamp",
                    ));
                }
                let ctx = self
                    .context(&p.database, p.oid, p.revision, p.stamp.as_deref())
                    .await?;
                let value = self.directory(kind, &p, &ctx).await?;
                self.context(&p.database, p.oid, p.revision, Some(&ctx.stamp))
                    .await?;
                Ok(value)
            }
            _ => Err(EngineError::new("42704", "browser route does not exist")),
        }
    }
    async fn sql(&self, ctx: &Context, sql: String, limit: usize) -> Result<QueryResult> {
        browser_query::execute(
            &self.hub,
            &ctx.database,
            sql,
            self.timeout(),
            CancellationToken::new(),
            limit,
        )
        .await
    }
    async fn directory(&self, kind: &str, p: &Parameters, ctx: &Context) -> Result<Value> {
        let filter = if p.system {
            "n.nspname <> '_fedengine'"
        } else {
            "n.nspname NOT IN ('pg_catalog','information_schema','_fedengine') AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp_%'"
        };
        if kind == "schemas" {
            let sql = format!(
                "SELECT n.nspname FROM pg_catalog.pg_namespace n WHERE {filter} AND lower(n.nspname) LIKE lower({}) ESCAPE '!' ORDER BY n.nspname LIMIT 101 OFFSET {}",
                pattern(&p.search),
                p.offset
            );
            let mut rows = self.sql(ctx, sql, 101).await?;
            let more = rows.rows.len() > 100;
            rows.rows.truncate(100);
            let schemas = rows.rows.iter().filter_map(|r| r.first()?.as_ref()).map(|name| {
                let source = ctx.scope.catalogs.iter().find(|c| c.catalog == p.database && c.schemas.contains(name));
                json!({"name":name,"stale":source.is_some_and(|c| c.stale),"source":source.map(|c| &c.id),"contributors":ctx.scope.catalogs.iter().filter(|c| c.catalog == p.database && c.schemas.contains(name)).map(|c| &c.id).collect::<Vec<_>>()})
            }).collect::<Vec<_>>();
            return Ok(
                json!({"stamp":ctx.stamp,"database":ctx.database.config.name,"principal":ctx.database.config.owner,"schemas":schemas,"offset":p.offset,"has_more":more}),
            );
        }
        if !p.system && is_system(&p.schema) {
            return Err(EngineError::new("42704", "schema is hidden"));
        }
        if p.schema == "_fedengine" {
            return Err(EngineError::denied("internal schema is not public"));
        }
        if kind == "objects"
            && let Some(source) = ctx
                .scope
                .catalogs
                .iter()
                .find(|c| c.catalog == p.database && c.schemas.contains(&p.schema))
        {
            return self.mapped_objects(p, ctx, source).await;
        }
        let kinds = match p.kind.as_str() {
            "" => "('r','p','f','v','m')",
            "table" => "('r','p','f')",
            "view" => "('v','m')",
            _ => return Err(EngineError::config("unknown object type")),
        };
        let predicate = format!(
            "{filter} AND n.nspname={} AND c.relkind IN {kinds} AND lower(c.relname) LIKE lower({}) ESCAPE '!'",
            quote_literal(&p.schema),
            pattern(&p.search)
        );
        let fields = "SELECT c.oid, c.relname, c.relkind, c.relnatts FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace";
        if kind == "objects" {
            let mut result = self
                .sql(
                    ctx,
                    format!(
                        "{fields} WHERE {predicate} ORDER BY c.relname,c.oid LIMIT 51 OFFSET {}",
                        p.offset
                    ),
                    51,
                )
                .await?;
            if result.truncated {
                return Err(EngineError::new(
                    "54000",
                    "directory page exceeds response budget",
                ));
            }
            let more = result.rows.len() > 50;
            result.rows.truncate(50);
            return Ok(
                json!({"stamp":ctx.stamp,"objects": result.rows.iter().map(|r| json!({"oid":r[0],"name":r[1],"kind":r[2],"columns":r[3]})).collect::<Vec<_>>(),"offset":p.offset,"has_more":more}),
            );
        }
        let result = self
            .sql(
                ctx,
                format!(
                    "{fields} WHERE {predicate} AND c.relname={} LIMIT 1",
                    quote_literal(&p.object)
                ),
                1,
            )
            .await?;
        let row = result
            .rows
            .first()
            .ok_or_else(|| EngineError::new("42704", "object is not visible in this snapshot"))?;
        let summaries = self.summaries(p, ctx, true).await?;
        let summary = summaries.iter().find(|s| s.name == p.object);
        if let Some(source) = ctx.scope.catalogs.iter().find(|c| {
            c.catalog == p.database
                && c.schemas.contains(&p.schema)
                && c.grants.iter().any(|g| {
                    g.actions.contains("SELECT")
                        && (g.schema == "*" || g.schema == p.schema)
                        && (g.object == "*" || g.object == p.object)
                })
                && summary.is_some_and(|s| {
                    source_snapshot(ctx, c).is_ok_and(|id| s.contributors.contains(&id))
                })
        }) {
            let mapping = source
                .mapping
                .as_ref()
                .ok_or_else(|| EngineError::new("XX000", "missing schema mapping"))?;
            let snapshot = source_snapshot(ctx, source)?;
            let request = DirectoryRequest {
                id: source.id.clone(),
                schema: Some(mapping.remote_schema.clone()),
                object: Some(p.object.clone()),
                object_type: None,
                snapshot_id: Some(snapshot.clone()),
                offset: 0,
                limit: 2,
                object_bytes_limit: Some(8 * 1024 * 1024),
            };
            let hub_payload = CatalogRequest::Directory(request);
            let hub_options = modorch::MessageOptions {
                payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                    &hub_payload,
                )),
                ..Default::default()
            };
            let reply = self
                .hub
                .request::<_, Result<CatalogReply>>(
                    CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    hub_payload,
                    hub_options,
                )
                .map_err(control_error)?
                .wait(Duration::from_millis(
                    self.config.management.request_timeout_ms,
                ))
                .await
                .map_err(control_error)?;
            let page = match reply.as_ref() {
                Ok(CatalogReply::Directory(p)) => p,
                Err(e) => return Err(e.clone()),
                _ => return Err(EngineError::new("XX000", "invalid directory reply")),
            };
            let object = page
                .objects
                .iter()
                .find(|o| o.name == p.object)
                .ok_or_else(changed)?;
            let value = json!({"stamp":ctx.stamp,"oid":row[0],"schema":p.schema,"name":p.object,"kind":row[2],"external":true,"metadata_conflict":summary.is_some_and(|s| s.conflict),"contributors":ctx.scope.catalogs.iter().filter(|c| c.catalog == p.database && c.schemas.contains(&p.schema) && summary.is_some_and(|s| source_snapshot(ctx,c).is_ok_and(|id| s.contributors.contains(&id)))).map(|c| json!({"id":c.id,"snapshot_id":source_snapshot(ctx,c).ok()})).collect::<Vec<_>>(),"source":{"id":source.id,"schema":mapping.remote_schema,"snapshot_id":snapshot,"mapping_snapshot_id":source.snapshot_id,"refreshed_at":page.refreshed_at,"stale":page.stale || source.stale},"object":object});
            if serde_json::to_vec(&value)
                .map_err(|_| EngineError::new("XX000", "metadata encoding failed"))?
                .len()
                > 8 * 1024 * 1024
            {
                return Err(EngineError::new(
                    "54000",
                    "object metadata exceeds 8 MiB browser budget; use the catalog API",
                ));
            }
            return Ok(value);
        }
        let oid = row[0]
            .as_ref()
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| EngineError::new("XX000", "invalid object OID"))?;
        let columns = self.sql(ctx, format!("SELECT a.attname,a.atttypid,a.attnotnull FROM pg_catalog.pg_attribute a WHERE a.attrelid={oid} AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum"), 1000).await?;
        Ok(
            json!({"stamp":ctx.stamp,"oid":row[0],"schema":p.schema,"name":p.object,"kind":row[2],"external":row[2].as_deref()==Some("f"),"columns":columns}),
        )
    }
    async fn summaries(
        &self,
        p: &Parameters,
        ctx: &Context,
        exact: bool,
    ) -> Result<Vec<DirectorySummary>> {
        let mut scopes = Vec::new();
        for source in ctx.scope.catalogs.iter().filter(|c| {
            c.catalog == p.database && c.schemas.contains(&p.schema) && c.mapping.is_some()
        }) {
            let mapping = source.mapping.as_ref().unwrap();
            let grants = source
                .grants
                .iter()
                .filter(|g| g.schema == "*" || g.schema == p.schema)
                .collect::<Vec<_>>();
            let owner = ctx
                .scope
                .virtual_database
                .as_ref()
                .is_some_and(|db| db.config.owner == ctx.scope.principal);
            let names = if owner {
                if exact {
                    Some(vec![p.object.clone()])
                } else {
                    None
                }
            } else if exact {
                Some(
                    if grants
                        .iter()
                        .any(|g| g.object == "*" || g.object == p.object)
                    {
                        vec![p.object.clone()]
                    } else {
                        vec![]
                    },
                )
            } else if grants.iter().any(|g| g.object == "*") {
                None
            } else {
                Some(grants.iter().map(|g| g.object.clone()).collect())
            };
            scopes.push(DirectorySummaryScope {
                snapshot_id: source_snapshot(ctx, source)?,
                schema: mapping.remote_schema.clone(),
                names,
            });
        }
        let request = DirectorySummaryRequest {
            scopes,
            search: if exact {
                String::new()
            } else {
                p.search.clone()
            },
            kind: if exact { String::new() } else { p.kind.clone() },
            offset: if exact { 0 } else { p.offset },
            limit: if exact { 1 } else { 51 },
        };
        let hub_payload = CatalogRequest::Summaries(request);
        let hub_options = modorch::MessageOptions {
            payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
                &hub_payload,
            )),
            ..Default::default()
        };
        let reply = self
            .hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                hub_payload,
                hub_options,
            )
            .map_err(control_error)?
            .wait(Duration::from_millis(
                self.config.management.request_timeout_ms,
            ))
            .await
            .map_err(control_error)?;
        match reply.as_ref() {
            Ok(CatalogReply::Summaries(s)) => Ok(s.clone()),
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid summaries reply")),
        }
    }
    async fn mapped_objects(
        &self,
        p: &Parameters,
        ctx: &Context,
        _source: &VisibleCatalog,
    ) -> Result<Value> {
        if !["", "table", "view"].contains(&p.kind.as_str()) {
            return Err(EngineError::config("unknown object type"));
        }
        let mut summaries = self.summaries(p, ctx, false).await?;
        let more = summaries.len() > 50;
        summaries.truncate(50);
        let objects = if summaries.is_empty() {
            vec![]
        } else {
            let names = summaries
                .iter()
                .map(|s| quote_literal(&s.name))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT c.oid,c.relname,c.relnatts FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={} AND c.relname IN ({names}) ORDER BY c.relname",
                quote_literal(&p.schema)
            );
            let rows = self.sql(ctx, sql, 50).await?;
            if rows.truncated || rows.rows.len() != summaries.len() {
                return Err(changed());
            }
            rows.rows.iter().map(|r| {
                let source_type=&summaries.iter().find(|s|Some(&s.name)==r[1].as_ref()).unwrap().object_type;
                let summary = summaries.iter().find(|s|Some(&s.name)==r[1].as_ref()).unwrap();
                json!({"oid":r[0],"name":r[1],"kind":"f","source_type":source_type,"columns":r[2],"metadata_conflict":summary.conflict,"contributors":summary.contributors.len()})
            }).collect::<Vec<_>>()
        };
        Ok(json!({"stamp":ctx.stamp,"objects":objects,"offset":p.offset,"has_more":more}))
    }
    async fn start(&self, id: &str, input: Start) -> Result<Value> {
        uuid::Uuid::parse_str(id).map_err(|_| EngineError::config("query id must be UUID"))?;
        let now = chrono::Utc::now().timestamp();
        if input.issued_at < now - RETENTION_SECONDS || input.issued_at > now + 30 {
            return Err(EngineError::config(
                "query submission expired; create a new explicit submission",
            ));
        }
        browser_query::read_only(&input.sql)?;
        let digest = Sha256::digest(serde_json::to_vec(&json!({"database":input.database,"oid":input.oid,"revision":input.revision,"sql":input.sql,"issued_at":input.issued_at})).unwrap()).to_vec();
        {
            let mut jobs = self.jobs.lock().unwrap();
            jobs.retain(|_, j| j.running || j.expires >= now);
            if let Some(job) = jobs.get(id) {
                return if job.digest == digest {
                    Ok(job.view.clone())
                } else {
                    Err(EngineError::new(
                        "40001",
                        "query id was used with different parameters",
                    ))
                };
            }
        }
        let ctx = self
            .context(&input.database, input.oid, input.revision, None)
            .await?;
        let cancel = CancellationToken::new();
        let view = json!({"id":id,"status":"running","principal":ctx.database.config.owner,"database":input.database,"row_limit":browser_query::ROW_LIMIT,"byte_limit":browser_query::BYTE_LIMIT});
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.get(id) {
                return if job.digest == digest {
                    Ok(job.view.clone())
                } else {
                    Err(EngineError::new("40001", "query id conflict"))
                };
            }
            if jobs.len() >= MAX_JOBS || jobs.values().filter(|j| j.running).count() >= MAX_RUNNING
            {
                return Err(EngineError::new(
                    "53300",
                    "SQL debugger capacity reached; wait for running queries or result expiry",
                ));
            }
            jobs.insert(
                id.into(),
                Job {
                    digest,
                    expires: input.issued_at + RETENTION_SECONDS + 30,
                    cancel: cancel.clone(),
                    view: view.clone(),
                    running: true,
                },
            );
        }
        let browser = self.clone();
        let key = id.to_string();
        let submitted = self.tasks.spawn(move |shutdown| async move {
            let started = Instant::now();
            let execution = browser_query::execute(&browser.hub, &ctx.database, input.sql, browser.timeout(), cancel.clone(), browser_query::ROW_LIMIT);
            tokio::pin!(execution);
            let result = tokio::select! { r=&mut execution => r, _=shutdown.cancelled()=> {cancel.cancel(); execution.await} };
            let mut jobs=browser.jobs.lock().unwrap();
            if let Some(job)=jobs.get_mut(&key) {
                job.running=false;
                match result {
                    Ok(result) => { job.view["status"]=json!("succeeded"); job.view["result"]=json!(result); },
                    Err(e) => { job.view["status"]=json!(if e.sqlstate=="57014" {"canceled"} else {"failed"}); job.view["error"]=json!({"code":e.sqlstate,"message":e.message}); }
                }
                job.view["elapsed_ms"]=json!(started.elapsed().as_millis() as u64);
            }
            Ok(())
        });
        if submitted.is_err() {
            self.jobs.lock().unwrap().remove(id);
            return Err(EngineError::new(
                "53300",
                "query task admission unavailable",
            ));
        }
        Ok(view)
    }
    fn job(&self, id: &str, cancel: bool) -> Result<Value> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|_, j| j.running || j.expires >= chrono::Utc::now().timestamp());
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| EngineError::new("42704", "query result missing or expired"))?;
        if cancel && job.running {
            job.cancel.cancel();
            job.view["status"] = json!("canceling");
        }
        Ok(job.view.clone())
    }
}
fn changed() -> EngineError {
    EngineError::new(
        "40001",
        "catalog snapshot or database definition changed; reload the workspace",
    )
}
fn control_error(_: FrameworkError) -> EngineError {
    EngineError::new("53300", "catalog control unavailable")
}
fn is_system(s: &str) -> bool {
    s == "pg_catalog"
        || s == "information_schema"
        || s.starts_with("pg_toast")
        || s.starts_with("pg_temp_")
}

fn pattern(s: &str) -> String {
    quote_literal(&format!(
        "%{}%",
        s.replace('!', "!!").replace('%', "!%").replace('_', "!_")
    ))
}

fn source_snapshot(ctx: &Context, mapping: &VisibleCatalog) -> Result<String> {
    ctx.scope
        .catalogs
        .iter()
        .find(|c| c.id == mapping.id && c.mapping.is_none())
        .map(|c| c.snapshot_id.clone())
        .ok_or_else(changed)
}
