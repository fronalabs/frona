//! Configuration data, startup loading and atomic file persistence.
//!
//! Load with `ConfigService::load`, then construct `ConfigService::new`.
//! Saving updates the persisted document, not the active snapshot.

mod document;
mod loader;
mod service;
mod types;

pub use document::*;
pub use loader::LoadedConfig;
pub(crate) use service::validate_document;
pub use service::{ConfigService, SaveResult};
pub use types::*;

#[cfg(test)]
mod tests;
