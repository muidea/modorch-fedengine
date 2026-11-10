//! Host-authorized typed clients. These never expose the hub or execution ports.
use crate::{FedEngineConfig, FedEngineHandle, FedEngineRegisteredComponents};
use modorch::{
    ApplicationHandle, ComponentRef, Destination, ExternalEventHub, FrameworkError, MessageOptions,
};
use modorch_fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::{EngineError, Result};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Protocol-neutral SQL values; no DuckDB or PGWire connection is exposed.
pub use modorch_fedengine_contract::Cell as FedEngineValue;
/// Column names, type/OID/modifier and optional relation provenance.
pub use modorch_fedengine_contract::ColumnMeta as FedEngineColumn;
/// Confirmed completion, notices and affected rows; inspect its outcome.
pub use modorch_fedengine_contract::Completion as FedEngineCompletion;
/// Completed, commit-confirmed or indeterminate execution outcome.
pub use modorch_fedengine_contract::ExecutionOutcome as FedEngineExecutionOutcome;
/// Parameter/column PostgreSQL type identity used by the existing coordinator.
pub use modorch_fedengine_contract::PgType as FedEngineDataType;
/// Source metadata and current desired/applied revisions.
pub use modorch_fedengine_contract::SourceView as FedEngineSourceView;
/// CAS virtual database changes.
pub use modorch_fedengine_contract::VirtualDatabaseMutation as FedEngineDatabaseMutation;
/// Durable virtual database idempotency receipt.
pub use modorch_fedengine_contract::VirtualDatabaseReceipt as FedEngineDatabaseReceipt;
/// Virtual database CAS target.
pub use modorch_fedengine_contract::VirtualDatabaseTarget as FedEngineDatabaseTarget;
/// Source registration DTO, preserving SecretRef rather than plaintext secrets.
pub use modorch_fedengine_core::DataSourceDefinition as FedEngineSourceDefinition;
/// Source identity used in explicit host management grants.
pub use modorch_fedengine_core::DataSourceId as FedEngineSourceId;
/// Supported PostgreSQL/MySQL source kind.
pub use modorch_fedengine_core::DataSourceKind as FedEngineSourceKind;
/// SQLSTATE error; outcome_unknown never implies rollback or safe replay.
pub use modorch_fedengine_core::EngineError as FedEngineClientError;
/// Existing CAS source management actions; not arbitrary internal notifications.
pub use modorch_fedengine_core::ManagementAction as FedEngineManagementAction;
/// Existing asynchronous management operation and durable idempotency receipt.
pub use modorch_fedengine_core::ManagementOperation as FedEngineManagementOperation;
/// Durable management operation status, independent of request admission.
pub use modorch_fedengine_core::OperationStatus as FedEngineOperationStatus;
/// Source schema mapping for the existing CAS management action.
pub use modorch_fedengine_core::SchemaBinding as FedEngineSchemaBinding;
/// Current source lifecycle state returned by scoped source reads.
pub use modorch_fedengine_core::SourceStatus as FedEngineSourceStatus;
/// Source CAS incarnation and revision.
pub use modorch_fedengine_core::SourceTarget as FedEngineSourceTarget;
/// Published virtual database, including stable identity and revision.
pub use modorch_fedengine_core::VirtualDatabase as FedEngineDatabase;
/// Explicit virtual database configuration.
pub use modorch_fedengine_core::VirtualDatabaseConfig as FedEngineDatabaseDefinition;
/// Virtual schema mapping configuration.
pub use modorch_fedengine_core::VirtualSchemaConfig as FedEngineSchemaMapping;

