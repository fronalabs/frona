use crate::{core::error::AppError, db::repo::pkm::PkmRepo};

impl PkmRepo {
    /// Read complete user-scoped research data, including cross-entity attachments.
    pub(crate) async fn portable_graph(
        &self,
        user_id: &str,
        entities: Vec<frona_pkm::model::KnowledgeConsolidationEntity>,
    ) -> Result<frona_pkm::GraphSnapshot, AppError> {
        let mut memory_entities = std::collections::HashMap::<String, Vec<String>>::new();
        for source in self.list_entity_sources(user_id).await? {
            memory_entities
                .entry(source.memory_id)
                .or_default()
                .push(source.entity_path);
        }
        // Staged entities can cite memories before publication creates durable links.
        for entity in &entities {
            for id in &entity.source_memory_ids {
                let paths = memory_entities.entry(id.clone()).or_default();
                if !paths.contains(&entity.path) {
                    paths.push(entity.path.clone());
                }
            }
        }
        Ok(frona_pkm::GraphSnapshot {
            scope_id: user_id.into(),
            entities,
            links: self
                .asserted_links(user_id)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
            memories: self
                .list_all_memories(user_id)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
            memory_entities,
        })
    }
}
