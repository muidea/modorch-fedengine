//! Public Facade registration and real host-owned application contracts.
mod common;
use common::TestDirectory;
use modorch::{prelude::*, *};
use modorch_fedengine::{
    FedEngineBundle, FedEngineConfig, FedEngineError, FedEngineObservation, FedEngineProfile,
    FedEngineRegisteredComponents,
};
use std::{
    collections::BTreeSet,
    net::{SocketAddr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Semaphore,
};

fn configuration(directory: &TestDirectory) -> FedEngineConfig {
    let mut config = FedEngineConfig::load("config/application.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.management.listen = "127.0.0.1:0".into();
    config.users[0].password_secret_ref = format!("FE_BUNDLE_MISSING_{}", std::process::id());
    assert!(std::env::var_os(&config.users[0].password_secret_ref).is_none());
    config.duckdb.database_path = directory
        .path()
        .join("engine/management.duckdb")
        .to_string_lossy()
        .into();
    config.duckdb.temp_directory = directory.path().join("tmp").to_string_lossy().into();
    config.duckdb.max_workers = 1;
    config.duckdb.threads = 1;
    config.management.credentials_directory =
        directory.path().join("secrets").to_string_lossy().into();
    config
}
fn selection(registered: &FedEngineRegisteredComponents) -> ComponentSelection {
    let mut selected = ComponentSelection::new();
    for id in registered.initiator_ids() {
        selected.enable_initiator(id);
    }
    for id in registered.module_ids() {
        selected.enable_module(id);
    }
    selected
}
fn empty(directory: &TestDirectory) {
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
fn identities(registered: &FedEngineRegisteredComponents) -> BTreeSet<ComponentRef> {
    registered.components().cloned().collect()
}

#[test]
fn registration_and_build_do_not_touch_files_ports_or_secrets() {
    // Synchronous test: registering/building does not require a Tokio runtime.
    for management in [false, true] {
        let directory = TestDirectory::new();
        let mut config = configuration(&directory);
        config.management.enabled = management;
        config.pgwire.tls_required = true;
        config.pgwire.tls_cert = Some(
            directory
                .path()
                .join("missing.crt")
                .to_string_lossy()
                .into(),
        );
        config.pgwire.tls_key = Some(
            directory
                .path()
                .join("missing.key")
                .to_string_lossy()
                .into(),
        );
        config.duckdb.install_extensions = true;
        config.duckdb.extension_directory =
            Some(directory.path().join("extensions").to_string_lossy().into());
        let pg = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        config.pgwire.listen = pg.local_addr().unwrap().to_string();
        config.management.listen = http.local_addr().unwrap().to_string();
        let mut builder = ApplicationBuilder::new("pure-registration");
        let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
        assert_eq!(
            registered.initiator_ids().count(),
            if management { 5 } else { 4 }
        );
        assert_eq!(registered.module_ids().count(), 4);
        assert_eq!(
            registered.components().count(),
            identities(&registered).len()
        );
        assert_eq!(
            registered.initiator_ids().any(|id| id == "management-api"),
            management
        );
        let app = builder.selection(selection(&registered)).build().unwrap();
        assert_eq!(
            app.startup_plan().iter().cloned().collect::<BTreeSet<_>>(),
            identities(&registered)
        );
        empty(&directory);
        std::net::TcpStream::connect(pg.local_addr().unwrap()).unwrap();
        std::net::TcpStream::connect(http.local_addr().unwrap()).unwrap();
    }
}

#[test]
fn registration_preserves_existing_state_without_opening_or_repairing_it() {
    let directory = TestDirectory::new();
    let mut config = configuration(&directory);
    let database = directory.path().join("existing.duckdb");
    let sentinel = b"existing state: registration must neither open nor rewrite this file";
    std::fs::write(&database, sentinel).unwrap();
    config.duckdb.database_path = database.to_string_lossy().into();
    let mut builder = ApplicationBuilder::new("existing-state");
    let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
    builder.selection(selection(&registered)).build().unwrap();
    assert_eq!(std::fs::read(&database).unwrap(), sentinel);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn invalid_configuration_and_overflow_fail_before_registering_factories() {
    let directory = TestDirectory::new();
    let valid = configuration(&directory);
    let mut invalid = valid.clone();
    invalid.duckdb.max_workers = 0;
    let mut overflow = valid.clone();
    overflow.management.enabled = false;
    overflow.management.queue_capacity = usize::MAX;
    for config in [invalid, overflow] {
        let mut builder = ApplicationBuilder::new("prevalidated");
        assert!(matches!(
            FedEngineBundle::new(config).register(&mut builder),
            Err(FedEngineError::Framework(FrameworkError::InvalidConfig(_)))
        ));
        // A valid registration would collide if the failed validation mutated the builder.
        FedEngineBundle::new(valid.clone())
            .register(&mut builder)
            .unwrap();
        assert!(builder.build().unwrap().startup_plan().is_empty());
        empty(&directory);
    }
}

struct Collision;
impl ModuleFactory for Collision {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::module("source-manager"))
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        panic!("collision factory must not be instantiated")
    }
}

#[test]
fn duplicate_bundle_and_late_collision_report_the_actual_identity() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("duplicate-bundle");
    FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    assert!(
        matches!(FedEngineBundle::new(configuration(&directory)).register(&mut builder), Err(FedEngineError::Framework(FrameworkError::DuplicateComponent(component))) if component == ComponentRef::initiator("duckdb-instance"))
    );
    drop(builder); // No rollback API: discard the composition on registration errors.
    let mut builder = ApplicationBuilder::new("late-collision");
    builder.register_module(Collision).unwrap();
    assert!(
        matches!(FedEngineBundle::new(configuration(&directory)).register(&mut builder), Err(FedEngineError::Framework(FrameworkError::DuplicateComponent(component))) if component == ComponentRef::module("source-manager"))
    );
    drop(builder);
    empty(&directory);
}

#[test]
fn host_must_explicitly_select_required_dependencies() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("incomplete-selection");
    let registered = FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    let mut selected = ComponentSelection::new();
    for component in registered.components() {
        if component == &ComponentRef::initiator("duckdb-instance") {
            continue;
        }
        match component.kind {
            ComponentKind::Initiator => {
                selected.enable_initiator(&component.id);
            }
            ComponentKind::Module => {
                selected.enable_module(&component.id);
            }
        }
    }
    assert!(
        matches!(builder.selection(selected).build(), Err(FrameworkError::DisabledDependency { dependency, .. }) if dependency == ComponentRef::initiator("duckdb-instance"))
    );
    empty(&directory);
}

