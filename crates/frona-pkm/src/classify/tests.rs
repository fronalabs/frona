use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;

use chrono::Utc;

use crate::classify::claim_automatic_identity_discovery;
use crate::classify::proposal::{
    AcceptedMint, AttributeDecisions, AttributeMapping, ClassChoice, Classification,
    EntityProposal, EntityShape, HasKeyMarker, NewEntity, OntologyDeclaration, ProposalSet,
    accept_mints, attribute_edits, classification_edits, render_value, search_terms,
};
use crate::model::{EntityCategory, KnowledgeConsolidationEntity, KnowledgeEntity};
use crate::ontology::PrefixMap;
use crate::ontology::SchemaEdit;

#[test]
fn classification_requires_the_entity_shape() {
    let result = serde_json::from_value::<Classification>(serde_json::json!({
        "classes": [{ "class": "schema:Person" }]
    }));

    assert!(result.is_err());
}

#[test]
fn minted_entity_requires_a_description() {
    let result = serde_json::from_value::<NewEntity>(serde_json::json!({
        "path": "people/taylor",
        "name": "Taylor",
        "class": "schema:Person",
        "from_facts": []
    }));

    assert!(result.is_err());
}

#[test]
fn classification_defaults_omitted_identity_markers() {
    let marked: Classification = serde_json::from_value(serde_json::json!({
        "entity": { "name": "Taylor Smith", "description": "A person", "aliases": [] },
        "classes": [{ "class": "schema:Person" }],
        "relations": [],
        "attributes": [
            { "from": "first name", "to": "schema:givenName", "targets": [] },
            { "from": "last name", "to": "schema:familyName", "targets": [] }
        ],
        "new_entities": [],
        "declarations": [],
        "has_keys": [{
            "class": "schema:Person",
            "properties": ["schema:givenName", "schema:familyName"]
        }],
        "inverse_functional_properties": []
    }))
    .unwrap();
    assert_eq!(marked.has_keys[0].class, "schema:Person");
    assert_eq!(
        marked.has_keys[0].properties,
        ["schema:givenName", "schema:familyName"]
    );

    let old: Classification = serde_json::from_value(serde_json::json!({
        "entity": { "name": "Taylor Smith", "description": "A person", "aliases": [] },
        "classes": [{ "class": "schema:Person" }],
        "relations": [], "attributes": [], "new_entities": [], "declarations": []
    }))
    .unwrap();
    assert!(old.has_keys.is_empty());
    assert!(old.inverse_functional_properties.is_empty());
}

#[test]
fn classification_rejects_identity_markers_that_do_not_describe_this_entity() {
    let candidate: Classification = serde_json::from_value(serde_json::json!({
        "entity": { "name": "Taylor", "description": "A person", "aliases": [] },
        "classes": [{ "class": "schema:Person" }],
        "relations": [{ "from": "profile", "to": "frona:ownedByProfile" }],
        "attributes": [
            { "from": "first name", "to": "schema:givenName", "targets": [] }
        ],
        "new_entities": [], "declarations": [],
        "has_keys": [{
            "class": "schema:Organization",
            "properties": ["schema:givenName", "schema:familyName"]
        }],
        "inverse_functional_properties": ["schema:givenName"]
    }))
    .unwrap();
    let feedback = candidate
        .identity_marker_feedback(&entity(
            "people/taylor",
            serde_json::json!({"first name": "Taylor"}),
        ))
        .unwrap();
    assert!(
        feedback.contains("schema:Organization is not one of this entity's classes"),
        "{feedback}"
    );
    assert!(
        feedback.contains("schema:familyName is not mapped or asserted on this entity"),
        "{feedback}"
    );
    assert!(
        feedback.contains("schema:givenName is not an object property"),
        "{feedback}"
    );
}

#[test]
fn proposal_trace_preserves_identity_markers_for_resume_and_resolve() {
    let mut proposals = ProposalSet::default();
    proposals.record(
        "people/taylor",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            has_keys: vec![HasKeyMarker {
                class: "schema:Person".into(),
                properties: vec!["schema:givenName".into(), "schema:familyName".into()],
            }],
            inverse_functional_properties: vec!["frona:ownedByProfile".into()],
            ..EntityProposal::default()
        },
    );
    let trace = proposals.trace_value();
    assert_eq!(
        trace["entities"]["people/taylor"]["has_keys"][0]["properties"],
        serde_json::json!(["schema:givenName", "schema:familyName"]),
    );
    assert_eq!(
        trace["entities"]["people/taylor"]["inverse_functional_properties"],
        serde_json::json!(["frona:ownedByProfile"]),
    );
}

