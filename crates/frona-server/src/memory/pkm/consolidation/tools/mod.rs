use std::sync::Arc;

use frona_derive::agent_tool;
use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::memory::pkm::model::{KnowledgeEntity, memory_bullet};
use crate::tool::{InferenceContext, ToolOutput, str_arg};

use super::prompt_evidence;

pub(crate) mod ontology;

const ENTITY_HIT_CAP: usize = 10;

#[derive(Clone)]
pub(crate) struct EntityToolView {
    entities: Arc<[KnowledgeEntity]>,
    tool_budget: usize,
    tool_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl EntityToolView {
    pub(crate) fn new(
        entities: Vec<KnowledgeEntity>,
        tool_budget: usize,
        tool_calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            entities: entities.into(),
            tool_budget,
            tool_calls,
        }
    }

    fn spend_tool_call(&self) -> Option<ToolOutput> {
        use std::sync::atomic::Ordering;
        let used = self.tool_calls.fetch_add(1, Ordering::Relaxed);
        (used >= self.tool_budget).then(|| ToolOutput::text(format!(
            "Research-tool budget exhausted ({} calls). Submit the best complete decision set now; unresolved terms will keep their validated proposals.",
            self.tool_budget,
        )))
    }

    pub(crate) fn entities(&self) -> &[KnowledgeEntity] {
        &self.entities
    }

    pub(crate) fn entity(&self, path: &str) -> Option<KnowledgeEntity> {
        self.entities
            .iter()
            .find(|entity| entity.path == path)
            .cloned()
    }
}

fn spend_entity_tool_call(
    overlay: &Option<Arc<EntityToolView>>,
    budget: &Option<Arc<std::sync::atomic::AtomicUsize>>,
) -> Option<ToolOutput> {
    if let Some(output) = overlay
        .as_ref()
        .and_then(|overlay| overlay.spend_tool_call())
    {
        return Some(output);
    }
    let budget = budget.as_ref()?;
    use std::sync::atomic::Ordering;
    budget
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .err()
        .map(|_| {
            ToolOutput::text("Research-tool budget exhausted. Submit the best complete result now.")
        })
}

/// Search the entity view: committed entities overlaid with the current consolidation.
pub(crate) struct SearchEntitiesTool {
    pub prompts: PromptLoader,
    pub repo: crate::memory::pkm::consolidation::view::EntityViewManager,
    pub overlay: Option<Arc<EntityToolView>>,
    pub budget: Option<Arc<std::sync::atomic::AtomicUsize>>,
    pub prefixes: crate::memory::pkm::ontology::PrefixMap,
}

#[agent_tool(name = "search_entities", dir = "pkm")]
impl SearchEntitiesTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if let Some(output) = spend_entity_tool_call(&self.overlay, &self.budget) {
            return Ok(output);
        }
        let Some(query) = str_arg(&arguments, "query") else {
            return Ok(ToolOutput::text(
                "Provide a `query` naming the entity to look for.",
            ));
        };
        let hits = if let Some(overlay) = &self.overlay {
            let needle = crate::memory::pkm::model::normalize_identity_name(query);
            let mut hits: Vec<crate::memory::pkm::model::EntityHit> = overlay
                .entities()
                .iter()
                .filter(|entity| {
                    crate::memory::pkm::model::normalize_identity_name(&entity.name)
                        .contains(&needle)
                        || crate::memory::pkm::model::normalize_identity_name(&entity.description)
                            .contains(&needle)
                        || entity.aliases.iter().any(|alias| {
                            crate::memory::pkm::model::normalize_identity_name(alias)
                                .contains(&needle)
                        })
                })
                .map(|entity| crate::memory::pkm::model::EntityHit {
                    path: entity.path.clone(),
                    origin: entity.origin,
                    category: entity.category,
                    kinds: entity.kinds.clone(),
                    name: entity.name.clone(),
                    description: entity.description.clone(),
                    aliases: entity.aliases.clone(),
                    search_name_tokens: entity.search_name_tokens.clone(),
                    search_assertions: entity.search_assertions.clone(),
                    body: entity.body.clone(),
                })
                .collect();
            hits.sort_by_key(|hit| {
                let exact = crate::memory::pkm::model::normalize_identity_name(&hit.name) == needle
                    || hit.aliases.iter().any(|alias| {
                        crate::memory::pkm::model::normalize_identity_name(alias) == needle
                    });
                !exact
            });
            hits
        } else {
            self.repo.search_entities(query).await?
        };
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No entity matches \"{query}\". Nothing here is that entity yet, so an \
                 attribute whose value is it stays a literal."
            )));
        }
        let px = &self.prefixes;
        let lines: Vec<String> = hits
            .iter()
            .take(ENTITY_HIT_CAP)
            .map(|hit| {
                let kinds = px.display_joined(&hit.kinds);
                let kinds = if kinds.is_empty() {
                    "untyped".to_string()
                } else {
                    kinds
                };
                format!(
                    "{} — {} [{}] — {}",
                    hit.path, hit.name, kinds, hit.description
                )
            })
            .collect();
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

