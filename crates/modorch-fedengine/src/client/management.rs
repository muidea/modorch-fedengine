//! Explicit host grants and bounded CAS/idempotency control requests.
use super::*;

/// Explicit, least-privilege host management authorization. Default denies all.
/// A configured SQL identity does not imply this grant. Treat these values as
/// trusted host policy, not values supplied by network callers. Grants are fixed
/// for the capability lifetime; drop/rebind clients when host policy changes.
#[derive(Clone, Debug, Default)]
pub struct FedEngineManagementGrant {
    sources: std::collections::BTreeSet<String>,
    databases: std::collections::BTreeSet<String>,
    writes: bool,
}
impl FedEngineManagementGrant {
    /// Grant read access to exactly this source ID (not a wildcard).
    pub fn source(mut self, id: impl Into<String>) -> Self {
        self.sources.insert(id.into());
        self
    }
    /// Grant read access to exactly this virtual database name.
    pub fn database(mut self, name: impl Into<String>) -> Self {
        self.databases.insert(name.into());
        self
    }
    /// Explicitly allow writes within the source/database scopes above.
    pub fn allow_writes(mut self) -> Self {
        self.writes = true;
        self
    }
    fn source_allowed(&self, id: &FedEngineSourceId) -> Result<()> {
        if self.sources.contains(&id.0) {
            Ok(())
        } else {
            Err(EngineError::denied("source outside host management grant"))
        }
    }
    fn database_allowed(&self, name: &str) -> Result<()> {
        if self.databases.contains(name) {
            Ok(())
        } else {
            Err(EngineError::denied(
                "database outside host management grant",
            ))
        }
    }
    fn write_allowed(&self) -> Result<()> {
        if self.writes {
            Ok(())
        } else {
            Err(EngineError::denied("host did not grant management writes"))
        }
    }
}
/// Scoped typed management capability. Uses existing CAS/idempotency handlers;
/// raw management requests, DDL notifications and global list APIs are not exposed.
#[derive(Clone)]
pub struct FedEngineManagementClient {
    pub(super) bound: Arc<BoundClient>,
    pub(super) grant: FedEngineManagementGrant,
}
impl FedEngineManagementClient {
    async fn call(
        &self,
        request: ManagementRequest,
        bytes: usize,
    ) -> Result<Arc<Result<ManagementReply>>> {
        let _permit = self.bound.admit()?;
        self.bound
            .control(
                MANAGEMENT_TOPIC,
                "source-manager",
                request,
                bytes,
                Instant::now() + self.bound.budget.timeout,
                None,
            )
            .await
    }
    fn bounded<T: serde::Serialize>(&self, value: &T) -> Result<usize> {
        measure(value, self.bound.budget.max_control_bytes)
    }
    /// Read one authorized source; no other source's state is returned.
    pub async fn source(&self, id: FedEngineSourceId) -> Result<FedEngineSourceView> {
        self.grant.source_allowed(&id)?;
        let reply = self
            .call(ManagementRequest::Get(id.clone()), id.0.len())
            .await?;
        match reply.as_ref() {
            Ok(ManagementReply::Source(view)) => {
                self.bounded(view)?;
                Ok((**view).clone())
            }
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid management reply")),
        }
    }
    /// Submit one authorized CAS operation with a caller-owned idempotency key.
    /// Timeout/drop after admission may leave durable work; inspect/retry the same
    /// key deliberately, never generate a new key as an automatic retry.
    pub async fn submit(
        &self,
        action: FedEngineManagementAction,
        key: &str,
    ) -> Result<FedEngineManagementOperation> {
        self.grant.write_allowed()?;
        self.grant.source_allowed(action.id())?;
        key_valid(key)?;
        if let FedEngineManagementAction::Register { definition } = &action
            && let Some(org) = &definition.organization
        {
            self.grant.database_allowed(&org.virtual_database)?;
        }
        let bytes = self.bounded(&action)?.saturating_add(key.len());
        if bytes > self.bound.budget.max_control_bytes {
            return Err(EngineError::new(
                "54000",
                "management input budget exceeded",
            ));
        }
        let reply = self
            .call(
                ManagementRequest::Submit {
                    action,
                    idempotency_key: key.into(),
                },
                bytes,
            )
            .await?;
        self.operation_reply(reply.as_ref())
    }
    fn operation_reply(
        &self,
        reply: &Result<ManagementReply>,
    ) -> Result<FedEngineManagementOperation> {
        match reply {
            Ok(ManagementReply::Operation(op)) => {
                self.grant.source_allowed(op.action.id())?;
                self.bounded(op)?;
                Ok(op.clone())
            }
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new(
                "XX000",
                "invalid management operation reply",
            )),
        }
    }
    /// Read a durable operation only when its source is in this grant.
    pub async fn operation(&self, id: &str) -> Result<FedEngineManagementOperation> {
        key_valid(id)?;
        let reply = self
            .call(ManagementRequest::Operation(id.into()), id.len())
            .await?;
        self.operation_reply(reply.as_ref())
    }
    /// Read one authorized virtual database and its bounded mapping.
    pub async fn database(&self, name: &str) -> Result<FedEngineDatabase> {
        self.grant.database_allowed(name)?;
        let reply = self
            .call(ManagementRequest::VirtualDatabase(name.into()), name.len())
            .await?;
        match reply.as_ref() {
            Ok(ManagementReply::VirtualDatabase(db)) => {
                self.bounded(db)?;
                Ok(db.clone())
            }
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid virtual database reply")),
        }
    }
    /// Create/replace/delete an authorized virtual mapping. New mappings must also
    /// use explicitly granted sources. Existing CAS, stable OID and replay rules apply.
    pub async fn mutate_database(
        &self,
        mutation: FedEngineDatabaseMutation,
        key: &str,
    ) -> Result<FedEngineDatabaseReceipt> {
        self.grant.write_allowed()?;
        key_valid(key)?;
        let (name, definition) = match &mutation {
            VirtualDatabaseMutation::Create { definition } => (&definition.name, Some(definition)),
            VirtualDatabaseMutation::Replace {
                name, definition, ..
            } => (name, Some(definition)),
            VirtualDatabaseMutation::Delete { name, .. } => (name, None),
        };
        self.grant.database_allowed(name)?;
        if let Some(d) = definition {
            self.grant.database_allowed(&d.name)?;
            for mapping in &d.schemas {
                self.grant.source_allowed(&mapping.source)?;
            }
        }
        let bytes = self.bounded(&mutation)?.saturating_add(key.len());
        if bytes > self.bound.budget.max_control_bytes {
            return Err(EngineError::new(
                "54000",
                "management input budget exceeded",
            ));
        }
        let reply = self
            .call(
                ManagementRequest::MutateVirtualDatabase {
                    mutation,
                    idempotency_key: key.into(),
                },
                bytes,
            )
            .await?;
        match reply.as_ref() {
            Ok(ManagementReply::VirtualDatabaseReceipt(receipt)) => Ok(receipt.clone()),
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid virtual mutation reply")),
        }
    }
}
fn key_valid(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        Err(EngineError::config(
            "invalid management key or operation ID",
        ))
    } else {
        Ok(())
    }
}
fn measure<T: serde::Serialize>(value: &T, limit: usize) -> Result<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(b.len());
            if self.bytes > self.limit {
                Err(std::io::Error::other("budget exceeded"))
            } else {
                Ok(b.len())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| EngineError::new("54000", "management control budget exceeded"))?;
    Ok(writer.bytes)
}