#[derive(Clone)]
struct HostProbe(Arc<AtomicBool>);
fn host_descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::module("host-probe"))
}
impl ModuleFactory for HostProbe {
    fn descriptor(&self) -> ComponentDescriptor {
        host_descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(self.clone()))
    }
}
#[async_trait]
impl Lifecycle for HostProbe {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let gate = Arc::new(Semaphore::new(0));
        let wait = gate.clone();
        let first = ctx.tasks.spawn(move |_| async move {
            wait.acquire().await.unwrap().forget();
            Ok(())
        })?;
        let second = ctx.tasks.spawn(|_| async { Ok(()) });
        assert!(
            matches!(second, Err(FrameworkError::Backpressure(_))),
            "host owner quota must survive registration"
        );
        gate.add_permits(1);
        first.wait_finished().await?;
        ctx.tasks
            .spawn_blocking_io(|_| Ok(()))?
            .wait_finished()
            .await?;
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl Module for HostProbe {
    fn descriptor(&self) -> ComponentDescriptor {
        host_descriptor()
    }
}
struct HostExecutor(Arc<AtomicUsize>);
impl BlockingExecutor for HostExecutor {
    fn execute(&self, job: BlockingJob) -> FrameworkResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::task::spawn_blocking(job);
        Ok(())
    }
}

