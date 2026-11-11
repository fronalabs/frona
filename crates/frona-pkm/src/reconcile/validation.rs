use std::collections::{HashMap, HashSet};

use tracing::warn;

use super::ReconcilePromotion;
use super::comparison_key;
use super::{ReconcileInput, ReconcilePrompts};
use crate::Error;
use crate::classify::{OntologyDeclaration, ProposalSet};
use crate::model::{
    AttributeSource, KnowledgeEntity, KnowledgeEntityLink, KnowledgeMemory, RelationType,
};
use crate::ontology::TermKind;
use crate::path::normalize_path;
use crate::reconcile::projection::{curie_key, entity_in_pass};
use crate::reconcile::{EntityVerdict, PromotionSuggestion, RelationInput};

pub(super) fn attribute_values(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    match value {
        serde_json::Value::Array(values) => values.iter().collect(),
        value => vec![value],
    }
}

pub(super) fn has_attribute_assertion(
    attributes: &serde_json::Value,
    property: &str,
    value: &serde_json::Value,
) -> bool {
    attributes
        .get(property)
        .is_some_and(|held| attribute_values(held).contains(&value))
}

/// Validate both halves of assertion provenance: every submitted assertion names an
/// active supporting memory, and every old assertion orphaned by this verdict is changed
/// or explicitly retracted in the same verdict.
pub(super) fn assertion_provenance_rejections(
    verdict: &EntityVerdict,
    memories: &[KnowledgeMemory],
    existing_attributes: &[AttributeSource],
    existing_links: &[KnowledgeEntityLink],
    memory_entities: &HashMap<String, Vec<String>>,
    current_page: &str,
) -> Vec<String> {
    let active: HashSet<&str> = memories.iter().map(|memory| memory.id.as_str()).collect();
    let retired: HashSet<&str> = verdict
        .outdated
        .iter()
        .map(|item| item.memory.trim())
        .chain(
            verdict
                .relations
                .iter()
                .filter(|item| !item.links.is_empty())
                .map(|item| item.memory.trim()),
        )
        .collect();
    let mut rejected = Vec::new();

    for (property, value) in verdict.attributes.as_object().into_iter().flatten() {
        for value in attribute_values(value) {
            let source = verdict
                .attribute_sources
                .iter()
                .find(|source| source.property == *property && source.value == *value);
            match source {
                None => rejected.push(format!(
                    "Attribute `{property}` value {} has no attribute_sources entry.", value
                )),
                Some(source) if source.source_memory_ids.is_empty()
                    || source.source_memory_ids.iter().any(|id| {
                        !active.contains(id.trim()) || retired.contains(id.trim())
                    }) => rejected.push(format!(
                        "Attribute `{property}` value {} must cite only current supporting memory ids.",
                        value
                    )),
                Some(source) if source.source_memory_ids.iter().any(|id| {
                    memory_entities.get(id.trim()).is_none_or(|entities| {
                        entities.len() != 1 || entities[0] != current_page
                    })
                }) => {
                    let invalid = source.source_memory_ids.iter().filter_map(|id| {
                        let entities = memory_entities.get(id.trim())?;
                        (entities.len() != 1 || entities[0] != current_page)
                            .then(|| format!("{} entities={:?}", id.trim(), entities))
                    }).collect::<Vec<_>>();
                    rejected.push(format!(
                        "Attribute `{property}` value {} cites multi-entity or wrong-entity memories: {}. A data attribute may cite only memories whose sole entity is `{current_page}`; represent multi-entity facts with entity_relations.",
                        value, invalid.join(", "),
                    ));
                }
                Some(_) => {}
            }
        }
    }
    for relation in &verdict.entity_relations {
        if relation.source_memory_ids.is_empty()
            || relation
                .source_memory_ids
                .iter()
                .any(|id| !active.contains(id.trim()) || retired.contains(id.trim()))
        {
            rejected.push(format!(
                "Relation `{}` → `{}` must cite only current supporting memory ids.",
                relation.property, relation.target
            ));
        }
    }

    for source in existing_attributes {
        if source.source_memory_ids.is_empty()
            || !source
                .source_memory_ids
                .iter()
                .all(|id| retired.contains(id.trim()))
        {
            continue;
        }
        if has_attribute_assertion(&verdict.attributes, &source.property, &source.value) {
            rejected.push(format!(
                "Attribute `{}` value {} loses all support when memories {:?} are retired. Remove or replace that value.",
                source.property, source.value, source.source_memory_ids
            ));
        }
    }
    for link in existing_links {
        if link.origin != crate::model::LinkOrigin::Asserted
            || link.source_memory_ids.is_empty()
            || !link
                .source_memory_ids
                .iter()
                .all(|id| retired.contains(id.trim()))
        {
            continue;
        }
        let retracted = verdict.relation_retractions.iter().any(|item| {
            item.property.trim() == link.relation && item.target.trim() == link.to_entity_path
        });
        if !retracted {
            rejected.push(format!(
                "Relation `{}` → `{}` loses all support when memories {:?} are retired. Add it to relation_retractions or provide another current source memory.",
                link.relation, link.to_entity_path, link.source_memory_ids
            ));
        }
    }
    rejected
}

