//! This package imports only the public FedEngine and Modorch facades.
use modorch::{prelude::*, *};
use modorch_fedengine::{
    FedEngineBundle, FedEngineConfig, FedEngineError, FedEngineObservation, FedEngineProfile,
    FedEngineRegisteredComponents,
};
use std::{
    collections::BTreeSet,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const DEADLINE: Duration = Duration::from_secs(10);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "fedengine-external-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }
    fn config(&self) -> FedEngineConfig {
        // Prove a shared build cache did not reuse a consumer compiled elsewhere.
        assert_eq!(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .canonicalize()
                .unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        let mut config =
            FedEngineConfig::load(concat!(env!("CARGO_MANIFEST_DIR"), "/application.toml"))
                .unwrap();
        config.duckdb.database_path = self.0.join("engine.duckdb").to_string_lossy().into();
        config.duckdb.temp_directory = self.0.join("tmp").to_string_lossy().into();
        config.management.credentials_directory = self.0.join("secrets").to_string_lossy().into();
        config.users[0].password_secret_ref = "USER".into();
        config
    }
    fn empty(&self) {
        assert_eq!(std::fs::read_dir(&self.0).unwrap().count(), 0);
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("cannot clean {}: {error}", self.0.display());
        }
    }
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
fn build(
    builder: ApplicationBuilder,
    registered: &FedEngineRegisteredComponents,
    selected: ComponentSelection,
) -> Application {
    let demand = registered.resource_requirements();
    builder
        .selection(selected)
        .execution_resources(ExecutionResources {
            blocking_io_threads: demand.blocking_io_threads,
            blocking_io_capacity: demand.blocking_io_capacity,
            max_tasks_per_owner: demand.max_tasks_per_owner,
            ..Default::default()
        })
        .build()
        .unwrap()
}
async fn stopped(app: &ApplicationHandle) {
    assert!(matches!(
        app.shutdown_checked(ShutdownOptions { timeout: DEADLINE })
            .await
            .unwrap(),
        ShutdownReceipt::GracefullyStopped
    ));
    assert_eq!(app.task_metrics().tasks, 0);
}
async fn http_sources(address: SocketAddr) {
    tokio::time::timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                b"GET /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!([])
        );
    })
    .await
    .unwrap();
}

#[derive(Clone, Default)]
struct Probe {
    application: Arc<Mutex<Option<ApplicationId>>>,
    require_engine: bool,
    fail_ready: bool,
}
impl Probe {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut descriptor = ComponentDescriptor::new(ComponentRef::module("external-host-probe"));
        if self.require_engine {
            descriptor.requires = vec![
                ComponentRef::module("query-coordinator"),
                ComponentRef::module("source-manager"),
            ];
        }
        descriptor
    }
}
impl ModuleFactory for Probe {
    fn descriptor(&self) -> ComponentDescriptor {
        self.descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(self.clone()))
    }
}
#[async_trait]
impl Lifecycle for Probe {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        *self.application.lock().unwrap() = Some(ctx.event_hub.metrics().application_id);
        Ok(())
    }
    async fn ready(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        if self.fail_ready {
            return Err(FrameworkError::InvalidConfig(
                "external host ready failure".into(),
            ));
        }
        Ok(())
    }
}
impl Module for Probe {
    fn descriptor(&self) -> ComponentDescriptor {
        self.descriptor()
    }
}

