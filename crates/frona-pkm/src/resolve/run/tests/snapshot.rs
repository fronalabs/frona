use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::candidates::{CandidateEvidence, RankedCandidate};
use crate::model::{EntityCategory, EntityHit, EntityOrigin, KnowledgeConsolidationEntity};

use super::super::{
    IdentityMatch, identity_matches, pair_change_requires_judgment,
    resolution_identity_fingerprint, resolution_pair_fingerprint,
};
use crate::classify::HasKeyMarker;
use crate::ontology::PrefixMap;

fn candidate(name: &str) -> RankedCandidate {
    RankedCandidate {
        entity: EntityHit {
            path: "people/jordan-example".into(),
            origin: EntityOrigin::Internal,
            category: EntityCategory::Concept,
            kinds: vec!["https://schema.org/Person".into()],
            name: name.into(),
            description: String::new(),
            aliases: HashSet::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            body: String::new(),
        },
        assertions: BTreeSet::new(),
        evidence: CandidateEvidence::default(),
    }
}

#[test]
fn pair_fingerprint_ignores_description_and_ordinary_assertion_churn() {
    let mut subject = KnowledgeConsolidationEntity::pending(
        "pass",
        "user",
        "people/parent",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    subject.name = "Jordan".into();
    subject.kinds = vec!["https://schema.org/Person".into()];
    subject.rederive_search();
    let before = candidate("Jordan Example");
    let mut after = before.clone();
    after.entity.description = "New prose that is irrelevant to identity.".into();
    after
        .entity
        .search_assertions
        .push(serde_json::json!(["attribute", "schema:jobTitle", "Engineer"]).to_string());

    assert_eq!(
        resolution_pair_fingerprint(&subject, &before.entity, &[]),
        resolution_pair_fingerprint(&subject, &after.entity, &[]),
    );
}

#[test]
fn pair_fingerprint_changes_for_name_class_or_identifying_match() {
    let mut subject = KnowledgeConsolidationEntity::pending(
        "pass",
        "user",
        "people/taylor",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    subject.name = "Taylor Smith".into();
    subject.kinds = vec!["https://schema.org/Person".into()];
    let before = candidate("Taylor Smith");
    let baseline = resolution_pair_fingerprint(&subject, &before.entity, &[]);

    let mut renamed = before.clone();
    renamed.entity.name = "Taylor Jones".into();
    assert_ne!(
        baseline,
        resolution_pair_fingerprint(&subject, &renamed.entity, &[])
    );

    let mut retyped = before.clone();
    retyped
        .entity
        .kinds
        .push("https://schema.org/Employee".into());
    assert_ne!(
        baseline,
        resolution_pair_fingerprint(&subject, &retyped.entity, &[])
    );

    let matches = vec![IdentityMatch::HasKey {
        class: "schema:Person".into(),
        properties: BTreeMap::from([
            ("schema:familyName".into(), vec!["smith".into()]),
            ("schema:givenName".into(), vec!["taylor".into()]),
        ]),
    }];
    assert_ne!(
        baseline,
        resolution_pair_fingerprint(&subject, &before.entity, &matches)
    );
}

#[test]
fn identity_matches_require_a_complete_key_or_shared_inverse_target() {
    let mut subject = KnowledgeConsolidationEntity::pending(
        "pass",
        "user",
        "people/taylor",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    subject.name = "Taylor Smith".into();
    subject.kinds = vec!["https://schema.org/Person".into()];
    subject.attributes = serde_json::json!({
        "schema:givenName": "Taylor",
        "schema:familyName": "Smith"
    });
    subject.rederive_search();
    subject
        .search_assertions
        .push(serde_json::json!(["relation", "frona:ownedByProfile", "profiles/123"]).to_string());
    let keys = vec![HasKeyMarker {
        class: "schema:Person".into(),
        properties: vec!["schema:givenName".into(), "schema:familyName".into()],
    }];
    let inverse = BTreeSet::from(["frona:ownedByProfile".to_string()]);
    let mut partial = candidate("Taylor");
    partial.entity.search_assertions =
        vec![serde_json::json!(["attribute", "schema:givenName", "Taylor"]).to_string()];
    assert!(
        identity_matches(
            &subject,
            &partial.entity,
            &keys,
            &inverse,
            &PrefixMap::standard(),
        )
        .is_empty()
    );

    partial.entity.search_assertions.extend([
        serde_json::json!(["attribute", "schema:familyName", "Smith"]).to_string(),
        serde_json::json!(["relation", "frona:ownedByProfile", "profiles/123"]).to_string(),
    ]);
    let matches = identity_matches(
        &subject,
        &partial.entity,
        &keys,
        &inverse,
        &PrefixMap::standard(),
    );
    assert!(
        matches
            .iter()
            .any(|matched| matches!(matched, IdentityMatch::HasKey { .. }))
    );
    assert!(
        matches
            .iter()
            .any(|matched| matches!(matched, IdentityMatch::InverseFunctional { .. }))
    );
}

#[test]
fn pair_change_only_rejudges_new_identity_evidence_or_name_and_class_changes() {
    let mut subject = KnowledgeConsolidationEntity::pending(
        "pass",
        "user",
        "people/taylor",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    subject.name = "Taylor Smith".into();
    subject.kinds = vec!["https://schema.org/Person".into()];
    let candidate = candidate("Taylor Smith");
    let key = IdentityMatch::HasKey {
        class: "schema:Person".into(),
        properties: BTreeMap::from([
            ("schema:familyName".into(), vec!["smith".into()]),
            ("schema:givenName".into(), vec!["taylor".into()]),
        ]),
    };
    let inverse = IdentityMatch::InverseFunctional {
        property: "frona:ownedByProfile".into(),
        targets: vec!["profiles 123".into()],
    };
    let old = resolution_pair_fingerprint(&subject, &candidate.entity, &[key.clone(), inverse]);
    let weaker =
        resolution_pair_fingerprint(&subject, &candidate.entity, std::slice::from_ref(&key));
    assert!(!pair_change_requires_judgment(Some(&old), &weaker));

    let stronger = resolution_pair_fingerprint(
        &subject,
        &candidate.entity,
        &[
            key,
            IdentityMatch::InverseFunctional {
                property: "frona:ownedByProfile".into(),
                targets: vec!["profiles 456".into()],
            },
        ],
    );
    assert!(pair_change_requires_judgment(Some(&weaker), &stronger));

    let mut renamed = candidate.clone();
    renamed.entity.name = "Taylor Jones".into();
    let renamed = resolution_pair_fingerprint(&subject, &renamed.entity, &[]);
    let no_matches = resolution_pair_fingerprint(&subject, &candidate.entity, &[]);
    assert!(pair_change_requires_judgment(Some(&no_matches), &renamed));
}

#[test]
fn identity_fingerprint_ignores_ordinary_assertions_but_tracks_marker_values() {
    let mut entity = KnowledgeConsolidationEntity::pending(
        "pass",
        "user",
        "people/taylor",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    entity.name = "Taylor Smith".into();
    entity.kinds = vec!["https://schema.org/Person".into()];
    entity.attributes = serde_json::json!({
        "schema:givenName": "Taylor",
        "schema:familyName": "Smith",
        "schema:jobTitle": "Engineer"
    });
    entity.rederive_search();
    let keys = vec![HasKeyMarker {
        class: "schema:Person".into(),
        properties: vec!["schema:givenName".into(), "schema:familyName".into()],
    }];
    let baseline =
        resolution_identity_fingerprint(&entity, &keys, &BTreeSet::new(), &PrefixMap::standard());
    entity.attributes["schema:jobTitle"] = serde_json::json!("Manager");
    entity.rederive_search();
    assert_eq!(
        baseline,
        resolution_identity_fingerprint(&entity, &keys, &BTreeSet::new(), &PrefixMap::standard(),)
    );
    entity.attributes["schema:familyName"] = serde_json::json!("Jones");
    entity.rederive_search();
    assert_ne!(
        baseline,
        resolution_identity_fingerprint(&entity, &keys, &BTreeSet::new(), &PrefixMap::standard(),)
    );
}
