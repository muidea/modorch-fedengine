//! Register FedEngine in an existing Modorch 0.0.3 application.
//!
//! The host owns configuration loading, paths, credentials, Tokio, logging,
//! component selection, budgets and checked shutdown. Registration creates only
//! factories: it never opens DuckDB, reads secrets, binds sockets or starts work.
//!
//! ```no_run
//! use modorch::{ApplicationBuilder, ComponentSelection, ExecutionResources};
//! use modorch_fedengine::{FedEngineBundle, FedEngineConfig, FedEngineRegisteredComponents};
//!
//! # fn composition() -> Result<(modorch::Application, FedEngineRegisteredComponents), Box<dyn std::error::Error + Send + Sync>> {
//! let config = FedEngineConfig::load("engine.toml")?;
//! let mut builder = ApplicationBuilder::new("host");
//! let engine = FedEngineBundle::new(config).register(&mut builder)?;
//! let mut selected = ComponentSelection::new();
//! for id in engine.initiator_ids() { selected.enable_initiator(id); }
//! for id in engine.module_ids() { selected.enable_module(id); }
//! let demand = engine.resource_requirements();
//! // For a combined application the host must merge all libraries' demands.
//! let resources = ExecutionResources {
//!     blocking_io_threads: demand.blocking_io_threads,
//!     blocking_io_capacity: demand.blocking_io_capacity,
//!     max_tasks_per_owner: demand.max_tasks_per_owner,
//!     ..Default::default()
//! };
//! // Return an unstarted application. The host starts it and checks shutdown.
//! let app = builder.selection(selected).execution_resources(resources).build()?;
//! // Keep engine to call engine.observe(&application_handle) after host startup.
//! Ok((app, engine))
//! # }
//! ```
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use modorch::{
    ApplicationBuilder, ComponentKind, ComponentRef, ExecutionResources, FrameworkError,
    FrameworkResult, InitiatorFactory, ModuleFactory,
};
use modorch_fedengine_contract::EngineMetrics;
use std::sync::Arc;
mod client;
mod handle;
mod profile;
mod witness;
pub use client::*;
pub use handle::{FedEngineHandle, FedEngineMetrics, FedEngineObservation, FedEngineStatus};
pub use profile::{FedEngineError, FedEngineProfile};

/// Existing FedEngine configuration; loading and path resolution belong to the host.
/// Loading does not rebase paths against the configuration file's directory.
/// Hosts should supply absolute filesystem paths and preserve deployed state paths
/// and SecretRefs when changing application or package names.
/// Organized PostgreSQL/MySQL sources expose one automatic database per native
/// database. Registrations for the same native target contribute to that database;
/// a connection discovering several native databases gives each a distinct name.
pub use modorch_fedengine_core::Config as FedEngineConfig;

/// Pure factory assembly for one FedEngine per Application (fixed component IDs).
///
/// The default profile preserves the default ingress selection. Explicit profiles
/// can disable HTTP while retaining managed storage, or run managed HTTP without PGWire.
/// Core storage/source-management components remain required in either case.
pub struct FedEngineBundle {
    config: FedEngineConfig,
    profile: FedEngineProfile,
}

impl FedEngineBundle {
    /// Store an already parsed configuration without reading files or credentials.
    pub fn new(config: FedEngineConfig) -> Self {
        let profile = FedEngineProfile::PgServer {
            management_api: config.management.enabled,
        };
        Self { config, profile }
    }

