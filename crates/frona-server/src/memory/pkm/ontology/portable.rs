use super::*;

impl OntologyManager {
    /// Freeze the persisted ontology and committed graph for a portable phase.
    pub(crate) async fn portable(
        &self,
        user_id: &str,
    ) -> Result<frona_pkm::ontology::Ontology, AppError> {
        let loaded = self.load(user_id).await?;
        let catalogue = self
            .catalogue()
            .ok_or_else(|| AppError::Internal("ontology: no catalogue installed yet".into()))?;
        let scope = loaded.effective_ontology();
        let state = frona_pkm::model::KnowledgeOntology {
            id: user_id.into(),
            user_id: user_id.into(),
            owl: loaded.delta_ofn().into(),
            format: DELTA_FORMAT.into(),
            version: loaded.version(),
            effective_ontology: scope.to_ntriples(),
            seeds: scope.seeds().to_vec(),
            sources: scope.sources().to_vec(),
            catalog_fingerprint: catalogue.fingerprint().into(),
            updated_at: chrono::Utc::now(),
        };
        let entities = self
            .repo
            .list_entities(user_id)
            .await?
            .into_iter()
            .map(|entity| {
                frona_pkm::model::KnowledgeConsolidationEntity::from_committed("", entity.into())
            })
            .collect();
        let graph = self.repo.portable_graph(user_id, entities).await?;
        Ok(frona_pkm::ontology::Ontology::new(catalogue, state, graph))
    }
}