#[tokio::test]
async fn unselected_engine_stays_uninstantiated_and_preserves_host_selection_and_budgets() {
    let directory = TestDirectory::new();
    let checked = Arc::new(AtomicBool::new(false));
    let executed = Arc::new(AtomicUsize::new(0));
    let resources = ExecutionResources {
        cpu_threads: 1,
        cpu_capacity: 3,
        blocking_io_threads: 1,
        blocking_io_capacity: 5,
        max_tasks_per_owner: 1,
        blocking_io_executor: Some(Arc::new(HostExecutor(executed.clone()))),
        ..Default::default()
    };
    let mut selected = ComponentSelection::new();
    selected.enable_module("host-probe");
    let mut builder = ApplicationBuilder::new("host-owned")
        .selection(selected)
        .task_limits(7, 2)
        .execution_resources(resources)
        .message_limits(HubLimits {
            max_topic_bytes: 8,
            ..Default::default()
        });
    builder.register_module(HostProbe(checked.clone())).unwrap();
    let registered = FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    let app = builder.build().unwrap();
    assert_eq!(app.startup_plan(), &[ComponentRef::module("host-probe")]);
    let handle = app.start().await.unwrap();
    registered.observe(&handle).unwrap();
    assert_eq!(
        registered.handle().status().observation,
        FedEngineObservation::NotSelected
    );
    assert!(!registered.handle().status().ready);
    assert!(registered.handle().pgwire_address().is_none());
    assert!(registered.handle().management_address().is_none());
    assert!(checked.load(Ordering::SeqCst));
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "host executor must survive registration"
    );
    assert!(handle.health().ready);
    assert_eq!(handle.health().components.len(), 1);
    let tasks = handle.task_metrics();
    assert_eq!(tasks.task_capacity, 7);
    assert_eq!(tasks.cpu.capacity, 3);
    assert_eq!(tasks.blocking_io.capacity, 5);
    assert!(matches!(
        handle.event_hub().external().request::<_, ()>(
            "exceeds-host-topic-budget",
            Destination::Broadcast,
            (),
            Default::default()
        ),
        Err(FrameworkError::ResourceLimit(_))
    ));
    empty(&directory);
    handle.shutdown_checked(Default::default()).await.unwrap();
    empty(&directory);
}

async fn http_sources(address: SocketAddr) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!([])
    );
}

#[tokio::test]
async fn returned_components_start_real_pgwire_and_management_in_one_host_application() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for management in [false, true] {
            let directory = TestDirectory::new();
            let mut config = configuration(&directory);
            config.management.enabled = management;
            config.users[0].password_secret_ref = "USER".into();
            if management {
                config.virtual_databases = serde_json::from_value(serde_json::json!([
                    {"name":"hostdb", "owner":config.users[0].name}
                ]))
                .unwrap();
            }
            let pg = TcpListener::bind("127.0.0.1:0").unwrap();
            let http = TcpListener::bind("127.0.0.1:0").unwrap();
            let pg_address = pg.local_addr().unwrap();
            let http_address = http.local_addr().unwrap();
            config.pgwire.listen = pg_address.to_string();
            config.management.listen = http_address.to_string();
            let database = config.duckdb.database_path.clone();
            let mut builder = ApplicationBuilder::new("existing-host");
            let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
            let observer = registered.handle().clone();
            assert_eq!(
                observer.status().observation,
                FedEngineObservation::Unobserved
            );
            let demand = registered.resource_requirements();
            let app = builder
                .selection(selection(&registered))
                .message_limits(HubLimits {
                    max_message_bytes: Some(1024 * 1024),
                    max_in_flight_bytes: Some(16 * 1024 * 1024),
                    max_in_flight_bytes_per_target: Some(8 * 1024 * 1024),
                    max_waiting_bytes: Some(8 * 1024 * 1024),
                    ..Default::default()
                })
                .execution_resources(ExecutionResources {
                    blocking_io_threads: demand.blocking_io_threads,
                    blocking_io_capacity: demand.blocking_io_capacity,
                    max_tasks_per_owner: demand.max_tasks_per_owner,
                    ..Default::default()
                })
                .build()
                .unwrap();
            empty(&directory);
            drop(pg);
            // With HTTP disabled its configured port stays occupied by the host.
            let occupied = if management {
                drop(http);
                None
            } else {
                Some(http)
            };
            let handle = app.start().await.unwrap();
            assert!(
                !observer.status().ready,
                "host must associate actual application readiness"
            );
            registered.observe(&handle).unwrap();
            registered.observe(&handle).unwrap(); // Idempotent for this application.
            assert!(observer.status().ready);
            assert_eq!(observer.status().application_id, Some(handle.id()));
            assert_eq!(observer.pgwire_address(), Some(pg_address));
            assert_eq!(
                observer.management_address(),
                management.then_some(http_address)
            );
            assert!(
                observer
                    .status()
                    .components
                    .values()
                    .all(|c| c.generation == 0)
            );
            assert!(handle.health().ready);
            assert_eq!(
                handle
                    .health()
                    .components
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                identities(&registered)
            );
            assert!(std::path::Path::new(&database).is_file());
            let mut pg = TcpStream::connect(pg_address).await.unwrap();
            pg.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).await.unwrap();
            assert_eq!(pg.read_u8().await.unwrap(), b'N');
            drop(pg);
            if management {
                http_sources(http_address).await;
                let (client, connection) = tokio_postgres::Config::new()
                    .host("127.0.0.1")
                    .port(pg_address.port())
                    .user("fedengine")
                    .password(std::env::var("USER").unwrap())
                    .dbname("hostdb")
                    .connect(tokio_postgres::NoTls)
                    .await
                    .unwrap();
                let connection = tokio::spawn(connection);
                let row = client
                    .query_one("SELECT $1::integer AS answer", &[&42i32])
                    .await
                    .unwrap();
                assert_eq!(row.get::<_, i32>(0), 42);
                assert!(observer.metrics().admitted > 0);
                drop(client);
                connection.await.unwrap().unwrap();
            }
            handle.shutdown_checked(Default::default()).await.unwrap();
            assert_eq!(
                observer.status().application_state,
                Some(ApplicationState::Stopped)
            );
            assert!(!observer.status().ready);
            assert!(observer.pgwire_address().is_none());
            assert!(observer.management_address().is_none());
            assert_eq!(handle.task_metrics().tasks, 0);
            for address in [pg_address, http_address] {
                if !management && address == http_address {
                    continue;
                }
                assert!(TcpStream::connect(address).await.is_err());
                TcpListener::bind(address).expect("bundle port released");
            }
            if let Some(listener) = occupied {
                TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .expect("real host startup, requests and checked drain");
}

