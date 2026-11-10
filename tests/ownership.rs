//! Real remote I/O and lifecycle boundaries. Only use the disposable PG fixture.
mod common;
#[path = "common/wire.rs"]
mod wire;

use common::TestDirectory;
use modorch::{prelude::*, *};
use modorch_fedengine::{FedEngineBundle, FedEngineRegisteredComponents};
use modorch_fedengine_contract::{EngineMetrics, ExecutionService, MetricsSnapshot};
use modorch_fedengine_core::{Config, DataSourceId};
use modorch_fedengine_datasource::DataSourceRegistry;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_postgres::{Client, NoTls};

const WAIT: Duration = Duration::from_secs(15);
const LOCK_KEY: i64 = 8_008_008;

#[derive(Clone)]
struct Capabilities {
    registry: ServiceHandle<DataSourceRegistry>,
    execution: ServiceHandle<ExecutionService>,
    tasks: TaskScheduler,
}
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<Capabilities>>>);
impl Probe {
    fn descriptor() -> ComponentDescriptor {
        let mut d = ComponentDescriptor::new(ComponentRef::module("ownership-probe"));
        d.requires = vec![
            ComponentRef::initiator("datasource-registry"),
            ComponentRef::initiator("duckdb-runtime"),
        ];
        d
    }
    fn latest(&self) -> Capabilities {
        self.0.lock().unwrap().last().unwrap().clone()
    }
}
impl ModuleFactory for Probe {
    fn descriptor(&self) -> ComponentDescriptor {
        Self::descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(self.clone()))
    }
}
#[async_trait]
impl Lifecycle for Probe {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.0.lock().unwrap().push(Capabilities {
            registry: ctx
                .services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
            execution: ctx
                .services
                .get(&ComponentRef::initiator("duckdb-runtime"), "execution")?,
            tasks: ctx.tasks.clone(),
        });
        Ok(())
    }
}
impl Module for Probe {
    fn descriptor(&self) -> ComponentDescriptor {
        Self::descriptor()
    }
}