#[test]
fn automatic_identity_discovery_is_one_shot_and_skipped_after_model_discovery() {
    let calls = 0;
    let challenged = AtomicBool::new(false);
    assert!(claim_automatic_identity_discovery(calls, &challenged));
    assert!(!claim_automatic_identity_discovery(calls, &challenged));

    let calls = 1;
    let challenged = AtomicBool::new(false);
    assert!(!claim_automatic_identity_discovery(calls, &challenged));
}

fn proposal(class: &str, edits: Vec<SchemaEdit>) -> EntityProposal {
    EntityProposal {
        classes: vec![class.into()],
        edits,
        rekeys: Vec::new(),
        attr_rekeys: Vec::new(),
        promoted: Vec::new(),
        promoted_sources: HashMap::new(),
        retracted: Vec::new(),
        has_keys: Vec::new(),
        inverse_functional_properties: Vec::new(),
    }
}

fn subclass(sub: &str, sup: &str) -> SchemaEdit {
    SchemaEdit::SubClassOf {
        sub: sub.into(),
        sup: sup.into(),
    }
}

fn entity(path: &str, attributes: serde_json::Value) -> KnowledgeConsolidationEntity {
    let mut entity = KnowledgeConsolidationEntity::from_committed(
        "run",
        KnowledgeEntity {
            id: String::new(),
            user_id: "u".into(),
            path: path.into(),
            origin: crate::model::EntityOrigin::Internal,
            category: EntityCategory::Concept,
            kinds: Vec::new(),
            name: path.into(),
            description: String::new(),
            identity_evidence: Vec::new(),
            attribute_sources: Vec::new(),
            source_memory_ids: vec!["memory-1".into()],
            body: String::new(),
            sync_content: None,
            mirrored_rev: None,
            extracted_rev: None,
            related_playbooks: Vec::new(),
            search_text: path.into(),
            attributes,
            search_names: Vec::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            use_count: 0,
            aliases: HashSet::new(),
            rev: None,
            updated_at: Utc::now(),
            rendered_at: chrono::DateTime::<Utc>::MIN_UTC,
        },
    );
    entity.entity_id = None;
    entity
}

#[test]
fn reasoning_excludes_unclassified_ingest_attributes_but_keeps_accepted_mappings() {
    let mut proposals = ProposalSet::default();
    proposals.stage_input_entity(entity(
        "services/quote-feed",
        serde_json::json!({"Vendor URL": "https://example.invalid"}),
    ));
    let projected = proposals.reasoning_entities(Vec::new());
    assert_eq!(projected[0].attributes, serde_json::json!({}));

    proposals.record(
        "services/quote-feed",
        EntityProposal {
            classes: vec!["schema:Service".into()],
            edits: Vec::new(),
            rekeys: Vec::new(),
            attr_rekeys: vec![("Vendor URL".into(), "schema:url".into())],
            promoted: Vec::new(),
            promoted_sources: HashMap::new(),
            retracted: Vec::new(),
            has_keys: Vec::new(),
            inverse_functional_properties: Vec::new(),
        },
    );
    let projected = proposals.reasoning_entities(Vec::new());
    assert_eq!(
        projected[0].attributes,
        serde_json::json!({"schema:url": "https://example.invalid"}),
    );
}

fn attr(from: &str, to: &str, target: Option<&str>) -> AttributeMapping {
    AttributeMapping {
        from: from.into(),
        to: to.into(),
        targets: target.map(str::to_string).into_iter().collect(),
    }
}

fn attr_many(from: &str, to: &str, targets: &[&str]) -> AttributeMapping {
    AttributeMapping {
        from: from.into(),
        to: to.into(),
        targets: targets.iter().map(|t| t.to_string()).collect(),
    }
}

/// Every target named is an entity that exists - the ordinary case, and what these tests
/// are about. `decide_known` is for the case where it does not.
fn decide(mappings: &[AttributeMapping], held: &[&str]) -> AttributeDecisions {
    let known: Vec<&str> = mappings
        .iter()
        .flat_map(|m| &m.targets)
        .map(String::as_str)
        .collect();
    decide_known(mappings, held, &known)
}

fn decide_known(
    mappings: &[AttributeMapping],
    held: &[&str],
    known: &[&str],
) -> AttributeDecisions {
    let held: HashSet<&str> = held.iter().copied().collect();
    let known: HashSet<String> = known.iter().map(|k| (*k).to_string()).collect();
    attribute_edits(mappings, "people/me", &held, &known, &PrefixMap::standard())
}

