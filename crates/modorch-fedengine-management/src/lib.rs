//! Persistent source operation coordination and a lifecycle-managed HTTP ingress.
mod api;
mod browser;
mod browser_query;
mod coordinator;
mod module;
mod pagination;
mod reducer;
mod web;
pub use api::{
    EmbeddedManagementApiFactory, ManagementApiFactory, ManagementListenerStatus,
    ManagementRouterSlot,
};
pub use module::SourceManagerFactory;

mod discovery;