// Test-only policy injection around the real instance factory. The public
// Bundle keeps its existing FailFast policy; this fixture verifies the actual
// database dependency closure if an owner is explicitly configured to Restart.
struct RestartInstance {
    inner: modorch_fedengine_duckdb::DuckdbInstanceFactory,
    owners: Arc<Mutex<Vec<TaskScheduler>>>,
}
struct RestartInstanceLifecycle {
    inner: Box<dyn Initiator>,
    owners: Arc<Mutex<Vec<TaskScheduler>>>,
}
fn restart_descriptor(mut d: ComponentDescriptor) -> ComponentDescriptor {
    d.failure = FailurePolicy::Restart {
        max_attempts: 1,
        backoff: Duration::ZERO,
    };
    d
}
impl InitiatorFactory for RestartInstance {
    fn descriptor(&self) -> ComponentDescriptor {
        restart_descriptor(self.inner.descriptor())
    }
    fn create(&self, config: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(RestartInstanceLifecycle {
            inner: self.inner.create(config)?,
            owners: self.owners.clone(),
        }))
    }
}
#[async_trait]
impl Lifecycle for RestartInstanceLifecycle {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.owners.lock().unwrap().push(ctx.tasks.clone());
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
impl Initiator for RestartInstanceLifecycle {
    fn descriptor(&self) -> ComponentDescriptor {
        restart_descriptor(self.inner.descriptor())
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

struct Engine {
    app: ApplicationHandle,
    bundle: Option<FedEngineRegisteredComponents>,
    metrics: Arc<EngineMetrics>,
    pg: Arc<modorch_fedengine_pgwire::ListenerStatus>,
    probe: Probe,
    owners: Arc<Mutex<Vec<TaskScheduler>>>,
}
impl Engine {
    fn address(&self) -> SocketAddr {
        if let Some(bundle) = &self.bundle {
            bundle.handle().pgwire_address().unwrap()
        } else {
            self.pg.address().unwrap()
        }
    }
    fn metrics(&self) -> MetricsSnapshot {
        if let Some(bundle) = &self.bundle {
            bundle.handle().metrics()
        } else {
            self.metrics.snapshot()
        }
    }
}
fn config(directory: &TestDirectory) -> Config {
    let mut c = Config::load("examples/config/federation.toml").unwrap();
    c.datasources.truncate(1);
    c.datasources[0].allowed_schemas = vec!["public".into()];
    c.grants.truncate(1);
    c.duckdb.database_path = directory
        .path()
        .join("state.duckdb")
        .to_string_lossy()
        .into();
    c.duckdb.temp_directory = directory.path().join("tmp").to_string_lossy().into();
    c.management.credentials_directory = directory.path().join("secrets").to_string_lossy().into();
    c.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    c.duckdb.max_workers = 1;
    c.duckdb.threads = 1;
    c.duckdb.batch_rows = 16;
    c.duckdb.result_channel_batches = 1;
    c.duckdb.acquire_timeout_ms = 150;
    c.pgwire.query_timeout_ms = 30_000;
    c.pgwire.listen = "127.0.0.1:0".into();
    c.management.listen = "127.0.0.1:0".into();
    c.users[0].password_secret_ref = "USER".into();
    c
}
async fn start(config: Config, supervised: bool) -> Engine {
    let mut builder = ApplicationBuilder::new("ownership");
    let probe = Probe::default();
    let metrics = Arc::new(EngineMetrics::default());
    let pg = Arc::default();
    let owners = Arc::default();
    let mut ids = vec![];
    let bundle = if supervised {
        let c = Arc::new(config);
        fn initiator(
            builder: &mut ApplicationBuilder,
            ids: &mut Vec<ComponentRef>,
            factory: impl InitiatorFactory + 'static,
        ) {
            ids.push(factory.descriptor().component());
            builder.register_initiator(factory).unwrap();
        }
        fn module(
            builder: &mut ApplicationBuilder,
            ids: &mut Vec<ComponentRef>,
            factory: impl ModuleFactory + 'static,
        ) {
            ids.push(factory.descriptor().component());
            builder.register_module(factory).unwrap();
        }
        initiator(
            &mut builder,
            &mut ids,
            RestartInstance {
                inner: modorch_fedengine_duckdb::DuckdbInstanceFactory { config: c.clone() },
                owners: Arc::clone(&owners),
            },
        );
        initiator(
            &mut builder,
            &mut ids,
            modorch_fedengine_datasource::DataSourceFactory {
                definitions: c.datasources.clone(),
                managed: true,
            },
        );
        initiator(
            &mut builder,
            &mut ids,
            modorch_fedengine_duckdb::DuckdbFactory {
                metrics: metrics.clone(),
                config: c.clone(),
            },
        );
        initiator(
            &mut builder,
            &mut ids,
            modorch_fedengine_pgwire::PgwireFactory {
                metrics: metrics.clone(),
                config: c.clone(),
                status: Arc::clone(&pg),
            },
        );
        initiator(
            &mut builder,
            &mut ids,
            modorch_fedengine_management::ManagementApiFactory {
                config: c.clone(),
                status: Arc::default(),
            },
        );
        module(
            &mut builder,
            &mut ids,
            modorch_fedengine_catalog::CatalogFactory { managed: true },
        );
        module(
            &mut builder,
            &mut ids,
            modorch_fedengine_policy::PolicyFactory { config: c.clone() },
        );
        module(
            &mut builder,
            &mut ids,
            modorch_fedengine_query::QueryFactory { config: c.clone() },
        );
        module(
            &mut builder,
            &mut ids,
            modorch_fedengine_management::SourceManagerFactory { config: c },
        );
        None
    } else {
        let registered = FedEngineBundle::new(config).register(&mut builder).unwrap();
        ids.extend(registered.components().cloned());
        Some(registered)
    };
    ids.push(Probe::descriptor().component());
    builder.register_module(probe.clone()).unwrap();
    let mut selection = ComponentSelection::new();
    for id in ids {
        match id.kind {
            ComponentKind::Initiator => {
                selection.enable_initiator(id.id);
            }
            ComponentKind::Module => {
                selection.enable_module(id.id);
            }
        }
    }
    let app = builder
        .selection(selection)
        .execution_resources(ExecutionResources {
            cpu_threads: 1,
            cpu_capacity: 2,
            blocking_io_threads: 6,
            blocking_io_capacity: 64,
            max_tasks_per_owner: 512,
            ..Default::default()
        })
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    if let Some(bundle) = &bundle {
        bundle.observe(&app).unwrap();
    }
    Engine {
        app,
        bundle,
        metrics,
        pg,
        probe,
        owners,
    }
}
async fn connect(address: SocketAddr) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, driver) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("pg_crm")
        .connect(NoTls)
        .await
        .unwrap();
    (
        client,
        tokio::spawn(async move {
            let _ = driver.await;
        }),
    )
}
async fn native() -> (Client, tokio::task::JoinHandle<()>) {
    let (client, driver) =
        tokio_postgres::connect(&std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(), NoTls)
            .await
            .unwrap();
    (
        client,
        tokio::spawn(async move {
            driver.await.unwrap();
        }),
    )
}
async fn prepare_remote(client: &Client) {
    client.batch_execute(&format!(
        "DROP TABLE IF EXISTS public.fe08_writes; DROP FUNCTION IF EXISTS public.fe08_block(); \
         DROP SEQUENCE IF EXISTS public.fe08_attempts; CREATE SEQUENCE public.fe08_attempts; \
         CREATE TABLE public.fe08_writes(id INTEGER); INSERT INTO public.fe08_writes VALUES(0); \
         CREATE FUNCTION public.fe08_block() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM nextval('public.fe08_attempts'); PERFORM set_config('lock_timeout','45s',true); \
         PERFORM pg_advisory_xact_lock({LOCK_KEY}); RETURN NEW; END $$; \
         CREATE TRIGGER fe08_gate BEFORE INSERT ON public.fe08_writes FOR EACH ROW EXECUTE FUNCTION public.fe08_block()"
    )).await.unwrap();
    client
        .query_one("SELECT pg_advisory_lock($1)", &[&LOCK_KEY])
        .await
        .unwrap();
}
async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn blocked(client: &Client) {
    tokio::time::timeout(WAIT, async {
        loop {
            let waiting: bool = client.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event='advisory' AND query LIKE '%fe08_writes%')", &[]
            ).await.unwrap().get(0);
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("remote INSERT entered the trigger and is blocked in native I/O");
}
async fn assert_one_attempt(client: &Client) {
    let row = client
        .query_one("SELECT last_value,is_called FROM public.fe08_attempts", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert!(row.get::<_, bool>(1));
    let count: i64 = client
        .query_one("SELECT count(*) FROM public.fe08_writes WHERE id=1", &[])
        .await
        .unwrap()
        .get(0);
    assert!(
        (0..=1).contains(&count),
        "cancellation cannot imply rollback, and must never replay a write"
    );
}
async fn clean_remote(client: &Client) {
    client
        .batch_execute(
            "DROP TABLE public.fe08_writes; DROP FUNCTION public.fe08_block(); DROP SEQUENCE public.fe08_attempts",
        )
        .await
        .unwrap();
}
fn locks(
    engine: &Engine,
) -> (
    Arc<tokio::sync::RwLock<()>>,
    Arc<tokio::sync::RwLock<()>>,
    String,
) {
    let registry = engine.probe.latest().registry.acquire().unwrap();
    let source = registry.get(&DataSourceId("crm".into())).unwrap();
    (
        registry.instance.clone(),
        source.fence.clone(),
        source.incarnation.clone(),
    )
}
async fn portal(address: SocketAddr) -> tokio::net::TcpStream {
    tokio::time::timeout(WAIT, async {
        let mut stream = wire::raw_start(address, "pg_crm").await;
        wire::packet(&mut stream, b'P', b"stmt\0SELECT r.i::INTEGER FROM pg_crm.public.fe08_writes t CROSS JOIN range(1000000) r(i)\0\0\0").await;
        wire::packet(&mut stream, b'B', b"p\0stmt\0\0\0\0\0\0\0").await;
        wire::packet(&mut stream, b'E', b"p\0\0\0\0\x01").await;
        wire::packet(&mut stream, b'H', &[]).await;
        assert_eq!(wire::until_boundary(&mut stream).await, (vec!["0".into()], b's'));
        stream
    }).await.unwrap()
}
async fn oid(client: &Client) -> u32 {
    client
        .query_one("SELECT oid FROM pg_class WHERE relname='fe08_writes'", &[])
        .await
        .unwrap()
        .get(0)
}
async fn stop(engine: &Engine) {
    assert!(matches!(
        engine
            .app
            .shutdown_checked(ShutdownOptions { timeout: WAIT })
            .await
            .unwrap(),
        ShutdownReceipt::GracefullyStopped
    ));
    assert_eq!(engine.app.health().pending.operations, 0);
    assert_eq!(engine.app.task_metrics().tasks, 0);
    assert_eq!(engine.metrics().active_workers, 0);
    assert_eq!(engine.metrics().active_sessions, 0);
}
// Re-enter this same ignored test in a child process, so DuckDB's real OS file
// lock is tested independently of its in-process connection cache.
fn lock_probe_child() -> bool {
    let Some(path) = std::env::var_os("FEDENGINE_FE08_LOCK_PROBE") else {
        return false;
    };
    let result = duckdb::Connection::open(path);
    if std::env::var("FEDENGINE_FE08_EXPECT_LOCKED").unwrap() == "yes" {
        let error = result
            .expect_err("a busy engine database must remain locked")
            .to_string();
        assert!(error.contains("lock"), "{error}");
    } else {
        assert!(
            result.is_ok(),
            "database must be released after graceful stop: {:?}",
            result.err()
        );
    }
    true
}
async fn file_lock(config: &Config, locked: bool) {
    let path = config.duckdb.database_path.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "blocked_write_cancel_and_shutdown_retain_real_resources",
            ])
            .env("FEDENGINE_FE08_LOCK_PROBE", path)
            .env(
                "FEDENGINE_FE08_EXPECT_LOCKED",
                if locked { "yes" } else { "no" },
            )
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and extensions; run scripts/test-federation.sh"]
async fn blocked_write_cancel_and_shutdown_retain_real_resources() {
    if lock_probe_child() {
        return;
    }
    let directory = TestDirectory::new();
    let config = config(&directory);
    let (native, native_driver) = native().await;
    prepare_remote(&native).await;
    let engine = start(config.clone(), false).await;
    let (client, driver) = connect(engine.address()).await;
    let object = oid(&client).await;
    let old = engine.probe.latest();
    let (instance, source, incarnation) = locks(&engine);
    let mut stream = portal(engine.address()).await;
    assert!(instance.clone().try_write_owned().is_err());
    assert!(source.clone().try_write_owned().is_err());
    wire::packet(&mut stream, b'C', b"Pp\0").await;
    wire::packet(&mut stream, b'S', &[]).await;
    assert_eq!(wire::message(&mut stream).await.0, b'3');
    assert_eq!(wire::message(&mut stream).await.0, b'Z');
    drop(stream);
    wait_for(|| engine.metrics().active_workers == 0).await;
    assert!(instance.clone().try_write_owned().is_ok());
    assert!(source.clone().try_write_owned().is_ok());
    let cancel = client.cancel_token();
    let write = client.simple_query("INSERT INTO pg_crm.public.fe08_writes VALUES(1)");
    tokio::pin!(write);
    tokio::select! { result = &mut write => panic!("write ended before remote block: {result:?}"), _ = blocked(&native) => {} }
    cancel.cancel_query(NoTls).await.unwrap();
    let error = tokio::time::timeout(WAIT, &mut write)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "57014");
    // This is deliberately a current-thread Tokio test: native I/O must not
    // stop the reactor, even after the client has received its cancellation.
    tokio::time::timeout(
        Duration::from_secs(1),
        tokio::time::sleep(Duration::from_millis(20)),
    )
    .await
    .unwrap();
    assert_eq!(engine.metrics().active_workers, 1);
    assert!(instance.clone().try_write_owned().is_err());
    assert!(source.clone().try_write_owned().is_err());
    let error = client.simple_query("SELECT 1").await.unwrap_err();
    assert_eq!(
        error.code().unwrap().code(),
        "53300",
        "canceled native I/O must retain its worker permit"
    );
    file_lock(&config, true).await;
    for _ in 0..2 {
        assert!(matches!(
            engine
                .app
                .shutdown_checked(ShutdownOptions {
                    timeout: Duration::from_millis(100)
                })
                .await,
            Err(FrameworkError::StopIncomplete { .. })
        ));
        assert_eq!(engine.metrics().active_workers, 1);
        assert!(engine.app.health().pending.operations > 0);
        assert!(
            engine
                .bundle
                .as_ref()
                .unwrap()
                .handle()
                .pgwire_address()
                .is_none()
        );
        assert!(instance.clone().try_write_owned().is_err());
        assert!(source.clone().try_write_owned().is_err());
        file_lock(&config, true).await;
        // A second in-process instance must not reuse DuckDB's still-live file,
        // even though the first Application has already entered teardown.
        let error = match modorch_fedengine_test_host::FedEngine::start(config.clone()).await {
            Ok(second) => {
                second.shutdown_until_drained().await.unwrap();
                panic!("StopIncomplete released management file ownership");
            }
            Err(error) => error.downcast::<StartupError>().unwrap(),
        };
        assert!(error.source.to_string().contains("already owned"));
        assert!(matches!(
            error.cleanup,
            Ok(ShutdownReceipt::GracefullyStopped)
        ));
    }
    native
        .query_one("SELECT pg_advisory_unlock($1)", &[&LOCK_KEY])
        .await
        .unwrap();
    stop(&engine).await;
    driver.await.unwrap();
    assert!(client.is_closed());
    assert!(old.registry.acquire().is_err());
    assert!(old.execution.acquire().is_err());
    assert!(old.tasks.spawn(|_| async { Ok(()) }).is_err());
    assert_one_attempt(&native).await;
    file_lock(&config, false).await;
    let rebuilt = start(config.clone(), false).await;
    assert_eq!(locks(&rebuilt).2, incarnation);
    let (new_client, new_driver) = connect(rebuilt.address()).await;
    assert_eq!(oid(&new_client).await, object);
    assert_one_attempt(&native).await;
    drop(new_client);
    new_driver.await.unwrap();
    stop(&rebuilt).await;
    clean_remote(&native).await;
    drop(native);
    native_driver.await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and extensions; run scripts/test-federation.sh"]
async fn supervised_instance_rebuild_drains_io_and_invalidates_sessions_and_capabilities() {
    let directory = TestDirectory::new();
    let mut config = config(&directory);
    config.duckdb.max_workers = 2;
    let (native, native_driver) = native().await;
    prepare_remote(&native).await;
    let engine = start(config.clone(), true).await;
    let (client, driver) = connect(engine.address()).await;
    let object = oid(&client).await;
    let _prepared = client
        .prepare("SELECT id FROM pg_crm.public.fe08_writes")
        .await
        .unwrap();
    let stream = portal(engine.address()).await;
    let old = engine.probe.latest();
    let (instance, source, incarnation) = locks(&engine);
    let owner = engine.owners.lock().unwrap()[0].clone();
    let write = client.simple_query("INSERT INTO pg_crm.public.fe08_writes VALUES(1)");
    tokio::pin!(write);
    tokio::select! { result = &mut write => panic!("write ended before remote block: {result:?}"), _ = blocked(&native) => {} }
    let fault = owner
        .spawn(|_| async {
            Err(FrameworkError::InvalidConfig(
                "injected instance owner failure".into(),
            ))
        })
        .unwrap();
    assert!(fault.wait_finished().await.is_err());
    wait_for(|| engine.app.health().state == ApplicationState::Recovering).await;
    wait_for(|| engine.metrics().active_sessions == 0 && client.is_closed()).await;
    assert!(
        client.is_closed(),
        "old authenticated/prepared sessions must not cross the rebuild"
    );
    assert_eq!(
        engine.app.health().components[&ComponentRef::initiator("pgwire-listener")].state,
        ComponentState::Stopping
    );
    assert_eq!(
        engine.app.health().components[&ComponentRef::initiator("duckdb-instance")].generation,
        0
    );
    assert_eq!(
        engine.owners.lock().unwrap().len(),
        1,
        "replacement must wait for real native I/O exit"
    );
    wait_for(|| engine.metrics().active_workers == 1).await;
    assert_eq!(engine.metrics().active_workers, 1);
    assert!(instance.clone().try_write_owned().is_err());
    assert!(source.clone().try_write_owned().is_err());
    file_lock(&config, true).await;
    native
        .query_one("SELECT pg_advisory_unlock($1)", &[&LOCK_KEY])
        .await
        .unwrap();
    wait_for(|| {
        engine.app.health().ready
            && engine.app.health().components[&ComponentRef::initiator("duckdb-instance")]
                .generation
                == 1
    })
    .await;
    assert!(
        tokio::time::timeout(WAIT, &mut write)
            .await
            .unwrap()
            .is_err()
    );
    drop(stream);
    driver.await.unwrap();
    assert!(old.registry.acquire().is_err());
    assert!(old.execution.acquire().is_err());
    assert!(old.tasks.spawn(|_| async { Ok(()) }).is_err());
    assert!(owner.spawn(|_| async { Ok(()) }).is_err());
    assert_eq!(engine.owners.lock().unwrap().len(), 2);
    assert_eq!(engine.probe.0.lock().unwrap().len(), 2);
    for id in [
        ComponentRef::initiator("pgwire-listener"),
        ComponentRef::initiator("management-api"),
    ] {
        assert_eq!(engine.app.health().components[&id].generation, 1);
    }
    assert_eq!(locks(&engine).2, incarnation);
    assert!(!Arc::ptr_eq(&locks(&engine).0, &instance));
    let (new_client, new_driver) = connect(engine.address()).await;
    assert_eq!(oid(&new_client).await, object);
    assert_one_attempt(&native).await;
    drop(new_client);
    new_driver.await.unwrap();
    stop(&engine).await;
    file_lock(&config, false).await;
    clean_remote(&native).await;
    drop(native);
    native_driver.await.unwrap();
}