/// Host assertion of an already authenticated configured principal and call origin.
/// This is not a password verifier. Only trusted host code may mint this value;
/// never derive its name or management grants directly from untrusted input.
#[derive(Clone, Debug)]
pub struct FedEnginePrincipal {
    name: String,
    origin: String,
}
impl FedEnginePrincipal {
    /// Attest an authenticated identity; binding also checks configured membership.
    pub fn authenticated(name: impl Into<String>, origin: impl Into<String>) -> Result<Self> {
        let identity = Self {
            name: name.into(),
            origin: origin.into(),
        };
        if identity.name.is_empty()
            || identity.name.len() > 128
            || identity.origin.is_empty()
            || identity.origin.len() > 128
            || identity.name.chars().any(char::is_control)
            || identity.origin.chars().any(char::is_control)
        {
            return Err(EngineError::config(
                "invalid client principal or call origin",
            ));
        }
        Ok(identity)
    }
}

/// Per-client admission/input/result budgets, shared by clones. Global native
/// workers, batches and blocking-I/O limits remain the host/configuration's job.
#[derive(Clone, Debug)]
pub struct FedEngineClientBudget {
    /// Per-control/SQL execution deadline; stream consumption shares SQL's deadline.
    pub timeout: Duration,
    /// Active client calls/streams; admission rejects rather than queuing without bound.
    pub max_in_flight: usize,
    /// Maximum UTF-8 SQL bytes.
    pub max_sql_bytes: usize,
    /// Maximum parameters per statement.
    pub max_parameters: usize,
    /// Maximum rows delivered by one execution, across all batches.
    pub max_result_rows: usize,
    /// Maximum estimated value/structure bytes delivered across all row batches.
    pub max_result_bytes: usize,
    /// Maximum serialized management input or reply/description size.
    pub max_control_bytes: usize,
}
impl Default for FedEngineClientBudget {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_in_flight: 4,
            max_sql_bytes: 65536,
            max_parameters: 128,
            max_result_rows: 10000,
            max_result_bytes: 8 * 1024 * 1024,
            max_control_bytes: 256 * 1024,
        }
    }
}
impl FedEngineClientBudget {
    fn validate(&self, config: &FedEngineConfig) -> Result<()> {
        if self.timeout.is_zero()
            || self.timeout
                > Duration::from_millis(
                    config
                        .pgwire
                        .query_timeout_ms
                        .min(config.management.request_timeout_ms),
                )
            || !(1..=128).contains(&self.max_in_flight)
            || !(1..=1024 * 1024).contains(&self.max_sql_bytes)
            || !(1..=4096).contains(&self.max_parameters)
            || !(1..=1_000_000).contains(&self.max_result_rows)
            || !(1..=64 * 1024 * 1024).contains(&self.max_result_bytes)
            || !(1..=4 * 1024 * 1024).contains(&self.max_control_bytes)
        {
            return Err(EngineError::config(
                "invalid client budget or deadline exceeds configured control limits",
            ));
        }
        Ok(())
    }
}

/// Cooperative cancellation. Canceling never claims native exit or write rollback.
#[derive(Clone, Default, Debug)]
pub struct FedEngineCancellation(CancellationToken);
impl FedEngineCancellation {
    /// New uncanceled token, independently scoped to one call/execution.
    pub fn new() -> Self {
        Self(CancellationToken::new())
    }
    /// Request cancellation; actual resources are retained until their worker exits.
    pub fn cancel(&self) {
        self.0.cancel();
    }
    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}
struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