/// Read one entity from the same view searched by [`SearchEntitiesTool`].
pub(crate) struct ReadEntityTool {
    pub prompts: PromptLoader,
    pub repo: crate::memory::pkm::consolidation::view::EntityViewManager,
    pub memories: Arc<crate::db::repo::pkm::PkmRepo>,
    pub user_id: String,
    pub overlay: Option<Arc<EntityToolView>>,
    pub budget: Option<Arc<std::sync::atomic::AtomicUsize>>,
    pub prefixes: crate::memory::pkm::ontology::PrefixMap,
}

#[agent_tool(name = "read_entity", dir = "pkm")]
impl ReadEntityTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if let Some(output) = spend_entity_tool_call(&self.overlay, &self.budget) {
            return Ok(output);
        }
        let Some(path) = str_arg(&arguments, "path") else {
            return Ok(ToolOutput::text(
                "Provide the exact `path` returned by `search_entities` or supplied in the stage input.",
            ));
        };
        let entity = if let Some(overlay) = &self.overlay {
            overlay.entity(path)
        } else {
            self.repo
                .entity_by_path(path)
                .await?
                .map(|entity| entity.as_knowledge_entity())
        };
        let Some(entity) = entity else {
            return Ok(ToolOutput::text(format!(
                "No entity exists at `{path}` in this consolidation view. Use `search_entities` to find its current path."
            )));
        };
        let px = &self.prefixes;
        let kinds = px.display_joined(&entity.kinds);
        let aliases = {
            let mut aliases: Vec<_> = entity.aliases.iter().cloned().collect();
            aliases.sort();
            if aliases.is_empty() {
                "(none)".to_string()
            } else {
                aliases.join(", ")
            }
        };
        let attributes = serde_json::to_string_pretty(&entity.attributes)
            .unwrap_or_else(|_| entity.attributes.to_string());
        let assertions = if entity.search_assertions.is_empty() {
            "(none)".to_string()
        } else {
            entity.search_assertions.join("\n")
        };
        let mut memories_by_id = std::collections::BTreeMap::new();
        for memory in self
            .memories
            .memories_for_entity(&self.user_id, &entity.path)
            .await?
            .into_iter()
            .chain(
                self.memories
                    .memories_by_ids(&self.user_id, &entity.source_memory_ids)
                    .await?,
            )
        {
            memories_by_id.insert(memory.id.clone(), memory);
        }
        let memories: Vec<_> = memories_by_id.into_values().collect();
        let memories = if memories.is_empty() {
            "(none)".to_string()
        } else {
            memories.iter().map(memory_bullet).collect::<String>()
        };
        let body = if entity.body.trim().is_empty() {
            "(not authored yet)"
        } else {
            &entity.body
        };
        Ok(ToolOutput::text(format!(
            "path={}\ncategory={:?}\nname={}\ndescription={}\naliases={}\ntypes={}\nidentity_evidence={}\nattributes:\n{}\nassertions:\n{}\nsource_memories:\n{}\nbody:\n{}",
            entity.path,
            entity.category,
            entity.name,
            entity.description,
            aliases,
            if kinds.is_empty() {
                "(untyped)"
            } else {
                &kinds
            },
            prompt_evidence(&entity.identity_evidence),
            attributes,
            assertions,
            memories,
            body,
        )))
    }
}
