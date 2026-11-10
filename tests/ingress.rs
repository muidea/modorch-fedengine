//! Real ingress lifecycle tests using the production PGWire and HTTP factories.
use modorch::{prelude::*, *};
use modorch_fedengine_contract::EngineMetrics;
use modorch_fedengine_core::Config;
use modorch_fedengine_management::{ManagementApiFactory, ManagementListenerStatus};
use modorch_fedengine_pgwire::{ListenerStatus, PgwireFactory};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{Notify, Semaphore},
};

const DEADLINE: Duration = Duration::from_secs(5);
const FAILURE: &str = "injected ingress lifecycle failure";

struct Gate {
    ready: Notify,
    release_ready: Semaphore,
    activate: Notify,
    release_activate: Semaphore,
    failure: Option<Phase>,
}
impl Gate {
    fn new(failure: Option<Phase>) -> Arc<Self> {
        Arc::new(Self {
            ready: Notify::new(),
            release_ready: Semaphore::new(0),
            activate: Notify::new(),
            release_activate: Semaphore::new(0),
            failure,
        })
    }
    fn descriptor() -> ComponentDescriptor {
        let mut descriptor = ComponentDescriptor::new(ComponentRef::module("ingress-gate"));
        descriptor.requires = vec![
            ComponentRef::initiator("pgwire-listener"),
            ComponentRef::initiator("management-api"),
        ];
        descriptor
    }
}
struct GateFactory(Arc<Gate>);
struct GateModule(Arc<Gate>);
// Isolate transport hooks from database initialization while preserving the
// production listener's declared execution-generation dependency.
struct RuntimeBoundary;
#[async_trait]
impl Lifecycle for RuntimeBoundary {}
impl Initiator for RuntimeBoundary {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::initiator("duckdb-runtime"))
    }
}
impl InitiatorFactory for RuntimeBoundary {
    fn descriptor(&self) -> ComponentDescriptor {
        Initiator::descriptor(self)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(Self))
    }
}
impl ModuleFactory for GateFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        Gate::descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(GateModule(self.0.clone())))
    }
}
#[async_trait]
impl Lifecycle for GateModule {
    async fn ready(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        self.0.ready.notify_one();
        self.0.release_ready.acquire().await.unwrap().forget();
        if self.0.failure == Some(Phase::Ready) {
            return Err(FrameworkError::InvalidConfig(FAILURE.into()));
        }
        Ok(())
    }
}
#[async_trait]
impl Module for GateModule {
    fn descriptor(&self) -> ComponentDescriptor {
        Gate::descriptor()
    }
    async fn activate_ingress(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        self.0.activate.notify_one();
        self.0.release_activate.acquire().await.unwrap().forget();
        if self.0.failure == Some(Phase::ActivateIngress) {
            return Err(FrameworkError::InvalidConfig(FAILURE.into()));
        }
        Ok(())
    }
}

struct IngressStatus {
    pg: Arc<ListenerStatus>,
    http: Arc<ManagementListenerStatus>,
}
impl IngressStatus {
    fn addresses(&self) -> [SocketAddr; 2] {
        [self.pg.address().unwrap(), self.http.address().unwrap()]
    }
    fn cleared(&self) -> bool {
        self.pg.address().is_none() && self.http.address().is_none()
    }
}
fn configuration(listen: &str) -> Config {
    let mut config = Config::load("config/application.toml").unwrap();
    config.pgwire.listen = listen.into();
    config.management.listen = listen.into();
    // Read an existing process variable; tests never mutate global credentials.
    config.users[0].password_secret_ref = "USER".into();
    config.validate().unwrap();
    config
}
fn build(config: Config, gate: Arc<Gate>) -> (Application, IngressStatus) {
    let config = Arc::new(config);
    let status = IngressStatus {
        pg: Arc::default(),
        http: Arc::default(),
    };
    let mut selection = ComponentSelection::new();
    for id in [
        "datasource-registry",
        "duckdb-runtime",
        "pgwire-listener",
        "management-api",
    ] {
        selection.enable_initiator(id);
    }
    selection.enable_module("ingress-gate");
    let mut builder = ApplicationBuilder::new("ingress-tests").selection(selection);
    builder
        .register_initiator(RuntimeBoundary)
        .unwrap()
        .register_initiator(modorch_fedengine_datasource::DataSourceFactory {
            definitions: vec![],
            // Use the real registry without opening a database: these tests exercise
            // network lifecycle and static HTTP/PG startup, not management queries.
            managed: false,
        })
        .unwrap()
        .register_initiator(PgwireFactory {
            metrics: Arc::new(EngineMetrics::default()),
            config: config.clone(),
            status: status.pg.clone(),
        })
        .unwrap()
        .register_initiator(ManagementApiFactory {
            config,
            status: status.http.clone(),
        })
        .unwrap()
        .register_module(GateFactory(gate))
        .unwrap();
    (builder.build().unwrap(), status)
}
async fn notified(notification: &Notify) {
    tokio::time::timeout(DEADLINE, notification.notified())
        .await
        .expect("lifecycle checkpoint");
}
async fn refuses(address: SocketAddr) {
    let error = tokio::time::timeout(DEADLINE, TcpStream::connect(address))
        .await
        .expect("TCP connect deadline")
        .expect_err("ingress must not listen");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}
