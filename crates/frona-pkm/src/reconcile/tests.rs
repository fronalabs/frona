use std::collections::{HashMap, HashSet};

use chrono::Utc;

use super::comparison_key;
use crate::model::{
    AttributeSource, EntityCategory, Episode, EpisodeStatus, KnowledgeEntity, KnowledgeEntityLink,
    KnowledgeMemory, RelationType, TemporalAnchor,
};
use crate::model::{Disposition, EvidenceSource, EvidenceStrength, MemoryEvidence, MemoryKind};
use crate::reconcile::projection::{
    close_property_replacements, curie_key, curie_key_attributes, drop_keys_held_as_relations,
    memory_replacement_property_rejections, property_replacement_rejections,
    unsupported_scope_relations,
};
use crate::reconcile::validation::{
    assertion_provenance_rejections, memory_names_entity, promotion_suggestions,
    reconcile_declaration_rejections, replace_is_supported, unsupported_replaces,
};
use crate::reconcile::{
    AttributeSourceInput, EntityRelation, EntityRetraction, EntityVerdict, Outdated, Related,
    RelationInput,
};

/// The bindings a unit test reasons against. Production takes these from the catalogue
/// (`OntologyManager::prefixes`); with none installed that is this same bundled set.
fn px() -> crate::ontology::PrefixMap {
    crate::ontology::PrefixMap::standard()
}

#[test]
fn empty_verdict_means_no_changes() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({})).unwrap();

    assert!(verdict.relations.is_empty());
    assert!(verdict.entity_relations.is_empty());
    assert!(verdict.relation_retractions.is_empty());
    assert!(verdict.entity_relation_replacements.is_empty());
    assert!(verdict.outdated.is_empty());
    assert!(verdict.attributes.is_null());
    assert!(verdict.attribute_sources.is_empty());
    assert!(verdict.attribute_replacements.is_empty());
    assert!(verdict.name.is_empty());
    assert!(verdict.description.is_empty());
    assert!(verdict.moves.is_empty());
    assert!(verdict.declarations.is_empty());
}

#[test]
fn reconcile_mint_requires_intent_and_does_not_infer_an_inverse_from_one_pair() {
    let without_declaration: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "plannedUser", "value": "Casey Owner",
            "property": "frona:plannedUser", "target": "people/me",
            "source_memory_ids": ["m1"]
        }]
    }))
    .unwrap();
    assert!(
        !reconcile_declaration_rejections(
            &without_declaration,
            &HashSet::new(),
            &HashSet::new(),
            &px()
        )
        .is_empty()
    );

    let with_intent: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "plannedUser", "value": "Casey Owner",
            "property": "frona:plannedUser", "target": "people/me",
            "source_memory_ids": ["m1"]
        }],
        "declarations": [{
            "kind": "object_property", "term": "frona:plannedUser",
            "description": "A person who plans to use the subject."
        }]
    }))
    .unwrap();
    assert!(
        reconcile_declaration_rejections(&with_intent, &HashSet::new(), &HashSet::new(), &px())
            .is_empty()
    );
    assert!(
        !with_intent.declarations[0]
            .edits()
            .iter()
            .any(|edit| { matches!(edit, crate::ontology::SchemaEdit::InverseProperties { .. }) })
    );
}

#[test]
fn object_property_cannot_be_reused_as_a_literal_attribute() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "attributes": {"frona:chip": "2nm A20 Pro"}
    }))
    .unwrap();
    let known = HashSet::from(["frona:chip".to_string()]);

    let rejections = reconcile_declaration_rejections(&verdict, &known, &HashSet::new(), &px());

    assert!(
        rejections
            .iter()
            .any(|rejection| rejection.contains("object property")),
        "using a known object property as a literal must be rejected immediately: {rejections:?}",
    );
}

#[test]
fn data_property_cannot_be_reused_as_an_entity_relation() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "serial",
            "value": "ACME",
            "property": "frona:serialNumber",
            "target": "organizations/acme"
        }]
    }))
    .unwrap();
    let known = HashSet::from(["frona:serialNumber".to_string()]);

    let rejections = reconcile_declaration_rejections(&verdict, &HashSet::new(), &known, &px());

    assert!(
        rejections
            .iter()
            .any(|rejection| rejection.contains("data property")),
        "using a known data property as an entity relation must be rejected immediately: {rejections:?}",
    );
}