/// One attribute value naming several entities becomes several edges - and **one**
/// declaration, because the property is an object property once, not once per edge.
#[test]
fn one_value_naming_several_entities_becomes_one_edge_each() {
    let (edits, rekeys, promoted) = decide(
        &[attr_many(
            "programming languages",
            "frona:knowsLanguage",
            &[
                "topics/language-alpha",
                "topics/language-beta",
                "topics/language-gamma",
                "topics/language-delta",
            ],
        )],
        &["programming languages"],
    );
    assert_eq!(promoted.len(), 4, "one edge per named entity: {promoted:?}");
    for path in [
        "topics/language-alpha",
        "topics/language-beta",
        "topics/language-gamma",
        "topics/language-delta",
    ] {
        assert!(
            promoted.iter().any(|(_, _, t)| t == path),
            "{path} must get an edge: {promoted:?}"
        );
    }
    assert_eq!(
        edits,
        vec![SchemaEdit::DeclareObjectProperty {
            property: "frona:knowsLanguage".into()
        }],
        "declared once, as an object property"
    );
    assert!(
        rekeys.is_empty(),
        "a promoted key does not stay an attribute: {rekeys:?}"
    );
}

/// Duplicates and self-references are not edges. A repeated path would be the same edge
/// twice, and an entity related to itself says nothing.
#[test]
fn repeated_and_self_targets_are_dropped() {
    let (_, _, promoted) = decide(
        &[attr_many(
            "colleagues",
            "frona:worksWith",
            &["people/sarah", "people/sarah", "people/me", "  "],
        )],
        &["colleagues"],
    );
    assert_eq!(
        promoted.len(),
        1,
        "one edge, to the one distinct other entity: {promoted:?}"
    );
    assert_eq!(promoted[0].2, "people/sarah");
}

/// A single bare string is accepted where the schema asks for an array: a model that has
/// settled on exactly one entity has an obvious reason to write it unwrapped, and rejecting
/// that costs a turn to learn nothing.
#[test]
fn single_target_may_arrive_unwrapped() {
    let one: AttributeMapping = serde_json::from_value(serde_json::json!({
        "from": "employer", "to": "schema:worksFor", "targets": "organizations/example-corp"
    }))
    .expect("a bare string must deserialize");
    assert_eq!(one.targets, ["organizations/example-corp"]);

    let many: AttributeMapping = serde_json::from_value(serde_json::json!({
        "from": "employer", "to": "schema:worksFor", "targets": ["organizations/example-corp"]
    }))
    .unwrap();
    assert_eq!(many.targets, ["organizations/example-corp"]);

    let none: AttributeMapping = serde_json::from_value(serde_json::json!({
        "from": "port", "to": "frona:port"
    }))
    .unwrap();
    assert!(none.targets.is_empty(), "absent means a literal");
}

/// Only strings name entities. Searching for `5432` or `true` returns noise, and a port is
/// not an entity.
#[test]
fn only_string_values_yield_search_terms() {
    use serde_json::json;
    assert_eq!(search_terms(&json!("Example Corp")), ["Example Corp"]);
    assert_eq!(search_terms(&json!(["Python", "Rust"])), ["Python", "Rust"]);
    assert!(search_terms(&json!(5432)).is_empty());
    assert!(search_terms(&json!(true)).is_empty());
    assert!(search_terms(&json!("   ")).is_empty());
    assert_eq!(search_terms(&json!(["Rust", 7, null])), ["Rust"]);
    assert_eq!(render_value(&json!(["Rust"])), "[\"Rust\"]");
    assert_eq!(render_value(&json!("Example Corp")), "\"Example Corp\"");
}

/// A value that names another entity makes the property an object property, and the
/// attribute becomes an edge rather than staying a literal.
#[test]
fn attribute_whose_value_names_an_entity_is_promoted_to_a_relation() {
    let (edits, rekeys, promoted) = decide(
        &[attr(
            "employer",
            "frona:worksFor",
            Some("organizations/example-corp"),
        )],
        &["employer"],
    );
    assert_eq!(
        promoted,
        [(
            "employer".into(),
            "frona:worksFor".into(),
            "organizations/example-corp".into()
        )]
    );
    assert!(
        rekeys.is_empty(),
        "a promoted key is not also re-keyed: {rekeys:?}"
    );
    assert_eq!(
        edits,
        [SchemaEdit::DeclareObjectProperty {
            property: "frona:worksFor".into()
        }],
        "declared an OBJECT property, because that is what the decision means"
    );
}

/// No target means the value is a literal: the key stays an attribute, re-keyed to
/// its CURIE, and the term is declared a data property.
#[test]
fn attribute_with_no_target_stays_a_data_property() {
    let (edits, rekeys, promoted) = decide(&[attr("port", "frona:port", None)], &["port"]);
    assert_eq!(rekeys, [("port".to_string(), "frona:port".to_string())]);
    assert!(promoted.is_empty());
    assert_eq!(
        edits,
        [SchemaEdit::DeclareDataProperty {
            property: "frona:port".into()
        }]
    );
}