async fn reusable(addresses: [SocketAddr; 2]) {
    for address in addresses {
        refuses(address).await;
        let listener = TcpListener::bind(address).await.expect("port released");
        assert_eq!(listener.local_addr().unwrap(), address);
    }
}
async fn serves(addresses: [SocketAddr; 2]) {
    tokio::time::timeout(DEADLINE, async {
        let mut pg = TcpStream::connect(addresses[0]).await.unwrap();
        // PostgreSQL SSLRequest, with TLS disabled in the test configuration.
        pg.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).await.unwrap();
        assert_eq!(pg.read_u8().await.unwrap(), b'N');
        let mut http = TcpStream::connect(addresses[1]).await.unwrap();
        http.write_all(b"GET /manage/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        http.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("<html"));
    })
    .await
    .expect("PGWire and HTTP response deadline");
}
fn assert_failure(error: &StartupError, phase: Phase) {
    assert_eq!(error.source.phase(), Some(phase));
    assert!(
        matches!(error.source.root_cause(), FrameworkError::InvalidConfig(message) if message == FAILURE)
    );
    assert!(error.cleanup.is_ok(), "{:?}", error.cleanup);
    assert!(!error.handle.health().ready);
    assert_eq!(error.handle.task_metrics().tasks, 0);
}

#[tokio::test]
async fn ipv4_and_ipv6_bind_without_listening_until_all_components_are_ready() {
    for listen in ["127.0.0.1:0", "[::1]:0"] {
        let gate = Gate::new(None);
        let (app, status) = build(configuration(listen), gate.clone());
        assert!(status.cleared(), "build must not bind sockets");
        let start = tokio::spawn(app.start());
        notified(&gate.ready).await;
        let addresses = status.addresses();
        assert_ne!(addresses[0], addresses[1]);
        for address in addresses {
            refuses(address).await;
            let probe = if address.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }
            .unwrap();
            assert!(probe.bind(address).is_err(), "setup must reserve its port");
        }
        gate.release_ready.add_permits(1);
        notified(&gate.activate).await;
        serves(addresses).await;
        gate.release_activate.add_permits(1);
        let handle = tokio::time::timeout(DEADLINE, start)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(handle.health().ready);
        handle.shutdown_checked(Default::default()).await.unwrap();
        assert_eq!(handle.task_metrics().tasks, 0);
        assert!(status.cleared());
        reusable(addresses).await;
    }
}

