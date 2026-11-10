//! A host-owned Runtime, Application, signal policy and checked shutdown.
mod config;
use modorch::{
    ApplicationBuilder, ApplicationHandle, ComponentSelection, ExecutionResources, FrameworkError,
    FrameworkResult, ShutdownOptions, ShutdownReceipt,
};
use modorch_fedengine::{FedEngineBundle, FedEngineConfig};
use std::time::Duration;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "application.toml".into());
    if args.next().is_some() {
        return Err("usage: embedded-in-modorch [configuration.toml]".into());
    }
    let config = config::prepare(FedEngineConfig::load(path)?, &std::env::current_dir()?)?;
    let timeout = Duration::from_secs(config.shutdown.grace_period_secs);
    let mut builder = ApplicationBuilder::new(&config.application.name);
    // The host may register its other libraries before/after this registration.
    let registered = FedEngineBundle::new(config).register(&mut builder)?;
    let mut selected = ComponentSelection::new();
    for id in registered.initiator_ids() {
        selected.enable_initiator(id);
    }
    for id in registered.module_ids() {
        selected.enable_module(id);
    }
    // This example contains only FedEngine. A combined host must merge demands.
    let demand = registered.resource_requirements();
    let app = builder
        .selection(selected)
        .execution_resources(ExecutionResources {
            blocking_io_threads: demand.blocking_io_threads,
            blocking_io_capacity: demand.blocking_io_capacity,
            max_tasks_per_owner: demand.max_tasks_per_owner,
            ..Default::default()
        })
        .build()?;
    let app = match app.start().await {
        Ok(app) => app,
        Err(error) => {
            eprintln!("startup cause={} cleanup={:?}", error.source, error.cleanup);
            if error.cleanup.is_err() {
                let cleanup = drain(&error.handle, timeout).await;
                eprintln!("startup cleanup retry={cleanup:?}");
            }
            return Err(Box::new(error) as Error);
        }
    };
    // Always drain after a successful start, even if observation or waiting fails.
    let run: Result<(), Error> = async {
        registered.observe(&app)?;
        let observer = registered.handle();
        if !observer.status().ready {
            return Err("FedEngine did not become ready".into());
        }
        println!(
            "embedded-in-modorch Ready pgwire={:?} management={:?}",
            observer.pgwire_address(),
            observer.management_address()
        );
        tokio::select! {
            result = wait_for_signal() => result.map_err(Into::into),
            result = app.wait() => result.map_err(Into::into),
        }
    }
    .await;
    let shutdown = drain(&app, timeout).await;
    println!("embedded-in-modorch shutdown={shutdown:?}");
    run?;
    match shutdown? {
        ShutdownReceipt::GracefullyStopped => Ok(()),
        ShutdownReceipt::Forced { .. } => {
            Err("forced shutdown did not prove resource release".into())
        }
    }
}

async fn drain(app: &ApplicationHandle, timeout: Duration) -> FrameworkResult<ShutdownReceipt> {
    loop {
        match app.shutdown_checked(ShutdownOptions { timeout }).await {
            Err(error @ FrameworkError::StopIncomplete { .. }) => {
                eprintln!("{error}; retaining Runtime and retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
}

async fn wait_for_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
