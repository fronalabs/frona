//! Validated external model metadata and persistent catalog downloads.
//! Provider execution, account access and server configuration are not catalog policy.

pub mod catalog;
pub mod loader;
pub mod parameters;
pub mod sources;

pub use catalog::{ModelCatalogSnapshot, ModelCatalogStore, ModelEntry};
pub use sources::{CatalogSources, Source, SourceStatus};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("{0}")]
    Internal(String),
    #[error("{0}")]
    Validation(String),
}
