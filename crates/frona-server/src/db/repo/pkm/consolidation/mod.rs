//! Durable effective-entity overlay repository for one consolidation run.

use std::sync::Arc;

use crate::core::error::AppError;
use crate::memory::pkm::model::{
    ConsolidationEntityLifecycle, EntityHit, KnowledgeConsolidationEntity,
};
use frona_pkm::model::normalize_identity_name;

use super::PkmRepo;

#[derive(Clone)]
pub struct PkmConsolidationStore {
    committed: Arc<PkmRepo>,
}

impl PkmConsolidationStore {
    pub fn new(committed: Arc<PkmRepo>) -> Self {
        Self { committed }
    }

    pub fn scoped(&self, consolidation_id: &str, user_id: &str) -> PkmConsolidationRepo {
        PkmConsolidationRepo {
            committed: self.committed.clone(),
            consolidation_id: consolidation_id.to_string(),
            user_id: user_id.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct PkmConsolidationRepo {
    committed: Arc<PkmRepo>,
    consolidation_id: String,
    user_id: String,
}

struct ResolutionCandidateQuery<'a> {
    names: &'a [String],
    name_tokens: &'a [String],
    assertions: &'a [String],
    kinds: &'a [String],
    text: &'a str,
    limit: i64,
}

impl PkmConsolidationRepo {
    pub fn consolidation_id(&self) -> &str {
        &self.consolidation_id
    }
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub async fn upsert_entity(
        &self,
        mut row: KnowledgeConsolidationEntity,
    ) -> Result<(), AppError> {
        if row.consolidation_id != self.consolidation_id || row.user_id != self.user_id {
            return Err(AppError::Database(
                "pkm/consolidation_entity: scope mismatch".into(),
            ));
        }
        if row.entity_id.is_none()
            && let Some(entity) = self
                .committed
                .entity_by_path(&self.user_id, &row.path)
                .await?
        {
            row.entity_id = Some(entity.id);
        }
        row.rederive_search();
        row.validate()?;
        self.committed.upsert_consolidation_entity(&row).await
    }

    pub async fn working_entity(
        &self,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, AppError> {
        self.committed
            .consolidation_entity_by_path(&self.consolidation_id, &self.user_id, path)
            .await
    }

    pub async fn redirects(&self) -> Result<std::collections::BTreeMap<String, String>, AppError> {
        self.committed
            .consolidation_entity_redirects(&self.consolidation_id, &self.user_id)
            .await
    }

    pub async fn entity_by_path(
        &self,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, AppError> {
        let mut current_path = path.to_string();
        let mut visited = std::collections::HashSet::new();
        loop {
            if !visited.insert(current_path.clone()) {
                return Err(AppError::Database(format!(
                    "pkm/consolidation_entity: redirect cycle at `{current_path}`"
                )));
            }
            let Some(row) = self.working_entity(&current_path).await? else {
                break;
            };
            if row.lifecycle == ConsolidationEntityLifecycle::Coalesced
                && let Some(canonical) = row.canonical_path
            {
                current_path = canonical;
                continue;
            }
            if row.lifecycle == ConsolidationEntityLifecycle::Pending
                && row.entity_id.is_some()
                && row
                    .contributions
                    .iter()
                    .any(|contribution| contribution.existing_only)
                && let Some(entity) = self
                    .committed
                    .entity_by_path(&self.user_id, &current_path)
                    .await?
            {
                let mut effective = row;
                effective.apply_committed(entity);
                return Ok(Some(effective));
            }
            return Ok(row.effective_entity());
        }
        let Some(entity) = self
            .committed
            .entity_by_path(&self.user_id, &current_path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(KnowledgeConsolidationEntity::from_committed(
            &self.consolidation_id,
            entity,
        )))
    }

    pub async fn list_entities(&self) -> Result<Vec<KnowledgeConsolidationEntity>, AppError> {
        self.committed
            .list_effective_entities(&self.consolidation_id, &self.user_id)
            .await
    }

    pub(crate) async fn rows(&self) -> Result<Vec<KnowledgeConsolidationEntity>, AppError> {
        self.committed
            .list_consolidation_entities(&self.consolidation_id, &self.user_id)
            .await
    }

    pub async fn search_entities(&self, query: &str) -> Result<Vec<EntityHit>, AppError> {
        let mut hits = self
            .committed
            .search_effective_entities(&self.consolidation_id, &self.user_id, query)
            .await?;
        self.prioritize_exact_name(query, &mut hits);
        Ok(hits)
    }

    pub async fn resolution_candidates(
        &self,
        names: &[String],
        name_tokens: &[String],
        assertions: &[String],
        kinds: &[String],
        query_text: &str,
        limit: i64,
    ) -> Result<Vec<EntityHit>, AppError> {
        self.committed
            .search_effective_resolution_candidates(
                &self.consolidation_id,
                &self.user_id,
                ResolutionCandidateQuery {
                    names,
                    name_tokens,
                    assertions,
                    kinds,
                    text: query_text,
                    limit,
                },
            )
            .await
    }

    fn prioritize_exact_name(&self, query: &str, hits: &mut [EntityHit]) {
        let exact = normalize_identity_name(query);
        hits.sort_by_key(|hit| {
            let matches = normalize_identity_name(&hit.name) == exact
                || hit
                    .aliases
                    .iter()
                    .any(|alias| normalize_identity_name(alias) == exact);
            !matches
        });
    }

    pub async fn clear(&self) -> Result<(), AppError> {
        self.committed
            .delete_consolidation_entities(&self.consolidation_id)
            .await
    }

    pub(crate) async fn commit_transition(
        &self,
        rows: &[KnowledgeConsolidationEntity],
        checkpoint: &crate::memory::pkm::KnowledgeConsolidationRecord,
    ) -> Result<(), AppError> {
        self.committed
            .commit_consolidation_transition(rows, checkpoint)
            .await
    }
}

mod checkpoint;
mod identity;
mod overlay;
mod playbook;
mod publication;
mod reconciliation;
mod recovery;
mod transition;