    /// Select network entry points independently of the internal management mode.
    /// Unsupported profiles are rejected by register before any builder mutation.
    pub fn profile(mut self, profile: FedEngineProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Validate pure values and register factories without enabling components.
    ///
    /// No Runtime, Application or EventHub is created. Host selection, execution
    /// resources, message limits and executor adapters are unchanged.
    ///
    /// # Errors
    /// EmbeddedCore returns FedEngineError::UnsupportedProfile. Validation and
    /// duplicate identities retain their underlying FrameworkError.
    /// Configuration/resource-demand errors occur before registering any factory.
    /// A duplicate component can fail after partial registration: Modorch 0.0.3
    /// has no registration transaction or rollback. On any registration error,
    /// discard this builder and reconstruct the host composition; do not start a
    /// partially registered application. A second bundle in the same builder is
    /// rejected by its fixed component identities.
    pub fn register(
        self,
        builder: &mut ApplicationBuilder,
    ) -> Result<FedEngineRegisteredComponents, FedEngineError> {
        let (pgwire, http) = self.profile.ingress()?;
        self.config
            .validate_for_ingress(pgwire, http)
            .map_err(|error| {
                FrameworkError::InvalidConfig(format!("FedEngine configuration: {error}"))
            })?;
        let demand = FedEngineResourceDemand::from_config(&self.config, pgwire)?;
        let config = Arc::new(self.config);
        let metrics = Arc::new(EngineMetrics::default());
        let pg_status = Arc::default();
        let http_status = Arc::default();
        let application = Arc::default();
        let mut registered = ComponentRegistrar::default();
        registered.initiator(
            builder,
            witness::WitnessFactory {
                factory: modorch_fedengine_duckdb::DuckdbInstanceFactory {
                    config: config.clone(),
                },
                application: Arc::clone(&application),
            },
        )?;
        registered.initiator(
            builder,
            modorch_fedengine_datasource::DataSourceFactory {
                definitions: config.datasources.clone(),
                managed: config.management.enabled,
            },
        )?;
        registered.initiator(
            builder,
            modorch_fedengine_duckdb::DuckdbFactory {
                metrics: metrics.clone(),
                config: config.clone(),
            },
        )?;
        if pgwire {
            registered.initiator(
                builder,
                modorch_fedengine_pgwire::PgwireFactory {
                    metrics: metrics.clone(),
                    config: config.clone(),
                    status: Arc::clone(&pg_status),
                },
            )?;
        }
        registered.module(
            builder,
            modorch_fedengine_catalog::CatalogFactory { managed: true },
        )?;
        registered.module(
            builder,
            modorch_fedengine_policy::PolicyFactory {
                config: config.clone(),
            },
        )?;
        registered.module(
            builder,
            modorch_fedengine_query::QueryFactory {
                config: config.clone(),
            },
        )?;
        registered.module(
            builder,
            modorch_fedengine_management::SourceManagerFactory {
                config: config.clone(),
            },
        )?;
        if http {
            registered.initiator(
                builder,
                modorch_fedengine_management::ManagementApiFactory {
                    config: config.clone(),
                    status: Arc::clone(&http_status),
                },
            )?;
        }
        let handle = FedEngineHandle::new(
            registered.components.clone(),
            metrics,
            pg_status,
            http_status,
            application,
        );
        Ok(FedEngineRegisteredComponents {
            components: registered.components,
            demand,
            handle,
            profile: self.profile,
            config,
        })
    }
}

/// Actual registered identities and advisory demand; no application ownership.
#[derive(Debug)]
pub struct FedEngineRegisteredComponents {
    components: Vec<ComponentRef>,
    demand: FedEngineResourceDemand,
    handle: FedEngineHandle,
    profile: FedEngineProfile,
    config: Arc<FedEngineConfig>,
}

impl FedEngineRegisteredComponents {
    /// The profile accepted during registration.
    pub fn profile(&self) -> FedEngineProfile {
        self.profile
    }

    /// Cloneable read-only state, metrics and usable-address observation.
    pub fn handle(&self) -> &FedEngineHandle {
        &self.handle
    }

    /// Associate live framework health after startup, or its cleanup handle on failure.
    ///
    /// Call after `start` returns, including `StartupError.handle`. Until then,
    /// handle clones remain Unobserved and return no usable addresses. Association
    /// is idempotent for one Application and rejects another application's selected
    /// components. If startup failed before this bundle began setup, identity is
    /// unverified: observation is rejected and the host retains the StartupError.
    /// This never starts/stops the application or changes its selection/budgets.
    pub fn observe(&self, app: &modorch::ApplicationHandle) -> Result<(), FedEngineError> {
        self.handle.observe(app)
    }
    /// Identities from the registered factories' actual descriptors, in registration order.
    pub fn components(&self) -> impl Iterator<Item = &ComponentRef> {
        self.components.iter()
    }