fn mem(id: &str, content: &str) -> KnowledgeMemory {
    KnowledgeMemory {
        id: id.into(),
        user_id: "u".into(),
        created_at: Utc::now(),
        kind: MemoryKind::Fact,
        episode: None,
        content: content.into(),
        relations: Vec::new(),
        disposition: Disposition::None,
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![MemoryEvidence {
            strength: EvidenceStrength::Explicit,
            source: EvidenceSource::HumanEdit {
                page_path: "people/test".into(),
                quote: content.into(),
            },
        }],
    }
}

fn task_mem(id: &str, task_id: &str, status: EpisodeStatus) -> KnowledgeMemory {
    let mut memory = mem(id, id);
    memory.kind = MemoryKind::Episodic;
    memory.episode = Some(Episode {
        status,
        anchor: TemporalAnchor {
            message: id.into(),
            quote: String::new(),
        },
        duration: None,
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    });
    memory.evidence = vec![MemoryEvidence {
        strength: EvidenceStrength::Explicit,
        source: EvidenceSource::TaskLifecycle {
            message_id: id.into(),
            chat_id: "chat".into(),
            task_id: task_id.into(),
        },
    }];
    memory
}

#[test]
fn reconcile_projects_completed_task_plans_as_history() {
    let memories = vec![
        task_mem("sun-planned", "task-sun", EpisodeStatus::Planned),
        task_mem("sun-completed", "task-sun", EpisodeStatus::Occurred),
        task_mem("mon-planned", "task-mon", EpisodeStatus::Planned),
        task_mem("mon-completed", "task-mon", EpisodeStatus::Occurred),
    ];

    let (current, historical) = super::run::partition_reconcile_memories(&memories);

    assert_eq!(
        current
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        ["sun-completed", "mon-completed"]
    );
    assert_eq!(
        historical
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        ["sun-planned", "mon-planned"]
    );
}

#[test]
fn promotion_suggestions_consider_pending_peer_entities() {
    let pending = KnowledgeEntity {
        id: String::new(),
        user_id: "u".into(),
        path: "services/quote-feed".into(),
        origin: crate::model::EntityOrigin::Internal,
        category: EntityCategory::Concept,
        kinds: vec!["schema:Service".into()],
        name: "Quote Feed".into(),
        description: String::new(),
        identity_evidence: Vec::new(),
        attribute_sources: Vec::new(),
        source_memory_ids: Vec::new(),
        body: String::new(),
        sync_content: None,
        mirrored_rev: None,
        extracted_rev: None,
        related_playbooks: Vec::new(),
        search_text: "Quote Feed".into(),
        search_names: Vec::new(),
        search_name_tokens: Vec::new(),
        search_assertions: Vec::new(),
        attributes: serde_json::json!({}),
        use_count: 0,
        aliases: ["Market API".to_string()].into_iter().collect(),
        rev: None,
        updated_at: Utc::now(),
        rendered_at: chrono::DateTime::<Utc>::MIN_UTC,
    };
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "attributes": {"provider": "Market API"}
    }))
    .unwrap();
    let suggestions = promotion_suggestions("projects/dashboard", &verdict, &[pending], &[]);
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].candidates[0].0, "services/quote-feed");
}

#[test]
fn global_relation_between_different_entity_scopes_is_rejected() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "relations": [{
            "memory": "agent-naming",
            "links": [{"relation": "duplicate", "to": "assistant-name"}]
        }]
    }))
    .unwrap();
    let scopes = HashMap::from([
        (
            "agent-naming".to_string(),
            vec![
                "assistant/example-assistant".to_string(),
                "people/me".to_string(),
            ],
        ),
        (
            "assistant-name".to_string(),
            vec!["assistant/example-assistant".to_string()],
        ),
    ]);

    let rejected = unsupported_scope_relations(&verdict, &scopes, "assistant/example-assistant");

    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].contains("identical scope"));
}

