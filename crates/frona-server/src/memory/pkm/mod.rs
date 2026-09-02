//! PKM (personal knowledge management) memory service.
//!
//! A background process turns chat transcripts into a user-scoped knowledge
//! base (atomic immutable entries ↔ entity nodes in SurrealDB), projected to
//! one markdown knowledge page per entity on disk, read by the foreground agent
//! through a pull-only tool surface. Selected at boot behind
//! [`crate::memory::service::MemoryService`].
//!
//! Modules: `model` (records), `projection` (what a page is on disk - the markdown it
//! composes to and the `rev` that identifies those bytes), `storage` (where that file
//! goes), `consolidation` (the background pipeline and its LLM seam), `ontology` (the OWL
//! layer), `recovery` (boot-time projection repair), `sync` (the Obsidian sync engine),
//! and `tools` (foreground remember, search, cite, and graph tools).
//! Classify's ontology tools are consolidation-internal and live in
//! `consolidation::tools::ontology`,
//! never in the agent registry. The SurrealQL lives in `crate::db::repo::pkm`.
//!
//! # Naming
//!
//! **`pkm` names the subsystem; `knowledge` names the persisted artifacts.** Module
//! paths, config keys, routes, and log prefixes use `pkm`; entity types and SurrealDB
//! tables use `Knowledge*` / `knowledge_*`. Semantic records use `entity`; `page` is
//! reserved for the authored Wiki projection. `wiki` is retired - do not reintroduce it. Log messages are
//! `pkm <stage>: <what>`, so the whole subsystem filters on one prefix.

pub mod model;
pub mod ontology;
pub mod read;
pub mod sync;

mod consolidation;
mod operations;
mod projection;
mod recovery;
mod rename;
mod reset;
mod retrieve;
mod search;
mod service;
mod storage;
mod sweep;
mod tools;
mod vault;

use consolidation::{ConsolidationContext, ConsolidationInference, Consolidator, Ingest};
pub use consolidation::{
    ConsolidationFailure, ConsolidationScope, ConsolidationStageState, ConsolidationStats,
    ConsolidationWorkState, IngestState, KnowledgeConsolidationRecord, PendingEntityContribution,
    PlaybookResolveState, ResearchCoverageStats, TemporalSource, TranscriptEvidenceKind,
    TranscriptEvidenceSource,
};
pub use model::PendingPlaybookCandidate;
pub use projection::sha256_hex;
pub use reset::{PkmResetState, PkmResetStatus};
pub use storage::PkmStorage;
pub use vault::VaultScope;

use std::sync::Arc;

use async_trait::async_trait;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

use std::time::Duration;

use crate::agent::prompt::PromptLoader;
use crate::core::config::MemoryConfig;
use crate::core::error::AppError;
use crate::db::repo::tool_calls::SurrealToolCallRepo;
use crate::inference::ModelProviderRegistry;
use crate::memory::service::{MemoryContext, MemoryService};
use crate::scheduler::Scheduler;
use crate::storage::StorageService;
use crate::tool::AgentTool;

use crate::db::repo::pkm::PkmRepo;

#[derive(Clone)]
pub struct PkmService {
    repo: Arc<PkmRepo>,
    /// Read-only handle on persisted tool calls. Playbook Author reconstructs invocation
    /// evidence from the source messages of its procedural memories.
    tool_calls: Arc<SurrealToolCallRepo>,
    messages: crate::db::repo::messages::SurrealMessageRepo,
    storage: PkmStorage,
    /// Kept only to resolve the background model group; all inference goes through
    /// the harness passed to `consolidate`.
    registry: Arc<ModelProviderRegistry>,
    prompts: PromptLoader,
    memory_config: MemoryConfig,
    /// For the self-entity → `User` write-through (`{name, timezone}`).
    user_service: crate::auth::user_service::UserService,
    /// The manager is always present; its catalogue can be absent until repair completes.
    ontology_manager: ontology::OntologyManager,
    operations: operations::PkmOperationCoordinator,
    reset_state: reset::PkmResetStateStore,
}

/// Resolve the background model group by name, falling back to `primary`. Shared by
/// `PkmService` and `PkmSyncService` (each supplies its own "undefined" error message).
/// `None` if neither the configured group nor `primary` is defined.
pub(crate) fn resolve_model_group<'a>(
    registry: &'a ModelProviderRegistry,
    configured: &str,
) -> Option<&'a crate::inference::config::ModelGroup> {
    registry
        .get_model_group(configured)
        .or_else(|_| registry.get_model_group("primary"))
        .ok()
}

#[async_trait]
impl MemoryService for PkmService {
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        tools::all(
            self.repo.clone(),
            self.storage.clone(),
            self.ontology_manager.clone(),
            self.prompts.clone(),
            self.user_service.clone(),
        )
    }

    fn register_maintenance(&self, scheduler: &Scheduler) {
        let reset_recovery = self.clone();
        tokio::spawn(async move {
            if let Err(e) = reset_recovery.recover_reset_requests().await {
                tracing::error!(error = %e, "pkm reset recovery failed");
            }
        });

        // Boot-time crash recovery: finish any rename left half-applied on disk before
        // the sweep runs. One-shot, so it's spawned rather than registered periodic.
        let recover = self.clone();
        tokio::spawn(async move {
            if let Err(e) = recover.reconcile_vault().await {
                tracing::warn!(error = %e, "pkm vault recovery failed");
            }
        });

        // Repair the ontology release if the image shipped without one, or its copy is
        // damaged. A no-op in the normal case - it verifies and returns. Spawned rather
        // than awaited so a slow or absent network cannot hold up boot; consolidation
        // skips its ticks until this lands.
        let ontology = self.ontology_manager.clone();
        tokio::spawn(async move {
            if let Err(e) = ontology.repair().await {
                tracing::error!(error = %e, "ontology release repair failed");
            }
        });

        let interval = Duration::from_secs(self.memory_config.pkm_consolidate_secs.max(1));
        let me = self.clone();
        let chat_service = scheduler.app_state.chat_service.clone();
        let contact_service = scheduler.app_state.contact_service.clone();
        let agent_service = scheduler.app_state.agent_service.clone();
        let harness = scheduler.app_state.harness.clone();
        scheduler.register_periodic(interval, "pkm_consolidation", move || {
            let me = me.clone();
            let chat_service = chat_service.clone();
            let contact_service = contact_service.clone();
            let agent_service = agent_service.clone();
            let harness = harness.clone();
            async move {
                me.run_consolidation_sweep(
                    &chat_service,
                    &contact_service,
                    &agent_service,
                    &harness,
                )
                .await
            }
        });
    }

    async fn retrieve(&self, mcx: &mut MemoryContext<'_>) -> Result<(), AppError> {
        self.retrieve_into(mcx).await
    }
}
