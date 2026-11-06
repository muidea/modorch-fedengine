//! Persistent source operation coordination and a lifecycle-managed HTTP ingress.
mod api;
mod browser;
mod browser_query;
mod coordinator;
mod module;
mod pagination;
mod reducer;
mod web;
pub use api::{ManagementApiFactory, ManagementListenerStatus};
pub use module::SourceManagerFactory;

mod discovery;