#[test]
fn global_relation_with_identical_entity_scope_is_allowed() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "relations": [{
            "memory": "echo",
            "links": [{"relation": "duplicate", "to": "original"}]
        }]
    }))
    .unwrap();
    let entities = vec!["assistant/example-assistant".to_string()];
    let scopes = HashMap::from([
        ("echo".to_string(), entities.clone()),
        ("original".to_string(), entities),
    ]);

    assert!(
        unsupported_scope_relations(&verdict, &scopes, "assistant/example-assistant").is_empty()
    );
}

#[test]
fn replacement_may_have_broader_scope_when_both_memories_share_the_current_entity() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
            "relations": [{
                "memory": "former-employer",
                "links": [{
                    "relation": "replace", "to": "current-employer", "was": "Former Corp", "now": "Example Corp"
                }]
            }]
        })).unwrap();
    let scopes = HashMap::from([
        (
            "former-employer".to_string(),
            vec![
                "organizations/former-corp".to_string(),
                "people/me".to_string(),
            ],
        ),
        (
            "current-employer".to_string(),
            vec![
                "organizations/former-corp".to_string(),
                "organizations/example-corp".to_string(),
                "people/me".to_string(),
            ],
        ),
    ]);

    assert!(unsupported_scope_relations(&verdict, &scopes, "people/me").is_empty());
}

#[test]
fn object_property_replacement_retires_all_old_source_memories() {
    let mut verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp",
            "property": "schema:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["new-a", "new-b"]
        }],
        "relation_retractions": [{
            "property": "schema:worksFor",
            "target": "organizations/former-corp"
        }],
        "entity_relation_replacements": [{
            "property": "schema:worksFor",
            "was_target": "organizations/former-corp",
            "now_target": "organizations/example-corp",
            "old_source_memory_ids": ["old-a", "old-b"],
            "new_source_memory_ids": ["new-a", "new-b"]
        }]
    }))
    .unwrap();
    let memories = [
        mem("old-a", "Casey Owner works for Former Corp"),
        mem("old-b", "employer: Former Corp"),
        mem("new-a", "Casey Owner works for Example Corp"),
        mem("new-b", "employer: Example Corp"),
    ];
    let mut old_link = link("schema:worksFor");
    old_link.to_entity_path = "organizations/former-corp".into();
    old_link.source_memory_ids = vec!["old-a".into(), "old-b".into()];

    assert!(property_replacement_rejections(&verdict, &memories, &[], &[old_link],).is_empty());
    close_property_replacements(&mut verdict);
    let replacements = verdict
        .relations
        .iter()
        .flat_map(|related| {
            related
                .links
                .iter()
                .map(move |link| (related.memory.clone(), link.to.clone()))
        })
        .collect::<HashSet<_>>();
    assert_eq!(replacements.len(), 4);
    assert!(replacements.contains(&("old-a".into(), "new-a".into())));
    assert!(replacements.contains(&("old-b".into(), "new-b".into())));
}

#[test]
fn object_property_rejection_explains_source_set_difference() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp",
            "property": "schema:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["new"]
        }],
        "relation_retractions": [{
            "property": "schema:worksFor",
            "target": "organizations/former-corp"
        }],
        "entity_relation_replacements": [{
            "property": "schema:worksFor",
            "was_target": "organizations/former-corp",
            "now_target": "organizations/example-corp",
            "old_source_memory_ids": ["old-fact", "old-broad"],
            "new_source_memory_ids": ["new"]
        }]
    }))
    .unwrap();
    let memories = [mem("new", "employer: Example Corp")];
    let mut old_link = link("schema:worksFor");
    old_link.to_entity_path = "organizations/former-corp".into();
    old_link.source_memory_ids = vec!["old-fact".into()];

    let rejected = property_replacement_rejections(&verdict, &memories, &[], &[old_link]);

    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(
        rejected[0].contains("expected [\"old-fact\"]"),
        "{rejected:?}"
    );
    assert!(
        rejected[0].contains("supplied [\"old-broad\", \"old-fact\"]"),
        "{rejected:?}"
    );
    assert!(
        rejected[0].contains("unexpected [\"old-broad\"]"),
        "{rejected:?}"
    );
}