pub(super) fn merge_explicit_retractions(
    mut retractions: Vec<(String, String)>,
    verdict: &EntityVerdict,
) -> Vec<(String, String)> {
    for item in &verdict.relation_retractions {
        let retraction = (
            item.property.trim().to_string(),
            item.target.trim().to_string(),
        );
        if !retraction.0.is_empty()
            && !retraction.1.is_empty()
            && !retractions.contains(&retraction)
        {
            retractions.push(retraction);
        }
    }
    retractions
}

/// Turn a validated memory value replacement into an asserted-edge replacement.
///
/// Reconcile supplies the new edge. Matching its property against an old edge whose
/// target is the submitted `was` value avoids broad target deletion: in particular,
/// `worksFor → Former Corp` is retired while `formerEmployer → Former Corp` survives.
pub(super) async fn replacement_retractions(
    verdict: &EntityVerdict,
    promotions: &[ReconcilePromotion],
    links: &[KnowledgeEntityLink],
    memories: &[KnowledgeMemory],
    ctx: &ReconcileInput,
    proposals: &ProposalSet,
    source_path: &str,
) -> Result<Vec<(String, String)>, Error> {
    let pending = proposals.promotions_for(source_path);
    let mut out = Vec::new();
    for related in &verdict.relations {
        for replacement in related
            .links
            .iter()
            .filter(|l| l.relation == RelationType::Replace)
        {
            let was = comparison_key(&replacement.was);
            let now = comparison_key(&replacement.now);
            for promotion in promotions {
                let (property, new_target) = (&promotion.property, &promotion.target);
                let Some(new_page) = entity_in_pass(ctx, proposals, new_target).await? else {
                    continue;
                };
                let new_matches = comparison_key(&new_page.name) == now
                    || new_page
                        .aliases
                        .iter()
                        .any(|alias| comparison_key(alias) == now);
                if !new_matches {
                    continue;
                }
                for old_link in links.iter().filter(|link| {
                    link.origin == crate::model::LinkOrigin::Asserted
                        && link.relation == *property
                        && link.to_entity_path != *new_target
                }) {
                    let Some(old_page) = ctx
                        .graph
                        .entity_by_path_with(&proposals.entity_draft(), &old_link.to_entity_path)?
                    else {
                        continue;
                    };
                    let old_matches = comparison_key(&old_page.name) == was
                        || old_page
                            .aliases
                            .iter()
                            .any(|alias| comparison_key(alias) == was);
                    let retraction = (property.clone(), old_link.to_entity_path.clone());
                    if old_matches && !out.contains(&retraction) {
                        out.push(retraction);
                    }
                }
                for (old_property, _, old_target) in
                    pending.iter().filter(|(_, _, target)| target != new_target)
                {
                    if old_property != property {
                        continue;
                    }
                    let Some(old_page) = entity_in_pass(ctx, proposals, old_target).await? else {
                        continue;
                    };
                    let old_matches = comparison_key(&old_page.name) == was
                        || old_page
                            .aliases
                            .iter()
                            .any(|alias| comparison_key(alias) == was);
                    let retraction = (property.clone(), old_target.clone());
                    if old_matches && !out.contains(&retraction) {
                        out.push(retraction);
                    }
                }
            }
        }
    }

    // A transition may be expressed as a unary `outdated` verdict rather than a
    // binary `replace`. If that retired memory names an existing target of the same
    // property, the newly promoted edge supersedes that old edge. Requiring the old
    // target's exact name/alias in the retired memory keeps this narrow: unrelated
    // edges of the same property are not removed merely because a new one appeared.
    for promotion in promotions {
        let (property, new_target) = (&promotion.property, &promotion.target);
        for outdated in &verdict.outdated {
            let Some(memory) = memories
                .iter()
                .find(|memory| memory.id == outdated.memory.trim())
            else {
                continue;
            };
            let content = comparison_key(&memory.content);
            for old_link in links.iter().filter(|link| {
                link.origin == crate::model::LinkOrigin::Asserted
                    && link.relation == *property
                    && link.to_entity_path != *new_target
            }) {
                let Some(old_page) = ctx
                    .graph
                    .entity_by_path_with(&proposals.entity_draft(), &old_link.to_entity_path)?
                else {
                    continue;
                };
                let old_is_named = memory_names_entity(&content, &old_page.name, &old_page.aliases);
                let retraction = (property.clone(), old_link.to_entity_path.clone());
                if old_is_named && !out.contains(&retraction) {
                    out.push(retraction);
                }
            }
        }
    }
    Ok(out)
}

