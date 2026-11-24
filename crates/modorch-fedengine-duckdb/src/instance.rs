use crate::{
    error::{engine_error, fw},
    storage::DuckdbStore,
};
use duckdb::Connection;
use modorch::{prelude::*, *};
use modorch_fedengine_contract::ManagementStorage;
use modorch_fedengine_core::*;
use std::sync::{Arc, Mutex};

pub struct DuckdbInstanceFactory {
    pub config: Arc<Config>,
}
pub(super) struct DuckdbInstance {
    pub root: Arc<Mutex<RootConnection>>,
}
// Field drop order closes the native root before releasing its file claim.
// Every storage/runtime root Arc retains this pair, including retained work.
pub(super) struct RootConnection {
    connection: Connection,
    _claim: Option<crate::file_claim::FileClaim>,
}
impl std::ops::Deref for RootConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}
struct InstanceInitiator {
    config: Arc<Config>,
    instance: Option<Arc<DuckdbInstance>>,
    storage: Option<Arc<ManagementStorage>>,
}
fn descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::initiator("duckdb-instance"))
}
impl InitiatorFactory for DuckdbInstanceFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(InstanceInitiator {
            config: self.config.clone(),
            instance: None,
            storage: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for InstanceInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let config = self.config.clone();
        let connection = ctx
            .tasks
            .spawn_blocking_io(move |_| {
                let (conn, claim) = crate::file_claim::open(&config.duckdb.database_path, || {
                    initialize_local(&config)
                })?;
                let conn = RootConnection {
                    connection: conn,
                    _claim: claim,
                };
                crate::storage::initialize(&conn, &config).map_err(fw)?;
                Ok(conn)
            })?
            .wait_finished()
            .await?;
        let root = Arc::new(Mutex::new(connection));
        self.instance = Some(Arc::new(DuckdbInstance { root: root.clone() }));
        {
            self.storage = Some(Arc::new(ManagementStorage {
                port: Arc::new(DuckdbStore {
                    root,
                    tasks: ctx.tasks.clone(),
                    writer: Arc::new(tokio::sync::Mutex::new(())),
                    views: Arc::new(Mutex::new(crate::storage::CatalogViews::default())),
                }),
            }));
        }
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.storage.take();
        self.instance.take();
        Ok(())
    }
}
impl Initiator for InstanceInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn export_services(&self, publisher: &mut ServicePublisher) -> FrameworkResult<()> {
        publisher.publish("instance", self.instance.as_ref().unwrap().clone())?;
        if let Some(storage) = &self.storage {
            publisher.publish("storage", storage.clone())?;
        }
        Ok(())
    }
}
pub(super) fn initialize_local(config: &Config) -> FrameworkResult<Connection> {
    let d = &config.duckdb;
    if d.database_path != ":memory:"
        && let Some(parent) = std::path::Path::new(&d.database_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|_| fw(EngineError::config("cannot create engine directory")))?;
    }
    std::fs::create_dir_all(&d.temp_directory)
        .map_err(|_| fw(EngineError::config("cannot create temp directory")))?;
    crate::memory::cleanup_stale_spools(&d.temp_directory);
    let mut options = duckdb::Config::default();
    // Dev-phase allowance for self-compiled custom *_scanner extensions;
    // extensions remain gated by autoload=false, autoinstall=false and manifest SHA-256.
    options = options
        .with("allow_unsigned_extensions", "true")
        .map_err(engine_error)
        .map_err(fw)?;
    options = options
        .with("autoload_known_extensions", "false")
        .map_err(engine_error)
        .map_err(fw)?;
    options = options
        .with("autoinstall_known_extensions", "false")
        .map_err(engine_error)
        .map_err(fw)?;
    if let Some(dir) = &d.extension_directory {
        options = options
            .with("extension_directory", dir)
            .map_err(engine_error)
            .map_err(fw)?;
    }
    let conn = Connection::open_with_flags(&d.database_path, options)
        .map_err(engine_error)
        .map_err(fw)?;
    let version: String = conn
        .query_row("SELECT version()", [], |r| r.get(0))
        .map_err(engine_error)
        .map_err(fw)?;
    if version != "v2.0.0" {
        return Err(fw(EngineError::config("DuckDB core must be v2.0.0")));
    }
    conn.execute_batch(&format!(
        "SET threads={}; SET memory_limit={}; SET temp_directory={}; SET TimeZone='UTC'",
        d.threads,
        quote_literal(&d.memory_limit),
        quote_literal(&d.temp_directory)
    ))
    .map_err(engine_error)
    .map_err(fw)?;
    if d.database_path != ":memory:" {
        // Publication performs a passive checkpoint after releasing its instance
        // barrier. Keep ordinary directory commits below the automatic threshold;
        // the WAL remains durable and automatic checkpointing is still a fallback.
        conn.execute_batch("SET checkpoint_threshold='1GB'")
            .map_err(engine_error)
            .map_err(fw)?;
    }
    crate::functions::register(&conn)
        .map_err(engine_error)
        .map_err(fw)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn initialize_local_loads_verified_custom_extensions() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/extensions");
        if !dir.join("manifest.json").is_file() {
            return;
        }
        let temp =
            std::env::temp_dir().join(format!("fedengine-ext-load-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(temp.clone());

        let mut config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        config.duckdb.database_path = ":memory:".into();
        config.duckdb.temp_directory = temp.to_string_lossy().into();
        config.duckdb.extension_directory =
            Some(dir.canonicalize().unwrap().to_string_lossy().into());
        config.duckdb.install_extensions = false;

        let conn = initialize_local(&config).unwrap();
        let ext_dir = config.duckdb.extension_directory.as_deref().unwrap();
        for logical in ["postgres", "mysql", "oracle", "sqlserver", "odbc_catalog"] {
            crate::extensions::verify_extension(ext_dir, logical).unwrap();
        }
        for logical in ["postgres", "mysql"] {
            let kind = if logical == "postgres" {
                DataSourceKind::Postgres
            } else {
                DataSourceKind::Mysql
            };
            crate::control::load_extension(&conn, &config, kind).unwrap();
        }
        let mut odbc_loaded = Vec::new();
        for logical in ["oracle", "sqlserver", "odbc_catalog"] {
            let module = crate::extensions::extension_module_name(logical);
            match conn.execute_batch(&format!("LOAD {module}")) {
                Ok(()) => odbc_loaded.push(logical),
                Err(err) if err.to_string().contains("libodbc.so.2") => {}
                Err(err) => panic!("unexpected load failure for {module}: {err}"),
            }
        }
        if std::env::var_os("FEDENGINE_REQUIRE_ODBC").is_some() {
            assert_eq!(odbc_loaded, vec!["oracle", "sqlserver", "odbc_catalog"]);
        }
        let versioned: i64 = conn
            .query_row(
                "SELECT count(*) FROM duckdb_functions() WHERE regexp_matches(function_name, '_v[0-9]+$')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(versioned, 0);
        let mut expected = vec![
            "pg_clear_cache",
            "mysql_clear_cache",
            "postgres_crud_metadata",
            "greenplum_crud_metadata",
            "kingbasees_crud_metadata",
            "postgres_guarded_insert_returning",
            "greenplum_guarded_insert_returning",
            "kingbasees_guarded_insert_returning",
            "postgres_ddl_capabilities",
            "greenplum_ddl_capabilities",
            "kingbasees_ddl_capabilities",
            "postgres_guarded_ddl",
            "greenplum_guarded_ddl",
            "kingbasees_guarded_ddl",
            "mysql_crud_metadata",
            "mysql_guarded_insert_returning",
            "mysql_ddl_capabilities",
            "mysql_guarded_ddl",
        ];
        if odbc_loaded.contains(&"oracle") {
            expected.extend([
                "oracle_crud_metadata",
                "oracle_guarded_mutation",
                "oracle_guarded_insert_returning",
                "oracle_ddl_capabilities",
                "oracle_guarded_ddl",
                "oracle_relational_metadata",
            ]);
        }
        if odbc_loaded.contains(&"sqlserver") {
            expected.extend([
                "sqlserver_crud_metadata",
                "sqlserver_guarded_mutation",
                "sqlserver_guarded_insert_returning",
                "sqlserver_ddl_capabilities",
                "sqlserver_guarded_ddl",
                "sqlserver_relational_metadata",
            ]);
        }
        if odbc_loaded.contains(&"odbc_catalog") {
            expected.extend([
                "dameng_crud_metadata",
                "dameng_guarded_mutation",
                "dameng_guarded_insert_returning",
                "db2_crud_metadata",
                "db2_guarded_mutation",
                "db2_guarded_insert_returning",
                "hana_crud_metadata",
                "hana_guarded_mutation",
                "hana_guarded_insert_returning",
                "dameng_ddl_capabilities",
                "dameng_guarded_ddl",
                "db2_ddl_capabilities",
                "db2_guarded_ddl",
                "hana_ddl_capabilities",
                "hana_guarded_ddl",
                "odbc_relational_metadata",
            ]);
        }
        for required in expected {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM duckdb_functions() WHERE function_name=?",
                    [required],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(count >= 1, "missing de-versioned function {required}");
        }
    }
}
