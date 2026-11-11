use super::PageAuthorInput;
use crate::tools::description;
use crate::{
    Error,
    tools::{allowance::ResearchAllowance, entity},
};
use frona_harness::{ToolRegistry, tool};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize, JsonSchema)]
struct Search {
    query: String,
}
#[derive(Deserialize, JsonSchema)]
struct Read {
    path: String,
}
const EXHAUSTED: &str = "Research-tool budget exhausted. Submit the best complete result now.";

pub(super) fn registry(input: &PageAuthorInput) -> Result<ToolRegistry<'static>, Error> {
    let mut entities = input.research_entities.clone();
    if !entities
        .iter()
        .any(|entity| entity.path == input.entity.path)
    {
        entities.push(input.entity.clone());
    }
    let entities = Arc::new(entities);
    let mut memories = input.research_memories.clone();
    memories
        .entry(input.entity.path.clone())
        .or_insert_with(|| input.memories.clone());
    let memories = Arc::new(memories);
    let budget = ResearchAllowance::new(20);
    let search_entities = entities.clone();
    let search_budget = budget.clone();
    let prefixes = input.prefixes.clone();
    let search = tool(
        "search_entities",
        description(include_str!(
            "../../resources/page-tools/search_entities.md"
        )),
        move |args: Search| {
            let output = if search_budget.try_acquire() {
                entity::render_search(&search_entities, &args.query, &prefixes)
            } else {
                EXHAUSTED.into()
            };
            async move { Ok(output) }
        },
    )
    .map_err(|e| Error::Internal(e.to_string()))?;
    let prefixes = input.prefixes.clone();
    let redirects = input.redirects.clone();
    let read = tool(
        "read_entity",
        description(include_str!("../../resources/page-tools/read_entity.md")),
        move |args: Read| {
            let output = if !budget.try_acquire() {
                Ok(EXHAUSTED.into())
            } else {
                crate::view::resolve_path(&args.path, |path| {
                    Ok(redirects.get(path).map(String::as_str))
                })
                .map(
                    |path| match entities.iter().find(|entity| entity.path == path) {
                        Some(found) => entity::render(
                            found,
                            memories.get(&found.path).into_iter().flatten(),
                            &prefixes,
                        ),
                        None => entity::missing(&args.path),
                    },
                )
                .map_err(frona_harness::ToolError::failed)
            };
            async move { output }
        },
    )
    .map_err(|e| Error::Internal(e.to_string()))?;
    let mut registry = ToolRegistry::default();
    registry
        .register(search)
        .map_err(|e| Error::Internal(e.to_string()))?;
    registry
        .register(read)
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(registry)
}
