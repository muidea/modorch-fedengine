//! Catalog control requests; remote indexes are discovered through fixed DuckDB queries.
use modorch::{prelude::*, *};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use modorch_fedengine_datasource::DataSourceRegistry;
use modorch_fedengine_ddl::{IndexOwner, ObjectRef};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
pub const CATALOG_TOPIC: &str = "catalog/control/v1";
#[derive(Clone)]
pub enum CatalogRequest {
    Snapshot,
    Indexes(DataSourceId),
    Headers,
    Names(String),
    Directory(DirectoryRequest),
    Summaries(DirectorySummaryRequest),
}
pub enum CatalogReply {
    Snapshot(BTreeMap<DataSourceId, SourceVersion>),
    Indexes(Vec<IndexOwner>),
    Directory(CatalogPage),
    Summaries(Vec<DirectorySummary>),
    Headers(CatalogHeads),
    Names(Arc<BTreeSet<(String, String)>>),
}
pub struct CatalogFactory {
    pub managed: bool,
}
struct CatalogModule {
    registry: Option<ServiceHandle<DataSourceRegistry>>,
    runtime: Option<ServiceHandle<ExecutionService>>,
    storage: Option<ServiceHandle<ManagementStorage>>,
    managed: bool,
}
fn descriptor(managed: bool) -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::module("catalog-manager"));
    d.execution = ExecutionPolicy::Concurrent {
        max_in_flight: NonZeroUsize::new(16).unwrap(),
    };
    d.requires.extend([
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::initiator("duckdb-runtime"),
    ]);
    if managed {
        d.requires.push(ComponentRef::initiator("duckdb-instance"));
    }
    d
}
impl ModuleFactory for CatalogFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(CatalogModule {
            registry: None,
            runtime: None,
            storage: None,
            managed: self.managed,
        }))
    }
}
#[async_trait]
impl Lifecycle for CatalogModule {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.registry = Some(
            ctx.services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
        );
        self.runtime = Some(
            ctx.services
                .get(&ComponentRef::initiator("duckdb-runtime"), "execution")?,
        );
        if self.managed {
            self.storage = Some(
                ctx.services
                    .get(&ComponentRef::initiator("duckdb-instance"), "storage")?,
            );
        }
        Ok(())
    }
}
impl Module for CatalogModule {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Concurrent {
            max_in_flight: NonZeroUsize::new(16).unwrap(),
            handler: Arc::new(CatalogHandler {
                registry: self.registry.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
                storage: self.storage.clone(),
                names_cache: Mutex::new(NamesCache::new(64)),
                names_flight: SingleFlight::default(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                10,
                descriptor(self.managed).component(),
                CATALOG_TOPIC,
            )],
        }))
    }
}
struct NamesCache {
    limit: usize,
    order: VecDeque<String>,
    entries: BTreeMap<String, Arc<BTreeSet<(String, String)>>>,
}
impl NamesCache {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            order: VecDeque::new(),
            entries: BTreeMap::new(),
        }
    }
    fn get(&self, key: &str) -> Option<Arc<BTreeSet<(String, String)>>> {
        self.entries.get(key).cloned()
    }
    fn insert(&mut self, key: String, value: Arc<BTreeSet<(String, String)>>) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.entries.len() >= self.limit
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }
}

struct SingleFlight<K: Ord, V: Clone> {
    flights: Mutex<BTreeMap<K, broadcast::Sender<Result<V>>>>,
}
impl<K: Ord, V: Clone> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: Mutex::new(BTreeMap::new()),
        }
    }
}
impl<K: Ord + Clone, V: Clone> SingleFlight<K, V> {
    async fn run<F, Fut>(&self, key: K, op: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V>>,
    {
        enum Role<V> {
            Leader(broadcast::Sender<Result<V>>),
            Follower(broadcast::Receiver<Result<V>>),
        }
        let role = {
            let mut guard = self.flights.lock().unwrap();
            if let Some(tx) = guard.get(&key) {
                Role::Follower(tx.subscribe())
            } else {
                let (tx, _) = broadcast::channel(1);
                guard.insert(key.clone(), tx.clone());
                Role::Leader(tx)
            }
        };
        match role {
            Role::Follower(mut rx) => rx.recv().await.unwrap_or_else(|_| {
                Err(EngineError::new("53300", "catalog single-flight aborted"))
            }),
            Role::Leader(tx) => {
                struct Cleanup<'a, K: Ord + Clone, V: Clone> {
                    flights: &'a Mutex<BTreeMap<K, broadcast::Sender<Result<V>>>>,
                    key: K,
                }
                impl<K: Ord + Clone, V: Clone> Drop for Cleanup<'_, K, V> {
                    fn drop(&mut self) {
                        self.flights.lock().unwrap().remove(&self.key);
                    }
                }
                let _cleanup = Cleanup {
                    flights: &self.flights,
                    key,
                };
                let result = op().await;
                let _ = tx.send(result.clone());
                result
            }
        }
    }
}

