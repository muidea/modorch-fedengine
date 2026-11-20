//! Versioned control messages and protocol independent streaming contracts.
use modorch_fedengine_core::{DataSourceId, DataSourceKind, EngineError, Result, SourceVersion};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const QUERY_TOPIC: &str = "query/execute/v1";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Session,
    Query,
    Insert,
    Update,
    Delete,
    Ddl,
}
impl StatementKind {
    pub fn writes(self) -> bool {
        matches!(self, Self::Insert | Self::Update | Self::Delete | Self::Ddl)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgType {
    /// Explicit PostgreSQL catalog scalar/vector/array type; never inferred from a column name.
    Catalog(u32),
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Date,
    Time,
    Timestamp,
    Timestamptz,
    Interval,
    Point,
    Uuid,
    Json,
}
impl PgType {
    pub fn duckdb_name(self) -> &'static str {
        match self {
            Self::Catalog(26 | 28 | 29 | 24 | 2205 | 2206) => "UINTEGER",
            Self::Catalog(22 | 1005) => "SMALLINT[]",
            Self::Catalog(30 | 1028) => "UINTEGER[]",
            Self::Catalog(1007) => "INTEGER[]",
            Self::Catalog(1016) => "BIGINT[]",
            Self::Catalog(1000) => "BOOLEAN[]",
            Self::Catalog(1001) => "BLOB[]",
            Self::Catalog(1002 | 1003 | 1009 | 1014 | 1015 | 1034) => "VARCHAR[]",
            Self::Catalog(1017) => "STRUCT(x DOUBLE, y DOUBLE)[]",
            Self::Catalog(1021) => "FLOAT[]",
            Self::Catalog(1022) => "DOUBLE[]",
            Self::Catalog(1115) => "TIMESTAMP[]",
            Self::Catalog(1182) => "DATE[]",
            Self::Catalog(1183) => "TIME[]",
            Self::Catalog(1185) => "TIMESTAMPTZ[]",
            Self::Catalog(1187) => "INTERVAL[]",
            Self::Catalog(1231) => "DECIMAL(38,10)[]",
            Self::Catalog(2951) => "UUID[]",
            Self::Catalog(199) => "JSON[]",
            Self::Catalog(_) => "VARCHAR",
            Self::Bool => "BOOLEAN",
            Self::Int2 => "SMALLINT",
            Self::Int4 => "INTEGER",
            Self::Int8 => "BIGINT",
            Self::Float4 => "FLOAT",
            Self::Float8 => "DOUBLE",
            Self::Numeric => "DECIMAL(38,10)",
            Self::Text => "VARCHAR",
            Self::Bytea => "BLOB",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Timestamp => "TIMESTAMP",
            Self::Timestamptz => "TIMESTAMPTZ",
            Self::Interval => "INTERVAL",
            Self::Point => "STRUCT(x DOUBLE, y DOUBLE)",
            Self::Uuid => "UUID",
            Self::Json => "JSON",
        }
    }
    pub fn oid(self) -> u32 {
        match self {
            Self::Catalog(oid) => oid,
            Self::Bool => 16,
            Self::Int2 => 21,
            Self::Int4 => 23,
            Self::Int8 => 20,
            Self::Float4 => 700,
            Self::Float8 => 701,
            Self::Numeric => 1700,
            Self::Text => 25,
            Self::Bytea => 17,
            Self::Date => 1082,
            Self::Time => 1083,
            Self::Timestamp => 1114,
            Self::Timestamptz => 1184,
            Self::Interval => 1186,
            Self::Point => 600,
            Self::Uuid => 2950,
            Self::Json => 114,
        }
    }
    pub fn from_oid(oid: u32) -> Option<Self> {
        Some(match oid {
            18 | 19 | 22 | 24 | 26 | 28 | 29 | 30 | 194 | 199 | 1000 | 1001 | 1002 | 1003
            | 1005 | 1007 | 1009 | 1014 | 1015 | 1016 | 1017 | 1021 | 1022 | 1028 | 1034 | 1115
            | 1182 | 1183 | 1185 | 1187 | 1231 | 2205 | 2206 | 2277 | 2951 => Self::Catalog(oid),
            16 => Self::Bool,
            21 => Self::Int2,
            23 => Self::Int4,
            20 => Self::Int8,
            700 => Self::Float4,
            701 => Self::Float8,
            1700 => Self::Numeric,
            25 | 1043 | 1042 => Self::Text,
            17 => Self::Bytea,
            1082 => Self::Date,
            1083 => Self::Time,
            1114 => Self::Timestamp,
            1184 => Self::Timestamptz,
            1186 => Self::Interval,
            600 => Self::Point,
            2950 => Self::Uuid,
            114 => Self::Json,
            _ => return None,
        })
    }
}
#[derive(Debug, Clone)]
pub enum Cell {
    Oid(u32),
    /// An OID value with snapshot-scoped PG text output; binary remains an OID.
    NamedOid {
        oid: u32,
        name: String,
    },
    Array {
        lower_bound: i32,
        values: Vec<Cell>,
    },
    Null,
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Numeric(String),
    Date(i32),
    Time(i64),
    Timestamp(i64),
    Interval {
        months: i32,
        days: i32,
        micros: i64,
    },
    Point {
        x: f64,
        y: f64,
    },
    Uuid([u8; 16]),
}
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: PgType,
    pub type_modifier: i32,
    pub table_oid: Option<u32>,
    pub column_attnum: Option<i16>,
}
impl ColumnMeta {
    pub fn new(name: impl Into<String>, data_type: PgType) -> Self {
        Self {
            name: name.into(),
            data_type,
            type_modifier: -1,
            table_oid: None,
            column_attnum: None,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub database: String,
    /// Identity admitted at startup, never rebound by a later same-name creation.
    pub database_oid: Option<u32>,
    /// None selects the effective source default; Some(empty) explicitly disables short names.
    pub search_path: Option<Vec<SessionIdentifier>>,
    pub namespace_version: u64,
    pub settings: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentifier {
    pub name: String,
    pub quoted: bool,
}
#[derive(Debug, Clone)]
pub struct PreparedDescriptor {
    pub namespace_version: u64,
    pub database_version: Option<DatabaseVersion>,
    pub sql: String,
    pub parameters: Vec<PgType>,
    pub columns: Vec<ColumnMeta>,
    pub generations: BTreeMap<DataSourceId, SourceVersion>,
    pub kind: StatementKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseVersion {
    pub oid: u32,
    pub revision: u64,
}
#[derive(Debug, Clone)]
pub enum QueryMode {
    Prepare {
        parameter_types: Vec<Option<PgType>>,
    },
    Execute {
        parameters: Vec<Cell>,
        parameter_types: Vec<PgType>,
        expected: BTreeMap<DataSourceId, SourceVersion>,
        expected_database: Option<DatabaseVersion>,
    },
}
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub session: SessionContext,
    pub principal: String,
    pub session_id: i32,
    pub sql: String,
    pub mode: QueryMode,
    pub deadline: Instant,
    pub cancel: CancellationToken,
}
pub enum QueryReply {
    Prepared(PreparedDescriptor),
    Accepted(AcceptedExecution),
}
pub struct AcceptedExecution {
    pub notices: Vec<(String, String)>,
    pub columns: Vec<ColumnMeta>,
    pub kind: StatementKind,
    stream: Mutex<Option<ExecutionStream>>,
}
impl AcceptedExecution {
    pub fn new(columns: Vec<ColumnMeta>, kind: StatementKind, stream: ExecutionStream) -> Self {
        Self {
            notices: vec![],
            columns,
            kind,
            stream: Mutex::new(Some(stream)),
        }
    }
    pub fn take_stream(&self) -> Result<ExecutionStream> {
        self.stream
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| EngineError::new("55000", "execution stream already consumed"))
    }
}
#[derive(Debug, Clone)]
pub struct Completion {
    /// Diagnostics after a confirmed commit, never a claim that the write rolled back.
    pub notices: Vec<(String, String)>,
    pub command: String,
    pub affected_rows: Option<u64>,
    pub outcome: ExecutionOutcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Completed,
    CommitConfirmed,
    Indeterminate,
}
pub enum ResultChunk {
    Rows(Vec<Vec<Cell>>),
    Complete(Completion),
    Error(EngineError),
}
/// Dropping a receiver requests cooperative cancellation; it never asserts rollback.
pub struct ExecutionStream {
    pub receiver: mpsc::Receiver<ResultChunk>,
    pub cancel: CancellationToken,
}
impl Drop for ExecutionStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[derive(Clone)]
pub struct ExecutionService {
    pub port: Arc<dyn ExecutionPort>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturningPlan {
    pub column: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedMutationPlan {
    pub source: DataSourceId,
    pub kind: DataSourceKind,
    pub catalog: String,
    pub schema: String,
    pub table: String,
    pub native_sql: String,
    pub returning: Option<ReturningPlan>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlColumnSpec {
    pub name: String,
    pub type_name: String,
    pub length: i32,
    pub precision: i32,
    pub scale: i32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlConstraintSpec {
    pub name: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub reference_schema: String,
    pub reference_table: String,
    pub reference_columns: Vec<String>,
    pub check_column: String,
    pub check_operator: String,
    pub check_value: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedDdlPlan {
    pub source: DataSourceId,
    pub kind: DataSourceKind,
    pub catalog: String,
    pub database: String,
    pub schema: String,
    pub action: String,
    pub table_name: String,
    pub index_name: String,
    pub owner: String,
    pub new_name: String,
    pub column_name: String,
    pub default_kind: String,
    pub default_value: String,
    pub constraint_name: String,
    pub constraint: Option<DdlConstraintSpec>,
    pub identity_mode: String,
    pub columns: Vec<DdlColumnSpec>,
    pub keys: Vec<String>,
    pub contract_version: i32,
}
pub struct ExecuteRequest {
    /// Present only for the validated, local-only system metadata execution path.
    pub metadata: Option<MetadataExecution>,
    pub principal: String,
    pub session_id: i32,
    pub sql: String,
    pub kind: StatementKind,
    pub parameters: Vec<Cell>,
    pub parameter_types: Vec<Option<PgType>>,
    pub prepare_only: bool,
    pub generations: BTreeMap<DataSourceId, SourceVersion>,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub ddl_source: Option<DataSourceId>,
    pub command: String,
    pub returning: Option<ReturningPlan>,
    pub guarded_mutation: Option<GuardedMutationPlan>,
    pub guarded_ddl: Option<GuardedDdlPlan>,
}

#[derive(Debug, Clone)]
pub struct MetadataExecution {
    pub scope: MetadataScope,
    pub columns: Vec<Option<ColumnMeta>>,
    pub search_path: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct VisibleCatalog {
    pub internal: bool,
    pub id: DataSourceId,
    pub catalog: String,
    pub snapshot_id: String,
    pub database_oid: u32,
    pub schemas: Vec<String>,
    pub default_schema: String,
    pub grants: Vec<modorch_fedengine_core::Grant>,
    pub stale: bool,
    pub mapping: Option<modorch_fedengine_core::VirtualSchemaConfig>,
}
#[derive(Debug, Clone, Default)]
pub struct MetadataScope {
    pub database: String,
    pub principal: String,
    pub catalogs: Vec<VisibleCatalog>,
    pub virtual_database: Option<modorch_fedengine_core::VirtualDatabase>,
    pub databases: Vec<modorch_fedengine_core::VirtualDatabase>,
}
pub enum RuntimeReply {
    Prepared(PreparedDescriptor),
    Accepted(AcceptedExecution),
}
#[async_trait::async_trait]
pub trait ExecutionPort: Send + Sync {
    /// Ownership contains service leases and source fences, retained until actual worker exit.
    async fn submit(
        &self,
        request: ExecuteRequest,
        ownership: Box<dyn Send>,
    ) -> Result<RuntimeReply>;
    async fn indexes(
        &self,
        source: DataSourceId,
        ownership: Box<dyn Send>,
    ) -> Result<Vec<RemoteIndex>>;
}

#[derive(Debug, Clone)]
pub struct RemoteIndex {
    pub source: DataSourceId,
    pub catalog: String,
    pub schema: String,
    pub index: String,
    pub table: String,
    pub protected: bool,
}

/// Process-local counters. Exporters can read snapshots without querying business SQL.
#[derive(Default)]
pub struct EngineMetrics {
    pub active_sessions: Arc<std::sync::atomic::AtomicU64>,
    pub active_workers: Arc<std::sync::atomic::AtomicU64>,
    pub worker_waiters: Arc<std::sync::atomic::AtomicU64>,
    pub admitted: std::sync::atomic::AtomicU64,
    pub completed: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub canceled: std::sync::atomic::AtomicU64,
    pub unknown_writes: std::sync::atomic::AtomicU64,
    pub ddl_confirmed: std::sync::atomic::AtomicU64,
    pub metadata_stale: std::sync::atomic::AtomicU64,
    pub rows_streamed: std::sync::atomic::AtomicU64,
    pub execution_micros: std::sync::atomic::AtomicU64,
    pub memory_pressure_events: std::sync::atomic::AtomicU64,
    pub spool_bytes_written: std::sync::atomic::AtomicU64,
    pub demand_repairs_coalesced: std::sync::atomic::AtomicU64,
}
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub active_sessions: u64,
    pub active_workers: u64,
    pub worker_waiters: u64,
    pub admitted: u64,
    pub completed: u64,
    pub failed: u64,
    pub canceled: u64,
    pub unknown_writes: u64,
    pub ddl_confirmed: u64,
    pub metadata_stale: u64,
    pub rows_streamed: u64,
    pub execution_micros: u64,
    pub memory_pressure_events: u64,
    pub spool_bytes_written: u64,
    pub demand_repairs_coalesced: u64,
}
impl EngineMetrics {
    /// Gauges and counters are individually atomic; a snapshot is not a global transaction.
    pub fn snapshot(&self) -> MetricsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        MetricsSnapshot {
            active_sessions: self.active_sessions.load(Relaxed),
            active_workers: self.active_workers.load(Relaxed),
            worker_waiters: self.worker_waiters.load(Relaxed),
            admitted: self.admitted.load(Relaxed),
            completed: self.completed.load(Relaxed),
            failed: self.failed.load(Relaxed),
            canceled: self.canceled.load(Relaxed),
            unknown_writes: self.unknown_writes.load(Relaxed),
            ddl_confirmed: self.ddl_confirmed.load(Relaxed),
            metadata_stale: self.metadata_stale.load(Relaxed),
            rows_streamed: self.rows_streamed.load(Relaxed),
            execution_micros: self.execution_micros.load(Relaxed),
            memory_pressure_events: self.memory_pressure_events.load(Relaxed),
            spool_bytes_written: self.spool_bytes_written.load(Relaxed),
            demand_repairs_coalesced: self.demand_repairs_coalesced.load(Relaxed),
        }
    }
}
/// Returns a gauge to its prior value on actual scope exit, including unwinding.
pub struct GaugeGuard(Arc<std::sync::atomic::AtomicU64>);
impl GaugeGuard {
    pub fn enter(gauge: Arc<std::sync::atomic::AtomicU64>) -> Self {
        gauge.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(gauge)
    }
}
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A short local transaction. The closure must never perform network I/O or wait for locks.
pub type ManagementUpdate =
    Box<dyn FnOnce(&mut modorch_fedengine_core::ManagementState) -> Result<()> + Send>;
/// An uncommitted local publication. Drop aborts it; commit retains ownership until actual exit.
#[async_trait::async_trait]
pub trait PreparedPublication: Send {
    async fn commit(
        self: Box<Self>,
        ownership: Box<dyn Send>,
        published: Box<dyn FnOnce() + Send>,
    ) -> Result<()>;
}
#[derive(Clone)]
pub struct ManagementStorage {
    pub port: Arc<dyn ManagementStore>,
}
#[async_trait::async_trait]
pub trait ManagementStore: Send + Sync {
    /// Prepare source-scoped identities and projections while existing readers use committed heads.
    async fn prepare_publication(
        &self,
        incarnation: String,
        update: ManagementUpdate,
        ownership: Box<dyn Send>,
    ) -> Result<Box<dyn PreparedPublication>>;

    async fn read(&self) -> Result<modorch_fedengine_core::ManagementState>;
    async fn update(
        &self,
        update: ManagementUpdate,
    ) -> Result<modorch_fedengine_core::ManagementState>;
    /// Caller transfers the instance barrier and service leases to the actual writer task.
    async fn virtual_database_mutation(
        &self,
        mutation: VirtualDatabaseMutation,
        key: String,
        users: std::collections::BTreeSet<String>,
        ownership: Box<dyn Send>,
    ) -> Result<VirtualDatabaseReceipt> {
        let _ = (mutation, key, users, ownership);
        Err(EngineError::unsupported(
            "virtual database writer is unavailable",
        ))
    }
    /// Small effective heads only; never loads the object payloads of unrelated catalogs.
    async fn relation_names(
        &self,
        _snapshot: String,
    ) -> Result<Arc<std::collections::BTreeSet<(String, String)>>> {
        Err(EngineError::unsupported(
            "catalog name reader is unavailable",
        ))
    }
    async fn catalog_headers(&self) -> Result<CatalogHeads> {
        Err(EngineError::unsupported(
            "catalog header reader is unavailable",
        ))
    }
    async fn directory(&self, request: DirectoryRequest) -> Result<CatalogPage> {
        let _ = request;
        Err(EngineError::unsupported(
            "catalog directory reader is unavailable",
        ))
    }
    /// Indexed names and source kinds only; never decompresses column payloads.
    async fn directory_summaries(
        &self,
        _request: DirectorySummaryRequest,
    ) -> Result<Vec<DirectorySummary>> {
        Err(EngineError::unsupported(
            "catalog summary reader is unavailable",
        ))
    }
}
#[derive(Debug, Clone, Default)]
pub struct CatalogHeads {
    pub sources: Vec<CatalogHeader>,
    pub databases: Vec<modorch_fedengine_core::VirtualDatabase>,
}
impl From<Vec<CatalogHeader>> for CatalogHeads {
    fn from(sources: Vec<CatalogHeader>) -> Self {
        Self {
            sources,
            databases: vec![],
        }
    }
}
impl std::ops::Deref for CatalogHeads {
    type Target = Vec<CatalogHeader>;
    fn deref(&self) -> &Self::Target {
        &self.sources
    }
}
#[derive(Debug, Clone)]
pub struct CatalogHeader {
    pub source: modorch_fedengine_core::ManagedSource,
    pub applied: Option<modorch_fedengine_core::SourceRevision>,
    pub snapshot: Option<modorch_fedengine_core::CatalogSnapshot>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectoryRequest {
    pub id: DataSourceId,
    pub schema: Option<String>,
    pub object_type: Option<String>,
    pub object: Option<String>,
    pub snapshot_id: Option<String>,
    pub offset: usize,
    pub limit: usize,
    /// Optional decompressed per-object budget for interactive consumers.
    pub object_bytes_limit: Option<usize>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectorySummaryRequest {
    /// Ordered, authorized source scopes. The first contributor wins per relation.
    pub scopes: Vec<DirectorySummaryScope>,
    pub search: String,
    pub kind: String,
    pub offset: usize,
    pub limit: usize,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectorySummaryScope {
    pub snapshot_id: String,
    pub schema: String,
    pub names: Option<Vec<String>>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectorySummary {
    pub name: String,
    pub object_type: String,
    pub snapshot_id: String,
    pub contributors: Vec<String>,
    pub conflict: bool,
}
#[derive(Clone)]
pub struct RuntimeControl {
    pub port: Arc<dyn RuntimeControlPort>,
}
#[derive(Clone)]
pub enum ControlAction {
    /// Short live mount mutation after isolated preparation; no metadata scan.
    Mount {
        previous: Option<modorch_fedengine_core::SourceRevision>,
        next: Option<modorch_fedengine_core::SourceRevision>,
    },
    ClearCache(modorch_fedengine_core::SourceRevision),
}
pub struct ControlOutcome {
    pub schemas: Vec<String>,
    pub catalog: Option<modorch_fedengine_core::CatalogSnapshot>,
    /// True only after the previous applied definition was successfully reattached.
    pub restored_previous: bool,
    pub error: Option<EngineError>,
}
#[async_trait::async_trait]
pub trait RuntimeControlPort: Send + Sync {
    async fn discover_databases(
        &self,
        _connection: modorch_fedengine_core::SourceConnection,
        _cancel: CancellationToken,
        _ownership: Box<dyn Send>,
    ) -> Result<Vec<modorch_fedengine_core::DatabaseCandidate>> {
        Err(EngineError::unsupported(
            "instance discovery is not available",
        ))
    }

    /// Isolated remote collection; never changes live attachments or extension caches.
    async fn capture(
        &self,
        revision: modorch_fedengine_core::SourceRevision,
        cancel: CancellationToken,
        ownership: Box<dyn Send>,
    ) -> Result<modorch_fedengine_core::CatalogSnapshot>;

    async fn inspect(
        &self,
        revision: modorch_fedengine_core::SourceRevision,
        cancel: CancellationToken,
        ownership: Box<dyn Send>,
    ) -> Result<ControlOutcome>;
    /// Caller owns the instance write barrier, then sorted source locks.
    /// Ownership (including service leases) survives until blocking I/O really exits.
    async fn control(
        &self,
        action: ControlAction,
        ownership: Box<dyn Send>,
    ) -> Result<ControlOutcome>;
}

pub const MANAGEMENT_TOPIC: &str = "management/control/v1";
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualDatabaseTarget {
    pub oid: u32,
    pub expected_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum VirtualDatabaseMutation {
    Create {
        definition: modorch_fedengine_core::VirtualDatabaseConfig,
    },
    Replace {
        name: String,
        target: VirtualDatabaseTarget,
        definition: modorch_fedengine_core::VirtualDatabaseConfig,
    },
    Delete {
        name: String,
        target: VirtualDatabaseTarget,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VirtualDatabaseReceipt {
    pub idempotency_key: String,
    pub name: String,
    pub oid: u32,
    pub revision: u64,
    pub deleted: bool,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveryJob {
    pub id: String,
    pub status: String,
    pub connection: modorch_fedengine_core::SourceConnection,
    pub candidates: Vec<modorch_fedengine_core::DatabaseCandidate>,
    pub error: Option<String>,
}
#[derive(Clone, serde::Serialize)]
pub enum DiscoveryRequest {
    Start {
        id: String,
        issued_at: i64,
        connection: modorch_fedengine_core::SourceConnection,
    },
    Get(String),
    Cancel(String),
}
#[derive(Clone, serde::Serialize)]
pub enum ManagementRequest {
    Discovery(DiscoveryRequest),
    VirtualDatabases,
    VirtualDatabase(String),
    MutateVirtualDatabase {
        mutation: VirtualDatabaseMutation,
        idempotency_key: String,
    },
    Submit {
        action: modorch_fedengine_core::ManagementAction,
        idempotency_key: String,
    },
    List,
    Get(DataSourceId),
    Operation(String),
    Operations {
        offset: usize,
        limit: usize,
        search: String,
    },
    Catalog {
        object: Option<String>,
        snapshot_id: Option<String>,
        id: DataSourceId,
        schema: Option<String>,
        object_type: Option<String>,
        offset: usize,
        limit: usize,
    },
    /// Trusted notification after confirmed DDL; never exposed as an HTTP action.
    DdlInvalidated(DataSourceId),
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceView {
    #[serde(flatten)]
    pub source: modorch_fedengine_core::ManagedSource,
    pub desired: modorch_fedengine_core::SourceRevision,
    pub applied: Option<modorch_fedengine_core::SourceRevision>,
    pub catalog_updated_at: Option<String>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogPage {
    pub snapshot_id: String,
    pub incarnation: String,
    pub revision: u64,
    pub generation: u64,
    pub refreshed_at: String,
    pub stale: bool,
    pub schemas: Vec<String>,
    pub total: usize,
    pub offset: usize,
    pub objects: Vec<modorch_fedengine_core::CatalogObject>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct OperationPage {
    pub items: Vec<modorch_fedengine_core::ManagementOperation>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum ManagementReply {
    Discovery(DiscoveryJob),
    VirtualDatabases(Vec<modorch_fedengine_core::VirtualDatabase>),
    VirtualDatabase(modorch_fedengine_core::VirtualDatabase),
    VirtualDatabaseReceipt(VirtualDatabaseReceipt),
    Sources(Vec<SourceView>),
    Source(Box<SourceView>),
    Operation(modorch_fedengine_core::ManagementOperation),
    Operations(OperationPage),
    Catalog(CatalogPage),
}

impl modorch_fedengine_core::HubPayloadBytes for ManagementRequest {
    fn hub_payload_bytes(&self) -> usize {
        modorch_fedengine_core::serialized_payload_bytes(self)
    }
}
impl modorch_fedengine_core::HubPayloadBytes for QueryRequest {
    fn hub_payload_bytes(&self) -> usize {
        fn cell(value: &Cell, depth: usize) -> usize {
            if depth >= 64 {
                return usize::MAX;
            }
            match value {
                Cell::Text(s) | Cell::Numeric(s) | Cell::NamedOid { name: s, .. } => {
                    64usize.saturating_add(s.len())
                }
                Cell::Bytes(b) => 64usize.saturating_add(b.len()),
                Cell::Array { values, .. } => values
                    .iter()
                    .fold(64usize, |n, v| n.saturating_add(cell(v, depth + 1))),
                _ => 64,
            }
        }
        let n = self.session.settings.iter().fold(512usize, |n, (k, v)| {
            n.saturating_add(k.len())
                .saturating_add(v.len())
                .saturating_add(64)
        });
        let n = self
            .session
            .search_path
            .iter()
            .flatten()
            .fold(n, |n, v| n.saturating_add(v.name.len()).saturating_add(32));
        let n = n
            .saturating_add(self.session.database.len())
            .saturating_add(self.principal.len())
            .saturating_add(self.sql.len());
        match &self.mode {
            QueryMode::Prepare { parameter_types } => {
                n.saturating_add(parameter_types.len().saturating_mul(16))
            }
            QueryMode::Execute {
                parameters,
                parameter_types,
                expected,
                ..
            } => {
                let n = parameters
                    .iter()
                    .fold(n, |n, v| n.saturating_add(cell(v, 0)));
                let n = expected.iter().fold(n, |n, (id, version)| {
                    n.saturating_add(id.0.len())
                        .saturating_add(version.incarnation.len())
                        .saturating_add(128)
                });
                n.saturating_add(parameter_types.len().saturating_mul(16))
            }
        }
    }
}

#[cfg(test)]
mod message_budget_tests {
    use super::*;
    use modorch_fedengine_core::HubPayloadBytes;
    #[test]
    fn query_payload_cost_accounts_for_variable_input_and_rejects_deep_arrays() {
        let mut request = QueryRequest {
            session: SessionContext::default(),
            principal: "owner".into(),
            session_id: 1,
            sql: "SELECT $1".into(),
            mode: QueryMode::Execute {
                parameters: vec![],
                parameter_types: vec![],
                expected: BTreeMap::new(),
                expected_database: None,
            },
            deadline: Instant::now(),
            cancel: CancellationToken::new(),
        };
        let base = request.hub_payload_bytes();
        request.sql.push_str(&"x".repeat(1024));
        request
            .session
            .settings
            .insert("application_name".into(), "y".repeat(2048));
        if let QueryMode::Execute {
            parameters,
            expected,
            ..
        } = &mut request.mode
        {
            parameters.push(Cell::Bytes(vec![0; 4096]));
            expected.insert(
                DataSourceId("source".into()),
                SourceVersion {
                    incarnation: "z".repeat(512),
                    revision: 1,
                    generation: 1,
                },
            );
        }
        assert!(request.hub_payload_bytes() >= base + 1024 + 2048 + 4096 + 512);
        let mut value = Cell::Null;
        for _ in 0..64 {
            value = Cell::Array {
                lower_bound: 1,
                values: vec![value],
            };
        }
        if let QueryMode::Execute { parameters, .. } = &mut request.mode {
            parameters.push(value);
        }
        assert_eq!(request.hub_payload_bytes(), usize::MAX);
    }
}