#[test]
fn old_replacement_sources_may_be_historical_when_the_stored_assertion_cites_them() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp",
            "property": "schema:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["new"]
        }],
        "relation_retractions": [{
            "property": "schema:worksFor",
            "target": "organizations/former-corp"
        }],
        "entity_relation_replacements": [{
            "property": "schema:worksFor",
            "was_target": "organizations/former-corp",
            "now_target": "organizations/example-corp",
            "old_source_memory_ids": ["historical-old"],
            "new_source_memory_ids": ["new"]
        }]
    }))
    .unwrap();
    let memories = [mem("new", "employer: Example Corp")];
    let mut old_link = link("schema:worksFor");
    old_link.to_entity_path = "organizations/former-corp".into();
    old_link.source_memory_ids = vec!["historical-old".into()];

    assert!(property_replacement_rejections(&verdict, &memories, &[], &[old_link],).is_empty());
}

#[test]
fn memory_replacement_must_replace_its_materialized_object_property() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "relations": [{
            "memory": "old",
            "links": [{
                "relation": "replace", "to": "new", "was": "Former Corp", "now": "Example Corp"
            }]
        }]
    }))
    .unwrap();
    let mut old_link = link("schema:worksFor");
    old_link.to_entity_path = "organizations/former-corp".into();
    old_link.source_memory_ids = vec!["old".into()];

    let rejected = memory_replacement_property_rejections(&verdict, &[], &[old_link]);

    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(rejected[0].contains("typed replacement"));
}

#[test]
fn memory_replacement_without_a_materialized_property_remains_valid() {
    let verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "relations": [{
            "memory": "old",
            "links": [{
                "relation": "replace", "to": "new", "was": "burned out", "now": "energized"
            }]
        }]
    }))
    .unwrap();

    assert!(memory_replacement_property_rejections(&verdict, &[], &[]).is_empty());
}

#[test]
fn data_property_replacement_retires_its_old_source_memory() {
    let mut verdict: EntityVerdict = serde_json::from_value(serde_json::json!({
        "attributes": {"frona:port": 5433},
        "attribute_sources": [{
            "property": "frona:port", "value": 5433, "source_memory_ids": ["new"]
        }],
        "attribute_replacements": [{
            "property": "frona:port", "was": 5432, "now": 5433,
            "old_source_memory_ids": ["old"], "new_source_memory_ids": ["new"]
        }]
    }))
    .unwrap();
    let old_source = AttributeSource {
        property: "frona:port".into(),
        value: serde_json::json!(5432),
        source_memory_ids: vec!["old".into()],
    };
    let memories = [mem("old", "port is 5432"), mem("new", "port is 5433")];

    assert!(property_replacement_rejections(&verdict, &memories, &[old_source], &[],).is_empty());
    close_property_replacements(&mut verdict);
    assert_eq!(verdict.relations.len(), 1);
    assert_eq!(verdict.relations[0].memory, "old");
    assert_eq!(verdict.relations[0].links[0].to, "new");
}

fn replace(was: &str, now: &str) -> RelationInput {
    RelationInput {
        relation: RelationType::Replace,
        to: "new".into(),
        was: was.into(),
        now: now.into(),
        note: String::new(),
        derived_from_property: false,
    }
}

/// A real correction: the older is now false, and the changed value is quotable from
/// each entry. This is the only case `replace` is for.
#[test]
fn named_and_quotable_change_is_accepted() {
    let memories = [
        mem("old", "postgres runs on port 5432"),
        mem("new", "postgres runs on port 5433"),
    ];
    assert!(replace_is_supported(
        &replace("5432", "5433"),
        "old",
        "new",
        &memories
    ));
}

