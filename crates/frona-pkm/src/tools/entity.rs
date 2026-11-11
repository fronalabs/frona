use crate::{
    model::{EntityHit, KnowledgeEntity, KnowledgeMemory, memory_bullet, normalize_identity_name},
    ontology::PrefixMap,
    prompt::prompt_evidence,
};
use std::collections::BTreeMap;

/// Match the same names, aliases, and descriptions in every research view.
pub(crate) fn search<'a>(entities: &'a [KnowledgeEntity], query: &str) -> Vec<&'a KnowledgeEntity> {
    let needle = normalize_identity_name(query);
    let mut hits: Vec<_> = entities
        .iter()
        .filter(|entity| {
            normalize_identity_name(&entity.name).contains(&needle)
                || normalize_identity_name(&entity.description).contains(&needle)
                || entity
                    .aliases
                    .iter()
                    .any(|alias| normalize_identity_name(alias).contains(&needle))
        })
        .collect();
    hits.sort_by_key(|entity| {
        !(normalize_identity_name(&entity.name) == needle
            || entity
                .aliases
                .iter()
                .any(|alias| normalize_identity_name(alias) == needle))
    });
    hits
}

pub(crate) fn hit(entity: &KnowledgeEntity) -> EntityHit {
    EntityHit {
        path: entity.path.clone(),
        origin: entity.origin,
        category: entity.category,
        kinds: entity.kinds.clone(),
        name: entity.name.clone(),
        description: entity.description.clone(),
        aliases: entity.aliases.clone(),
        body: entity.body.clone(),
        search_name_tokens: entity.search_name_tokens.clone(),
        search_assertions: entity.search_assertions.clone(),
    }
}

pub(crate) fn render_search(
    entities: &[KnowledgeEntity],
    query: &str,
    prefixes: &PrefixMap,
) -> String {
    let hits = search(entities, query);
    if hits.is_empty() {
        return format!(
            "No entity matches \"{query}\". Nothing here is that entity yet, so an attribute whose value is it stays a literal."
        );
    }
    hits.into_iter()
        .take(10)
        .map(|entity| {
            let kinds = prefixes.display_joined(&entity.kinds);
            format!(
                "{} — {} [{}] — {}",
                entity.path,
                entity.name,
                if kinds.is_empty() { "untyped" } else { &kinds },
                entity.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn missing(path: &str) -> String {
    format!(
        "No entity exists at `{path}` in this consolidation view. Use `search_entities` to find its current path."
    )
}

/// Callers decide which memories are visible and whether a missing collection is
/// an error; formatting deduplicates by durable identity in stable order.
pub(crate) fn render<'a>(
    entity: &KnowledgeEntity,
    memories: impl IntoIterator<Item = &'a KnowledgeMemory>,
    prefixes: &PrefixMap,
) -> String {
    let kinds = prefixes.display_joined(&entity.kinds);
    let mut aliases: Vec<_> = entity.aliases.iter().cloned().collect();
    aliases.sort();
    let aliases = if aliases.is_empty() {
        "(none)".into()
    } else {
        aliases.join(", ")
    };
    let attributes = serde_json::to_string_pretty(&entity.attributes)
        .unwrap_or_else(|_| entity.attributes.to_string());
    let assertions = if entity.search_assertions.is_empty() {
        "(none)".into()
    } else {
        entity.search_assertions.join("\n")
    };
    let memories: BTreeMap<_, _> = memories
        .into_iter()
        .map(|memory| (&memory.id, memory))
        .collect();
    let memories = if memories.is_empty() {
        "(none)".into()
    } else {
        memories
            .into_values()
            .map(memory_bullet)
            .collect::<String>()
    };
    let body = if entity.body.trim().is_empty() {
        "(not authored yet)"
    } else {
        &entity.body
    };
    format!(
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
        body
    )
}

#[cfg(test)]
mod tests;
