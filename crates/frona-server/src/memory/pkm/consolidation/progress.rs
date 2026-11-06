use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::core::error::AppError;
use crate::db::repo::pkm::ReconcileCommit;
use crate::memory::pkm::consolidation::ConsolidationStageState;
use crate::memory::pkm::consolidation::assemble::adjudicate::AdjudicationResult;
use crate::memory::pkm::consolidation::classify::{Classification, ProposalSet};
use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::consolidation::view::EntityTransition;
use crate::memory::pkm::model::{
    ClassificationProgress, IdentityProgress, KnowledgeConsolidationEntity, ReconciliationProgress,
};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ReconcilePromotion {
    pub key: String,
    pub property: String,
    pub target: String,
    pub source_memory_ids: Vec<String>,
    pub declaration: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub(super) struct AcceptedReconciliation {
    promotions: Vec<ReconcilePromotion>,
    retractions: Vec<(String, String)>,
}

impl AcceptedReconciliation {
    pub(super) fn promotions(&self) -> &[ReconcilePromotion] {
        &self.promotions
    }

    pub(super) fn retractions(&self) -> &[(String, String)] {
        &self.retractions
    }
}

/// Global Consolidator checkpoint data. Entity payload and per-entity progress live on
/// `KnowledgeConsolidationEntity` rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ConsolidationWorkState {
    /// Canonically ordered entity pair → the narrow identity evidence most recently
    /// presented to Resolve. It records only names, classes, and shared marker evidence.
    pub resolution_pair_fingerprints: BTreeMap<String, String>,
    /// The batch schema decision, once adjudicate has produced one.
    pub adjudicated: Option<serde_json::Value>,
    /// Per-term decisions accumulated across bounded adjudication batches.
    pub adjudicated_terms: BTreeMap<String, serde_json::Value>,
    /// Normalized vocabulary query → rendered catalogue hits. The catalogue is immutable
    /// for the lifetime of a pass, so every later classify iteration can reuse these.
    pub vocabulary_search_cache: BTreeMap<String, String>,
    /// Normalized entity query → `{ revision, result }`. Entity search is revision-scoped
    /// because an accepted classify/resolve iteration may mint or merge an entity.
    pub entity_search_cache: BTreeMap<String, serde_json::Value>,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkStage {
    Classify,
    Resolve,
    Reconcile,
    Assemble,
}

/// This stage's progress against the pass record.
///
/// Holds its own copy of the state, so reads are synchronous and the record's lock is
/// touched only to write. Every mutator persists before returning: a banked conversation
/// that is not on disk is the failure this whole type exists to prevent.
pub(crate) struct ConsolidationProgress<'a> {
    pub(super) ctx: &'a ConsolidationContext,
    pub(super) state: ConsolidationWorkState,
    stage: WorkStage,
    rows: BTreeMap<String, crate::memory::pkm::model::KnowledgeConsolidationEntity>,
    banked_classifications: BTreeMap<String, serde_json::Value>,
    pending_identity: BTreeMap<String, IdentityProgress>,
    pending_resolution_evidence: BTreeMap<String, serde_json::Value>,
    redirects: BTreeMap<String, String>,
    reconciliations: BTreeMap<String, AcceptedReconciliation>,
}

impl<'a> ConsolidationProgress<'a> {
    fn checkpoint_state(&self) -> ConsolidationStageState {
        match self.stage {
            WorkStage::Classify => ConsolidationStageState::Classify(self.state.clone()),
            WorkStage::Resolve => ConsolidationStageState::Resolve(self.state.clone()),
            WorkStage::Reconcile => ConsolidationStageState::Reconcile(self.state.clone()),
            WorkStage::Assemble => ConsolidationStageState::Assemble(self.state.clone()),
        }
    }

    pub(super) async fn advance_to(&mut self, stage: WorkStage) -> Result<(), AppError> {
        self.stage = stage;
        self.persist().await
    }

    pub(super) fn entity_row(
        &self,
        path: &str,
    ) -> Option<&crate::memory::pkm::model::KnowledgeConsolidationEntity> {
        self.rows.get(path)
    }

    pub(super) fn concept_rows(
        &self,
    ) -> impl Iterator<Item = &crate::memory::pkm::model::KnowledgeConsolidationEntity> {
        self.rows.values().filter(|row| {
            row.category == crate::memory::pkm::model::EntityCategory::Concept
                && row.lifecycle.searchable()
        })
    }