/// The decisive case from the live DB: the model chained two **identical** entries as
/// a supersession, its own note reading "Reaffirmed - identical content". Forced to
/// name what changed, it has nothing to put.
#[test]
fn identical_restatement_is_refused() {
    let same = "the api service is written in Rust.";
    let memories = [mem("old", same), mem("new", same)];
    for link in [
        replace("", ""),
        replace(same, same),
        replace("Rust", "Rust"),
    ] {
        assert!(
            !replace_is_supported(&link, "old", "new", &memories),
            "nothing changed, so it is not a replacement"
        );
    }
}

/// A required field invites a model with nothing to say to invent something. The
/// values are checked against the entries they claim to come from.
#[test]
fn confabulated_change_is_refused() {
    let memories = [
        mem("old", "the api service is written in Rust."),
        mem("new", "the api service is written in Rust."),
    ];
    assert!(
        !replace_is_supported(&replace("Go", "Rust"), "old", "new", &memories),
        "`was` appears nowhere in the older entry"
    );
    assert!(
        !replace_is_supported(&replace("Rust", "Zig"), "old", "new", &memories),
        "`now` appears nowhere in the newer entry"
    );
}

/// Quoting is allowed to differ in case and spacing from the entry it came from -
/// a model that re-types a value should not be punished for capitalising it.
#[test]
fn quoting_is_matched_leniently() {
    let memories = [mem("old", "Port is 5432"), mem("new", "port  is   5433")];
    assert!(replace_is_supported(
        &replace("port IS 5432", "Port is 5433"),
        "old",
        "new",
        &memories
    ));
}

#[test]
fn outdated_memory_names_the_edge_target_it_retires() {
    let content = comparison_key("employer: Former Corp");
    assert!(memory_names_entity(
        &content,
        "Former Corp",
        &HashSet::new()
    ));
    assert!(memory_names_entity(
        &comparison_key("employer: EXC"),
        "Former Corp",
        &["EXC".to_string()].into_iter().collect()
    ));
    assert!(
        !memory_names_entity(
            &comparison_key("actively searching for a job"),
            "Former Corp",
            &HashSet::new()
        ),
        "an unrelated outdated memory must not retract the employer edge"
    );
}

/// What the guard deliberately cannot catch, recorded so the limit is not mistaken
/// for a bug: a rephrase names a real, quotable difference and still leaves the older
/// **true**. No text comparison separates that from a genuine correction - "5432" →
/// "5433" is more textually similar than most rephrases are. That distinction is
/// semantic, and it is the prompt's job.
#[test]
fn rephrase_still_passes_the_text_checks() {
    let memories = [
        mem("old", "the cache uses Redis."),
        mem("new", "the cache runs on a Redis instance."),
    ];
    assert!(replace_is_supported(
        &replace("uses Redis", "runs on a Redis instance"),
        "old",
        "new",
        &memories
    ));
}

/// The rejection list is what the model is shown, so it must name the offending pair.
#[test]
fn unsupported_replaces_reports_the_pairs_it_refused() {
    let memories = [mem("old", "same"), mem("new", "same")];
    let verdict = EntityVerdict {
        relations: vec![
            Related {
                memory: "old".into(),
                links: vec![replace("same", "same")],
            },
            // A non-`replace` is never the guard's business - only `replace` can file
            // a still-true fact under "do not use".
            Related {
                memory: "old".into(),
                links: vec![RelationInput {
                    relation: RelationType::Duplicate,
                    to: "new".into(),
                    was: String::new(),
                    now: String::new(),
                    note: String::new(),
                    derived_from_property: false,
                }],
            },
        ],
        entity_relations: Vec::new(),
        relation_retractions: Vec::new(),
        entity_relation_replacements: Vec::new(),
        outdated: Vec::new(),
        attributes: serde_json::Value::Null,
        attribute_sources: Vec::new(),
        attribute_replacements: Vec::new(),
        name: String::new(),
        description: String::new(),
        moves: Vec::new(),
        declarations: Vec::new(),
    };
    let rejected = unsupported_replaces(&verdict, &memories);
    assert_eq!(rejected.len(), 1, "only the replace: {rejected:?}");
    assert!(
        rejected[0].contains("old") && rejected[0].contains("new"),
        "{rejected:?}"
    );
}

