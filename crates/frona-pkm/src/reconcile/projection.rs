use std::collections::{HashMap, HashSet};

use chrono::Utc;

use super::ReconcileInput;
use super::comparison_key;
use crate::Error;
use crate::classify::ProposalSet;
use crate::model::{
    AttributeSource, KnowledgeEntity, KnowledgeEntityLink, KnowledgeMemory, RelationType,
};
use crate::ontology::TermKind;
use crate::prompt::prompt_evidence;
use crate::reconcile::validation::has_attribute_assertion;
use crate::reconcile::{EntityVerdict, Related, RelationInput};

pub(super) async fn entity_in_pass(
    ctx: &ReconcileInput,
    proposals: &ProposalSet,
    path: &str,
) -> Result<Option<KnowledgeEntity>, Error> {
    let draft = proposals.entity_draft();
    Ok(ctx
        .graph
        .entity_by_path_with(&draft, path)?
        .map(|entity| entity.as_knowledge_entity()))
}

pub(super) fn render_memory_lines(
    memories: &[KnowledgeMemory],
    memory_entities: &HashMap<String, Vec<String>>,
) -> String {
    let now = Utc::now();
    let mut out = String::new();
    for m in memories {
        let age = (now - m.created_at).num_seconds().max(0);
        let when = if age < 3600 {
            format!("{}m ago", age / 60)
        } else if age < 86400 {
            format!("{}h ago", age / 3600)
        } else {
            format!("{}d ago", age / 86400)
        };
        out.push_str(&format!(
            "- [{:?}] id={}  ({when})  entities={}  evidence={}  {}\n",
            m.kind,
            m.id,
            serde_json::to_string(memory_entities.get(&m.id).map(Vec::as_slice).unwrap_or(&[]))
                .unwrap_or_else(|_| "[]".into()),
            prompt_evidence(&m.evidence),
            m.content
        ));
    }
    out
}

pub(super) fn replacement_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

pub(super) fn target_value(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).replace('-', " ")
}

pub(super) fn sources_include(held: &[String], required: &[String]) -> bool {
    !required.is_empty() && required.iter().all(|id| held.iter().any(|held| held == id))
}

pub(super) fn sources_equal(held: &[String], submitted: &[String]) -> bool {
    held.len() == submitted.len()
        && sources_include(held, submitted)
        && sources_include(submitted, held)
}

