use std::collections::HashSet;

use crate::model::{EntityCategory, EntityHit, EntityOrigin};

use super::{CandidateEvidence, RankedCandidate, Subject, rank_candidates};

fn hit(path: &str, name: &str, kinds: &[&str], assertions: &[&str]) -> RankedCandidate {
    RankedCandidate {
        entity: EntityHit {
            path: path.into(),
            origin: EntityOrigin::Internal,
            category: EntityCategory::Concept,
            kinds: kinds.iter().map(|kind| (*kind).into()).collect(),
            name: name.into(),
            description: String::new(),
            aliases: HashSet::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            body: String::new(),
        },
        assertions: assertions.iter().map(|value| (*value).into()).collect(),
        evidence: CandidateEvidence::default(),
    }
}

#[test]
fn full_name_with_the_same_arbitrary_kind_outranks_named_artifacts() {
    let subject = Subject::new(
        "people/child",
        "Taylor",
        &["urn:example:ArbitraryPerson"],
        &[],
    );
    let ranked = rank_candidates(
        &subject,
        vec![
            hit(
                "projects/reel",
                "Taylor's Reel",
                &["urn:example:ArbitraryVideo"],
                &[],
            ),
            hit(
                "people/full",
                "Taylor Example",
                &["urn:example:ArbitraryPerson"],
                &[],
            ),
            hit(
                "events/party",
                "Taylor's Birthday Party",
                &["urn:example:ArbitraryEvent"],
                &[],
            ),
        ],
        8,
    );

    assert_eq!(ranked[0].entity.path, "people/full");
    assert!(ranked[0].evidence.token_containment);
    assert_eq!(
        ranked[0].evidence.shared_kinds,
        ["urn:example:ArbitraryPerson"]
    );
}

#[test]
fn multiple_generic_assertions_outrank_one_without_property_specific_rules() {
    let subject = Subject::new(
        "accounts/rewards",
        "Rewards account",
        &["urn:example:Account"],
        &[
            "attribute|urn:example:lastDigits|string|0000",
            "relation|urn:example:issuedBy|orgs/bank",
        ],
    );
    let ranked = rank_candidates(
        &subject,
        vec![
            hit(
                "accounts/other",
                "Bank account",
                &["urn:example:Account"],
                &["relation|urn:example:issuedBy|orgs/bank"],
            ),
            hit(
                "payments/card",
                "Example Card ending 0000",
                &["urn:example:PaymentCard"],
                &[
                    "attribute|urn:example:lastDigits|string|0000",
                    "relation|urn:example:issuedBy|orgs/bank",
                ],
            ),
        ],
        8,
    );

    assert_eq!(ranked[0].entity.path, "payments/card");
    assert_eq!(ranked[0].evidence.shared_assertions.len(), 2);
}

#[test]
fn forced_candidates_survive_the_prompt_limit() {
    let subject = Subject::new("things/current", "Current", &[], &[]);
    let mut candidates = (0..10)
        .map(|index| {
            hit(
                &format!("things/{index}"),
                &format!("Current {index}"),
                &[],
                &[],
            )
        })
        .collect::<Vec<_>>();
    candidates[9].evidence.forced = true;

    let ranked = rank_candidates(&subject, candidates, 3);

    assert!(
        ranked
            .iter()
            .any(|candidate| candidate.entity.path == "things/9")
    );
    assert_eq!(ranked.len(), 3);
}

#[test]
fn playbooks_use_the_same_name_ranking_contract() {
    let mut subject = Subject::new(
        "procedures/device-update",
        "Update device firmware",
        &["https://schema.org/HowTo"],
        &[],
    );
    subject.category = EntityCategory::Playbook;
    let mut candidates = vec![
        hit(
            "procedures/device-reset",
            "Reset device",
            &["https://schema.org/HowTo"],
            &[],
        ),
        hit(
            "procedures/update-device-firmware",
            "Update the device firmware",
            &["https://schema.org/HowTo"],
            &[],
        ),
    ];
    for candidate in &mut candidates {
        candidate.entity.category = EntityCategory::Playbook;
    }

    let ranked = rank_candidates(&subject, candidates, super::RESOLUTION_PROMPT_LIMIT);

    assert_eq!(ranked[0].entity.path, "procedures/update-device-firmware");
    assert!(ranked[0].evidence.token_containment);
    assert_eq!(ranked[0].evidence.type_affinity, 3);
}

#[tokio::test]
async fn snapshot_recall_excludes_unrelated_rows_but_keeps_shared_assertions_and_forced_paths() {
    use crate::{GraphSnapshot, model::KnowledgeConsolidationEntity};
    let row = |path: &str, name: &str| {
        let mut entity = KnowledgeConsolidationEntity::pending(
            "run",
            "owner",
            path,
            EntityCategory::Concept,
            vec![],
            Default::default(),
        );
        entity.name = name.into();
        entity.rederive_search();
        entity
    };
    let mut assertion_match = row("accounts/card", "Different label");
    assertion_match.search_assertions = vec!["attribute|urn:lastDigits|string|1234".into()];
    let search = super::Search::new(GraphSnapshot {
        scope_id: "owner".into(),
        entities: vec![
            row("people/taylor", "Taylor Example"),
            row("things/unrelated", "Unrelated"),
            assertion_match,
            row("things/forced", "Forced"),
        ],
        ..Default::default()
    });
    let hits = search
        .find_candidates(
            super::Request {
                subject: Subject::new(
                    "people/subject",
                    "Taylor",
                    &[],
                    &["attribute|urn:lastDigits|string|1234"],
                ),
                eligible_paths: None,
                additional_candidates: vec![],
                forced_paths: vec!["things/forced".into()],
                limit: 8,
            },
            |_| {},
            |_, _| Some(0),
        )
        .await
        .unwrap();
    let paths: HashSet<_> = hits.iter().map(|h| h.entity.path.as_str()).collect();
    assert_eq!(
        paths,
        HashSet::from(["people/taylor", "accounts/card", "things/forced"])
    );
}
