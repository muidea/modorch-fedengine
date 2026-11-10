//! Exercise the observer against actual framework supervision without database I/O.
use super::*;
use crate::witness::WitnessFactory;
use modorch::{prelude::*, *};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone)]
struct RestartProbe {
    tasks: Arc<Mutex<Vec<TaskScheduler>>>,
    rebuilding: Arc<Notify>,
    release: Arc<Semaphore>,
}
fn descriptor() -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::initiator("probe"));
    descriptor.failure = FailurePolicy::Restart {
        max_attempts: 1,
        backoff: std::time::Duration::ZERO,
    };
    descriptor
}
impl InitiatorFactory for RestartProbe {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(self.clone()))
    }
}
#[async_trait]
impl Lifecycle for RestartProbe {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.tasks.lock().unwrap().push(ctx.tasks.clone());
        Ok(())
    }
    async fn ready(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        if self.tasks.lock().unwrap().len() > 1 {
            self.rebuilding.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        Ok(())
    }
}
impl Initiator for RestartProbe {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
}

fn fixture() -> (Application, FedEngineHandle, RestartProbe) {
    let witness = Arc::default();
    let observer = FedEngineHandle::new(
        vec![ComponentRef::initiator("probe")],
        Arc::default(),
        Arc::default(),
        Arc::default(),
        Arc::clone(&witness),
    );
    let probe = RestartProbe {
        tasks: Arc::default(),
        rebuilding: Arc::default(),
        release: Arc::new(Semaphore::new(0)),
    };
    let mut selection = ComponentSelection::new();
    selection.enable_initiator("probe");
    let mut builder = ApplicationBuilder::new("observer-probe").selection(selection);
    builder
        .register_initiator(WitnessFactory {
            factory: probe.clone(),
            application: witness,
        })
        .unwrap();
    (builder.build().unwrap(), observer, probe)
}

#[tokio::test]
async fn live_observation_tracks_real_recovery_and_component_generations() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (app, observer, probe) = fixture();
        let app = app.start().await.unwrap();
        observer.observe(&app).unwrap();
        let component = ComponentRef::initiator("probe");
        assert!(observer.status().ready);
        assert_eq!(observer.status().components[&component].generation, 0);
        let old_tasks = probe.tasks.lock().unwrap()[0].clone();
        let task = old_tasks
            .spawn(|_| async {
                Err(FrameworkError::InvalidConfig(
                    "injected generation failure".into(),
                ))
            })
            .unwrap();
        assert!(task.wait_finished().await.is_err());
        probe.rebuilding.notified().await;
        let status = observer.status();
        assert!(!status.ready);
        assert_eq!(status.application_state, Some(ApplicationState::Recovering));
        assert!(status.pgwire_address.is_none());
        assert!(status.management_address.is_none());
        probe.release.add_permits(1);
        while !observer.status().ready {
            tokio::task::yield_now().await;
        }
        let status = observer.status();
        assert_eq!(status.components[&component].generation, 1);
        assert_eq!(status.components[&component].restarts, 1);
        assert!(status.components[&component].last_failure.is_some());
        assert!(matches!(
            old_tasks.spawn(|_| async { Ok(()) }),
            Err(FrameworkError::Closed)
        ));
        app.shutdown_checked(Default::default()).await.unwrap();
        assert_eq!(
            observer.status().application_state,
            Some(ApplicationState::Stopped)
        );
        assert_eq!(observer.status().components[&component].generation, 1);
    })
    .await
    .expect("actual framework recovery and observer convergence");
}

#[tokio::test]
async fn forced_shutdown_is_observed_as_forced_instead_of_graceful_or_ready() {
    let (app, observer, _) = fixture();
    let app = app.start().await.unwrap();
    observer.observe(&app).unwrap();
    assert!(observer.status().ready);
    assert!(matches!(
        app.force_shutdown().await,
        ShutdownReceipt::Forced { .. }
    ));
    let status = observer.status();
    assert!(!status.ready);
    assert_eq!(status.application_state, Some(ApplicationState::Forced));
    assert!(status.pgwire_address.is_none());
    assert!(status.management_address.is_none());
}