    /// Initiator IDs for explicit host selection.
    pub fn initiator_ids(&self) -> impl Iterator<Item = &str> {
        self.components
            .iter()
            .filter(|c| c.kind == ComponentKind::Initiator)
            .map(|c| c.id.as_str())
    }

    /// Module IDs for explicit host selection.
    pub fn module_ids(&self) -> impl Iterator<Item = &str> {
        self.components
            .iter()
            .filter(|c| c.kind == ComponentKind::Module)
            .map(|c| c.id.as_str())
    }

    /// Single-engine execution suggestions; the host chooses combined budgets.
    pub fn resource_requirements(&self) -> FedEngineResourceDemand {
        self.demand
    }
}

#[derive(Default)]
struct ComponentRegistrar {
    components: Vec<ComponentRef>,
}
impl ComponentRegistrar {
    fn initiator(
        &mut self,
        builder: &mut ApplicationBuilder,
        factory: impl InitiatorFactory + 'static,
    ) -> FrameworkResult<()> {
        let component = factory.descriptor().component();
        builder.register_initiator(factory)?;
        self.components.push(component);
        Ok(())
    }

    fn module(
        &mut self,
        builder: &mut ApplicationBuilder,
        factory: impl ModuleFactory + 'static,
    ) -> FrameworkResult<()> {
        let component = factory.descriptor().component();
        builder.register_module(factory)?;
        self.components.push(component);
        Ok(())
    }
}

/// Advisory execution resources using the existing single-engine sizing formula.
///
/// These values are never applied to the host builder. They exclude the host's
/// other components, global task/message budgets, and database memory/disk needs;
/// they are not a bound on total process resources or a guarantee against load rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FedEngineResourceDemand {
    /// Suggested synchronous I/O worker threads.
    pub blocking_io_threads: usize,
    /// Suggested admitted synchronous I/O jobs (queued and executing).
    pub blocking_io_capacity: usize,
    /// Suggested shared task quota for each owner, including listener/session work.
    pub max_tasks_per_owner: usize,
}

impl FedEngineResourceDemand {
    fn from_config(config: &FedEngineConfig, pgwire: bool) -> FrameworkResult<Self> {
        let workers = config.duckdb.max_workers;
        let queue = config.management.queue_capacity;
        let overflow =
            || FrameworkError::InvalidConfig("FedEngine resource demand overflow".into());
        let demand = Self {
            blocking_io_threads: workers
                .checked_add(config.management.max_concurrent_operations)
                .and_then(|n| n.checked_add(2))
                .ok_or_else(overflow)?,
            blocking_io_capacity: workers
                .checked_mul(2)
                .and_then(|n| n.checked_add(queue))
                .and_then(|n| n.checked_add(8))
                .ok_or_else(overflow)?,
            max_tasks_per_owner: workers
                .checked_mul(4)
                .and_then(|n| {
                    n.checked_add(if pgwire {
                        config.pgwire.max_connections
                    } else {
                        0
                    })
                })
                .and_then(|n| n.checked_add(queue))
                .and_then(|n| n.checked_add(32))
                .ok_or_else(overflow)?,
        };
        ExecutionResources {
            cpu_threads: 1,
            cpu_capacity: 1,
            blocking_io_threads: demand.blocking_io_threads,
            blocking_io_capacity: demand.blocking_io_capacity,
            max_tasks_per_owner: demand.max_tasks_per_owner,
            max_waiters: 1,
            max_waiters_per_owner: 1,
            cpu_executor: None,
            blocking_io_executor: None,
        }
        .validate()?;
        Ok(demand)
    }
}
