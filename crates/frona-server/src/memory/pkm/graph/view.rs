use crate::core::error::AppError;
use crate::db::repo::pkm::PkmConsolidationRepo;
use crate::memory::pkm::model::KnowledgeConsolidationEntity;

use frona_pkm::view::EntityDraft;

#[derive(Clone)]
pub(crate) struct EntityViewManager {
    durable: PkmConsolidationRepo,
}

impl EntityViewManager {
    pub(crate) fn new(durable: PkmConsolidationRepo) -> Self {
        Self { durable }
    }

    pub(crate) fn consolidation_id(&self) -> &str {
        self.durable.consolidation_id()
    }

    pub(crate) async fn working_entity(
        &self,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, AppError> {
        self.durable.working_entity(path).await
    }

    pub(crate) async fn redirects(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, AppError> {
        self.durable.redirects().await
    }

    pub(crate) async fn entity_by_path(
        &self,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, AppError> {
        self.durable.entity_by_path(path).await
    }

    pub(crate) async fn entity_by_path_with(
        &self,
        draft: &EntityDraft,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, AppError> {
        let mut entities: std::collections::BTreeMap<_, _> = self
            .list_entities()
            .await?
            .into_iter()
            .map(|row| (row.path.clone(), row.into()))
            .collect();
        for row in self.rows().await? {
            entities.insert(row.path.clone(), row.into());
        }
        let graph = frona_pkm::GraphSnapshot {
            entities: entities.into_values().collect(),
            ..Default::default()
        };
        Ok(graph.entity_by_path_with(draft, path)?.map(Into::into))
    }

    pub(crate) async fn list_entities(
        &self,
    ) -> Result<Vec<KnowledgeConsolidationEntity>, AppError> {
        self.durable.list_entities().await
    }

    pub(crate) async fn rows(&self) -> Result<Vec<KnowledgeConsolidationEntity>, AppError> {
        self.durable.rows().await
    }

    pub(crate) async fn commit_transition(
        &self,
        transition: &EntityTransition,
    ) -> Result<(), AppError> {
        if transition.checkpoint.consolidation_id != self.durable.consolidation_id()
            || transition.checkpoint.user_id != self.durable.user_id()
        {
            return Err(AppError::Database(
                "entity transition: checkpoint scope mismatch".into(),
            ));
        }
        let revision = transition.checkpoint.state.revision();
        if !transition.rows.is_empty() && revision.is_none() {
            return Err(AppError::Database(
                "entity transition: entity rows require a revisioned stage".into(),
            ));
        }
        let mut paths = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for row in &transition.rows {
            if row.consolidation_id != self.durable.consolidation_id()
                || row.user_id != self.durable.user_id()
            {
                return Err(AppError::Database(
                    "entity transition: entity scope mismatch".into(),
                ));
            }
            if revision != Some(row.checkpoint_revision) {
                return Err(AppError::Database(
                    "entity transition: row revision mismatch".into(),
                ));
            }
            if !paths.insert(row.path.as_str()) || !ids.insert(row.consolidation_entity_id.as_str())
            {
                return Err(AppError::Database(
                    "entity transition: duplicate entity row".into(),
                ));
            }
        }
        self.durable
            .commit_transition(&transition.rows, &transition.checkpoint)
            .await
    }
}

impl EntityViewManager {
    pub(crate) async fn portable_snapshot(
        &self,
        repo: &crate::db::repo::pkm::PkmRepo,
    ) -> Result<frona_pkm::GraphSnapshot, AppError> {
        let mut rows: std::collections::BTreeMap<_, _> = self
            .list_entities()
            .await?
            .into_iter()
            .map(|row| (row.path.clone(), row))
            .collect();
        // Keep discarded/coalesced overlay rows so redirects and tombstones remain visible.
        for row in self.rows().await? {
            rows.insert(row.path.clone(), row);
        }
        repo.portable_graph(
            self.durable.user_id(),
            rows.into_values().map(Into::into).collect(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl frona_pkm::candidates::CandidateRetrieval for EntityViewManager {
    async fn retrieve(
        &self,
        subject: &frona_pkm::candidates::Subject,
        limit: usize,
    ) -> Result<Vec<frona_pkm::model::EntityHit>, frona_pkm::Error> {
        let names = subject
            .names
            .iter()
            .map(|name| frona_pkm::model::normalize_identity_name(name))
            .collect::<Vec<_>>();
        let assertions = subject.assertions.iter().cloned().collect::<Vec<_>>();
        self.durable
            .resolution_candidates(
                &names,
                &subject.name_tokens(),
                &assertions,
                &subject.kinds,
                &subject.query_text(),
                i64::try_from(limit).unwrap_or(i64::MAX),
            )
            .await
            .map(|hits| hits.into_iter().map(Into::into).collect())
            .map_err(|error| frona_pkm::Error::Internal(format!("candidate retrieval: {error}")))
    }
}

use crate::memory::pkm::KnowledgeConsolidationRecord;

pub(crate) struct EntityTransition {
    pub(crate) rows: Vec<KnowledgeConsolidationEntity>,
    pub(crate) checkpoint: KnowledgeConsolidationRecord,
}

impl EntityTransition {
    pub(crate) fn new(checkpoint: KnowledgeConsolidationRecord) -> Self {
        Self {
            rows: Vec::new(),
            checkpoint,
        }
    }

    pub(crate) fn with_row(mut self, row: KnowledgeConsolidationEntity) -> Self {
        self.rows.push(row);
        self
    }

    pub(crate) fn with_rows(
        mut self,
        rows: impl IntoIterator<Item = KnowledgeConsolidationEntity>,
    ) -> Self {
        self.rows.extend(rows);
        self
    }
}
