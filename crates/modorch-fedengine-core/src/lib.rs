//! Configuration and domain types, independent of orchestration and protocol crates.
use serde::{Deserialize, Serialize};
mod organization;
pub use organization::*;
mod management;
pub use management::*;
mod metadata;
pub use metadata::*;
pub mod plugins;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message} (SQLSTATE {sqlstate})")]
pub struct EngineError {
    pub sqlstate: &'static str,
    pub message: String,
    pub outcome_unknown: bool,
}
impl EngineError {
    pub fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
            outcome_unknown: false,
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("0A000", message)
    }
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("42501", message)
    }
    pub fn config(message: impl Into<String>) -> Self {
        Self::new("22023", message)
    }
    pub fn unknown(mut self) -> Self {
        self.outcome_unknown = true;
        self
    }
}
pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DataSourceId(pub String);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSourceKind {
    Postgres,
    Greenplum,
    Kingbasees,
    Mysql,
    Mariadb,
    Starrocks,
    Oracle,
    Sqlserver,
    Dameng,
    Db2,
    Hana,
}
impl DataSourceKind {
    pub fn dialect_name(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Greenplum => "greenplum",
            Self::Kingbasees => "kingbasees",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Starrocks => "starrocks",
            Self::Oracle => "oracle",
            Self::Sqlserver => "sqlserver",
            Self::Dameng => "dameng",
            Self::Db2 => "db2",
            Self::Hana => "hana",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Postgres | Self::Greenplum | Self::Kingbasees => "postgres",
            Self::Mysql | Self::Mariadb | Self::Starrocks => "mysql",
            Self::Oracle => "oracle",
            Self::Sqlserver => "sqlserver",
            Self::Dameng | Self::Db2 | Self::Hana => "odbc_catalog",
        }
    }
    pub fn attach_type(self) -> &'static str {
        match self.extension() {
            "oracle" => "oracle_scanner",
            "sqlserver" => "sqlserver_scanner",
            other => other,
        }
    }
    pub fn is_postgres_family(self) -> bool {
        matches!(self, Self::Postgres | Self::Greenplum | Self::Kingbasees)
    }
    pub fn is_mysql_family(self) -> bool {
        matches!(self, Self::Mysql | Self::Mariadb | Self::Starrocks)
    }
    pub fn is_odbc_catalog_family(self) -> bool {
        matches!(self, Self::Dameng | Self::Db2 | Self::Hana)
    }
    pub fn is_read_only_scanner(self) -> bool {
        matches!(
            self,
            Self::Oracle | Self::Sqlserver | Self::Dameng | Self::Db2 | Self::Hana
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataSourceDefinition {
    pub id: DataSourceId,
    pub kind: DataSourceKind,
    pub catalog_alias: String,
    /// Environment variable name or a file: reference under the configured credential directory.
    pub secret_ref: String,
    #[serde(default)]
    pub organization: Option<Box<SourceOrganization>>,
    pub default_schema: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub ddl_enabled: bool,
    pub allowed_schemas: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub application: ApplicationConfig,
    pub pgwire: PgwireConfig,
    pub duckdb: DuckdbConfig,
    pub ddl: DdlConfig,
    pub shutdown: ShutdownConfig,
    #[serde(default)]
    pub management: ManagementConfig,
    #[serde(default)]
    pub datasources: Vec<DataSourceDefinition>,
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub virtual_databases: Vec<VirtualDatabaseConfig>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub capabilities: Vec<CapabilityRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationConfig {
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PgwireConfig {
    pub listen: String,
    pub max_connections: usize,
    pub startup_timeout_ms: u64,
    pub query_timeout_ms: u64,
    pub output_timeout_ms: u64,
    pub tls_required: bool,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub max_statements: usize,
    pub max_portals: usize,
    #[serde(default)]
    pub allow_cross_database: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DuckdbConfig {
    pub database_path: String,
    pub max_workers: usize,
    pub threads: usize,
    pub memory_limit: String,
    pub temp_directory: String,
    pub result_channel_batches: usize,
    pub batch_rows: usize,
    pub acquire_timeout_ms: u64,
    pub install_extensions: bool,
    pub extension_directory: Option<String>,
    #[serde(default)]
    pub plugin_extension_directory: Option<String>,
    pub pool_max_connections: usize,
    pub pool_wait_timeout_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DdlConfig {
    pub enabled: bool,
    pub allow_drop_schema: bool,
    pub source_lock_timeout_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    pub grace_period_secs: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub name: String,
    pub password_secret_ref: String,
}
/// Explicit business database; CONNECT never grants access to a remote object.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualDatabaseConfig {
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub connect: Vec<String>,
    #[serde(default = "yes")]
    pub allow_connections: bool,
    #[serde(default)]
    pub schemas: Vec<VirtualSchemaConfig>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualSchemaConfig {
    pub name: String,
    pub source: DataSourceId,
    pub remote_schema: String,
}
/// Durable logical identities and projection pointers, published with source snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VirtualDatabase {
    pub config: VirtualDatabaseConfig,
    pub managed: bool,
    pub oid: u32,
    pub revision: u64,
    pub public_oid: u32,
    pub schemas: Vec<VirtualSchema>,
}
impl VirtualDatabaseConfig {
    /// Shared validation for TOML reconciliation and trusted management requests.
    pub fn validate(&self, users: &BTreeSet<String>) -> Result<()> {
        if !identifier(&self.name)
            || self.name.len() > 63
            || !allowed_namespace(&self.name)
            || ["memory", "main", "temp", "system"]
                .contains(&self.name.to_ascii_lowercase().as_str())
            || !users.contains(&self.owner)
            || self.connect.len() > 1024
            || self.connect.iter().any(|u| !users.contains(u))
            || self.schemas.len() > 256
        {
            return Err(EngineError::config(
                "invalid virtual database name, owner, CONNECT principal or resource bounds",
            ));
        }
        let mut schemas = BTreeSet::new();
        for binding in &self.schemas {
            if !identifier(&binding.name)
                || binding.name.len() > 63
                || !allowed_namespace(&binding.name)
                || !schemas.insert(binding.name.clone())
                || !allowed_namespace(&binding.remote_schema)
                || binding.remote_schema.len() > 128
                || !identifier(&binding.source.0)
                || binding.source.0.len() > 128
            {
                return Err(EngineError::config(
                    "invalid or duplicate virtual schema binding",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VirtualSchema {
    pub object_epoch: String,
    pub config: VirtualSchemaConfig,
    pub oid: u32,
    pub snapshot_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub principal: String,
    pub source: DataSourceId,
    pub schema: String,
    pub object: String,
    pub actions: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRecord {
    pub source: DataSourceId,
    pub duckdb_version: String,
    pub extension_version: String,
    pub remote_version: String,
    pub evidence: String,
    pub operations: BTreeSet<String>,
    #[serde(default)]
    pub index_adapter: bool,
}
impl Config {
    /// Read and validate the current TOML without rebasing configured paths.
    /// Relative filesystem paths retain process-working-directory semantics;
    /// embedded hosts should resolve them explicitly before Bundle registration.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| EngineError::config(e.to_string()))?;
        let config: Self = toml::from_str(&text).map_err(|e| EngineError::config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_for_ingress(true, self.management.enabled)
    }
    /// Validate core values and only the selected network ingress configuration.
    /// Managed storage, principals, source operations and shared query timeouts
    /// remain validated even when their network ingress is disabled.
    pub fn validate_for_ingress(&self, pgwire: bool, management_http: bool) -> Result<()> {
        let p = &self.pgwire;
        let d = &self.duckdb;
        if self.application.name.is_empty()
            || p.query_timeout_ms == 0
            || (pgwire
                && (p.max_connections == 0
                    || p.max_connections > 4096
                    || p.startup_timeout_ms == 0
                    || p.output_timeout_ms == 0
                    || p.max_statements == 0
                    || p.max_portals == 0))
            || d.max_workers == 0
            || d.max_workers > 128
            || d.threads == 0
            || d.threads > 128
            || !(1..=64).contains(&d.result_channel_batches)
            || !(1..=4096).contains(&d.batch_rows)
            || d.acquire_timeout_ms == 0
            || d.pool_max_connections == 0
            || d.pool_wait_timeout_ms == 0
            || self.ddl.source_lock_timeout_ms == 0
            || self.shutdown.grace_period_secs == 0
        {
            return Err(EngineError::config("invalid resource bounds"));
        }
        if pgwire {
            p.listen
                .parse::<std::net::SocketAddr>()
                .map_err(|_| EngineError::config("listen must be an IP socket address"))?;
            if p.tls_required && (p.tls_cert.is_none() || p.tls_key.is_none()) {
                return Err(EngineError::config("TLS requires both certificate and key"));
            }
            if p.tls_cert.is_some() != p.tls_key.is_some() {
                return Err(EngineError::config(
                    "TLS certificate/key must be configured together",
                ));
            }
        }
        let mut ids = BTreeSet::new();
        let mut aliases = BTreeSet::new();
        for s in &self.datasources {
            validate_source(s)?;
            if !ids.insert(s.id.clone()) || !aliases.insert(s.catalog_alias.to_lowercase()) {
                return Err(EngineError::config("duplicate source id or catalog alias"));
            }
        }
        let m = &self.management;
        if management_http && !m.enabled {
            return Err(EngineError::config(
                "management HTTP requires managed storage mode",
            ));
        }
        // Static sources also use the metadata collector after confirmed DDL.
        if !(32..=16384).contains(&m.metadata_memory_budget_mb)
            || !(32..=4096).contains(&m.metadata_operation_memory_mb)
            || m.metadata_operation_memory_mb > m.metadata_memory_budget_mb
        {
            return Err(EngineError::config("invalid metadata memory bounds"));
        }
        if m.enabled && (d.database_path.is_empty() || d.database_path == ":memory:") {
            return Err(EngineError::config(
                "management requires a file-backed duckdb.database_path",
            ));
        }
        if management_http {
            m.listen.parse::<std::net::SocketAddr>().map_err(|_| {
                EngineError::config("management.listen must be an IP socket address")
            })?;
        }
        if !(1..=128).contains(&m.max_concurrent_operations)
            || !(1..=4096).contains(&m.queue_capacity)
            || m.drain_timeout_ms == 0
            || m.request_timeout_ms == 0
            || m.credentials_directory.is_empty()
        {
            return Err(EngineError::config("invalid management resource bounds"));
        }

        let mut users = BTreeSet::new();
        for u in &self.users {
            if u.name.is_empty()
                || u.name == "fedengine_system"
                || !users.insert(u.name.clone())
                || (pgwire && !identifier(&u.password_secret_ref))
            {
                return Err(EngineError::config("invalid/duplicate user"));
            }
        }
        if users.is_empty() {
            return Err(EngineError::config("at least one principal is required"));
        }
        if (!self.virtual_databases.is_empty()
            || self.datasources.iter().any(|s| s.organization.is_some()))
            && !self.management.enabled
        {
            return Err(EngineError::config(
                "virtual databases require managed storage",
            ));
        }
        if self.virtual_databases.len() > 256 {
            return Err(EngineError::config("virtual database limit exceeded"));
        }
        let mut databases = BTreeSet::new();
        for db in &self.virtual_databases {
            db.validate(&users)?;
            if !databases.insert(db.name.to_lowercase())
                || aliases.contains(&db.name.to_lowercase())
            {
                return Err(EngineError::config(
                    "duplicate virtual database name or source alias",
                ));
            }
        }
        for g in &self.grants {
            if !users.contains(&g.principal)
                || (!self.management.enabled && !ids.contains(&g.source))
                || g.actions.is_empty()
            {
                return Err(EngineError::config(
                    "grant refers to unknown principal/source or has no actions",
                ));
            }
        }
        let mut capabilities = BTreeSet::new();
        for c in &self.capabilities {
            if (!self.management.enabled && !ids.contains(&c.source))
                || !capabilities.insert(c.source.clone())
                || c.duckdb_version != "v2.0.0"
                || c.extension_version.is_empty()
                || c.remote_version.is_empty()
                || c.evidence.is_empty()
            {
                return Err(EngineError::config(
                    "capability record requires unique source and pinned version/evidence",
                ));
            }
        }
        Ok(())
    }
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}
pub fn quote_identifier(v: &str) -> String {
    format!("\"{}\"", v.replace('"', "\"\""))
}
pub fn quote_literal(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// Logical Hub admission cost, including owned variable input, not deep RSS.
pub trait HubPayloadBytes {
    fn hub_payload_bytes(&self) -> usize;
}
/// Count serialized input without allocating or retaining a second secret-bearing buffer.
pub fn serialized_payload_bytes(value: &impl serde::Serialize) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(256);
    if serde_json::to_writer(&mut counter, value).is_err() {
        usize::MAX
    } else {
        counter.0
    }
}