    pub(super) fn discarded_paths(&self) -> impl Iterator<Item = &String> {
        self.rows.iter().filter_map(|(path, row)| {
            (row.lifecycle == crate::memory::pkm::model::ConsolidationEntityLifecycle::Discarded)
                .then_some(path)
        })
    }

    pub(super) async fn checkpoint_transition(
        &mut self,
        stage: &str,
        item: &str,
        proposals: &ProposalSet,
    ) -> Result<(), AppError> {
        self.state.revision += 1;
        let working = proposals.trace_value();
        let mut checkpoint = self.ctx.record().await;
        checkpoint.state = self.checkpoint_state();
        checkpoint.updated_at = chrono::Utc::now();
        let mut transition = EntityTransition::new(checkpoint.clone());
        for mut row in proposals
            .input_entities
            .values()
            .cloned()
            .map(|entity| proposals.project_entity(entity))
        {
            let is_new = !self.rows.contains_key(&row.path);
            if row.path != item && !is_new {
                continue;
            }
            if let Some(durable) = self.rows.get(&row.path) {
                row.consolidation_entity_id = durable.consolidation_entity_id.clone();
                if row.entity_id.is_none() {
                    row.entity_id = durable.entity_id.clone();
                }
                row.progress = durable.progress.clone();
            }
            row.checkpoint_revision = self.state.revision;
            if row.path == item
                && let Some(decision) = self.banked_classifications.get(item)
            {
                row.progress.classification = ClassificationProgress::Accepted {
                    decision: decision.clone(),
                };
            }
            if row.path == item
                && let Some(identity) = self.pending_identity.get(item)
            {
                row.progress.identity = identity.clone();
            }
            row.rederive_search();
            transition = transition.with_row(row);
        }
        self.ctx.view.commit_transition(&transition).await?;
        self.rows.extend(
            transition
                .rows
                .iter()
                .cloned()
                .map(|row| (row.path.clone(), row)),
        );
        self.banked_classifications.remove(item);
        self.pending_identity.remove(item);
        self.ctx.adopt_committed_record(checkpoint).await;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            stage,
            item,
            &serde_json::json!({
                "checkpoint": self.state,
                "working": working,
            }),
        );
        Ok(())
    }

    /// Open against the pass record, or fail if it is not on this stage. Fallible once,
    /// at entry, rather than silently banking into the wrong variant at every call.
    pub(super) async fn open(ctx: &'a ConsolidationContext) -> Result<Self, AppError> {
        let (stage, mut state) = match ctx.stage().await {
            ConsolidationStageState::Classify(state) => (WorkStage::Classify, state),
            ConsolidationStageState::Resolve(state) => (WorkStage::Resolve, state),
            ConsolidationStageState::Reconcile(state) => (WorkStage::Reconcile, state),
            ConsolidationStageState::Assemble(state) => (WorkStage::Assemble, state),
            other => {
                return Err(AppError::Internal(format!(
                    "consolidation work: the record is on `{}`",
                    other.label()
                )));
            }
        };
        let rows: BTreeMap<_, _> = ctx
            .view
            .rows()
            .await?
            .into_iter()
            .map(|row| (row.path.clone(), row))
            .collect();
        for row in rows.values() {
            state.revision = state.revision.max(row.checkpoint_revision);
        }
        let redirects = rows
            .values()
            .filter_map(|row| match &row.progress.identity {
                IdentityProgress::Coalesced { canonical_path, .. } => {
                    Some((row.path.clone(), canonical_path.clone()))
                }
                IdentityProgress::Pending
                | IdentityProgress::Distinct { .. }
                | IdentityProgress::Unresolved { .. } => None,
            })
            .collect();
        let reconciliations = rows
            .values()
            .filter_map(|row| {
                let ReconciliationProgress::Accepted {
                    promotions,
                    retractions,
                } = &row.progress.reconciliation
                else {
                    return None;
                };
                let promotions = serde_json::from_value(promotions.clone()).ok()?;
                Some((
                    row.path.clone(),
                    AcceptedReconciliation {
                        promotions,
                        retractions: retractions.clone(),
                    },
                ))
            })
            .collect();
        Ok(Self {
            ctx,
            state,
            stage,
            rows,
            banked_classifications: BTreeMap::new(),
            pending_identity: BTreeMap::new(),
            pending_resolution_evidence: BTreeMap::new(),
            redirects,
            reconciliations,
        })
    }

    /// The classification banked for an entity, if one was and it still parses.
    pub(super) fn classification(&self, path: &str) -> Option<Classification> {
        let decision = self
            .rows
            .get(path)
            .and_then(|row| match &row.progress.classification {
                ClassificationProgress::Accepted { decision, .. } => Some(decision),
                ClassificationProgress::Pending | ClassificationProgress::Discarded { .. } => None,
            })
            .or_else(|| self.banked_classifications.get(path))?;
        serde_json::from_value(decision.clone()).ok()
    }

    /// Bank an entity's accepted classification for the atomic projection transition.
    pub(super) async fn bank_classification(
        &mut self,
        path: &str,
        c: &Classification,
    ) -> Result<(), AppError> {
        let v = serde_json::to_value(c)
            .map_err(|e| AppError::Internal(format!("serialize classification checkpoint: {e}")))?;
        self.banked_classifications.insert(path.to_string(), v);
        Ok(())
    }

    pub(super) async fn discard_classification(&mut self, path: &str) -> Result<(), AppError> {
        self.banked_classifications.remove(path);
        let mut row = self
            .rows
            .get(path)
            .cloned()
            .or(self.ctx.view.entity_by_path(path).await?)
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "consolidation: classification row `{path}` disappeared"
                ))
            })?;
        row.progress.classification = ClassificationProgress::Pending;
        self.commit_row_progress(path, row).await
    }

    pub(super) fn vocabulary_search(&self, term: &str) -> Option<&str> {
        self.state
            .vocabulary_search_cache
            .get(&term.trim().to_lowercase())
            .map(String::as_str)
    }

    pub(super) fn bank_vocabulary_search(&mut self, term: &str, result: String) {
        self.state
            .vocabulary_search_cache
            .insert(term.trim().to_lowercase(), result);
    }

    pub(super) fn entity_search(&self, query: &str) -> Option<&str> {
        let cached = self
            .state
            .entity_search_cache
            .get(&query.trim().to_lowercase())?;
        (cached.get("revision")?.as_u64()? == self.state.revision)
            .then(|| cached.get("result")?.as_str())
            .flatten()
    }

    pub(super) fn bank_entity_search(&mut self, query: &str, result: String) {
        self.state.entity_search_cache.insert(
            query.trim().to_lowercase(),
            serde_json::json!({
                "revision": self.state.revision,
                "result": result,
            }),
        );
    }

    pub(super) async fn bank_classify_diagnostic(
        &mut self,
        path: &str,
        diagnostic: serde_json::Value,
    ) -> Result<(), AppError> {
        let mut row = self
            .rows
            .get(path)
            .cloned()
            .or(self.ctx.view.entity_by_path(path).await?)
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "consolidation: diagnostic row `{path}` disappeared"
                ))
            })?;
        row.progress.classification_diagnostic = Some(diagnostic);
        self.commit_row_progress(path, row).await
    }

    async fn commit_row_progress(
        &mut self,
        path: &str,
        mut row: KnowledgeConsolidationEntity,
    ) -> Result<(), AppError> {
        row.checkpoint_revision = self.state.revision;
        let mut checkpoint = self.ctx.record().await;
        checkpoint.state = self.checkpoint_state();
        checkpoint.updated_at = chrono::Utc::now();
        let transition = EntityTransition::new(checkpoint.clone()).with_row(row.clone());
        self.ctx.view.commit_transition(&transition).await?;
        self.rows.insert(path.to_string(), row);
        self.ctx.adopt_committed_record(checkpoint).await;
        Ok(())
    }

    pub(super) async fn discard(&mut self, path: &str, reason: &str) -> Result<(), AppError> {
        self.state.revision += 1;
        let mut row = self
            .ctx
            .view
            .working_entity(path)
            .await?
            .or(self.ctx.view.entity_by_path(path).await?)
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "consolidation: discarded entity `{path}` is absent from the entity view"
                ))
            })?;
        row.mark_discarded(reason);
        row.checkpoint_revision = self.state.revision;
        let mut checkpoint = self.ctx.record().await;
        checkpoint.state = self.checkpoint_state();
        checkpoint.updated_at = chrono::Utc::now();
        let transition = EntityTransition::new(checkpoint.clone()).with_row(row);
        self.ctx.view.commit_transition(&transition).await?;
        self.rows
            .insert(path.to_string(), transition.rows[0].clone());
        self.ctx.adopt_committed_record(checkpoint).await;
        Ok(())
    }

    pub(super) fn is_resolved(&self, path: &str) -> bool {
        self.rows
            .get(path)
            .is_some_and(|row| !matches!(row.progress.identity, IdentityProgress::Pending))
    }

    pub(super) fn resolution_fingerprint(&self, path: &str) -> Option<&str> {
        self.rows
            .get(path)
            .and_then(|row| match &row.progress.identity {
                IdentityProgress::Distinct { fingerprint, .. }
                | IdentityProgress::Unresolved { fingerprint, .. } => Some(fingerprint.as_str()),
                IdentityProgress::Pending | IdentityProgress::Coalesced { .. } => None,
            })
    }

    pub(super) fn resolution_pair_fingerprint(&self, pair: &str) -> Option<&str> {
        self.state
            .resolution_pair_fingerprints
            .get(pair)
            .map(String::as_str)
    }

    pub(super) fn remember_resolution_pairs(
        &mut self,
        pairs: impl IntoIterator<Item = (String, String)>,
    ) {
        self.state.resolution_pair_fingerprints.extend(pairs);
    }

    pub(super) async fn bank_resolution_pairs(
        &mut self,
        pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Result<(), AppError> {
        self.remember_resolution_pairs(pairs);
        self.persist().await
    }

    /// Bank a distinct identity verdict and its complete proposal snapshot together.
    pub(super) async fn commit_resolved_distinct(
        &mut self,
        path: &str,
        fingerprint: String,
        evidence: serde_json::Value,
        proposals: &ProposalSet,
    ) -> Result<(), AppError> {
        self.pending_identity.insert(
            path.to_string(),
            IdentityProgress::Distinct {
                fingerprint,
                evidence,
            },
        );
        self.checkpoint_transition("resolve-distinct", path, proposals)
            .await
    }

    pub(super) async fn commit_resolution_unresolved(
        &mut self,
        path: &str,
        fingerprint: String,
        diagnostic: serde_json::Value,
        proposals: &ProposalSet,
    ) -> Result<(), AppError> {
        self.pending_identity.insert(
            path.to_string(),
            IdentityProgress::Unresolved {
                fingerprint,
                diagnostic,
            },
        );
        self.checkpoint_transition("resolve-unresolved", path, proposals)
            .await
    }

    pub(super) fn remember_resolution_evidence(&mut self, path: &str, evidence: serde_json::Value) {
        self.pending_resolution_evidence
            .insert(path.to_string(), evidence);
    }

    pub(super) async fn commit_resolved_merge(
        &mut self,
        path: &str,
        into: &str,
        proposals: &ProposalSet,
    ) -> Result<(), AppError> {
        let canonical_path = self.canonical_path(into);
        self.reconciliations.remove(path);
        self.reconciliations.remove(&canonical_path);
        for target in self.redirects.values_mut() {
            if target == path {
                *target = canonical_path.clone();
            }
        }
        self.redirects
            .insert(path.to_string(), canonical_path.clone());
        self.state.revision += 1;
        let working = proposals.trace_value();

        let mut canonical = proposals.input_entity(&canonical_path).ok_or_else(|| {
            AppError::Internal(format!(
                "consolidation: resolved canonical entity `{canonical_path}` is absent from working state"
            ))
        })?;
        canonical.progress.reconciliation = ReconciliationProgress::Pending;
        canonical.checkpoint_revision = self.state.revision;
        canonical.rederive_search();
        let mut losing = self
            .rows
            .get(path)
            .cloned()
            .or(self.ctx.view.working_entity(path).await?)
            .or(self.ctx.view.entity_by_path(path).await?)
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "consolidation: resolved losing entity `{path}` is absent from working state"
                ))
            })?;
        let evidence = self.pending_resolution_evidence.remove(path);
        losing.mark_coalesced_with_evidence(&canonical_path, evidence);
        losing.checkpoint_revision = self.state.revision;

        let mut checkpoint = self.ctx.record().await;
        checkpoint.state = self.checkpoint_state();
        self.ctx
            .repo
            .commit_entity_identity_merge(&canonical, &losing, &checkpoint)
            .await?;
        self.rows = self
            .ctx
            .view
            .rows()
            .await?
            .into_iter()
            .map(|row| (row.path.clone(), row))
            .collect();
        self.ctx.adopt_committed_record(checkpoint).await;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            "resolve-merge",
            path,
            &serde_json::json!({
                "checkpoint": self.state,
                "working": working,
            }),
        );
        Ok(())
    }

    pub(super) fn resolved_into(&self) -> &BTreeMap<String, String> {
        &self.redirects
    }

    pub(super) fn canonical_path(&self, path: &str) -> String {
        let mut current = path;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(current) {
            let Some(next) = self.redirects.get(current) else {
                break;
            };
            current = next;
        }
        current.to_string()
    }

    /// The batch schema decision, if one was banked and still parses.
    pub(super) fn adjudication(&self) -> Option<AdjudicationResult> {
        serde_json::from_value(self.state.adjudicated.clone()?).ok()
    }

    pub(super) async fn bank_adjudication(
        &mut self,
        a: &AdjudicationResult,
    ) -> Result<(), AppError> {
        for decision in &a.decisions {
            if let Ok(value) = serde_json::to_value(decision) {
                self.state
                    .adjudicated_terms
                    .insert(decision.term.clone(), value);
            }
        }
        let decisions = self
            .state
            .adjudicated_terms
            .values()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect();
        self.state.adjudicated = serde_json::to_value(AdjudicationResult {
            decisions,
            amendment_nominations: a.amendment_nominations.clone(),
        })
        .ok();
        self.persist().await
    }

    pub(super) fn reconciliation(&self, path: &str) -> Option<&AcceptedReconciliation> {
        self.reconciliations.get(path)
    }

    pub(super) async fn commit_reconciliation(
        &mut self,
        path: &str,
        promotions: &[ReconcilePromotion],
        retractions: &[(String, String)],
        counted: &crate::memory::pkm::consolidation::ReconcileOutcome,
        proposals: &ProposalSet,
        write: &ReconcileCommit,
    ) -> Result<(), AppError> {
        let mut next_state = self.state.clone();
        next_state.revision += 1;
        let working = proposals.trace_value();

        let mut checkpoint = self.ctx.record().await;
        checkpoint.state = match self.stage {
            WorkStage::Classify => ConsolidationStageState::Classify(next_state.clone()),
            WorkStage::Resolve => ConsolidationStageState::Resolve(next_state.clone()),
            WorkStage::Reconcile => ConsolidationStageState::Reconcile(next_state.clone()),
            WorkStage::Assemble => ConsolidationStageState::Assemble(next_state.clone()),
        };
        checkpoint.stats.absorb_reconcile(counted);
        checkpoint.updated_at = chrono::Utc::now();
        let mut write = write.clone();
        if let Some(entity) = write.entity.as_mut() {
            if let Some(durable) = self.rows.get(&entity.path) {
                entity.consolidation_entity_id = durable.consolidation_entity_id.clone();
                if entity.entity_id.is_none() {
                    entity.entity_id = durable.entity_id.clone();
                }
                entity.progress = durable.progress.clone();
            }
            entity.progress.reconciliation = ReconciliationProgress::Accepted {
                promotions: serde_json::to_value(promotions)
                    .unwrap_or_else(|_| serde_json::json!([])),
                retractions: retractions.to_vec(),
            };
            entity.checkpoint_revision = next_state.revision;
        }
        self.ctx
            .repo
            .commit_reconciliation(&write, &checkpoint)
            .await?;

        self.state = next_state;
        if let Some(entity) = write.entity {
            self.rows.insert(entity.path.clone(), entity);
        }
        self.reconciliations.insert(
            path.to_string(),
            AcceptedReconciliation {
                promotions: promotions.to_vec(),
                retractions: retractions.to_vec(),
            },
        );
        self.ctx.adopt_committed_record(checkpoint).await;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            "reconcile",
            path,
            &serde_json::json!({
                "checkpoint": self.state,
                "working": working,
            }),
        );
        Ok(())
    }

    pub(super) fn reopen_reconciliation(&mut self, path: &str) {
        self.reconciliations.remove(path);
        if let Some(row) = self.rows.get_mut(path) {
            row.progress.reconciliation = ReconciliationProgress::Pending;
        }
    }

    pub(super) async fn persist(&self) -> Result<(), AppError> {
        self.ctx.persist_required(self.checkpoint_state()).await
    }
}