#[test]
fn unsupported_profiles_and_invalid_core_modes_fail_before_registration() {
    let directory = TestDirectory::new();
    let config = configuration(&directory);
    let mut builder = ApplicationBuilder::new("unsupported");
    assert!(matches!(
        FedEngineBundle::new(config.clone())
            .profile(FedEngineProfile::EmbeddedCore)
            .register(&mut builder),
        Err(FedEngineError::UnsupportedProfile(
            FedEngineProfile::EmbeddedCore
        ))
    ));
    FedEngineBundle::new(config.clone())
        .register(&mut builder)
        .unwrap();
    assert!(builder.build().unwrap().startup_plan().is_empty());
    for profile in [
        FedEngineProfile::ManagedServer { pgwire: false },
        FedEngineProfile::PgServer {
            management_api: true,
        },
    ] {
        let mut disabled = config.clone();
        disabled.management.enabled = false;
        let mut builder = ApplicationBuilder::new("invalid-managed-mode");
        assert!(matches!(
            FedEngineBundle::new(disabled)
                .profile(profile)
                .register(&mut builder),
            Err(FedEngineError::Framework(FrameworkError::InvalidConfig(_)))
        ));
        FedEngineBundle::new(config.clone())
            .register(&mut builder)
            .unwrap();
    }
    let mut no_storage = config.clone();
    no_storage.duckdb.database_path = ":memory:".into();
    assert!(
        FedEngineBundle::new(no_storage)
            .profile(FedEngineProfile::PgServer {
                management_api: false
            })
            .register(&mut ApplicationBuilder::new("still-managed"))
            .is_err()
    );
    empty(&directory);
}

