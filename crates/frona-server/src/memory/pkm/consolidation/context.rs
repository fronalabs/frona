//! Shared state for one consolidation pass.
//!
//! Tuning values stay on the stages that use them rather than in this context.

use std::sync::Arc;

use crate::core::error::AppError;
use crate::db::repo::pkm::PkmConsolidationStore;
use crate::db::repo::pkm::PkmRepo;
use crate::memory::pkm::graph::EntityViewManager;

use crate::memory::pkm::consolidation::inference::ConsolidationInference;
use crate::memory::pkm::consolidation::{
    ConsolidationScope, ConsolidationStageState, ConsolidationStats, KnowledgeConsolidationRecord,
};
use crate::memory::pkm::page::projection::write_page_and_rev;
use crate::memory::pkm::rename;
use crate::memory::pkm::storage::PkmStorage;

pub struct ConsolidationContext {
    pub scope: ConsolidationScope,
    pub repo: Arc<PkmRepo>,
    /// Entity reads and searches must use this run-scoped view.
    pub view: EntityViewManager,
    pub storage: PkmStorage,
    pub llm: ConsolidationInference,
    /// The pass record must remain shared so a stage can make item progress durable.
    ///
    /// Deliberately opaque: this prevents the shared context from depending on
    /// stage-specific checkpoint payloads.
    record: tokio::sync::Mutex<KnowledgeConsolidationRecord>,
}

impl ConsolidationContext {
    pub fn new(
        scope: ConsolidationScope,
        repo: Arc<PkmRepo>,
        storage: PkmStorage,
        llm: ConsolidationInference,
        record: KnowledgeConsolidationRecord,
    ) -> Self {
        let view = EntityViewManager::new(
            PkmConsolidationStore::new(repo.clone())
                .scoped(&record.consolidation_id, &record.user_id),
        );
        Self {
            scope,
            repo,
            view,
            storage,
            llm,
            record: tokio::sync::Mutex::new(record),
        }
    }

    /// A fresh context for mining or authoring work outside an open sweep.
    ///
    /// Mining returns a prepared batch. A caller that commits it uses this fresh record
    /// with `commit_extract_patch_with_checkpoint`. Focused authoring uses only its scoped
    /// entity view. Keeping a real record here avoids optional checkpoint state
    /// in every consolidation stage.
    pub fn detached(
        scope: ConsolidationScope,
        repo: Arc<PkmRepo>,
        storage: PkmStorage,
        llm: ConsolidationInference,
    ) -> Self {
        let user_id = scope.user_id.clone();
        Self::new(
            scope,
            repo,
            storage,
            llm,
            KnowledgeConsolidationRecord {
                id: crate::core::repository::new_id(),
                consolidation_id: crate::core::repository::new_id(),
                user_id,
                state: ConsolidationStageState::Ingest(Default::default()),
                stats: ConsolidationStats::default(),
                attempts: 0,
                restart_count: 0,
                failure: None,
                next_attempt_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
    }

    /// Commit the exact page bytes and manifest revision, then update the `.md` mirror.
    /// Recovery retries a delayed or failed mirror write from the durable bytes.
    pub async fn write_page_and_rev(&self, path: &str, file: &str) -> Result<String, AppError> {
        write_page_and_rev(
            &self.repo,
            &self.storage,
            &self.scope.vault,
            &self.scope.user_id,
            path,
            file,
        )
        .await
    }

    pub(super) async fn stage(&self) -> ConsolidationStageState {
        self.record.lock().await.state.clone()
    }

    /// Persist the stage state together with the counts the item produced.
    ///
    /// The counts have to travel with the marker rather than being tallied and folded in
    /// at the end: a resumed stage sees only the items it still owes, so a local total
    /// would lose everything finished before the crash.
    pub(super) async fn persist_with_stats(
        &self,
        state: ConsolidationStageState,
        absorb: impl FnOnce(&mut ConsolidationStats),
    ) {
        let mut record = self.record.lock().await;
        record.state = state;
        absorb(&mut record.stats);
        if let Err(e) = self.repo.save_consolidation_record(&record).await {
            tracing::warn!(
                error = %e,
                stage = record.state.label(),
                "pkm consolidation: checkpoint failed — the pass continues, but a crash \
                 now would replay from the last stage that saved"
            );
        }
    }

    /// Persist state whose effects exist only in the consolidation record.
    ///
    /// Classify does not mutate the knowledge graph until its final transaction, so
    /// continuing after one of its checkpoint writes fails would acknowledge model work
    /// that cannot be reconstructed after a restart.
    pub(super) async fn persist_required(
        &self,
        state: ConsolidationStageState,
    ) -> Result<(), AppError> {
        self.persist_required_with_stats(state, |_| {}).await
    }

    pub(super) async fn persist_required_with_stats(
        &self,
        state: ConsolidationStageState,
        absorb: impl FnOnce(&mut ConsolidationStats),
    ) -> Result<(), AppError> {
        let mut record = self.record.lock().await;
        record.state = state;
        absorb(&mut record.stats);
        self.repo.save_consolidation_record(&record).await
    }

    /// Fold in a stage's counts without moving the record on - for the stages that return
    /// an outcome at the end rather than banking per item.
    pub(super) async fn absorb(&self, f: impl FnOnce(&mut ConsolidationStats)) {
        let state = self.record.lock().await.state.clone();
        self.persist_with_stats(state, f).await;
    }

    pub(super) async fn finish_cleanup(
        &self,
        outcome: super::CleanupOutcome,
    ) -> Result<(), AppError> {
        let mut record = self.record.lock().await;
        record.stats.absorb_cleanup(outcome);
        record.state = ConsolidationStageState::Done;
        record.attempts = 0;
        self.repo.complete_consolidation(&record).await
    }

    /// Move the record on to the next stage. Advancing resets the retry budget: the pass
    /// made progress, so whatever went wrong before was not this stage refusing to move.
    pub(super) async fn advance(&self) -> Result<ConsolidationStageState, AppError> {
        let next = {
            let mut record = self.record.lock().await;
            record.state = record.state.next();
            record.attempts = 0;
            record.state.clone()
        };
        self.persist_required(next.clone()).await?;
        Ok(next)
    }

    pub(crate) async fn record(&self) -> KnowledgeConsolidationRecord {
        self.record.lock().await.clone()
    }

    /// Mirror a record transition that already committed atomically with live graph
    /// changes. This updates only the in-process cursor; writing it again here would
    /// reopen the crash window the atomic transaction closed.
    pub(super) async fn adopt_committed_record(&self, record: KnowledgeConsolidationRecord) {
        *self.record.lock().await = record;
    }

    /// Rename a page and every reference to it - see [`rename::page_everywhere`], which
    /// the sync engine shares.
    pub async fn rename_page_everywhere(&self, from: &str, to: &str) -> Result<(), AppError> {
        rename::page_everywhere(
            &self.repo,
            &self.storage,
            &self.scope.vault,
            &self.scope.user_id,
            from,
            to,
        )
        .await
    }
}
