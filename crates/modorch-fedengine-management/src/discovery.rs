//! Read-only, bounded discovery jobs. Activation uses durable per-target management operations.
use crate::coordinator::Coordinator;
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub(super) struct DiscoveryJobs(Arc<Mutex<BTreeMap<String, Job>>>);
impl DiscoveryJobs {
    pub fn running(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .values()
            .filter(|j| j.view.status == "running")
            .count()
    }
}
struct Job {
    digest: Vec<u8>,
    expires: i64,
    cancel: CancellationToken,
    view: DiscoveryJob,
}
fn engine(_: modorch::FrameworkError) -> EngineError {
    EngineError::new("53300", "discovery resource unavailable")
}
impl Coordinator {
    pub async fn discovery(&self, request: &DiscoveryRequest) -> Result<DiscoveryJob> {
        let now = chrono::Utc::now().timestamp();
        let (id, issued_at, connection) = match request {
            DiscoveryRequest::Get(id) | DiscoveryRequest::Cancel(id) => {
                let jobs = self.discovery.0.lock().unwrap();
                let job = jobs
                    .get(id)
                    .filter(|j| j.view.status == "running" || j.expires >= now)
                    .ok_or_else(|| {
                        EngineError::new(
                            "42704",
                            "discovery expired or does not exist; start a new discovery",
                        )
                    })?;
                if matches!(request, DiscoveryRequest::Cancel(_)) {
                    job.cancel.cancel();
                }
                return Ok(job.view.clone());
            }
            DiscoveryRequest::Start {
                id,
                issued_at,
                connection,
            } => (id, *issued_at, connection),
        };
        connection.validate()?;
        uuid::Uuid::parse_str(id).map_err(|_| EngineError::config("invalid discovery id"))?;
        if issued_at < now - 300 || issued_at > now + 30 {
            return Err(EngineError::config("discovery submission expired"));
        }
        let digest = Sha256::digest(serde_json::to_vec(&(issued_at, connection)).unwrap()).to_vec();
        let leases = (
            self.control.acquire().map_err(engine)?,
            self.storage.acquire().map_err(engine)?,
        );
        let pending = leases
            .1
            .port
            .read()
            .await?
            .operations
            .values()
            .filter(|o| !o.status.finished())
            .count();
        let cancel = CancellationToken::new();
        let view = DiscoveryJob {
            id: id.clone(),
            status: "running".into(),
            connection: connection.clone(),
            candidates: vec![],
            error: None,
        };
        {
            let mut jobs = self.discovery.0.lock().unwrap();
            jobs.retain(|_, j| j.view.status == "running" || j.expires >= now);
            if let Some(job) = jobs.get(id) {
                return if job.digest == digest {
                    Ok(job.view.clone())
                } else {
                    Err(EngineError::new(
                        "40001",
                        "discovery id was used for another request",
                    ))
                };
            }
            if jobs.len() >= 16
                || jobs.values().filter(|j| j.view.status == "running").count()
                    >= self
                        .config
                        .management
                        .queue_capacity
                        .saturating_sub(pending)
            {
                return Err(EngineError::new("53300", "discovery job capacity reached"));
            }
            jobs.insert(
                id.clone(),
                Job {
                    digest,
                    expires: now + 300,
                    cancel: cancel.clone(),
                    view: view.clone(),
                },
            );
        }
        let coordinator = self.clone();
        let id = id.clone();
        let connection = connection.clone();
        // Acquire before scheduling; the worker retains these leases and permits until actual I/O exit.
        let failure_id = id.clone();
        if let Err(error) = self.tasks.spawn(move |shutdown| async move {
            let result = async {
                let slot = tokio::select! { biased;
                    _ = shutdown.cancelled() => return Err(EngineError::new("57014", "discovery stopped")),
                    _ = cancel.cancelled() => return Err(EngineError::new("57014", "discovery canceled")),
                    slot = coordinator.slots.clone().acquire_owned() => slot.map_err(|_| EngineError::new("57P01", "management stopped"))?,
                };
                let memory = tokio::select! {
                    _ = shutdown.cancelled() => return Err(EngineError::new("57014", "discovery stopped")),
                    _ = cancel.cancelled() => return Err(EngineError::new("57014", "discovery canceled")),
                    memory = coordinator.memory.clone().acquire_many_owned(coordinator.config.management.metadata_operation_memory_mb) => memory.map_err(|_| EngineError::new("57P01", "management stopped"))?,
                };
                let state = leases.1.port.read().await?;
                let port = leases.0.port.clone();
                let mut candidates = port.discover_databases(connection.clone(), cancel.clone(), Box::new((slot,memory,leases))).await?;
                for candidate in &mut candidates {
                    let key = SourceOrganization { connection_id: connection.id.clone(), instance: connection.instance.clone(), database: candidate.database.clone(), virtual_database: "discovery".into(), owner: "discovery".into(), connect: vec![] }.native_key(connection.kind);
                    for source in state.sources.values().filter(|s| !s.deleted) {
                        let definition = &state.revision(&source.incarnation, source.desired_revision)?.definition;
                        if let Some(target) = &definition.organization && target.native_key(definition.kind) == key {
                            candidate.virtual_database = Some(target.virtual_database.clone());
                            candidate.merged = true;
                            break;
                        }
                    }
                    if candidate.virtual_database.is_none() && candidate.status == DatabaseDiscoveryStatus::Available {
                        let name = format!("{}_{}", connection.id.0, candidate.database);
                        candidate.virtual_database = Some(name.chars().take(63).collect());
                    }
                }
                Ok(candidates)
            }.await;
            if let Some(job) = coordinator.discovery.0.lock().unwrap().get_mut(&id) {
                job.expires = chrono::Utc::now().timestamp() + 300;
                match result {
                    Ok(candidates) => { job.view.status = "succeeded".into(); job.view.candidates = candidates; },
                    Err(e) => { job.view.status = if e.sqlstate == "57014" {"canceled"} else {"failed"}.into(); job.view.error = Some(e.to_string()); },
                }
            }
            Ok(())
        }) {
            self.discovery.0.lock().unwrap().remove(&failure_id);
            return Err(engine(error));
        }
        Ok(view)
    }
}