async fn http_json(address: SocketAddr, path: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{response}");
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn managed_http_only_ignores_pg_transport_and_auth_but_keeps_virtual_databases() {
    for invalid_address in [false, true] {
        let directory = TestDirectory::new();
        let mut config = configuration(&directory);
        let pg = TcpListener::bind("127.0.0.1:0").unwrap();
        config.pgwire.listen = if invalid_address {
            "not-an-address".into()
        } else {
            pg.local_addr().unwrap().to_string()
        };
        config.pgwire.max_connections = 0;
        config.pgwire.startup_timeout_ms = 0;
        config.pgwire.output_timeout_ms = 0;
        config.pgwire.max_portals = 0;
        config.pgwire.max_statements = 0;
        config.pgwire.tls_required = true;
        config.pgwire.tls_cert = Some("not-a-real-certificate".into());
        config.pgwire.tls_key = None;
        config.users[0].password_secret_ref.clear();
        config.virtual_databases = serde_json::from_value(serde_json::json!([
            {"name":"http_only", "owner":config.users[0].name}
        ]))
        .unwrap();
        assert!(
            config.validate().is_err(),
            "default standalone validation still requires PGWire"
        );
        let mut builder = ApplicationBuilder::new("http-only");
        let profile = FedEngineProfile::ManagedServer { pgwire: false };
        let registered = FedEngineBundle::new(config)
            .profile(profile)
            .register(&mut builder)
            .unwrap();
        assert_eq!(registered.profile(), profile);
        assert!(!registered.initiator_ids().any(|id| id == "pgwire-listener"));
        assert!(registered.module_ids().any(|id| id == "source-manager"));
        let observer = registered.handle().clone();
        let handle = builder
            .selection(selection(&registered))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();
        registered.observe(&handle).unwrap();
        assert!(observer.status().ready);
        assert!(observer.pgwire_address().is_none());
        let http = observer.management_address().unwrap();
        assert_eq!(
            http_json(http, "/api/v1/sources").await,
            serde_json::json!([])
        );
        let databases = http_json(http, "/api/v1/virtual-databases").await;
        assert!(databases.to_string().contains("http_only"), "{databases}");
        handle.shutdown_checked(Default::default()).await.unwrap();
        assert!(observer.management_address().is_none());
        TcpStream::connect(pg.local_addr().unwrap())
            .await
            .expect("disabled PG ingress must not touch host socket");
    }
}

#[tokio::test]
async fn pg_server_without_http_preserves_managed_virtual_database_queries() {
    let directory = TestDirectory::new();
    let mut config = configuration(&directory);
    config.management.listen = "disabled HTTP has no address".into();
    config.users[0].password_secret_ref = "USER".into();
    config.virtual_databases = serde_json::from_value(serde_json::json!([
        {"name":"managed_pg", "owner":config.users[0].name}
    ]))
    .unwrap();
    let mut builder = ApplicationBuilder::new("managed-pg-only");
    let registered = FedEngineBundle::new(config)
        .profile(FedEngineProfile::PgServer {
            management_api: false,
        })
        .register(&mut builder)
        .unwrap();
    assert!(!registered.initiator_ids().any(|id| id == "management-api"));
    let handle = builder
        .selection(selection(&registered))
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    registered.observe(&handle).unwrap();
    let observer = registered.handle();
    assert!(observer.status().ready);
    assert!(observer.management_address().is_none());
    let pg = observer.pgwire_address().unwrap();
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(pg.port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("managed_pg")
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    assert_eq!(
        client
            .query_one("SELECT $1::integer", &[&17i32])
            .await
            .unwrap()
            .get::<_, i32>(0),
        17
    );
    drop(client);
    connection.await.unwrap().unwrap();
    assert!(observer.metrics().completed > 0);
    handle.shutdown_checked(Default::default()).await.unwrap();
    assert!(!observer.status().ready);
    assert!(observer.pgwire_address().is_none());
}

#[tokio::test]
async fn failed_startup_observation_keeps_failure_and_hides_prebound_addresses() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("missing-secret");
    let registered = FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    let observer = registered.handle().clone();
    let error = builder
        .selection(selection(&registered))
        .build()
        .unwrap()
        .start()
        .await
        .err()
        .unwrap();
    assert_eq!(error.source.phase(), Some(Phase::Setup));
    assert!(error.cleanup.is_ok());
    registered.observe(&error.handle).unwrap();
    let status = observer.status();
    assert!(!status.ready);
    assert_eq!(status.application_state, Some(ApplicationState::Stopped));
    assert!(
        status.components[&ComponentRef::initiator("pgwire-listener")]
            .last_failure
            .is_some()
    );
    assert!(observer.pgwire_address().is_none());
    assert!(observer.management_address().is_none());
    assert_eq!(error.handle.task_metrics().tasks, 0);
}

#[tokio::test]
async fn incomplete_shutdown_hides_addresses_before_retained_http_work_exits() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("slow-request");
    let registered = FedEngineBundle::new(configuration(&directory))
        .profile(FedEngineProfile::ManagedServer { pgwire: false })
        .register(&mut builder)
        .unwrap();
    let app = builder
        .selection(selection(&registered))
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    registered.observe(&app).unwrap();
    let observer = registered.handle().clone();
    let address = observer.management_address().unwrap();
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"POST /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 1000\r\n\r\n").await.unwrap();
    let mut interim = [0; 25];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert!(matches!(
        app.shutdown_checked(ShutdownOptions {
            timeout: Duration::from_millis(50)
        })
        .await,
        Err(FrameworkError::StopIncomplete { .. })
    ));
    let status = observer.status();
    assert!(!status.ready);
    assert_eq!(
        status.application_state,
        Some(ApplicationState::StopIncomplete)
    );
    assert!(observer.management_address().is_none());
    assert!(
        app.task_metrics().tasks > 0,
        "an observer must not release retained work"
    );
    drop(stream);
    app.shutdown_checked(Default::default()).await.unwrap();
    assert_eq!(
        observer.status().application_state,
        Some(ApplicationState::Stopped)
    );
    assert_eq!(app.task_metrics().tasks, 0);
}

