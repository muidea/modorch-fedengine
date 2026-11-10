use crate::{
    auth::{Authentication, load_tls},
    cancel::Cancellations,
    session::Backend,
};
use modorch::{prelude::*, *};
use modorch_fedengine_contract::{EngineMetrics, GaugeGuard};
use modorch_fedengine_core::Config;
use pgwire::api::{
    PgWireServerHandlers,
    auth::StartupHandler,
    auth::{Password, sasl::scram::gen_salted_password},
    cancel::CancelHandler,
    query::{ExtendedQueryHandler, SimpleQueryHandler},
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpSocket,
    sync::{Notify, Semaphore},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
#[derive(Default)]
pub struct ListenerStatus {
    address: Mutex<Option<SocketAddr>>,
}
impl ListenerStatus {
    /// Bound address, including during setup; use application health for readiness.
    pub fn address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap()
    }
}
pub struct PgwireFactory {
    pub metrics: Arc<EngineMetrics>,
    pub config: Arc<Config>,
    pub status: Arc<ListenerStatus>,
}
struct PgwireInitiator {
    metrics: Arc<EngineMetrics>,
    config: Arc<Config>,
    status: Arc<ListenerStatus>,
    socket: Option<TcpSocket>,
    tls: Option<TlsAcceptor>,
    passwords: Arc<BTreeMap<String, Password>>,
    stop: CancellationToken,
}
fn descriptor() -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::initiator("pgwire-listener"));
    // Sessions and suspended Portals belong to the execution generation. A
    // runtime rebuild must close them before the replacement accepts traffic.
    descriptor
        .requires
        .push(ComponentRef::initiator("duckdb-runtime"));
    descriptor
}
impl InitiatorFactory for PgwireFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(PgwireInitiator {
            metrics: self.metrics.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            socket: None,
            tls: None,
            passwords: Arc::new(BTreeMap::new()),
            stop: CancellationToken::new(),
        }))
    }
}
pub(super) fn fw(message: impl Into<String>) -> FrameworkError {
    FrameworkError::InvalidConfig(message.into())
}
#[async_trait]
impl Lifecycle for PgwireInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let users = self.config.users.clone();
        self.passwords = Arc::new(
            ctx.tasks
                .spawn_blocking_io(move |_| {
                    let mut passwords = BTreeMap::new();
                    for user in users {
                        let password = std::env::var(&user.password_secret_ref)
                            .map_err(|_| fw("authentication SecretRef is missing"))?;
                        if password.is_empty() {
                            return Err(fw("authentication secret is empty"));
                        }
                        let salt = rand::random::<[u8; 16]>().to_vec();
                        let hash = gen_salted_password(&password, &salt, 4096);
                        passwords.insert(user.name, Password::new(Some(salt), hash));
                    }
                    Ok(passwords)
                })?
                .wait_finished()
                .await?,
        );
        self.tls = load_tls(&self.config.pgwire)?;
        let address: SocketAddr = self
            .config
            .pgwire
            .listen
            .parse()
            .map_err(|e| fw(format!("invalid PGWire address: {e}")))?;
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|e| fw(e.to_string()))?;
        socket.set_reuseaddr(true).map_err(|e| fw(e.to_string()))?;
        socket.bind(address).map_err(|e| fw(e.to_string()))?;
        *self.status.address.lock().unwrap() =
            Some(socket.local_addr().map_err(|e| fw(e.to_string()))?);
        self.socket = Some(socket);
        Ok(())
    }
    async fn begin_shutdown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.stop.cancel();
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.socket.take();
        *self.status.address.lock().unwrap() = None;
        Ok(())
    }
}
#[async_trait]
impl Initiator for PgwireInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    async fn activate_ingress(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        let listener = self
            .socket
            .take()
            .unwrap()
            .listen(1024)
            .map_err(|e| fw(e.to_string()))?;
        let tasks = ctx.tasks.clone();
        let hub = ctx.event_hub.external();
        let config = self.config.clone();
        let tls = self.tls.clone();
        let passwords = self.passwords.clone();
        let stop = self.stop.clone();
        let slots = Arc::new(Semaphore::new(config.pgwire.max_connections));
        let cancellations = Arc::new(Cancellations::default());
        let metrics = self.metrics.clone();
        ctx.tasks.spawn(move |cancel|async move{
            loop{
                let (socket,_)=tokio::select!{biased;_=stop.cancelled()=>break,_=cancel.cancelled()=>break,result=listener.accept()=>result.map_err(|e|fw(e.to_string()))?};
                let Ok(permit)=slots.clone().try_acquire_owned()else{drop(socket);continue;};
                socket.set_nodelay(true).map_err(|e|fw(e.to_string()))?;
                let config=config.clone();let tls=tls.clone();let passwords=passwords.clone();let hub=hub.clone();let cancellations=cancellations.clone();let stopped=stop.clone();let metrics=metrics.clone();
                tasks.spawn(move |shutdown|async move{
                    let _session=GaugeGuard::enter(metrics.active_sessions.clone());
                    let _permit=permit;let ready=Arc::new(Notify::new());let authenticated=ready.clone();
                    let backend=Arc::new(Backend::new(hub.clone(), config.clone(), cancellations.clone()));
                    let failed=CancellationToken::new();
                    let auth=Arc::new(Authentication::new(passwords, config.clone(), hub, ready, failed.clone()));
                    let handlers=Arc::new(Handlers{backend,auth,cancellations});
                    let process=pgwire::tokio::process_socket(socket,tls,handlers);tokio::pin!(process);
                    let startup=async{tokio::time::timeout(Duration::from_millis(config.pgwire.startup_timeout_ms),authenticated.notified()).await};
                    tokio::pin!(startup);
                    tokio::select!{biased;_=failed.cancelled()=>{},_=stopped.cancelled()=>{},_=shutdown.cancelled()=>{},_=&mut process=>{},result=&mut startup=>{
                        if result.is_ok(){tokio::select!{_=stopped.cancelled()=>{},_=shutdown.cancelled()=>{},_=&mut process=>{}}}
                    }}
                    Ok(())
                })?;
            }Ok(())
        })?;
        Ok(())
    }
}
struct Handlers {
    backend: Arc<Backend>,
    auth: Arc<Authentication>,
    cancellations: Arc<Cancellations>,
}
impl PgWireServerHandlers for Handlers {
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.auth.clone()
    }
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.backend.clone()
    }
    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.backend.clone()
    }
    fn cancel_handler(&self) -> Arc<impl CancelHandler> {
        self.cancellations.clone()
    }
}