#[test]
fn registration_and_build_preserve_existing_bytes_without_io_or_runtime() {
    let directory = Directory::new();
    let mut config = directory.config();
    let pg = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    config.pgwire.listen = pg.local_addr().unwrap().to_string();
    config.management.listen = http.local_addr().unwrap().to_string();
    config.pgwire.tls_required = true;
    config.pgwire.tls_cert = Some(directory.0.join("missing.crt").to_string_lossy().into());
    config.pgwire.tls_key = Some(directory.0.join("missing.key").to_string_lossy().into());
    config.users[0].password_secret_ref = format!("FE_EXTERNAL_MISSING_{}", std::process::id());
    assert!(std::env::var_os(&config.users[0].password_secret_ref).is_none());
    config.duckdb.install_extensions = true;
    config.duckdb.extension_directory = Some(
        directory
            .0
            .join("missing-extensions")
            .to_string_lossy()
            .into(),
    );
    let database = PathBuf::from(&config.duckdb.database_path);
    let bytes = b"existing state must not be opened, migrated or repaired during registration";
    std::fs::write(&database, bytes).unwrap();
    let mut builder = ApplicationBuilder::new("pure-external-consumer");
    let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
    assert!(!registered.handle().status().ready);
    let app = build(builder, &registered, selection(&registered));
    assert_eq!(app.startup_plan().len(), registered.components().count());
    assert_eq!(std::fs::read(&database).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    assert!(!directory.0.join("missing-extensions").exists());
}

#[tokio::test]
async fn unselected_bundle_keeps_host_running_without_creating_engine_resources() {
    let directory = Directory::new();
    let pg = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = directory.config();
    config.pgwire.listen = pg.local_addr().unwrap().to_string();
    config.management.listen = http.local_addr().unwrap().to_string();
    let probe = Probe::default();
    let mut selected = ComponentSelection::new();
    selected.enable_module("external-host-probe");
    let mut builder = ApplicationBuilder::new("host-only")
        .selection(selected)
        .task_limits(73, 5)
        .execution_resources(ExecutionResources {
            blocking_io_threads: 1,
            blocking_io_capacity: 7,
            ..Default::default()
        });
    builder.register_module(probe.clone()).unwrap();
    let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
    let app = builder.build().unwrap();
    assert_eq!(
        app.startup_plan(),
        &[ComponentRef::module("external-host-probe")]
    );
    let app = app.start().await.unwrap();
    registered.observe(&app).unwrap();
    assert!(app.health().ready);
    assert_eq!(*probe.application.lock().unwrap(), Some(app.id()));
    assert_eq!(app.task_metrics().task_capacity, 73);
    assert_eq!(app.task_metrics().blocking_io.capacity, 7);
    assert_eq!(
        registered.handle().status().observation,
        FedEngineObservation::NotSelected
    );
    assert!(registered.handle().pgwire_address().is_none());
    assert!(registered.handle().management_address().is_none());
    directory.empty();
    stopped(&app).await;
    directory.empty();
    std::net::TcpStream::connect(pg.local_addr().unwrap()).unwrap();
    std::net::TcpStream::connect(http.local_addr().unwrap()).unwrap();
}

#[test]
fn duplicate_ids_and_unselected_required_dependency_fail_before_start() {
    let directory = Directory::new();
    let mut builder = ApplicationBuilder::new("duplicate");
    FedEngineBundle::new(directory.config())
        .register(&mut builder)
        .unwrap();
    assert!(matches!(
        FedEngineBundle::new(directory.config()).register(&mut builder),
        Err(FedEngineError::Framework(FrameworkError::DuplicateComponent(component)))
            if component == ComponentRef::initiator("duckdb-instance")
    ));
    drop(builder); // Registration is not transactional: discard failures.
    let mut builder = ApplicationBuilder::new("missing-dependency");
    let registered = FedEngineBundle::new(directory.config())
        .register(&mut builder)
        .unwrap();
    let mut selected = ComponentSelection::new();
    for id in registered
        .initiator_ids()
        .filter(|id| *id != "duckdb-instance")
    {
        selected.enable_initiator(id);
    }
    for id in registered.module_ids() {
        selected.enable_module(id);
    }
    assert!(matches!(
        builder.selection(selected).build(),
        Err(FrameworkError::DisabledDependency { dependency, .. })
            if dependency == ComponentRef::initiator("duckdb-instance")
    ));
    directory.empty();
}

#[tokio::test]
async fn profiles_serve_pg_queries_and_http_in_the_same_application_as_the_host() {
    for profile in [
        FedEngineProfile::PgServer {
            management_api: true,
        },
        FedEngineProfile::PgServer {
            management_api: false,
        },
        FedEngineProfile::ManagedServer { pgwire: false },
    ] {
        let directory = Directory::new();
        let probe = Probe {
            require_engine: true,
            ..Default::default()
        };
        let mut builder = ApplicationBuilder::new("external-profile-host");
        builder.register_module(probe.clone()).unwrap();
        let registered = FedEngineBundle::new(directory.config())
            .profile(profile)
            .register(&mut builder)
            .unwrap();
        let observer = registered.handle().clone();
        let mut selected = selection(&registered);
        selected.enable_module("external-host-probe");
        let app = build(builder, &registered, selected).start().await.unwrap();
        registered.observe(&app).unwrap();
        assert!(observer.status().ready);
        assert_eq!(observer.status().application_id, Some(app.id()));
        assert_eq!(*probe.application.lock().unwrap(), Some(app.id()));
        let mut expected: BTreeSet<_> = registered.components().cloned().collect();
        expected.insert(ComponentRef::module("external-host-probe"));
        assert_eq!(
            app.health()
                .components
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected
        );
        let pg = observer.pgwire_address();
        let http = observer.management_address();
        assert_eq!(
            pg.is_some(),
            profile != FedEngineProfile::ManagedServer { pgwire: false }
        );
        assert_eq!(
            http.is_some(),
            profile
                != FedEngineProfile::PgServer {
                    management_api: false
                }
        );
        if let Some(address) = pg {
            let (client, connection) = tokio_postgres::Config::new()
                .host("127.0.0.1")
                .port(address.port())
                .user("fedengine")
                .password(std::env::var("USER").unwrap())
                .dbname("hostdb")
                .connect(tokio_postgres::NoTls)
                .await
                .unwrap();
            let connection = tokio::spawn(connection);
            let row = client
                .query_one("SELECT $1::integer + 1 AS answer", &[&41i32])
                .await
                .unwrap();
            assert_eq!(row.get::<_, i32>(0), 42);
            assert!(observer.metrics().admitted > 0);
            drop(client);
            connection.await.unwrap().unwrap();
        }
        if let Some(address) = http {
            http_sources(address).await;
        }
        stopped(&app).await;
        assert!(!observer.status().ready);
        assert!(observer.pgwire_address().is_none());
        assert!(observer.management_address().is_none());
        for address in [pg, http].into_iter().flatten() {
            TcpListener::bind(address).expect("external application released its ingress");
        }
    }
}

#[tokio::test]
async fn host_ready_failure_preserves_startup_error_and_compensates_engine() {
    let directory = Directory::new();
    let mut config = directory.config();
    let pg = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let addresses = [pg.local_addr().unwrap(), http.local_addr().unwrap()];
    config.pgwire.listen = addresses[0].to_string();
    config.management.listen = addresses[1].to_string();
    let mut builder = ApplicationBuilder::new("failing-external-host");
    builder
        .register_module(Probe {
            require_engine: true,
            fail_ready: true,
            ..Default::default()
        })
        .unwrap();
    let registered = FedEngineBundle::new(config.clone())
        .register(&mut builder)
        .unwrap();
    let mut selected = selection(&registered);
    selected.enable_module("external-host-probe");
    let app = build(builder, &registered, selected);
    drop(pg);
    drop(http);
    let error = app.start().await.err().unwrap();
    assert_eq!(error.source.phase(), Some(Phase::Ready));
    assert!(
        error
            .source
            .to_string()
            .contains("external host ready failure")
    );
    assert!(matches!(
        error.cleanup,
        Ok(ShutdownReceipt::GracefullyStopped)
    ));
    assert_eq!(error.handle.task_metrics().tasks, 0);
    registered.observe(&error.handle).unwrap();
    assert_eq!(
        registered.handle().status().application_state,
        Some(ApplicationState::Stopped)
    );
    assert!(!registered.handle().status().ready);
    assert!(registered.handle().pgwire_address().is_none());
    assert!(registered.handle().management_address().is_none());
    for address in addresses {
        TcpListener::bind(address).expect("failure compensated prebound port");
    }
    drop(error);
    // The host retries with a new builder, retaining the same current database.
    let mut builder = ApplicationBuilder::new("retry-external-host");
    let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
    let app = build(builder, &registered, selection(&registered))
        .start()
        .await
        .unwrap();
    registered.observe(&app).unwrap();
    http_sources(registered.handle().management_address().unwrap()).await;
    stopped(&app).await;
}

#[tokio::test]
async fn slow_http_request_retains_work_until_the_external_host_retries_shutdown() {
    let directory = Directory::new();
    let mut builder = ApplicationBuilder::new("external-slow-http");
    let registered = FedEngineBundle::new(directory.config())
        .profile(FedEngineProfile::ManagedServer { pgwire: false })
        .register(&mut builder)
        .unwrap();
    let app = build(builder, &registered, selection(&registered))
        .start()
        .await
        .unwrap();
    registered.observe(&app).unwrap();
    let address = registered.handle().management_address().unwrap();
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"POST /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 1000\r\n\r\n").await.unwrap();
    let mut interim = [0; 25];
    tokio::time::timeout(DEADLINE, stream.read_exact(&mut interim))
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
    assert!(app.task_metrics().tasks > 0);
    assert!(registered.handle().management_address().is_none());
    assert_eq!(
        registered.handle().status().application_state,
        Some(ApplicationState::StopIncomplete)
    );
    drop(stream);
    stopped(&app).await;
    TcpListener::bind(address).expect("HTTP released after actual exit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn facade_clients_stream_parameters_and_require_explicit_management_grants() {
    use modorch_fedengine::{
        FedEngineCancellation, FedEngineDataType, FedEngineManagementGrant, FedEnginePrincipal,
        FedEngineResultChunk, FedEngineValue,
    };
    let directory = Directory::new();
    let mut builder = ApplicationBuilder::new("external-client-host");
    let registered = FedEngineBundle::new(directory.config())
        .profile(FedEngineProfile::ManagedServer { pgwire: false })
        .register(&mut builder)
        .unwrap();
    let app = build(builder, &registered, selection(&registered))
        .start()
        .await
        .unwrap();
    let principal =
        FedEnginePrincipal::authenticated("fedengine", "external-host/session").unwrap();
    let query = registered
        .query_client(&app, principal.clone(), Default::default())
        .unwrap();
    let cancel = FedEngineCancellation::new();
    let prepared = query
        .prepare(
            "hostdb",
            "SELECT $1::INTEGER AS value",
            &[Some(FedEngineDataType::Int4)],
            &cancel,
        )
        .await
        .unwrap();
    assert!(!cancel.is_cancelled());
    let mut stream = query
        .execute(&prepared, vec![FedEngineValue::I32(42)], &cancel)
        .await
        .unwrap();
    let mut seen = false;
    let mut completed = false;
    while let Some(chunk) = stream.next().await.unwrap() {
        match chunk {
            FedEngineResultChunk::Rows(rows) => {
                assert!(matches!(rows[0][0], FedEngineValue::I32(42)));
                seen = true;
            }
            FedEngineResultChunk::Complete(_) => completed = true,
        }
    }
    assert!(seen && completed);
    let denied = registered
        .management_client(
            &app,
            principal.clone(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        denied.database("hostdb").await.err().unwrap().sqlstate,
        "42501"
    );
    let scoped = registered
        .management_client(
            &app,
            principal,
            Default::default(),
            FedEngineManagementGrant::default().database("hostdb"),
        )
        .unwrap();
    assert_eq!(
        scoped.database("hostdb").await.unwrap().config.name,
        "hostdb"
    );
    stopped(&app).await;
    assert_eq!(
        query
            .prepare("hostdb", "SELECT 1", &[], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "55000"
    );
}
