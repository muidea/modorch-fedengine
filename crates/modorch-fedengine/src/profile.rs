use modorch::FrameworkError;

/// Supported full component assemblies; the host still explicitly selects their IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FedEngineProfile {
    /// PGWire plus core capabilities; HTTP is independent of managed storage mode.
    PgServer {
        /// Register management HTTP (requires `config.management.enabled`).
        management_api: bool,
    },
    /// Managed core and HTTP, optionally with PGWire. Requires managed storage mode.
    ManagedServer {
        /// Register PGWire and validate its transport/authentication configuration.
        pgwire: bool,
    },
    /// Reserved; not supported until the core-only capability contract is verified.
    EmbeddedCore,
}

impl FedEngineProfile {
    pub(crate) fn ingress(self) -> Result<(bool, bool), FedEngineError> {
        match self {
            Self::PgServer { management_api } => Ok((true, management_api)),
            Self::ManagedServer { pgwire } => Ok((pgwire, true)),
            Self::EmbeddedCore => Err(FedEngineError::UnsupportedProfile(self)),
        }
    }
}

/// Registration/observation error, preserving the underlying framework error.
#[derive(Debug, thiserror::Error)]
pub enum FedEngineError {
    /// The requested assembly has not been implemented and accepted.
    #[error("unsupported FedEngine profile: {0:?}")]
    UnsupportedProfile(FedEngineProfile),
    /// Framework validation, registration or observation identity error.
    #[error(transparent)]
    Framework(#[from] FrameworkError),
}
