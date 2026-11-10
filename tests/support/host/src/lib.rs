//! Workspace-only test adapter. Service orchestration belongs to magicDataHub.
//! Development test adapter for the shared FedEngine Bundle and checked shutdown.
use modorch::{prelude::*, *};
use modorch_fedengine::{FedEngineBundle, FedEngineConfig, FedEngineHandle, FedEngineMetrics};
use std::{net::SocketAddr, time::Duration};

pub struct FedEngine {
    observation: FedEngineHandle,
    pub handle: ApplicationHandle,
    grace: Duration,
}
impl FedEngine {
    pub async fn start(
        config: FedEngineConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let grace = Duration::from_secs(config.shutdown.grace_period_secs);
        let mut builder = ApplicationBuilder::new(&config.application.name);
        let registered = FedEngineBundle::new(config).register(&mut builder)?;
        let mut selection = ComponentSelection::new();
        for id in registered.initiator_ids() {
            selection.enable_initiator(id);
        }
        for id in registered.module_ids() {
            selection.enable_module(id);
        }
        let demand = registered.resource_requirements();
        let resources = ExecutionResources {
            blocking_io_threads: demand.blocking_io_threads,
            blocking_io_capacity: demand.blocking_io_capacity,
            max_tasks_per_owner: demand.max_tasks_per_owner,
            ..Default::default()
        };
        let builder = builder
            .selection(selection)
            .execution_resources(resources)
            .queue_capacities(64, 64);
        let handle = match builder.build()?.start().await {
            Ok(handle) => handle,
            Err(error) => {
                tracing::error!(cause=%error.source,cleanup=?error.cleanup,"startup failed");
                // Preserve both the original failure and compensation result; retry incomplete cleanup.
                if error.cleanup.is_err() {
                    let retry = drain(&error.handle, grace).await;
                    tracing::error!(cleanup=?retry,"startup cleanup completion");
                }
                return Err(Box::new(error));
            }
        };
        if let Err(error) = registered.observe(&handle) {
            let cleanup = drain(&handle, grace).await;
            tracing::error!(%error, ?cleanup, "startup observation failed");
            return Err(Box::new(error));
        }
        let observation = registered.handle().clone();
        if !observation.status().ready {
            let cleanup = drain(&handle, grace).await;
            tracing::error!(?cleanup, "application did not become ready");
            return Err("application did not become ready".into());
        }
        Ok(Self {
            observation,
            handle,
            grace,
        })
    }
    pub fn metrics(&self) -> FedEngineMetrics {
        self.observation.metrics()
    }
    /// Usable PGWire address; absent once application readiness is lost.
    pub fn address(&self) -> Option<SocketAddr> {
        self.observation.pgwire_address()
    }
    /// Usable management HTTP address; absent when disabled or not ready.
    pub fn management_address(&self) -> Option<SocketAddr> {
        self.observation.management_address()
    }
    /// Retain the application and retry while owned work is still draining.
    pub async fn shutdown_until_drained(&self) -> FrameworkResult<ShutdownReceipt> {
        drain(&self.handle, self.grace).await
    }
    pub async fn shutdown(&self) -> FrameworkResult<ShutdownReceipt> {
        self.handle
            .shutdown_checked(ShutdownOptions {
                timeout: self.grace,
            })
            .await
    }
}

async fn drain(handle: &ApplicationHandle, timeout: Duration) -> FrameworkResult<ShutdownReceipt> {
    loop {
        match handle.shutdown_checked(ShutdownOptions { timeout }).await {
            Err(error @ FrameworkError::StopIncomplete { .. }) => {
                tracing::error!(%error, "shutdown incomplete; retaining resources and waiting for actual exit");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            result => return result,
        }
    }
}