pub(super) fn memory_names_entity(content: &str, name: &str, aliases: &HashSet<String>) -> bool {
    content.contains(&comparison_key(name))
        || aliases
            .iter()
            .any(|alias| content.contains(&comparison_key(alias)))
}

/// Whether a `replace` is one the entries actually support.
///
/// `replace` is the only verdict that declares an older memory historical, so it is the
/// only one that can mislabel a still-true fact. Three checks, none of them fuzzy:
///
///  - `was` and `now` are both named - if no value can be pointed at, nothing changed;
///  - they differ - a model with nothing to say often repeats itself;
///  - each is actually present in its own entry - a required field invites a model with
///    nothing to put there to invent something, and this is what catches that.
///
/// What it deliberately cannot catch is a change that is real *and* leaves the older
/// true: a rephrase, a split, a merge. No text comparison separates those from a genuine
/// correction - "port is 5432" → "port is 5433" is more textually similar than most
/// rephrases. That distinction is semantic, and it is the prompt's job.
pub(super) fn replace_is_supported(
    link: &RelationInput,
    older_id: &str,
    newer_id: &str,
    memories: &[KnowledgeMemory],
) -> bool {
    let (was, now) = (comparison_key(&link.was), comparison_key(&link.now));
    if was.is_empty() || now.is_empty() || was == now {
        return false;
    }
    let content = |id: &str| {
        memories
            .iter()
            .find(|m| m.id == id)
            .map(|m| comparison_key(&m.content))
    };
    let (Some(older), Some(newer)) = (content(older_id), content(newer_id)) else {
        return false;
    };
    older.contains(&was) && newer.contains(&now)
}

/// The `replace` links this verdict cannot support, rendered for the model.
pub(super) fn unsupported_replaces(v: &EntityVerdict, memories: &[KnowledgeMemory]) -> Vec<String> {
    let mut out = Vec::new();
    for related in &v.relations {
        for link in &related.links {
            if link.relation != RelationType::Replace {
                continue;
            }
            if link.derived_from_property
                || replace_is_supported(link, related.memory.trim(), link.to.trim(), memories)
            {
                continue;
            }
            out.push(format!(
                "- `{}` replacing `{}`: was=\"{}\" now=\"{}\"",
                related.memory.trim(),
                link.to.trim(),
                link.was,
                link.now
            ));
        }
    }
    out
}