/// A standard term is already declared by the bundled ontologies - minting it again
/// would put a duplicate axiom in the user's delta.
#[test]
fn standard_term_is_used_without_being_minted() {
    let (edits, rekeys, _) = decide(&[attr("role", "schema:jobTitle", None)], &["role"]);
    assert_eq!(
        rekeys,
        [("role".to_string(), "schema:jobTitle".to_string())]
    );
    assert!(edits.is_empty(), "nothing to declare: {edits:?}");
}

/// The model may return a key the entity does not have. Acting on it would write an
/// attribute nothing ever stated, or promote a fact out of thin air.
#[test]
fn key_the_entity_does_not_carry_is_ignored() {
    let (edits, rekeys, promoted) = decide(
        &[
            attr("salary", "frona:salary", None),
            attr("spouse", "frona:marriedTo", Some("people/sam")),
        ],
        &["port"],
    );
    assert!(edits.is_empty() && rekeys.is_empty() && promoted.is_empty());
}

/// An edge may only point at an entity that exists or one this pass just minted.
///
/// `knowledge_entity_link.to_entity_path` is a plain string and the commit writes it without a
/// lookup, so an invented path becomes an edge to nothing that reads exactly like a
/// real one. Falling back to a literal is the honest answer: the value named nothing
/// this knowledge base has.
#[test]
fn target_that_names_no_entity_is_not_an_edge() {
    let (edits, rekeys, promoted) = decide_known(
        &[attr(
            "employer",
            "frona:worksFor",
            Some("organizations/acme"),
        )],
        &["employer"],
        &[],
    );
    assert!(promoted.is_empty(), "nothing to point at: {promoted:?}");
    assert_eq!(
        rekeys,
        [("employer".to_string(), "frona:worksFor".to_string())]
    );
    assert_eq!(
        edits,
        [SchemaEdit::DeclareDataProperty {
            property: "frona:worksFor".into()
        }],
        "with no target it is a data property, same as any other literal"
    );
}

/// An entity minted this pass is a legitimate target the moment it is minted - that is
/// the whole point of minting it.
#[test]
fn freshly_minted_entity_is_a_valid_target() {
    let (edits, rekeys, promoted) = decide_known(
        &[attr(
            "employer",
            "frona:worksFor",
            Some("organizations/example-corp"),
        )],
        &["employer"],
        &["organizations/example-corp"],
    );
    assert_eq!(
        promoted,
        [(
            "employer".into(),
            "frona:worksFor".into(),
            "organizations/example-corp".into()
        )]
    );
    assert!(rekeys.is_empty());
    assert_eq!(
        edits,
        [SchemaEdit::DeclareObjectProperty {
            property: "frona:worksFor".into()
        }]
    );
}

/// Some targets real, some not: the real ones still become edges. Dropping the whole
/// attribute because one name was wrong would lose the facts the model got right.
#[test]
fn unknown_targets_are_dropped_without_taking_the_known_ones_with_them() {
    let (_, rekeys, promoted) = decide_known(
        &[attr_many(
            "programming languages",
            "schema:knowsLanguage",
            &["topics/python", "topics/cobol"],
        )],
        &["programming languages"],
        &["topics/python"],
    );
    assert_eq!(
        promoted.len(),
        1,
        "only the entity that exists: {promoted:?}"
    );
    assert_eq!(promoted[0].2, "topics/python");
    assert!(
        rekeys.is_empty(),
        "still an object property, so not also a literal"
    );
}

/// A merge mid-pass moves every promotion that named the losing entity.
///
/// Promotions are decided before Resolve and written after it. If Resolve merges the
/// target, the promotion must follow the surviving path rather than create a dangling edge.
#[test]
fn merge_moves_promotions_that_named_the_losing_entity() {
    let mut p = ProposalSet::default();
    p.record(
        "devices/device-x",
        EntityProposal {
            classes: vec!["schema:Product".into()],
            edits: Vec::new(),
            rekeys: Vec::new(),
            attr_rekeys: Vec::new(),
            promoted: vec![(
                "manufacturer".into(),
                "schema:manufacturer".into(),
                "organizations/example-tools".into(),
            )],
            promoted_sources: HashMap::new(),
            retracted: Vec::new(),
            has_keys: Vec::new(),
            inverse_functional_properties: Vec::new(),
        },
    );
    p.retarget(
        "organizations/example-tools",
        "organizations/example-electronics",
    );
    assert_eq!(
        p.by_path["devices/device-x"].promoted[0].2, "organizations/example-electronics",
        "the edge follows the survivor"
    );
}