pub(super) fn sorted_sources(ids: &[String]) -> Vec<&str> {
    let mut ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

pub(super) fn source_set_mismatch(side: &str, supplied: &[String], expected: &[String]) -> String {
    let missing = expected
        .iter()
        .filter(|id| !supplied.contains(id))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let unexpected = supplied
        .iter()
        .filter(|id| !expected.contains(id))
        .map(String::as_str)
        .collect::<Vec<_>>();
    format!(
        "{side} source ids do not match stored provenance: expected {:?}, supplied {:?}, missing {:?}, unexpected {:?}.",
        sorted_sources(expected),
        sorted_sources(supplied),
        missing,
        unexpected,
    )
}

pub(super) fn add_inferred_replace(
    relations: &mut Vec<Related>,
    old: &str,
    new: &str,
    was: &str,
    now: &str,
    note: &str,
) {
    if old == new || old.is_empty() || new.is_empty() {
        return;
    }
    let related = match relations.iter_mut().find(|related| related.memory == old) {
        Some(related) => related,
        None => {
            relations.push(Related {
                memory: old.to_string(),
                links: Vec::new(),
            });
            relations.last_mut().expect("just pushed")
        }
    };
    if related
        .links
        .iter()
        .any(|link| link.relation == RelationType::Replace && link.to == new)
    {
        return;
    }
    related.links.push(RelationInput {
        relation: RelationType::Replace,
        to: new.to_string(),
        was: was.to_string(),
        now: now.to_string(),
        note: note.to_string(),
        derived_from_property: true,
    });
}

/// Close explicit data/object transitions over whole-memory replacement. The model owns
/// the semantic property decision; provenance deterministically supplies the memory
/// lifecycle decision so the graph and History can never disagree.
pub(super) fn close_property_replacements(verdict: &mut EntityVerdict) {
    let attribute_edges = verdict
        .attribute_replacements
        .iter()
        .flat_map(|replacement| {
            let was = replacement_value(&replacement.was);
            let now = replacement_value(&replacement.now);
            replacement
                .old_source_memory_ids
                .iter()
                .flat_map(move |old| {
                    let was = was.clone();
                    let now = now.clone();
                    replacement.new_source_memory_ids.iter().map(move |new| {
                        (
                            old.clone(),
                            new.clone(),
                            was.clone(),
                            now.clone(),
                            replacement.property.clone(),
                        )
                    })
                })
        });
    let relation_edges = verdict
        .entity_relation_replacements
        .iter()
        .flat_map(|replacement| {
            let was = target_value(&replacement.was_target);
            let now = target_value(&replacement.now_target);
            replacement
                .old_source_memory_ids
                .iter()
                .flat_map(move |old| {
                    let was = was.clone();
                    let now = now.clone();
                    replacement.new_source_memory_ids.iter().map(move |new| {
                        (
                            old.clone(),
                            new.clone(),
                            was.clone(),
                            now.clone(),
                            replacement.property.clone(),
                        )
                    })
                })
        });
    let relations = &mut verdict.relations;
    for (old, new, was, now, property) in attribute_edges.chain(relation_edges) {
        add_inferred_replace(
            relations,
            &old,
            &new,
            &was,
            &now,
            &format!("Derived from replacement of `{property}`."),
        );
    }
}

pub(super) fn property_replacement_rejections(
    verdict: &EntityVerdict,
    memories: &[KnowledgeMemory],
    existing_attributes: &[AttributeSource],
    existing_links: &[KnowledgeEntityLink],
) -> Vec<String> {
    let active: HashSet<&str> = memories.iter().map(|memory| memory.id.as_str()).collect();
    let mut rejected = Vec::new();
    for replacement in &verdict.attribute_replacements {
        let property = replacement.property.trim();
        let old_assertion = existing_attributes
            .iter()
            .find(|source| source.property == property && source.value == replacement.was);
        let new_assertion = verdict
            .attribute_sources
            .iter()
            .find(|source| source.property == property && source.value == replacement.now);
        let new_held = has_attribute_assertion(&verdict.attributes, property, &replacement.now);
        let old_removed = !has_attribute_assertion(&verdict.attributes, property, &replacement.was);
        let mut reasons = Vec::new();
        if replacement.was == replacement.now {
            reasons.push("`was` and `now` are identical.".to_string());
        }
        match old_assertion {
            None => reasons.push(format!(
                "No stored `{property}` assertion has old value {}.",
                replacement.was
            )),
            Some(source)
                if !sources_equal(
                    &source.source_memory_ids,
                    &replacement.old_source_memory_ids,
                ) =>
            {
                reasons.push(source_set_mismatch(
                    "Old",
                    &replacement.old_source_memory_ids,
                    &source.source_memory_ids,
                ))
            }
            Some(_) => {}
        }
        match new_assertion {
            None => reasons.push(format!(
                "The submitted attribute_sources do not contain `{property}` value {}.",
                replacement.now,
            )),
            Some(source)
                if !sources_equal(
                    &source.source_memory_ids,
                    &replacement.new_source_memory_ids,
                ) =>
            {
                reasons.push(source_set_mismatch(
                    "New",
                    &replacement.new_source_memory_ids,
                    &source.source_memory_ids,
                ))
            }
            Some(_) => {}
        }
        if !new_held {
            reasons.push(format!(
                "The new value {} is not retained in attributes.",
                replacement.now
            ));
        }
        if !old_removed {
            reasons.push(format!(
                "The old value {} is still present in attributes.",
                replacement.was
            ));
        }
        let inactive = replacement
            .new_source_memory_ids
            .iter()
            .filter(|id| !active.contains(id.as_str()))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !inactive.is_empty() {
            reasons.push(format!(
                "New source ids {inactive:?} are not current memories."
            ));
        }
        if !reasons.is_empty() {
            rejected.push(format!(
                "Data-property replacement `{property}` was rejected:\n- {}",
                reasons.join("\n- "),
            ));
        }
    }
    for replacement in &verdict.entity_relation_replacements {
        let property = replacement.property.trim();
        let old_assertion = existing_links.iter().find(|link| {
            link.relation == property && link.to_entity_path == replacement.was_target
        });
        let new_existing = existing_links.iter().find(|link| {
            link.relation == property && link.to_entity_path == replacement.now_target
        });
        let new_submitted = verdict.entity_relations.iter().find(|relation| {
            relation.property == property && relation.target == replacement.now_target
        });
        let old_retracted = verdict.relation_retractions.iter().any(|retraction| {
            retraction.property == property && retraction.target == replacement.was_target
        });
        let mut reasons = Vec::new();
        if replacement.was_target == replacement.now_target {
            reasons.push("`was_target` and `now_target` are identical.".to_string());
        }
        match old_assertion {
            None => reasons.push(format!(
                "No stored `{property}` assertion targets `{}`.",
                replacement.was_target
            )),
            Some(link)
                if !sources_equal(&link.source_memory_ids, &replacement.old_source_memory_ids) =>
            {
                reasons.push(source_set_mismatch(
                    "Old",
                    &replacement.old_source_memory_ids,
                    &link.source_memory_ids,
                ))
            }
            Some(_) => {}
        }
        let expected_new_sources = new_submitted
            .map(|relation| &relation.source_memory_ids)
            .or_else(|| new_existing.map(|link| &link.source_memory_ids));
        match expected_new_sources {
            None => reasons.push(format!(
                "No retained or submitted `{property}` assertion targets `{}`.",
                replacement.now_target,
            )),
            Some(expected) if !sources_equal(expected, &replacement.new_source_memory_ids) => {
                reasons.push(source_set_mismatch(
                    "New",
                    &replacement.new_source_memory_ids,
                    expected,
                ));
            }
            Some(_) => {}
        }
        if !old_retracted {
            reasons.push(format!(
                "relation_retractions does not retract `{property}` → `{}`.",
                replacement.was_target,
            ));
        }
        let inactive = replacement
            .new_source_memory_ids
            .iter()
            .filter(|id| !active.contains(id.as_str()))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !inactive.is_empty() {
            reasons.push(format!(
                "New source ids {inactive:?} are not current memories."
            ));
        }
        if !reasons.is_empty() {
            rejected.push(format!(
                "Object-property replacement `{property}` was rejected:\n- {}",
                reasons.join("\n- "),
            ));
        }
    }
    rejected
}

pub(super) fn memory_replacement_property_rejections(
    verdict: &EntityVerdict,
    existing_attributes: &[AttributeSource],
    existing_links: &[KnowledgeEntityLink],
) -> Vec<String> {
    let mut rejected = Vec::new();
    for related in &verdict.relations {
        for link in related
            .links
            .iter()
            .filter(|link| link.relation == RelationType::Replace)
        {
            let was = comparison_key(&link.was);
            let now = comparison_key(&link.now);
            let matching_attributes = existing_attributes
                .iter()
                .filter(|source| {
                    source
                        .source_memory_ids
                        .iter()
                        .any(|id| id == &related.memory)
                        && comparison_key(&replacement_value(&source.value)) == was
                })
                .collect::<Vec<_>>();
            let matching_links = existing_links
                .iter()
                .filter(|held| {
                    held.source_memory_ids
                        .iter()
                        .any(|id| id == &related.memory)
                        && comparison_key(&target_value(&held.to_entity_path)) == was
                })
                .collect::<Vec<_>>();
            let attribute_covered = matching_attributes.iter().all(|source| {
                verdict.attribute_replacements.iter().any(|replacement| {
                    replacement.property == source.property
                        && replacement.was == source.value
                        && comparison_key(&replacement_value(&replacement.now)) == now
                        && replacement.old_source_memory_ids.contains(&related.memory)
                        && replacement.new_source_memory_ids.contains(&link.to)
                })
            });
            let relation_covered = matching_links.iter().all(|held| {
                verdict
                    .entity_relation_replacements
                    .iter()
                    .any(|replacement| {
                        replacement.property == held.relation
                            && replacement.was_target == held.to_entity_path
                            && comparison_key(&target_value(&replacement.now_target)) == now
                            && replacement.old_source_memory_ids.contains(&related.memory)
                            && replacement.new_source_memory_ids.contains(&link.to)
                    })
            });
            if (!matching_attributes.is_empty() && !attribute_covered)
                || (!matching_links.is_empty() && !relation_covered)
            {
                rejected.push(format!(
                    "Memory replacement `{}` → `{}` changes `{}` to `{}` but its materialized assertion was not replaced in the typed replacement list.",
                    related.memory, link.to, link.was, link.now
                ));
            }
        }
    }
    rejected
}

pub(super) fn unsupported_scope_relations(
    candidate: &EntityVerdict,
    memory_entities: &HashMap<String, Vec<String>>,
    current_page: &str,
) -> Vec<String> {
    let mut rejected = Vec::new();
    for related in &candidate.relations {
        let subordinate = related.memory.trim();
        let Some(subordinate_pages) = memory_entities.get(subordinate) else {
            continue;
        };
        for link in &related.links {
            let survivor = link.to.trim();
            let Some(survivor_pages) = memory_entities.get(survivor) else {
                continue;
            };
            let scope_is_safe = if link.relation == RelationType::Replace {
                subordinate_pages
                    .iter()
                    .any(|entity| entity == current_page)
                    && survivor_pages.iter().any(|entity| entity == current_page)
            } else {
                subordinate_pages == survivor_pages
            };
            if !scope_is_safe {
                rejected.push(format!(
                    "Cannot apply global {:?} from memory {} (entities={}) to {} (entities={}): both replacement memories must include the entity being reconciled; duplicate/absorbed memories must have identical scope.",
                    link.relation,
                    subordinate,
                    serde_json::to_string(subordinate_pages).unwrap_or_else(|_| "[]".into()),
                    survivor,
                    serde_json::to_string(survivor_pages).unwrap_or_else(|_| "[]".into()),
                ));
            }
        }
    }
    rejected
}

/// Common free-text attribute keys → their standard datatype-property CURIE. Anything
/// not here (and not already a CURIE) falls back to a `frona:` term.
pub(super) const STD_ATTRIBUTE_CURIES: &[(&str, &str)] = &[
    ("name", "schema:name"),
    ("email", "schema:email"),
    ("url", "schema:url"),
    ("website", "schema:url"),
    ("homepage", "schema:url"),
    ("telephone", "schema:telephone"),
    ("phone", "schema:telephone"),
    ("address", "schema:address"),
    ("description", "schema:description"),
    ("givenname", "schema:givenName"),
    ("familyname", "schema:familyName"),
    ("jobtitle", "schema:jobTitle"),
    ("role", "schema:roleName"),
    ("timezone", "schema:timezone"),
];

/// Render the entity's current attributes for the prompt, so the model refines them rather
/// than re-deriving the map from scratch.
pub(super) fn render_attribute_lines(attrs: &serde_json::Value) -> String {
    let Some(map) = attrs.as_object() else {
        return String::new();
    };
    map.iter()
        .map(|(k, v)| {
            let v = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("- {k}: {v}\n")
        })
        .collect()
}

/// Drop any attribute the entity already holds as a **relation** - the fact is recorded as
/// an edge, and holding it as a literal too is the duplication this guards.
///
/// Two ways an attribute can be that same fact, and both are needed:
///
///   * **Same property.** The key expands to a relation the entity already has. Compared by
///     expanded IRI, because the link carries whatever spelling was committed and a
///     CURIE/absolute mismatch would silently match nothing.
///   * **Same entity.** The value names an entity this one already links to. This is the
///     case that actually recurs: the Classify stage promoted `employer: "Acme"` to
///     `--frona:worksFor--> orgs/acme`, but the memory `"employer: Acme"` is still live,
///     so re-deriving it mints `frona:employer` - a *different* key pointing at the same
///     entity, which the property check alone cannot see. Without this the literal
///     reappears every pass, is promoted away every pass, and never settles.
///
/// The entity check is deliberately blunt: a genuinely distinct property that happens to
/// name an already-linked entity ("formerEmployer: Acme" beside a current `worksFor` edge)
/// is dropped with it. That costs a literal whose memory is still on the entity and still
/// rendered in History - against a duplicate that never converges, which is what the
/// alternative buys.
pub(super) fn drop_keys_held_as_relations(
    attrs: serde_json::Value,
    links: &[KnowledgeEntityLink],
    linked_names: &HashSet<String>,
    px: &crate::ontology::PrefixMap,
) -> serde_json::Value {
    let serde_json::Value::Object(map) = attrs else {
        return attrs;
    };
    let held: HashSet<String> = links.iter().map(|l| px.expand(&l.relation)).collect();
    let names_a_linked_page = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => linked_names.contains(&comparison_key(s)),
        _ => false,
    };
    serde_json::Value::Object(
        map.into_iter()
            .filter(|(k, v)| !held.contains(&px.expand(k)) && !names_a_linked_page(v))
            .collect(),
    )
}