pub(super) fn promotion_suggestions(
    source_path: &str,
    verdict: &EntityVerdict,
    entities: &[KnowledgeEntity],
    links: &[KnowledgeEntityLink],
) -> Vec<PromotionSuggestion> {
    let linked_paths: HashSet<&str> = links
        .iter()
        .filter(|link| link.from_entity_path == source_path)
        .map(|link| link.to_entity_path.as_str())
        .collect();
    let mut already_linked_values = HashSet::new();
    for entity in entities {
        if !linked_paths.contains(entity.path.as_str()) {
            continue;
        }
        already_linked_values.insert(comparison_key(&entity.name));
        already_linked_values.insert(comparison_key(&entity.path));
        already_linked_values.extend(entity.aliases.iter().map(|alias| comparison_key(alias)));
    }
    let already_decided: HashSet<(String, String)> = verdict
        .entity_relations
        .iter()
        .map(|r| (comparison_key(&r.attribute), comparison_key(&r.value)))
        .collect();
    let mut out = Vec::new();
    let Some(attributes) = verdict.attributes.as_object() else {
        return out;
    };
    for (attribute, value) in attributes {
        let values: Vec<&str> = match value {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(items) => {
                items.iter().filter_map(serde_json::Value::as_str).collect()
            }
            _ => Vec::new(),
        };
        for value in values {
            let value_key = comparison_key(value);
            if value_key.is_empty()
                || already_linked_values.contains(&value_key)
                || already_decided.contains(&(comparison_key(attribute), value_key.clone()))
            {
                continue;
            }
            let normal_path = normalize_path(value);
            let mut candidates = Vec::new();
            for entity in entities {
                let exact = comparison_key(&entity.name) == value_key
                    || entity
                        .aliases
                        .iter()
                        .any(|a| comparison_key(a) == value_key)
                    || normal_path.as_ref().is_some_and(|p| p == &entity.path);
                if exact {
                    candidates.push((entity.path.clone(), entity.name.clone()));
                }
            }
            candidates.sort();
            candidates.dedup();
            if !candidates.is_empty() {
                out.push(PromotionSuggestion {
                    attribute: attribute.clone(),
                    value: value.to_string(),
                    candidates,
                });
            }
        }
    }
    out
}

pub(super) fn render_promotion_suggestions(
    prompts: &ReconcilePrompts,
    suggestions: &[PromotionSuggestion],
) -> Result<String, Error> {
    let mut block = String::new();
    for suggestion in suggestions {
        block.push_str(&format!(
            "- attribute `{}` = \"{}\"\n",
            suggestion.attribute, suggestion.value
        ));
        for (path, name) in &suggestion.candidates {
            block.push_str(&format!("  - `{path}` — {name}\n"));
        }
    }
    prompts.advisory(&block)
}

pub(super) struct RelationPromotionValidation<'a> {
    pub(super) ctx: &'a ReconcileInput,
    pub(super) proposals: &'a ProposalSet,
    pub(super) source_path: &'a str,
    pub(super) prefixes: &'a crate::ontology::PrefixMap,
    pub(super) memories: &'a [KnowledgeMemory],
    pub(super) known_data_properties: &'a HashSet<String>,
    pub(super) existing: &'a [(String, String, String)],
}

