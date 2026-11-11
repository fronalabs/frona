use std::collections::BTreeMap;

use crate::model::{ConsolidationEntityLifecycle, KnowledgeConsolidationEntity, KnowledgeEntity};

use super::EntityDraft;

#[derive(Debug, Clone, Default)]
pub struct EntitySnapshot {
    entities: BTreeMap<String, KnowledgeEntity>,
}

impl EntitySnapshot {
    pub fn new(
        durable: impl IntoIterator<Item = KnowledgeConsolidationEntity>,
        draft: &EntityDraft,
    ) -> Self {
        let mut entities: BTreeMap<String, KnowledgeEntity> = durable
            .into_iter()
            .filter_map(|entity| entity.effective_entity())
            .map(|entity| {
                let path = entity.path.clone();
                (path, entity.as_knowledge_entity())
            })
            .collect();
        for row in draft.rows() {
            entities.remove(&row.path);
            if matches!(
                row.lifecycle,
                ConsolidationEntityLifecycle::Pending | ConsolidationEntityLifecycle::Active
            ) {
                entities.insert(row.path.clone(), row.as_knowledge_entity());
            }
        }
        Self { entities }
    }

    #[cfg(test)]
    pub fn entity_by_path(&self, path: &str) -> Option<&KnowledgeEntity> {
        self.entities.get(path)
    }

    pub fn into_entities(self) -> Vec<KnowledgeEntity> {
        self.entities.into_values().collect()
    }
}
