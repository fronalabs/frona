use crate::{
    Error,
    model::{KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeEntityLink, KnowledgeMemory},
    view::{DraftEntityLookup, EntityDraft, EntitySnapshot},
};

/// Effective, caller-scoped knowledge at the start of a phase. Include staged rows
/// and redirects from the active consolidation, not just committed entities.
#[derive(Debug, Clone, Default)]
pub struct GraphSnapshot {
    pub scope_id: String,
    pub entities: Vec<KnowledgeConsolidationEntity>,
    pub links: Vec<KnowledgeEntityLink>,
    pub memories: Vec<KnowledgeMemory>,
    /// Complete memory-to-entity attachment scopes, including cross-entity references.
    pub memory_entities: std::collections::HashMap<String, Vec<String>>,
}
impl GraphSnapshot {
    /// Caller assertions only; previous reasoning output must not become new input.
    pub fn asserted_links(&self) -> Vec<KnowledgeEntityLink> {
        self.links
            .iter()
            .filter(|link| link.origin != crate::model::LinkOrigin::Inferred)
            .cloned()
            .collect()
    }

    pub fn entity_by_path_with(
        &self,
        draft: &EntityDraft,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, Error> {
        // Resolve one effective map. Following the layers independently can
        // resurrect an overridden row or miss a cycle spanning base and draft.
        let effective =
            EntityDraft::from_rows(self.entities.iter().cloned().chain(draft.rows().cloned()));
        match effective.resolve(path)? {
            DraftEntityLookup::Found(row) => Ok(Some(*row)),
            DraftEntityLookup::Missing => Ok(None),
        }
    }
    pub fn snapshot_with(&self, draft: &EntityDraft) -> Result<EntitySnapshot, Error> {
        Ok(EntitySnapshot::new(self.entities.clone(), draft))
    }
    pub fn knowledge_entities(&self) -> Vec<KnowledgeEntity> {
        EntitySnapshot::new(self.entities.clone(), &EntityDraft::default()).into_entities()
    }
    pub fn entity_by_path(
        &self,
        path: &str,
    ) -> Result<Option<KnowledgeConsolidationEntity>, Error> {
        self.entity_by_path_with(&EntityDraft::default(), path)
    }
}

impl GraphSnapshot {
    pub fn memories_for_entity(&self, path: &str) -> Vec<KnowledgeMemory> {
        let mut out: Vec<_> = self
            .memories
            .iter()
            .filter(|memory| {
                self.memory_entities
                    .get(&memory.id)
                    .is_some_and(|paths| paths.iter().any(|p| p == path))
            })
            .cloned()
            .collect();
        out.sort_by_key(|memory| std::cmp::Reverse(memory.created_at));
        out
    }
    pub fn current_memories_for_entity(&self, path: &str) -> Vec<KnowledgeMemory> {
        self.memories_for_entity(path)
            .into_iter()
            .filter(|m| m.relations.is_empty() && m.disposition == crate::model::Disposition::None)
            .collect()
    }
    pub fn memories_by_ids(&self, ids: &[String]) -> Vec<KnowledgeMemory> {
        let mut out: Vec<_> = self
            .memories
            .iter()
            .filter(|m| ids.contains(&m.id))
            .cloned()
            .collect();
        out.sort_by_key(|memory| std::cmp::Reverse(memory.created_at));
        out
    }
    pub fn search_entities(&self, query: &str) -> Vec<crate::model::EntityHit> {
        let entities = self.knowledge_entities();
        crate::tools::entity::search(&entities, query)
            .into_iter()
            .map(crate::tools::entity::hit)
            .collect()
    }
}

#[cfg(test)]
mod tests;
