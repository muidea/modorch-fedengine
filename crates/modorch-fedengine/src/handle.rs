use crate::FedEngineError;
use modorch::{
    ApplicationHandle, ApplicationId, ApplicationState, ComponentHealth, ComponentRef,
    ComponentState, FrameworkError,
};
use modorch_fedengine_contract::EngineMetrics;
use modorch_fedengine_management::ManagementListenerStatus;
use modorch_fedengine_pgwire::ListenerStatus;
use std::{
    collections::BTreeMap,
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

/// Independently sampled session, worker and query counters; not a transaction.
pub use modorch_fedengine_contract::MetricsSnapshot as FedEngineMetrics;

/// How much of the registered profile is selected in the observed application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FedEngineObservation {
    /// The host has not associated a startup/cleanup ApplicationHandle yet.
    Unobserved,
    /// None of this registration's components were selected.
    NotSelected,
    /// Only part of the profile was selected; this cannot report profile readiness.
    PartialSelection,
    /// All profile components were selected; inspect application/component state.
    Selected,
}

/// Current framework lifecycle and component generations, sampled on demand.
///
/// Lifecycle, addresses and counters are separate snapshots. A Ready observation
/// is not a lease or a promise that a subsequent connection cannot race shutdown.
#[derive(Clone, Debug)]
pub struct FedEngineStatus {
    /// Associated framework application identity, absent until host observation.
    pub application_id: Option<ApplicationId>,
    /// Actual application state, including StopIncomplete/Forced without remapping.
    pub application_state: Option<ApplicationState>,
    /// Explicit selection coverage of this registered profile.
    pub observation: FedEngineObservation,
    /// True only when the entire profile and its application are currently Ready.
    pub ready: bool,
    /// Selected profile components, with real framework generations/retained failures.
    pub components: BTreeMap<ComponentRef, ComponentHealth>,
    /// Current application failure; component last_failure may also retain past failures.
    pub failure: Option<FrameworkError>,
    /// PGWire address, hidden before readiness and after shutdown/recovery begins.
    pub pgwire_address: Option<SocketAddr>,
    /// HTTP address, hidden before readiness and after shutdown/recovery begins.
    pub management_address: Option<SocketAddr>,
}

/// Cloneable, read-only observation. No Runtime, messaging, or shutdown capability is exposed.
///
/// The host calls `FedEngineRegisteredComponents::observe` after start returns
/// (or on its StartupError cleanup handle). This avoids claiming global Ready
/// from component hooks, which cannot observe the application's final barrier.
/// An associated handle privately retains an application reference for live reads;
/// the host remains responsible for checked shutdown and Runtime lifetime.
#[derive(Clone)]
pub struct FedEngineHandle {
    application: Arc<Mutex<Option<ApplicationHandle>>>,
    witness: Arc<Mutex<Option<ApplicationId>>>,
    metrics: Arc<EngineMetrics>,
    pg: Arc<ListenerStatus>,
    http: Arc<ManagementListenerStatus>,
    components: Arc<[ComponentRef]>,
}
impl fmt::Debug for FedEngineHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FedEngineHandle")
            .field("components", &self.components)
            .finish_non_exhaustive()
    }
}
impl FedEngineHandle {
    pub(crate) fn new(
        components: Vec<ComponentRef>,
        metrics: Arc<EngineMetrics>,
        pg: Arc<ListenerStatus>,
        http: Arc<ManagementListenerStatus>,
        witness: Arc<Mutex<Option<ApplicationId>>>,
    ) -> Self {
        Self {
            components: components.into(),
            metrics,
            pg,
            http,
            witness,
            application: Arc::default(),
        }
    }
    pub(crate) fn observe(&self, app: &ApplicationHandle) -> Result<(), FedEngineError> {
        let health = app.health();
        let selected = self
            .components
            .iter()
            .any(|id| health.components.contains_key(id));
        if selected && *self.witness.lock().unwrap() != Some(app.id()) {
            return Err(FrameworkError::InvalidConfig("cannot associate FedEngine observation: application identity was not witnessed by this bundle".into()).into());
        }
        let mut application = self.application.lock().unwrap();
        if application
            .as_ref()
            .is_some_and(|bound| bound.id() != app.id())
        {
            return Err(FrameworkError::InvalidConfig(
                "FedEngine observation is already associated with another application".into(),
            )
            .into());
        }
        *application = Some(app.clone());
        Ok(())
    }
    /// Read actual application/component state; unassociated observations never report Ready.
    pub fn status(&self) -> FedEngineStatus {
        let application = self.application.lock().unwrap().clone();
        let Some(app) = application else {
            return FedEngineStatus {
                application_id: None,
                application_state: None,
                observation: FedEngineObservation::Unobserved,
                ready: false,
                components: BTreeMap::new(),
                failure: None,
                pgwire_address: None,
                management_address: None,
            };
        };
        let mut health = app.health();
        health
            .components
            .retain(|id, _| self.components.contains(id));
        let observation = if health.components.is_empty() {
            FedEngineObservation::NotSelected
        } else if health.components.len() != self.components.len() {
            FedEngineObservation::PartialSelection
        } else {
            FedEngineObservation::Selected
        };
        let ready = observation == FedEngineObservation::Selected
            && health.ready
            && health
                .components
                .values()
                .all(|component| component.state == ComponentState::Ready);
        FedEngineStatus {
            application_id: Some(health.application_id),
            application_state: Some(health.state),
            observation,
            ready,
            components: health.components,
            failure: health.failure,
            pgwire_address: ready.then(|| self.pg.address()).flatten(),
            management_address: ready.then(|| self.http.address()).flatten(),
        }
    }
    /// Currently usable PGWire address; never returns a merely prebound/previous port.
    pub fn pgwire_address(&self) -> Option<SocketAddr> {
        self.status().pgwire_address
    }
    /// Currently usable management HTTP address, if selected and the profile is Ready.
    pub fn management_address(&self) -> Option<SocketAddr> {
        self.status().management_address
    }
    /// Sample counters without business SQL, message submission or resource mutation.
    pub fn metrics(&self) -> FedEngineMetrics {
        self.metrics.snapshot()
    }
}

#[cfg(test)]
mod tests;
