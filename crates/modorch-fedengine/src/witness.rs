//! Associate the bundle with its actual application without changing component IDs.
use modorch::{prelude::*, *};
use std::sync::{Arc, Mutex};

pub(crate) struct WitnessFactory<F> {
    pub factory: F,
    pub application: Arc<Mutex<Option<ApplicationId>>>,
}
struct WitnessInitiator {
    inner: Box<dyn Initiator>,
    application: Arc<Mutex<Option<ApplicationId>>>,
}
impl<F: InitiatorFactory> InitiatorFactory for WitnessFactory<F> {
    fn descriptor(&self) -> ComponentDescriptor {
        self.factory.descriptor()
    }
    fn validate_config(&self, config: &ComponentConfig) -> FrameworkResult<()> {
        self.factory.validate_config(config)
    }
    fn create(&self, config: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(WitnessInitiator {
            inner: self.factory.create(config)?,
            application: self.application.clone(),
        }))
    }
}
#[async_trait]
impl Lifecycle for WitnessInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        *self.application.lock().unwrap() = Some(ctx.event_hub.metrics().application_id);
        self.inner.setup(ctx).await
    }
    async fn start(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        self.inner.start(ctx).await
    }
    async fn ready(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        self.inner.ready(ctx).await
    }
    async fn begin_shutdown(&mut self, ctx: &ShutdownContext) -> FrameworkResult<()> {
        self.inner.begin_shutdown(ctx).await
    }
    async fn quiesce(&mut self, ctx: &ShutdownContext) -> FrameworkResult<()> {
        self.inner.quiesce(ctx).await
    }
    async fn teardown(&mut self, ctx: &ShutdownContext) -> FrameworkResult<()> {
        self.inner.teardown(ctx).await
    }
}
#[async_trait]
impl Initiator for WitnessInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        self.inner.descriptor()
    }
    fn export_services(&self, publisher: &mut ServicePublisher) -> FrameworkResult<()> {
        self.inner.export_services(publisher)
    }
    fn event_endpoint(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        self.inner.event_endpoint()
    }
    async fn activate_ingress(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        self.inner.activate_ingress(ctx).await
    }
}