#[tokio::test]
async fn incomplete_http_request_retains_resources_until_checked_shutdown_can_finish() {
    let gate = Gate::new(None);
    gate.release_ready.add_permits(1);
    gate.release_activate.add_permits(1);
    let (app, status) = build(configuration("127.0.0.1:0"), gate);
    let handle = app.start().await.unwrap();
    let addresses = status.addresses();
    let mut http = TcpStream::connect(addresses[1]).await.unwrap();
    http.write_all(b"POST /api/v1/datasources HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 1000\r\n\r\n")
        .await.unwrap();
    // The interim response proves the server has accepted the request and is
    // awaiting its body, so shutdown cannot race an unaccepted TCP connection.
    let mut interim = [0; 25];
    tokio::time::timeout(DEADLINE, http.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    let result = handle
        .shutdown_checked(ShutdownOptions {
            timeout: Duration::from_millis(50),
        })
        .await;
    assert!(matches!(result, Err(FrameworkError::StopIncomplete { .. })));
    assert!(!handle.health().ready);
    assert!(
        !status.cleared(),
        "in-use resources must survive StopIncomplete"
    );
    for address in addresses {
        refuses(address).await;
    }
    drop(http);
    handle.shutdown_checked(Default::default()).await.unwrap();
    assert_eq!(handle.task_metrics().tasks, 0);
    assert!(status.cleared());
    reusable(addresses).await;
}

#[tokio::test]
async fn ready_failure_releases_both_prebound_ports_without_listening() {
    let gate = Gate::new(Some(Phase::Ready));
    let (app, status) = build(configuration("127.0.0.1:0"), gate.clone());
    let start = tokio::spawn(app.start());
    notified(&gate.ready).await;
    let addresses = status.addresses();
    for address in addresses {
        refuses(address).await;
    }
    gate.release_ready.add_permits(1);
    let error = tokio::time::timeout(DEADLINE, start)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert_failure(&error, Phase::Ready);
    assert!(status.cleared());
    reusable(addresses).await;
}

#[tokio::test]
async fn later_activation_failure_compensates_both_live_ingresses() {
    let gate = Gate::new(Some(Phase::ActivateIngress));
    let (app, status) = build(configuration("127.0.0.1:0"), gate.clone());
    let start = tokio::spawn(app.start());
    notified(&gate.ready).await;
    let addresses = status.addresses();
    gate.release_ready.add_permits(1);
    notified(&gate.activate).await;
    serves(addresses).await;
    gate.release_activate.add_permits(1);
    let error = tokio::time::timeout(DEADLINE, start)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert_failure(&error, Phase::ActivateIngress);
    assert!(status.cleared());
    reusable(addresses).await;
}

#[tokio::test]
async fn occupied_port_fails_setup_without_closing_the_other_listener() {
    for pg in [false, true] {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let mut config = configuration("127.0.0.1:0");
        if pg {
            config.pgwire.listen = address.to_string();
        } else {
            config.management.listen = address.to_string();
        }
        let (app, status) = build(config, Gate::new(None));
        let error = tokio::time::timeout(DEADLINE, app.start())
            .await
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.source.phase(), Some(Phase::Setup));
        assert_eq!(
            error.source.component(),
            Some(&ComponentRef::initiator(if pg {
                "pgwire-listener"
            } else {
                "management-api"
            }))
        );
        assert!(error.cleanup.is_ok());
        assert!(!error.handle.health().ready);
        assert!(status.cleared());
        TcpStream::connect(address)
            .await
            .expect("test-owned listener remains open");
    }
}

#[tokio::test]
async fn real_listen_failure_compensates_prebound_and_activated_ingresses() {
    for index in [0, 1] {
        let gate = Gate::new(None);
        let (app, status) = build(configuration("127.0.0.1:0"), gate.clone());
        let start = tokio::spawn(app.start());
        notified(&gate.ready).await;
        let addresses = status.addresses();
        // SO_REUSEADDR allows a second prebind; only one socket can listen.
        // This injects a real OS listen error after successful setup.
        let occupied = TcpListener::bind(addresses[index]).await.unwrap();
        gate.release_ready.add_permits(1);
        let error = tokio::time::timeout(DEADLINE, start)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.source.phase(), Some(Phase::ActivateIngress));
        assert_eq!(
            error.source.component(),
            Some(&ComponentRef::initiator(if index == 0 {
                "pgwire-listener"
            } else {
                "management-api"
            }))
        );
        assert!(
            matches!(error.source.root_cause(), FrameworkError::InvalidConfig(message) if !message.is_empty())
        );
        assert!(error.cleanup.is_ok());
        assert!(!error.handle.health().ready);
        assert_eq!(error.handle.task_metrics().tasks, 0);
        assert!(status.cleared());
        refuses(addresses[1 - index]).await;
        TcpStream::connect(addresses[index])
            .await
            .expect("test-owned listener remains open");
        drop(occupied);
        reusable(addresses).await;
    }
}

#[tokio::test]
async fn cancelling_start_wait_still_drains_both_live_ingresses() {
    let gate = Gate::new(None);
    let (app, status) = build(configuration("127.0.0.1:0"), gate.clone());
    let start = tokio::spawn(app.start());
    notified(&gate.ready).await;
    let addresses = status.addresses();
    gate.release_ready.add_permits(1);
    notified(&gate.activate).await;
    serves(addresses).await;
    start.abort();
    assert!(start.await.err().unwrap().is_cancelled());
    gate.release_activate.add_permits(1);
    tokio::time::timeout(DEADLINE, async {
        while !status.cleared() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled startup compensation");
    reusable(addresses).await;
}