/// A merge can make the subject and the target one entity. Nothing relates to itself,
/// so the promotion goes rather than becoming a self-link.
#[test]
fn merge_that_collapses_both_ends_drops_the_promotion() {
    let mut p = ProposalSet::default();
    p.record(
        "people/me",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            edits: Vec::new(),
            rekeys: Vec::new(),
            attr_rekeys: Vec::new(),
            promoted: vec![(
                "alias".into(),
                "frona:alias".into(),
                "people/casey-owner".into(),
            )],
            promoted_sources: HashMap::new(),
            retracted: Vec::new(),
            has_keys: Vec::new(),
            inverse_functional_properties: Vec::new(),
        },
    );
    p.retarget("people/casey-owner", "people/me");
    assert!(p.by_path["people/me"].promoted.is_empty(), "no self-link");
}

#[test]
fn merge_unions_provisional_identity_markers_on_the_winner() {
    let mut p = ProposalSet::default();
    p.record(
        "people/taylor",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            has_keys: vec![HasKeyMarker {
                class: "schema:Person".into(),
                properties: vec!["schema:givenName".into(), "schema:familyName".into()],
            }],
            ..EntityProposal::default()
        },
    );
    p.record(
        "contacts/taylor",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            inverse_functional_properties: vec!["frona:ownedByProfile".into()],
            ..EntityProposal::default()
        },
    );
    p.retarget("contacts/taylor", "people/taylor");
    let winner = &p.by_path["people/taylor"];
    assert_eq!(winner.has_keys.len(), 1);
    assert_eq!(
        winner.inverse_functional_properties,
        ["frona:ownedByProfile"]
    );
}

#[test]
fn merge_into_an_unbacked_shell_transfers_the_backing_and_classification() {
    let mut p = ProposalSet::default();
    let mut shell = entity("organizations/acme", serde_json::json!({}));
    shell.name = "Acme".into();
    shell.source_memory_ids.clear();
    let mut backed = entity(
        "services/acme",
        serde_json::json!({"schema:url": "https://acme.invalid"}),
    );
    backed.name = "Acme service".into();
    p.stage_input_entity(shell);
    p.stage_entity(backed);
    p.record("services/acme", proposal("schema:Organization", Vec::new()));
    p.entity_shapes.insert(
        "services/acme".into(),
        EntityShape {
            name: "Acme service".into(),
            description: "A service operated by Acme.".into(),
            aliases: Vec::new(),
        },
    );

    p.retarget("services/acme", "organizations/acme");
    p.forget("services/acme");

    let canonical = p.input_entity("organizations/acme").unwrap();
    assert_eq!(canonical.source_memory_ids, ["memory-1"]);
    assert_eq!(canonical.kinds, ["schema:Organization"]);
    assert_eq!(
        canonical.attributes,
        serde_json::json!({"schema:url": "https://acme.invalid"}),
    );
    assert!(
        p.staged_entities.contains_key("organizations/acme"),
        "a newly backed canonical shell must materialize at commit",
    );
    assert!(
        p.by_path.contains_key("organizations/acme"),
        "forgetting the loser must not discard its accepted classification",
    );
}

/// An entity cannot relate to itself, and a blank target is "no target" - the model may
/// send `""` for not-applicable rather than omitting the field.
#[test]
fn self_target_or_a_blank_one_is_not_a_promotion() {
    let (_, rekeys, promoted) = decide(
        &[
            attr("owner", "frona:owner", Some("people/me")),
            attr("port", "frona:port", Some("  ")),
        ],
        &["owner", "port"],
    );
    assert!(promoted.is_empty(), "neither is a real edge: {promoted:?}");
    assert_eq!(
        rekeys.len(),
        2,
        "both fall back to being literals: {rekeys:?}"
    );
}

fn mint(path: &str, name: &str, class: &str) -> NewEntity {
    NewEntity {
        path: path.into(),
        name: name.into(),
        description: String::new(),
        class: class.into(),
        new_class_parent: None,
        from_facts: Vec::new(),
    }
}

fn accept(mints: &[NewEntity]) -> Vec<AcceptedMint> {
    accept_mints(mints, "people/me", &PrefixMap::standard())
}

/// The case the whole change exists for. "Example Corp" is an organization whether or not this
/// vault happens to hold an entity for it - the missing entity is a hole in the data, not
/// evidence that `worksFor` relates a person to a string.
#[test]
fn value_naming_an_unmaterialized_entity_mints_one() {
    let accepted = accept(&[mint(
        "organizations/example-corp",
        "Example Corp",
        "schema:Organization",
    )]);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].path, "organizations/example-corp");
    assert_eq!(accepted[0].classes, ["schema:Organization"]);
    assert!(
        accepted[0].edits.is_empty(),
        "a standard class needs no axiom"
    );
}