#[tokio::test]
async fn observations_reject_other_applications_and_remain_isolated() {
    let first_dir = TestDirectory::new();
    let second_dir = TestDirectory::new();
    let mut first_builder = ApplicationBuilder::new("first-engine");
    let mut second_builder = ApplicationBuilder::new("second-engine");
    let profile = FedEngineProfile::ManagedServer { pgwire: false };
    let first = FedEngineBundle::new(configuration(&first_dir))
        .profile(profile)
        .register(&mut first_builder)
        .unwrap();
    let second = FedEngineBundle::new(configuration(&second_dir))
        .profile(profile)
        .register(&mut second_builder)
        .unwrap();
    let first_app = first_builder
        .selection(selection(&first))
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    let second_app = second_builder
        .selection(selection(&second))
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(first.observe(&second_app).is_err());
    assert_eq!(
        first.handle().status().observation,
        FedEngineObservation::Unobserved
    );
    first.observe(&first_app).unwrap();
    second.observe(&second_app).unwrap();
    assert!(first.observe(&second_app).is_err());
    assert_ne!(
        first.handle().status().application_id,
        second.handle().status().application_id
    );
    first_app
        .shutdown_checked(Default::default())
        .await
        .unwrap();
    assert!(!first.handle().status().ready);
    assert!(second.handle().status().ready);
    http_sources(second.handle().management_address().unwrap()).await;
    second_app
        .shutdown_checked(Default::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn partially_selected_profile_never_claims_full_readiness() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("partial-profile");
    let registered = FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    let mut selected = ComponentSelection::new();
    selected.enable_initiator("duckdb-instance");
    let app = builder
        .selection(selected)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    registered.observe(&app).unwrap();
    assert!(app.health().ready);
    let status = registered.handle().status();
    assert_eq!(status.observation, FedEngineObservation::PartialSelection);
    assert_eq!(status.components.len(), 1);
    assert!(!status.ready);
    assert!(status.pgwire_address.is_none());
    assert!(status.management_address.is_none());
    app.shutdown_checked(Default::default()).await.unwrap();
}

struct EarlyFailure;
fn early_descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::initiator("a-host-failure"))
}
impl InitiatorFactory for EarlyFailure {
    fn descriptor(&self) -> ComponentDescriptor {
        early_descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(EarlyFailure))
    }
}
#[async_trait]
impl Lifecycle for EarlyFailure {
    async fn setup(&mut self, _: &SetupContext) -> FrameworkResult<()> {
        Err(FrameworkError::InvalidConfig(
            "host failed before engine setup".into(),
        ))
    }
}
impl Initiator for EarlyFailure {
    fn descriptor(&self) -> ComponentDescriptor {
        early_descriptor()
    }
}

#[tokio::test]
async fn failure_before_engine_setup_retains_unknown_identity_without_false_readiness() {
    let directory = TestDirectory::new();
    let mut builder = ApplicationBuilder::new("early-host-failure");
    builder.register_initiator(EarlyFailure).unwrap();
    let registered = FedEngineBundle::new(configuration(&directory))
        .register(&mut builder)
        .unwrap();
    let mut selected = selection(&registered);
    selected.enable_initiator("a-host-failure");
    let error = builder
        .selection(selected)
        .build()
        .unwrap()
        .start()
        .await
        .err()
        .unwrap();
    assert!(error.cleanup.is_ok());
    assert_eq!(
        error.source.component(),
        Some(&ComponentRef::initiator("a-host-failure"))
    );
    assert!(registered.observe(&error.handle).is_err());
    assert_eq!(
        registered.handle().status().observation,
        FedEngineObservation::Unobserved
    );
    assert!(!registered.handle().status().ready);
    assert!(registered.handle().pgwire_address().is_none());
    assert!(registered.handle().management_address().is_none());
    empty(&directory);
}