struct CatalogHandler {
    registry: ServiceHandle<DataSourceRegistry>,
    runtime: ServiceHandle<ExecutionService>,
    storage: Option<ServiceHandle<ManagementStorage>>,
    names_cache: Mutex<NamesCache>,
    names_flight: SingleFlight<String, Arc<BTreeSet<(String, String)>>>,
}
#[async_trait]
impl ConcurrentHandler for CatalogHandler {
    async fn handle(
        &self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<CatalogRequest>()?;
        let result: Result<CatalogReply> = match request.as_ref() {
            CatalogRequest::Snapshot => {
                let r = self.registry.acquire()?;
                Ok(CatalogReply::Snapshot(
                    r.sources()
                        .map(|s| (s.definition.id.clone(), s.version()))
                        .collect(),
                ))
            }
            CatalogRequest::Names(snapshot) => {
                if let Some(cached) = self.names_cache.lock().unwrap().get(snapshot) {
                    Ok(CatalogReply::Names(cached))
                } else {
                    match &self.storage {
                        Some(handle) => {
                            let port = handle.acquire()?.port.clone();
                            let snap = snapshot.clone();
                            let set = self
                                .names_flight
                                .run(snapshot.clone(), || async move {
                                    port.relation_names(snap).await
                                })
                                .await;
                            if let Ok(ref value) = set {
                                self.names_cache
                                    .lock()
                                    .unwrap()
                                    .insert(snapshot.clone(), value.clone());
                            }
                            set.map(CatalogReply::Names)
                        }
                        None => Ok(CatalogReply::Names(Default::default())),
                    }
                }
            }
            CatalogRequest::Headers => match &self.storage {
                Some(handle) => handle
                    .acquire()?
                    .port
                    .catalog_headers()
                    .await
                    .map(CatalogReply::Headers),
                None => Ok(CatalogReply::Headers(CatalogHeads::default())),
            },
            CatalogRequest::Directory(r) => self.directory(r).await.map(CatalogReply::Directory),
            CatalogRequest::Summaries(r) => match &self.storage {
                Some(handle) => handle
                    .acquire()?
                    .port
                    .directory_summaries(r.clone())
                    .await
                    .map(CatalogReply::Summaries),
                None => Err(EngineError::unsupported(
                    "catalog summaries require management storage",
                )),
            },
            CatalogRequest::Indexes(id) => {
                let runtime = self.runtime.acquire()?;
                runtime
                    .port
                    .indexes(id.clone(), Box::new(self.runtime.acquire()?))
                    .await
                    .map(|indexes| {
                        CatalogReply::Indexes(
                            indexes
                                .into_iter()
                                .map(|i| {
                                    let index = ObjectRef {
                                        source: i.source,
                                        catalog: i.catalog,
                                        schema: i.schema,
                                        name: i.index,
                                    };
                                    let table = ObjectRef {
                                        name: i.table,
                                        ..index.clone()
                                    };
                                    IndexOwner {
                                        index,
                                        table,
                                        protected: i.protected,
                                    }
                                })
                                .collect(),
                        )
                    })
            }
        };
        Ok(Arc::new(result))
    }
}

impl CatalogHandler {
    async fn directory(&self, request: &DirectoryRequest) -> Result<CatalogPage> {
        let handle = self
            .storage
            .as_ref()
            .ok_or_else(|| EngineError::unsupported("management is disabled"))?;
        let storage = handle
            .acquire()
            .map_err(|_| EngineError::new("53300", "catalog storage unavailable"))?;
        let mut page = storage.port.directory(request.clone()).await?;
        let registry = self
            .registry
            .acquire()
            .map_err(|_| EngineError::new("53300", "catalog registry unavailable"))?;
        page.stale |= registry
            .get(&request.id)
            .is_ok_and(|s| s.stale.load(std::sync::atomic::Ordering::SeqCst));
        Ok(page)
    }
}

impl HubPayloadBytes for CatalogRequest {
    fn hub_payload_bytes(&self) -> usize {
        match self {
            Self::Snapshot | Self::Headers => 256,
            Self::Indexes(id) => 256usize.saturating_add(id.0.len()),
            Self::Names(id) => 256usize.saturating_add(id.len()),
            Self::Directory(request) => serialized_payload_bytes(request),
            Self::Summaries(request) => serialized_payload_bytes(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn single_flight_coalesces_concurrent_identical_lookups() {
        let flight = Arc::new(SingleFlight::<String, usize>::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let f = flight.clone();
            let c = calls.clone();
            tasks.push(tokio::spawn(async move {
                f.run("snap-1".to_string(), || async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Ok(42usize)
                })
                .await
                .unwrap()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), 42);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let mut cache = NamesCache::new(2);
        cache.insert("s1".into(), Arc::new(BTreeSet::new()));
        cache.insert("s2".into(), Arc::new(BTreeSet::new()));
        cache.insert("s3".into(), Arc::new(BTreeSet::new()));
        assert!(cache.get("s1").is_none());
        assert!(cache.get("s2").is_some());
        assert!(cache.get("s3").is_some());
    }
}