/// A `frona:` class carries its parent axiom, exactly as one minted for the entity being
/// classified does - otherwise the term is used and never declared.
#[test]
fn minted_frona_class_proposes_its_parent_axiom() {
    let mut m = mint("tools/device-x", "Device X", "frona:SolderingIron");
    m.new_class_parent = Some("schema:Product".into());
    assert_eq!(
        accept(&[m])[0].edits,
        [subclass("frona:SolderingIron", "schema:Product")]
    );
}

/// Paths are slugged like every other entity path: the model writes a display-shaped
/// path often enough that rejecting it would cost an entity for a formatting choice.
#[test]
fn minted_path_is_slugged() {
    assert_eq!(
        accept(&[mint(
            "/Organizations/Example Corporation.md",
            "Example Corp",
            "schema:Organization"
        )])[0]
            .path,
        "organizations/example-corporation"
    );
}

/// Nothing usable as an entity: no path, no name, no class. Each is the model answering
/// the shape of the question rather than the question.
#[test]
fn mint_missing_what_an_entity_needs_is_refused() {
    assert!(
        accept(&[mint("  ", "Example Corp", "schema:Organization")]).is_empty(),
        "no path"
    );
    assert!(
        accept(&[mint(
            "organizations/example-corp",
            " ",
            "schema:Organization"
        )])
        .is_empty(),
        "no name"
    );
    assert!(
        accept(&[mint("organizations/example-corp", "Example Corp", "")]).is_empty(),
        "no class"
    );
}

#[test]
fn mint_of_the_entity_being_classified_is_refused() {
    assert!(accept(&[mint("people/me", "Casey Owner", "schema:Person")]).is_empty());
}

/// Two mints of one path is the model equivocating. The first answer stands - merging
/// two descriptions would invent a third nothing proposed.
#[test]
fn repeated_minted_path_is_taken_once() {
    let accepted = accept(&[
        mint(
            "organizations/example-corp",
            "Example Corp",
            "schema:Organization",
        ),
        mint(
            "organizations/example-corp",
            "Example Corporation",
            "frona:Employer",
        ),
    ]);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].name, "Example Corp");
}

/// The mint's class terms reach `terms()`, which is what `bad_term_feedback` validates.
/// An unchecked CURIE is not a rejected classification - it is a delta that stops
/// parsing, after which every schema call for this user fails.
#[test]
fn minted_entity_class_is_validated_with_the_rest() {
    let mut m = mint(
        "organizations/example-corp",
        "Example Corp",
        "frona:Employer",
    );
    m.new_class_parent = Some("schema:Organization".into());
    let c = Classification {
        entity: EntityShape::default(),
        classes: vec![ClassChoice {
            class: "schema:Person".into(),
            new_class_parent: None,
        }],
        relations: Vec::new(),
        attributes: Vec::new(),
        new_entities: vec![m],
        declarations: Vec::new(),
        has_keys: Vec::new(),
        inverse_functional_properties: Vec::new(),
    };
    let terms = c.terms();
    assert!(
        terms.contains(&"frona:Employer"),
        "the minted class: {terms:?}"
    );
    assert!(
        terms.contains(&"schema:Organization"),
        "and its parent: {terms:?}"
    );
}

/// Cited fact IDs survive the accept step; whether the model was allowed to cite them
/// is checked against what it was shown, which needs an entity and so lives in the stage.
#[test]
fn cited_facts_are_carried_through_trimmed() {
    let mut m = mint(
        "organizations/example-corp",
        "Example Corp",
        "schema:Organization",
    );
    m.from_facts = vec!["  mem_a  ".into(), "  ".into(), "mem_b".into()];
    assert_eq!(accept(&[m])[0].from_facts, ["mem_a", "mem_b"]);
}

/// A model that has settled on one fact has the same reason to write it bare as it
/// does for a single target.
#[test]
fn single_cited_fact_may_arrive_unwrapped() {
    let e: NewEntity = serde_json::from_value(serde_json::json!({
        "path": "organizations/example-corp", "name": "Example Corp",
        "description": "An organization",
        "class": "schema:Organization", "from_facts": "mem_a"
    }))
    .expect("a bare string must deserialize");
    assert_eq!(e.from_facts, ["mem_a"]);
}

