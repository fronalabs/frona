//! PKM memory storage - the SurrealDB layer. All raw SurrealQL for the PKM
//! knowledge base lives here; the service/consolidation/tool layers call these
//! methods and never touch `db` directly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use surrealdb::types::{RecordId, SurrealValue};

use crate::chat::message::models::MessageStatus;
use crate::core::error::AppError;
use crate::core::repository::new_id;

use crate::memory::pkm::model::{
    Disposition, ENTITY_NAME_PROPERTY_IRI, ENTITY_PATH_PROPERTY_IRI, EntityCategory, EntityHit,
    EntityOrigin, KnowledgeEntity, KnowledgeEntityLink, KnowledgeEntitySource, KnowledgeMemory,
    KnowledgeOntology, KnowledgeShortMemory, LinkOrigin, MemoryEvidence, MemoryKind,
    MemoryRelation, PLAYBOOK_KIND_IRI, RankedEntityHit, RelationType, SELF_ENTITY_PATH,
    derive_resolution_search, derive_search_text,
};
use crate::memory::pkm::{KnowledgeConsolidationRecord, PendingEntityContribution};

const SELECT: &str = "SELECT *, meta::id(id) as id";

/// Attempts for a transaction the engine reports as a retryable write conflict.
const CONFLICT_RETRIES: usize = 4;

macro_rules! tx_try {
    ($tx:ident, query $query:expr, $ctx:literal) => {
        match $query.await.and_then(|response| response.check()) {
            Ok(response) => response,
            Err(error) => {
                let _ = $tx.cancel().await;
                return Err(Self::err($ctx, error));
            }
        }
    };
    ($tx:ident, $result:expr, $ctx:literal) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                let _ = $tx.cancel().await;
                return Err(Self::err($ctx, error));
            }
        }
    };
}

mod entity;
mod entity_link;
mod extraction;
mod memory;
mod ontology;
mod reset;
mod short_memory;
mod types;

pub mod consolidation;

pub use crate::memory::pkm::model::{
    ConsolidationEntityLifecycle, ConsolidationEntityLink, KnowledgeConsolidationEntity,
};
pub use consolidation::{PkmConsolidationRepo, PkmConsolidationStore};
pub use types::{
    AttributeOps, AuthoredPageWrite, ExternalPageProgress, IngestBatch, IngestCounts, IngestWindow,
    PendingEntity, PendingEntityUpdate, PendingMemory, PlaybookResolutionWrite,
};
pub(crate) use types::{
    ExternalExtractionWrite, ExtractCommit, PageEditBase, PageEditCommit, PageEditMemoryOp,
    PageEditWrite, ReconcileCommit, ReconcileEntityLinkSourceWrite, ReconcileMemoryRelationWrite,
    ReconcileOutdatedWrite,
};

#[derive(Clone)]
pub struct PkmRepo {
    db: Surreal<Db>,
    /// Max hits returned by `search_entities` (`memory.pkm_search_top_k`).
    search_top_k: i64,
}

impl PkmRepo {
    pub fn new(db: Surreal<Db>, search_top_k: i64) -> Self {
        Self { db, search_top_k }
    }

    fn err(ctx: &str, e: impl std::fmt::Display) -> AppError {
        AppError::Database(format!("pkm/{ctx}: {e}"))
    }
}

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