pub(super) async fn relation_promotion_rejections(
    verdict: &EntityVerdict,
    validation: RelationPromotionValidation<'_>,
) -> Result<Vec<String>, Error> {
    let RelationPromotionValidation {
        ctx,
        proposals,
        source_path,
        prefixes,
        memories,
        known_data_properties,
        existing,
    } = validation;
    let mut out = Vec::new();
    for relation in &verdict.entity_relations {
        let (attribute, value, target) = (
            relation.attribute.trim(),
            relation.value.trim(),
            relation.target.trim(),
        );
        let cited = memories
            .iter()
            .filter(|memory| {
                relation
                    .source_memory_ids
                    .iter()
                    .any(|id| id.trim() == memory.id)
            })
            .collect::<Vec<_>>();
        let memory_context = if cited.is_empty() {
            "(no current cited memory was found)".to_string()
        } else {
            cited
                .iter()
                .map(|memory| format!("{}: {}", memory.id, memory.content.trim()))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let context = |rule: &str, target_identity: &str| {
            format!(
                "Relation `{source_path}` attribute `{attribute}` value {value:?} -> `{target}` property `{}` with source memories {:?} failed `{rule}`. Supporting memory: {memory_context}. Target identity: {target_identity}. Correct the value, target, property, or source memory IDs, or remove this relation explicitly.",
                relation.property.trim(),
                relation.source_memory_ids,
            )
        };
        let semantic_key = comparison_key(attribute.rsplit(':').next().unwrap_or(attribute));
        if let Some((_, held_property, _)) =
            existing
                .iter()
                .find(|(held_key, held_property, held_target)| {
                    held_target == target
                        && comparison_key(held_key.rsplit(':').next().unwrap_or(held_key))
                            == semantic_key
                        && prefixes.expand(held_property)
                            != prefixes.expand(relation.property.trim())
                })
        {
            out.push(context(
                "pending_relation_is_authoritative",
                &format!(
                    "the staged Classify relation already maps this attribute and target with property `{held_property}`"
                ),
            ));
            continue;
        }
        if attribute.is_empty() || value.is_empty() || target.is_empty() || target == source_path {
            out.push(context(
                "complete_non_self_relation",
                "the target path must be present and different from the source entity",
            ));
            continue;
        }
        let value_key = comparison_key(value);
        if !cited
            .iter()
            .any(|memory| comparison_key(&memory.content).contains(&value_key))
        {
            out.push(context(
                "value_supported_by_cited_memory",
                "not evaluated because the cited memory did not contain the proposed value",
            ));
            continue;
        }
        let Some(entity) = entity_in_pass(ctx, proposals, target).await? else {
            out.push(context(
                "known_target_page",
                "no staged or committed target entity exists at this path",
            ));
            continue;
        };
        let target_identity = format!(
            "name={:?}, aliases={:?}, path={:?}",
            entity.name, entity.aliases, entity.path,
        );
        let exact = comparison_key(&entity.name) == value_key
            || entity
                .aliases
                .iter()
                .any(|alias| comparison_key(alias) == value_key)
            || normalize_path(value).is_some_and(|path| path == entity.path);
        if !exact {
            out.push(context(
                "value_resolves_to_target_identity",
                &target_identity,
            ));
            continue;
        }
        let property = match prefixes.repair_term(relation.property.trim(), TermKind::Property) {
            Ok(property) => property,
            Err(_) => {
                out.push(context("valid_object_property", &target_identity));
                continue;
            }
        };
        if known_data_properties
            .iter()
            .any(|known| prefixes.expand(known) == prefixes.expand(&property))
        {
            out.push(context("property_is_not_a_data_property", &target_identity));
        }
    }
    Ok(out)
}

pub(super) async fn accepted_promotions(
    verdict: &EntityVerdict,
    ctx: &ReconcileInput,
    proposals: &ProposalSet,
    source_path: &str,
    prefixes: &crate::ontology::PrefixMap,
    memories: &[KnowledgeMemory],
    existing: &[(String, String, String)],
) -> Result<Vec<ReconcilePromotion>, Error> {
    let memory_text = memories
        .iter()
        .map(|m| comparison_key(&m.content))
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for relation in &verdict.entity_relations {
        let (attribute, value, target) = (
            relation.attribute.trim(),
            relation.value.trim(),
            relation.target.trim(),
        );
        if attribute.is_empty() || value.is_empty() || target.is_empty() || target == source_path {
            continue;
        }
        let value_key = comparison_key(value);
        if !memory_text.iter().any(|text| text.contains(&value_key)) {
            warn!(
                source = %source_path,
                attribute,
                value,
                target,
                "pkm reconcile: ungrounded entity relation dropped"
            );
            continue;
        }
        let Some(entity) = entity_in_pass(ctx, proposals, target).await? else {
            warn!(
                source = %source_path,
                attribute,
                value,
                target,
                "pkm reconcile: entity relation with unknown target dropped"
            );
            continue;
        };
        let exact = comparison_key(&entity.name) == value_key
            || entity
                .aliases
                .iter()
                .any(|a| comparison_key(a) == value_key)
            || normalize_path(value).is_some_and(|p| p == entity.path);
        if !exact {
            warn!(
                source = %source_path,
                attribute,
                value,
                target,
                "pkm reconcile: entity relation target was not an exact value match"
            );
            continue;
        }
        let property = match prefixes.repair_term(relation.property.trim(), TermKind::Property) {
            Ok(property) => property,
            Err(_) => {
                warn!(
                    source = %source_path,
                    attribute,
                    value,
                    target,
                    property = %relation.property,
                    "pkm reconcile: entity relation with invalid property dropped"
                );
                continue;
            }
        };
        let catalogue = ctx.ontology.catalog(&ctx.graph.scope_id).await?;
        if catalogue.data_properties.contains(&property) {
            warn!(
                source = %source_path,
                attribute,
                property,
                "pkm reconcile: refusing to redeclare a data property as an object property"
            );
            continue;
        }
        let stored_key = curie_key(attribute, prefixes);
        let semantic_key = |key: &str| comparison_key(key.rsplit(':').next().unwrap_or(key));
        if existing.iter().any(|(held_key, _, held_target)| {
            held_target == target && semantic_key(held_key) == semantic_key(&stored_key)
        }) {
            tracing::debug!(
                source = %source_path,
                attribute = %stored_key,
                target,
                proposed_property = %property,
                "pkm reconcile: attaching provenance to pending relation"
            );
        }
        if seen.insert((stored_key.clone(), property.clone(), target.to_string())) {
            let declaration = verdict
                .declarations
                .iter()
                .find(|declaration| {
                    prefixes.expand(declaration.term()) == prefixes.expand(&property)
                })
                .and_then(|declaration| serde_json::to_value(declaration).ok());
            out.push(ReconcilePromotion {
                key: stored_key,
                property,
                target: target.to_string(),
                source_memory_ids: relation.source_memory_ids.clone(),
                declaration,
            });
        }
    }
    Ok(out)
}

pub(super) fn reconcile_declaration_rejections(
    verdict: &EntityVerdict,
    known_object_properties: &HashSet<String>,
    known_data_properties: &HashSet<String>,
    prefixes: &crate::ontology::PrefixMap,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut declarations = HashMap::new();
    for declaration in &verdict.declarations {
        let term = declaration.term().trim();
        if term.is_empty() {
            out.push("- an ontology declaration has an empty term".into());
        } else if declaration.description().is_empty() {
            out.push(format!("- `{term}` needs a semantic description"));
        } else if declarations
            .insert(prefixes.expand(term), declaration)
            .is_some()
        {
            out.push(format!("- `{term}` is declared more than once"));
        }
    }
    for relation in &verdict.entity_relations {
        let Ok(property) = prefixes.repair_term(relation.property.trim(), TermKind::Property)
        else {
            continue;
        };
        if known_data_properties
            .iter()
            .any(|held| prefixes.expand(held) == prefixes.expand(&property))
        {
            out.push(format!(
                "- relation `{property}` is already a data property and cannot point to an entity"
            ));
            continue;
        }
        if !prefixes.expand(&property).starts_with("urn:frona:")
            || known_object_properties
                .iter()
                .any(|held| prefixes.expand(held) == prefixes.expand(&property))
        {
            continue;
        }
        match declarations.get(&prefixes.expand(&property)) {
            Some(OntologyDeclaration::ObjectProperty { .. }) => {}
            _ => out.push(format!(
                "- new relation `{property}` needs one object_property declaration with its intent"
            )),
        }
    }
    if let Some(attributes) = verdict.attributes.as_object() {
        for key in attributes.keys() {
            let property = curie_key(key, prefixes);
            if known_object_properties
                .iter()
                .any(|held| prefixes.expand(held) == prefixes.expand(&property))
            {
                out.push(format!(
                    "- attribute `{property}` is already an object property and cannot hold a literal"
                ));
                continue;
            }
            if !prefixes.expand(&property).starts_with("urn:frona:")
                || known_data_properties
                    .iter()
                    .any(|held| prefixes.expand(held) == prefixes.expand(&property))
            {
                continue;
            }
            match declarations.get(&prefixes.expand(&property)) {
                Some(OntologyDeclaration::DataProperty { .. }) => {}
                _ => out.push(format!(
                    "- new attribute `{property}` needs one data_property declaration with its intent"
                )),
            }
        }
    }
    out
}