#[test]
fn retiring_a_memory_requires_every_orphaned_assertion_to_change() {
    let memories = [
        mem("old", "employer: Former Corp"),
        mem("new", "employer: Example Corp"),
    ];
    let existing_attributes = [AttributeSource {
        property: "frona:employer".into(),
        value: serde_json::json!("Former Corp"),
        source_memory_ids: vec!["old".into()],
    }];
    let mut existing_link = link("schema:worksFor");
    existing_link.to_entity_path = "organizations/former-corp".into();
    existing_link.source_memory_ids = vec!["old".into()];
    let stale = EntityVerdict {
        relations: Vec::new(),
        entity_relations: Vec::new(),
        relation_retractions: Vec::new(),
        entity_relation_replacements: Vec::new(),
        outdated: vec![Outdated {
            memory: "old".into(),
            note: "joined Example Corp".into(),
        }],
        attributes: serde_json::json!({ "frona:employer": "Former Corp" }),
        attribute_sources: vec![AttributeSourceInput {
            property: "frona:employer".into(),
            value: serde_json::json!("Former Corp"),
            source_memory_ids: vec!["old".into()],
        }],
        attribute_replacements: Vec::new(),
        name: String::new(),
        description: String::new(),
        moves: Vec::new(),
        declarations: Vec::new(),
    };
    let rejected = assertion_provenance_rejections(
        &stale,
        &memories,
        &existing_attributes,
        &[existing_link.clone()],
        &HashMap::new(),
        "people/me",
    );
    assert!(
        rejected.iter().any(|line| line.contains("frona:employer")),
        "{rejected:?}"
    );
    assert!(
        rejected
            .iter()
            .any(|line| line.contains("relation_retractions")),
        "{rejected:?}"
    );

    let repaired = EntityVerdict {
        relations: Vec::new(),
        entity_relations: vec![EntityRelation {
            attribute: "employer".into(),
            value: "Example Corp".into(),
            property: "schema:worksFor".into(),
            target: "organizations/example-corp".into(),
            source_memory_ids: vec!["new".into()],
        }],
        relation_retractions: vec![EntityRetraction {
            property: "schema:worksFor".into(),
            target: "organizations/former-corp".into(),
        }],
        entity_relation_replacements: Vec::new(),
        outdated: vec![Outdated {
            memory: "old".into(),
            note: "joined Example Corp".into(),
        }],
        attributes: serde_json::json!({}),
        attribute_sources: Vec::new(),
        attribute_replacements: Vec::new(),
        name: String::new(),
        description: String::new(),
        moves: Vec::new(),
        declarations: Vec::new(),
    };
    assert!(
        assertion_provenance_rejections(
            &repaired,
            &memories,
            &existing_attributes,
            &[existing_link],
            &HashMap::new(),
            "people/me",
        )
        .is_empty()
    );
}

#[test]
fn multi_entity_memory_cannot_source_a_data_attribute() {
    let memories = [mem(
        "relationship",
        "Casey Owner named the assistant Example Assistant",
    )];
    let verdict = EntityVerdict {
        relations: Vec::new(),
        entity_relations: Vec::new(),
        relation_retractions: Vec::new(),
        entity_relation_replacements: Vec::new(),
        outdated: Vec::new(),
        attributes: serde_json::json!({ "frona:assistantName": "Example Assistant" }),
        attribute_sources: vec![AttributeSourceInput {
            property: "frona:assistantName".into(),
            value: serde_json::json!("Example Assistant"),
            source_memory_ids: vec!["relationship".into()],
        }],
        attribute_replacements: Vec::new(),
        name: String::new(),
        description: String::new(),
        moves: Vec::new(),
        declarations: Vec::new(),
    };
    let scopes = HashMap::from([(
        "relationship".into(),
        vec!["assistants/example-assistant".into(), "people/me".into()],
    )]);

    let rejected =
        assertion_provenance_rejections(&verdict, &memories, &[], &[], &scopes, "people/me");

    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(rejected[0].contains("multi-entity"), "{rejected:?}");
    assert!(
        rejected[0].contains("assistants/example-assistant"),
        "{rejected:?}"
    );
}