struct BoundClient {
    hub: ExternalEventHub,
    handle: FedEngineHandle,
    config: Arc<FedEngineConfig>,
    identity: FedEnginePrincipal,
    budget: FedEngineClientBudget,
    generations: BTreeMap<ComponentRef, u64>,
    slots: Arc<Semaphore>,
}
impl BoundClient {
    fn check(&self) -> Result<()> {
        let status = self.handle.status();
        if !status.ready
            || status.components.len() != self.generations.len()
            || status
                .components
                .iter()
                .any(|(id, h)| self.generations.get(id) != Some(&h.generation))
        {
            return Err(EngineError::new(
                "55000",
                "client application is not Ready or its component generation changed; bind a new client",
            ));
        }
        Ok(())
    }
    fn admit(&self) -> Result<OwnedSemaphorePermit> {
        self.check()?;
        self.slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| EngineError::new("53300", "client in-flight budget exhausted"))
    }
    fn options(&self, bytes: usize, deadline: Instant) -> MessageOptions {
        MessageOptions {
            payload_bytes: Some(bytes),
            deadline: Some(deadline),
            headers: BTreeMap::from([
                ("fedengine.principal".into(), self.identity.name.clone()),
                ("fedengine.origin".into(), self.identity.origin.clone()),
            ]),
            ..Default::default()
        }
    }
    async fn control<T: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        topic: &str,
        component: &str,
        request: T,
        bytes: usize,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> Result<Arc<Result<R>>> {
        self.check()?;
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(EngineError::new(
                "57014",
                "client request canceled before admission",
            ));
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(EngineError::new(
                "57014",
                "client deadline exceeded before admission",
            ));
        }
        let handle = self
            .hub
            .request::<_, Result<R>>(
                topic,
                Destination::Component(ComponentRef::module(component)),
                request,
                self.options(bytes, deadline),
            )
            .map_err(|_| EngineError::new("53300", "client control admission rejected"))?;
        let reply = if let Some(token) = cancel {
            tokio::select! { biased; _=token.cancelled()=>return Err(EngineError::new("57014", "client canceled after admission; outcome may be unknown").unknown()), result=handle.wait(timeout)=>result }
        } else {
            handle.wait(timeout).await
        };
        reply.map_err(|e| match e {
            FrameworkError::CanceledBeforeStart(_) => {
                EngineError::new("57014", "client request canceled before handler start")
            }
            _ => EngineError::new(
                "57014",
                "client control result unavailable; outcome may be unknown",
            )
            .unknown(),
        })
    }
}
impl FedEngineRegisteredComponents {
    fn bind_client(
        &self,
        app: &ApplicationHandle,
        identity: FedEnginePrincipal,
        budget: FedEngineClientBudget,
    ) -> Result<Arc<BoundClient>> {
        budget.validate(&self.config)?;
        if !self.config.users.iter().any(|u| u.name == identity.name) {
            return Err(EngineError::denied("unknown client principal"));
        }
        self.observe(app).map_err(|_| {
            EngineError::new("55000", "client application does not match this bundle")
        })?;
        let status = self.handle.status();
        if !status.ready {
            return Err(EngineError::new(
                "55000",
                "clients require the complete bundle and application to be Ready",
            ));
        }
        Ok(Arc::new(BoundClient {
            hub: app.event_hub().external(),
            handle: self.handle().clone(),
            config: self.config.clone(),
            identity,
            slots: Arc::new(Semaphore::new(budget.max_in_flight)),
            budget,
            generations: status
                .components
                .into_iter()
                .map(|(id, h)| (id, h.generation))
                .collect(),
        }))
    }
    /// Bind a SQL capability to a host-authenticated configured principal, this
    /// application and its current generations. This grants no management access.
    pub fn query_client(
        &self,
        app: &ApplicationHandle,
        identity: FedEnginePrincipal,
        budget: FedEngineClientBudget,
    ) -> Result<FedEngineQueryClient> {
        Ok(FedEngineQueryClient(
            self.bind_client(app, identity, budget)?,
        ))
    }
    /// Bind an explicitly scoped host authorization. No implicit administrator
    /// exists; an empty grant permits nothing, even for a configured SQL user.
    pub fn management_client(
        &self,
        app: &ApplicationHandle,
        identity: FedEnginePrincipal,
        budget: FedEngineClientBudget,
        grant: FedEngineManagementGrant,
    ) -> Result<FedEngineManagementClient> {
        Ok(FedEngineManagementClient {
            bound: self.bind_client(app, identity, budget)?,
            grant,
        })
    }
}

mod management;
mod query;
pub use management::{FedEngineManagementClient, FedEngineManagementGrant};
pub use query::{
    FedEnginePreparedQuery, FedEngineQueryClient, FedEngineResultChunk, FedEngineResultStream,
};