/// A key already CURIE-shaped from an earlier pass needs no re-key - re-emitting it
/// as a rename would be a no-op the commit has to filter anyway.
#[test]
fn key_that_is_already_its_curie_is_not_re_keyed() {
    let (_, rekeys, _) = decide(&[attr("frona:port", "frona:port", None)], &["frona:port"]);
    assert!(rekeys.is_empty(), "{rekeys:?}");
}

/// A model that names the same class twice with something else in between is not
/// proposing it twice. Order is first-seen, not sorted: `kinds` is chronological.
#[test]
fn class_curies_deduplicate_non_adjacent_repeats_in_first_seen_order() {
    let choice = |c: &str| ClassChoice {
        class: c.into(),
        new_class_parent: None,
    };
    let c = Classification {
        entity: EntityShape::default(),
        classes: vec![
            choice("schema:Person"),
            choice(" schema:Employee "),
            choice("schema:Person"),
            choice("  "),
        ],
        relations: Vec::new(),
        attributes: Vec::new(),
        new_entities: Vec::new(),
        declarations: Vec::new(),
        has_keys: Vec::new(),
        inverse_functional_properties: Vec::new(),
    };
    assert_eq!(c.class_curies(), ["schema:Person", "schema:Employee"]);
}

#[test]
fn every_new_term_requires_one_central_declaration_of_the_used_kind() {
    let c = Classification {
        entity: EntityShape::default(),
        classes: vec![ClassChoice {
            class: "frona:Robotaxi".into(),
            new_class_parent: None,
        }],
        relations: Vec::new(),
        attributes: vec![AttributeMapping {
            from: "operator".into(),
            to: "frona:operator".into(),
            targets: vec!["organizations/waymo".into()],
        }],
        new_entities: Vec::new(),
        declarations: vec![OntologyDeclaration::Class {
            term: "frona:Robotaxi".into(),
            description: "An autonomous taxi service.".into(),
            parents: vec!["schema:Service".into()],
            equivalent_to: Vec::new(),
            disjoint_with: Vec::new(),
        }],
        has_keys: Vec::new(),
        inverse_functional_properties: Vec::new(),
    };
    let feedback = c
        .declaration_feedback(&HashSet::new(), &PrefixMap::standard())
        .unwrap();
    assert!(feedback.contains("frona:operator"), "{feedback}");
    assert!(feedback.contains("object_property"), "{feedback}");
}

#[test]
fn model_authored_declarations_lower_to_rich_schema_edits() {
    let c = Classification {
        entity: EntityShape::default(),
        classes: vec![ClassChoice {
            class: "frona:Robotaxi".into(),
            new_class_parent: None,
        }],
        relations: Vec::new(),
        attributes: Vec::new(),
        new_entities: Vec::new(),
        declarations: vec![OntologyDeclaration::Class {
            term: "frona:Robotaxi".into(),
            description: "An autonomous taxi service.".into(),
            parents: vec!["schema:Service".into()],
            equivalent_to: Vec::new(),
            disjoint_with: vec!["schema:Person".into()],
        }],
        has_keys: Vec::new(),
        inverse_functional_properties: Vec::new(),
    };
    let edits = classification_edits(&c);
    assert!(edits.contains(&SchemaEdit::SubClassOf {
        sub: "frona:Robotaxi".into(),
        sup: "schema:Service".into(),
    }));
    assert!(edits.contains(&SchemaEdit::DisjointClasses {
        a: "frona:Robotaxi".into(),
        b: "schema:Person".into(),
    }));
}

/// The proposed layer is the union of every proposal's edits - two entities minting the
/// same term contribute one axiom, not two.
#[test]
fn proposed_layer_is_the_deduplicated_union_of_edits() {
    let mut p = ProposalSet::default();
    p.record(
        "a",
        proposal(
            "frona:Service",
            vec![subclass("frona:Service", "schema:Thing")],
        ),
    );
    p.record(
        "b",
        proposal(
            "frona:Service",
            vec![subclass("frona:Service", "schema:Thing")],
        ),
    );
    p.record(
        "c",
        proposal("frona:Host", vec![subclass("frona:Host", "schema:Thing")]),
    );
    assert_eq!(
        p.proposed_edits.len(),
        2,
        "duplicate axiom folded: {:?}",
        p.proposed_edits
    );
    assert_eq!(
        p.by_path.len(),
        3,
        "but every entity keeps its own proposal"
    );
}