#[test]
fn free_text_key_becomes_a_curie() {
    assert_eq!(curie_key("port", &px()), "frona:port");
    assert_eq!(curie_key("host name", &px()), "frona:hostName");
    assert_eq!(curie_key("Restart Command", &px()), "frona:restartCommand");
    assert_eq!(curie_key("!!!", &px()), "frona:attribute");
}

/// A key that is *already* camelCase has no separator, so it arrives as one word - and
/// lowercasing a whole word is what produced every flat key in the vault
/// (`frona:firmwareversion`, `frona:supportemail`, `frona:aiassistantname`). The words
/// are still there, so nothing downstream reports it; it just reads as a different term
/// from the one the schema declared.
#[test]
fn already_camel_case_key_keeps_its_case() {
    assert_eq!(curie_key("supportEmail", &px()), "frona:supportEmail");
    assert_eq!(curie_key("firmwareVersion", &px()), "frona:firmwareVersion");
    assert_eq!(curie_key("aiAssistantName", &px()), "frona:aiAssistantName");
    assert_eq!(curie_key("support_email", &px()), "frona:supportEmail");
    assert_eq!(
        curie_key("firmware releaseDate", &px()),
        "frona:firmwareReleaseDate"
    );
}

/// An acronym is the one case that should be folded: `AI` as a first word is `ai`, not
/// `aI`, and a lone `URL` is `url`.
#[test]
fn acronym_is_lowered_rather_than_kept() {
    assert_eq!(
        curie_key("AI assistant name", &px()),
        "frona:aiAssistantName"
    );
    assert_eq!(curie_key("URL", &px()), "schema:url");
    assert_eq!(curie_key("support URL", &px()), "frona:supportUrl");
}

/// The leak this closes: a colon was taken as proof of a CURIE, so a model-written key
/// went to storage verbatim. `frona:firmware download` expands to an IRI the delta can
/// never parse back, and there is no conversation left to push back into here - so it is
/// slugged instead of kept.
#[test]
fn key_with_a_colon_is_only_kept_when_it_is_a_usable_term() {
    let v = serde_json::json!({
        "frona:firmware download": "1.0",   // a space - cannot be written
        "dc:title": "x",                    // `dc:` is not bound (`dcterms:` is)
        "frona:firmwareVersion": "1.0",     // usable → kept verbatim
    });
    let m = curie_key_attributes(&v, &px()).as_object().unwrap().clone();
    assert!(
        m.contains_key("frona:firmwareVersion"),
        "a usable CURIE is kept: {m:?}"
    );
    assert!(
        !m.contains_key("frona:firmware download"),
        "an unwritable key must not reach storage: {m:?}"
    );
    assert!(
        m.contains_key("frona:firmwareDownload"),
        "its local name is re-slugged, without folding the prefix in: {m:?}"
    );
    assert!(
        !m.contains_key("dc:title"),
        "an unbound prefix is not a CURIE: {m:?}"
    );
    assert!(
        m.contains_key("frona:title"),
        "`dc:title` keeps its local name: {m:?}"
    );
    for k in m.keys() {
        assert!(px().validate_term(k).is_ok(), "`{k}` must be usable");
    }
}

#[test]
fn curie_keys_map_standard_mint_frona_and_keep_curies() {
    let v = serde_json::json!({
        "email": "a@b.com",   // standard → schema:email
        "port": 5432,         // bespoke → frona:port
        "host name": "db",    // bespoke, spaced → frona:hostName
        "schema:url": "x",    // already a CURIE → kept
    });
    let out = curie_key_attributes(&v, &px());
    let m = out.as_object().unwrap();
    assert!(m.contains_key("schema:email"), "{m:?}");
    assert!(m.contains_key("frona:port"), "{m:?}");
    assert!(m.contains_key("frona:hostName"), "{m:?}");
    assert!(m.contains_key("schema:url"), "{m:?}");
    assert!(
        !m.contains_key("email") && !m.contains_key("port"),
        "originals removed: {m:?}"
    );
}