/// Re-key an attributes object so every key is a CURIE (YAML-LD-liftable). A key that is
/// already a **usable** CURIE is kept; a standard-looking key maps to its standard property
/// (see [`STD_ATTRIBUTE_CURIES`]); anything else becomes `frona:{camelCase}`. Non-object
/// values pass through unchanged.
///
/// "Usable" rather than "contains a colon", because this is the one place a model-written
/// key reached storage unexamined: `frona:firmware download` has a colon, so it was kept
/// verbatim, and it expands to an IRI the delta can never parse back. There is no
/// conversation left to push back into by the time keys are re-keyed, so an unusable one is
/// slugged as if it were free text - `frona:firmwareDownload` - which is wrong in name only,
/// where keeping it would be wrong in kind.
pub(super) fn curie_key_attributes(
    attrs: &serde_json::Value,
    px: &crate::ontology::PrefixMap,
) -> serde_json::Value {
    let Some(map) = attrs.as_object() else {
        return attrs.clone();
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (k, v) in map {
        let key = curie_key(k.trim(), px);
        out.insert(key, v.clone());
    }
    serde_json::Value::Object(out)
}

/// One attribute key → the CURIE it is stored under.
///
/// A bare name that a standard vocabulary already has a property for takes that property -
/// checked first, because [`PrefixMap::repair_term`] would happily mint `frona:email` for a
/// key `schema:email` exists for. Everything else goes through repair, and if repair refuses
/// (an unbound prefix, which it will not guess at) the local name is repaired on its own:
/// `dc:title` becomes `frona:title` rather than a key that expands to `urn:frona:dc:title`.
pub(super) fn curie_key(k: &str, px: &crate::ontology::PrefixMap) -> String {
    if !k.contains(':')
        && let Some((_, curie)) = STD_ATTRIBUTE_CURIES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
    {
        return (*curie).to_string();
    }
    if let Ok(t) = px.repair_term(k, TermKind::Property) {
        return t;
    }
    let local = k.rsplit(':').next().unwrap_or(k);
    px.repair_term(local, TermKind::Property)
        .unwrap_or_else(|_| "frona:attribute".to_string())
}