#[test]
fn reconcile_promotion_retires_the_earlier_data_property_decision() {
    let mut p = ProposalSet::default();
    p.record(
        "people/me",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            edits: vec![SchemaEdit::DeclareDataProperty {
                property: "frona:employer".into(),
            }],
            rekeys: Vec::new(),
            attr_rekeys: vec![("employer".into(), "frona:employer".into())],
            promoted: Vec::new(),
            promoted_sources: HashMap::new(),
            retracted: Vec::new(),
            has_keys: Vec::new(),
            inverse_functional_properties: Vec::new(),
        },
    );
    p.add_reconcile_promotions(
        "people/me",
        &[crate::reconcile::ReconcilePromotion {
            key: "employer".into(),
            property: "schema:worksFor".into(),
            target: "organizations/example-corp".into(),
            source_memory_ids: vec!["memory-employer".into()],
            declaration: None,
        }],
        &[],
        &crate::ontology::PrefixMap::standard(),
    );

    let proposal = &p.by_path["people/me"];
    assert!(proposal.attr_rekeys.is_empty());
    assert_eq!(
        proposal.promoted,
        [(
            "employer".into(),
            "schema:worksFor".into(),
            "organizations/example-corp".into()
        )]
    );
    assert_eq!(
        proposal.promoted_sources.get(&(
            "schema:worksFor".to_string(),
            "organizations/example-corp".to_string(),
        )),
        Some(&vec!["memory-employer".to_string()]),
    );
    assert!(
        !p.proposed_edits.iter().any(|edit| matches!(
            edit,
            SchemaEdit::DeclareDataProperty { property } if property == "frona:employer"
        )),
        "the single final commit must not retain classify's superseded literal kind"
    );
    assert!(
        !p.proposed_edits.iter().any(|edit| matches!(
            edit,
            SchemaEdit::DeclareObjectProperty { property } if property == "schema:worksFor"
        )),
        "a standard base property is used, not redeclared in the user delta"
    );
}

/// Resolve asks for "the classes in force" - the entity as it will exist once this
/// pass stamps. Stamping *adds*, so that is the stored set plus what was proposed,
/// not the proposal in place of it: replacing would judge the entity against a
/// version of itself that never exists.
#[test]
fn kinds_in_force_are_the_stored_ones_plus_the_proposal() {
    let mut p = ProposalSet::default();
    p.record("people/sarah", proposal("schema:Person", vec![]));
    let stored = |s: &str| vec![s.to_string()];

    assert_eq!(
        p.kinds_for("people/sarah", &[]),
        ["schema:Person"],
        "unstamped entity is typed"
    );
    assert_eq!(
        p.kinds_for("people/sarah", &stored("schema:Employee")),
        ["schema:Employee", "schema:Person"],
        "the entity keeps what it had and gains what was proposed"
    );
    assert_eq!(
        p.kinds_for("people/sarah", &stored("schema:Person")),
        ["schema:Person"],
        "re-proposing a class it already has is not a duplicate"
    );
    assert_eq!(
        p.kinds_for("orgs/acme", &stored("schema:Organization")),
        ["schema:Organization"],
        "no proposal → whatever is stored"
    );
    assert!(
        p.kinds_for("orgs/none", &[]).is_empty(),
        "neither → untyped"
    );
}

#[test]
fn dropping_a_merged_entity_removes_it_from_the_stamp_set_but_not_the_proposed_layer() {
    let mut p = ProposalSet::default();
    p.record(
        "dup",
        proposal(
            "frona:Service",
            vec![subclass("frona:Service", "schema:Thing")],
        ),
    );
    p.record("keep", proposal("schema:Person", vec![]));
    p.forget("dup");
    assert!(p.kinds_for("dup", &[]).is_empty());
    assert_eq!(p.kinds_for("keep", &[]), ["schema:Person"]);
    assert_eq!(
        p.proposed_edits.len(),
        1,
        "the merged entity's axiom stays proposed — the surviving entity may still need it"
    );
}

mod execution;

#[test]
fn shared_classification_prompt_contract_rejects_missing_extra_and_duplicate_arguments() {
    let prompts = super::ClassifyPrompts::default();
    let vars = [
        "name",
        "description",
        "contributions",
        "identity_evidence",
        "facts",
        "relations",
        "attributes",
        "minted",
        "evidence",
    ]
    .map(|name| (name, "sample"));
    assert!(prompts.render(&vars).is_ok());
    assert!(prompts.render(&vars[..vars.len() - 1]).is_err());
    let mut extra = vars.to_vec();
    extra.push(("extra", "unexpected"));
    assert!(prompts.render(&extra).is_err());
    let mut duplicate = vars.to_vec();
    duplicate.push(("NAME", "ambiguous"));
    assert!(prompts.render(&duplicate).is_err());
    assert!(
        prompts
            .reject(&[("violations", "failure"), ("class", "schema:Person")])
            .is_ok()
    );
    assert!(prompts.reject(&[("violations", "failure")]).is_err());
}