fn link(relation: &str) -> KnowledgeEntityLink {
    KnowledgeEntityLink {
        id: relation.into(),
        user_id: "u".into(),
        from_entity_path: "people/me".into(),
        to_entity_path: "organizations/example-corp".into(),
        relation: relation.into(),
        source_memory_ids: Vec::new(),
        origin: crate::model::LinkOrigin::Asserted,
        created_at: Utc::now(),
    }
}

/// The guarantee behind the prompt: once the Classify has decided a fact is an edge,
/// re-deriving it from the same memory must not put the literal back. This is the
/// revert that used to happen every pass, whichever way the decision had gone.
#[test]
fn attribute_already_held_as_a_relation_is_dropped() {
    let attrs = serde_json::json!({
        "schema:worksFor": "Example Corp",     // promoted - the edge below holds this
        "schema:jobTitle": "Engineer", // a real literal, untouched
    });
    let out =
        drop_keys_held_as_relations(attrs, &[link("schema:worksFor")], &HashSet::new(), &px());
    let m = out.as_object().unwrap();
    assert!(
        !m.contains_key("schema:worksFor"),
        "the promoted fact stays an edge: {m:?}"
    );
    assert_eq!(m.len(), 1, "and nothing else is disturbed: {m:?}");
}

/// The link and the attribute can be spelled differently - the entity stores whatever
/// was committed. Comparing raw strings matches nothing, which reads as "no relation
/// holds this" and lets the duplicate straight through.
#[test]
fn relation_check_compares_expanded_iris_not_spellings() {
    let attrs = serde_json::json!({ "schema:worksFor": "Example Corp" });
    let out = drop_keys_held_as_relations(
        attrs,
        &[link("https://schema.org/worksFor")],
        &HashSet::new(),
        &px(),
    );
    assert!(
        out.as_object().unwrap().is_empty(),
        "CURIE attribute vs absolute-IRI relation is the same property: {out:?}"
    );
}

#[test]
fn unrelated_relations_drop_nothing() {
    let attrs = serde_json::json!({ "schema:jobTitle": "Engineer" });
    let out = drop_keys_held_as_relations(
        attrs.clone(),
        &[link("schema:knows")],
        &HashSet::new(),
        &px(),
    );
    assert_eq!(out, attrs);
}

/// A promoted relation and a re-derived attribute can use different properties while
/// naming the same entity. Matching only on the property would restore the literal.
#[test]
fn attribute_naming_an_already_linked_entity_is_dropped() {
    let linked: HashSet<String> = [comparison_key("Example Corp")].into_iter().collect();
    let attrs = serde_json::json!({
        "frona:employer": "Example Corp",       // the same fact as the edge, re-derived
        "schema:jobTitle": "Engineer",  // about no entity at all
    });
    let out = drop_keys_held_as_relations(attrs, &[link("frona:worksFor")], &linked, &px());
    let m = out.as_object().unwrap();
    assert!(
        !m.contains_key("frona:employer"),
        "a different key for an already-linked entity is still the duplicate: {m:?}"
    );
    assert!(m.contains_key("schema:jobTitle"), "{m:?}");
}

#[test]
fn entity_check_ignores_case_and_spacing() {
    let linked: HashSet<String> = [comparison_key("Example Assistant")].into_iter().collect();
    let attrs = serde_json::json!({ "frona:assistantName": "  example   assistant " });
    let out = drop_keys_held_as_relations(attrs, &[link("frona:namedBy")], &linked, &px());
    assert!(out.as_object().unwrap().is_empty(), "{out:?}");
}

/// Only *string* values can name an entity. A number that happens to render like a
/// entity name is not a reference.
#[test]
fn non_string_value_is_never_an_entity_reference() {
    let linked: HashSet<String> = ["5432".to_string()].into_iter().collect();
    let attrs = serde_json::json!({ "frona:port": 5432 });
    let out = drop_keys_held_as_relations(attrs.clone(), &[link("frona:runsOn")], &linked, &px());
    assert_eq!(out, attrs);
}

mod execution;
