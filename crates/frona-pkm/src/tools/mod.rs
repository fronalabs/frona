use crate::{Error, model::KnowledgeEntity};
use allowance::ResearchAllowance;
use frona_harness::ToolOutput;
use serde_json::Value;
use std::sync::Arc;

pub(crate) mod allowance;
pub(crate) mod entity;
pub(crate) mod ontology;

#[derive(Clone)]
pub(crate) struct EntityToolView {
    entities: Arc<[KnowledgeEntity]>,
    allowance: ResearchAllowance,
}
impl EntityToolView {
    pub(crate) fn new(entities: Vec<KnowledgeEntity>, allowance: ResearchAllowance) -> Self {
        Self {
            entities: entities.into(),
            allowance,
        }
    }
    fn spend_tool_call(&self) -> Option<ToolOutput> {
        (!self.allowance.try_acquire()).then(|| ToolOutput::text(format!(
            "Research-tool budget exhausted ({} calls). Submit the best complete decision set now; unresolved terms will keep their validated proposals.", self.allowance.limit()
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
    budget: &Option<ResearchAllowance>,
) -> Option<ToolOutput> {
    if let Some(output) = overlay
        .as_ref()
        .and_then(|overlay| overlay.spend_tool_call())
    {
        return Some(output);
    }
    budget
        .as_ref()
        .filter(|budget| !budget.try_acquire())
        .map(|_| {
            ToolOutput::text("Research-tool budget exhausted. Submit the best complete result now.")
        })
}

pub(crate) struct SearchEntitiesTool {
    pub repo: crate::GraphSnapshot,
    pub overlay: Option<Arc<EntityToolView>>,
    pub budget: Option<ResearchAllowance>,
    pub prefixes: crate::ontology::PrefixMap,
}
impl SearchEntitiesTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_entity_tool_call(&self.overlay, &self.budget) {
            return Ok(output);
        }
        let Some(query) = str_arg(&arguments, "query") else {
            return Ok(ToolOutput::text(
                "Provide a `query` naming the entity to look for.",
            ));
        };
        let durable;
        let entities = if let Some(view) = &self.overlay {
            view.entities()
        } else {
            durable = self.repo.knowledge_entities();
            &durable
        };
        Ok(ToolOutput::text(entity::render_search(
            entities,
            query,
            &self.prefixes,
        )))
    }
}

pub(crate) struct ReadEntityTool {
    pub repo: crate::GraphSnapshot,
    pub memories: crate::GraphSnapshot,
    pub overlay: Option<Arc<EntityToolView>>,
    pub budget: Option<ResearchAllowance>,
    pub prefixes: crate::ontology::PrefixMap,
}
impl ReadEntityTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_entity_tool_call(&self.overlay, &self.budget) {
            return Ok(output);
        }
        let Some(path) = str_arg(&arguments, "path") else {
            return Ok(ToolOutput::text(
                "Provide the exact `path` returned by `search_entities` or supplied in the stage input.",
            ));
        };
        let found = if let Some(overlay) = &self.overlay {
            // An exact overlay row wins. Otherwise resolve the base redirect and
            // look up its terminal path in the effective overlay, never the stale row.
            if let Some(entity) = overlay.entity(path) {
                Some(entity)
            } else {
                self.repo
                    .entity_by_path(path)?
                    .and_then(|entity| overlay.entity(&entity.path))
            }
        } else {
            self.repo
                .entity_by_path(path)?
                .map(|entity| entity.as_knowledge_entity())
        };
        let Some(found) = found else {
            return Ok(ToolOutput::text(entity::missing(path)));
        };
        let memories = self
            .memories
            .memories_for_entity(&found.path)
            .into_iter()
            .chain(self.memories.memories_by_ids(&found.source_memory_ids))
            .collect::<Vec<_>>();
        Ok(ToolOutput::text(entity::render(
            &found,
            &memories,
            &self.prefixes,
        )))
    }
}

#[async_trait::async_trait]
impl self::Handler for SearchEntitiesTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        SearchEntitiesTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl self::Handler for ReadEntityTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        ReadEntityTool::execute(self, arguments).await
    }
}

fn str_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

#[async_trait::async_trait]
trait Handler: Send + Sync {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error>;
}
struct PortableTool<T> {
    handler: T,
    definition: rig_core::completion::request::ToolDefinition,
}
impl<T> PortableTool<T> {
    fn new(handler: T, name: &str) -> Result<Self, Error> {
        let resource = match name {
            "search_entities" => include_str!("../../resources/tools/search_entities.json"),
            "read_entity" => include_str!("../../resources/tools/read_entity.json"),
            "validation_details" => include_str!("../../resources/tools/validation_details.json"),
            "ontology_sparql" => include_str!("../../resources/tools/ontology_sparql.json"),
            "ontology_term_search" => {
                include_str!("../../resources/tools/ontology_term_search.json")
            }
            "inspect_ontology_terms" => {
                include_str!("../../resources/tools/inspect_ontology_terms.json")
            }
            "usage_impact" => include_str!("../../resources/tools/usage_impact.json"),
            "test_edit" => include_str!("../../resources/tools/test_edit.json"),
            _ => return Err(Error::Internal(format!("unknown ontology tool {name}"))),
        };
        let definition = serde_json::from_str(resource)
            .map_err(|e| Error::Internal(format!("tool definition: {e}")))?;
        Ok(Self {
            handler,
            definition,
        })
    }
}
#[async_trait::async_trait]
impl<T: Handler> frona_harness::Tool for PortableTool<T> {
    fn definition(&self) -> &rig_core::completion::request::ToolDefinition {
        &self.definition
    }
    async fn call(
        &mut self,
        arguments: Value,
        _context: &frona_harness::ToolContext<'_>,
    ) -> frona_harness::ToolResult {
        self.handler
            .execute(arguments)
            .await
            .map_err(frona_harness::ToolError::failed)
    }
}

pub(crate) fn description(resource: &str) -> &str {
    resource
        .splitn(3, "---")
        .nth(2)
        .expect("embedded tool description")
        .trim()
}
