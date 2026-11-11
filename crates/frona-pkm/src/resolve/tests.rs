use crate::test_support::{Requests, harness, tool_output};
use crate::{
    GraphSnapshot, Phase,
    classify::ProposalSet,
    model::{
        EntityCategory, EntityOrigin, KnowledgeConsolidationEntity, KnowledgeEntity,
        KnowledgeOntology,
    },
    ontology::{Ontology, OntologyCatalogue, catalogue::roots::Root},
    resolve::{IdentityResolution, Resolve, ResolveInput},
};
use chrono::Utc;
use frona_harness::{BudgetLimits, DefaultBudget};

use serde_json::{Value, json};

fn phase(values: Vec<Value>) -> (Resolve<'static>, Requests) {
    let outputs = values.into_iter().map(|value| tool_output("submit", value));
    let (harness, requests) = harness(outputs.map(Ok));
    (Resolve::new(harness), requests)
}

fn entity(path: &str, name: &str, memories: &[&str]) -> KnowledgeConsolidationEntity {
    KnowledgeConsolidationEntity::from_committed(
        "run",
        KnowledgeEntity {
            id: path.into(),
            user_id: "u".into(),
            path: path.into(),
            origin: EntityOrigin::Internal,
            category: EntityCategory::Concept,
            kinds: vec!["https://schema.org/Person".into()],
            name: name.into(),
            description: "Before".into(),
            identity_evidence: Vec::new(),
            attribute_sources: Vec::new(),
            source_memory_ids: memories.iter().map(|id| (*id).into()).collect(),
            body: String::new(),
            sync_content: None,
            mirrored_rev: None,
            extracted_rev: None,
            related_playbooks: Vec::new(),
            search_text: String::new(),
            search_names: Vec::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            attributes: json!({}),
            use_count: 0,
            aliases: Default::default(),
            rev: None,
            updated_at: Utc::now(),
            rendered_at: chrono::DateTime::<Utc>::MIN_UTC,
        },
    )
}
fn input() -> ResolveInput {
    let graph = GraphSnapshot {
        scope_id: "u".into(),
        entities: vec![
            entity("people/test", "Casey", &["m1"]),
            entity("people/candidate", "Casey", &["m2"]),
        ],
        ..Default::default()
    };
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("schema.ttl"),
        r#"
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix schema: <https://schema.org/> .
        schema: a owl:Ontology . schema:Person a owl:Class .
        schema:description a owl:DatatypeProperty .
    "#,
    )
    .unwrap();
    let catalogue = OntologyCatalogue::load(&[(Root::User, temp.path())]).unwrap();
    let state = KnowledgeOntology {
        id: "ontology".into(),
        user_id: "u".into(),
        owl: "Ontology()".into(),
        format: "ofn".into(),
        version: 1,
        effective_ontology: String::new(),
        seeds: Vec::new(),
        sources: Vec::new(),
        catalog_fingerprint: String::new(),
        updated_at: Utc::now(),
    };
    let ontology = Ontology::new(catalogue, state, graph.clone());
    ResolveInput {
        path: "people/test".into(),
        graph,
        ontology,
        proposals: ProposalSet::default(),
        eligible_targets: ["people/candidate".into()].into(),
        reconsider: false,
        prior_identity_fingerprint: None,
        prior_pair_fingerprints: Default::default(),
    }
}

fn distinct() -> Value {
    json!({"canonical":"","same_as":[],"merge_because":[],"distinct_because":[{"candidate":"people/candidate","reason":"different_entity_role","evidence":[{"side":"subject","field":"description","quote":"Before"},{"side":"candidate","field":"description","quote":"Before"}]}]})
}
#[tokio::test]
async fn distinct_identity_returns_fingerprints_without_persisting() {
    let (phase, calls) = phase(vec![distinct()]);
    let out = Phase::run(&phase, input()).await.unwrap();
    assert!(matches!(
        out.decision,
        Some(IdentityResolution::Distinct { .. })
    ));
    assert!(
        out.prepared_proposals
            .input_entity("people/candidate")
            .is_some()
    );
    assert!(out.prepared_proposals.input_entity("people/test").is_some());
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(out.identity_fingerprint.is_some());
    assert_eq!(out.pair_fingerprints.len(), 1);
}
#[tokio::test]
async fn invalid_candidate_is_revised_and_exhaustion_is_unresolved() {
    let invalid = json!({"canonical":"people/hallucinated","same_as":[],"merge_because":[],"distinct_because":[]});
    let (phase, calls) = phase(vec![invalid]);
    let out = phase.submission_attempts(1).run(input()).await.unwrap();
    assert!(matches!(
        out.decision,
        Some(IdentityResolution::Unresolved { pair_count: 1, .. })
    ));
    assert_eq!(out.corrections, 1);
    assert_eq!(calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn invalid_candidate_can_be_corrected_to_distinct() {
    let (phase, calls) = phase(vec![
        json!({"canonical":"people/hallucinated","same_as":[],"merge_because":[],"distinct_because":[]}),
        distinct(),
    ]);
    let out = phase.run(input()).await.unwrap();
    assert!(matches!(
        out.decision,
        Some(IdentityResolution::Distinct { .. })
    ));
    assert_eq!(out.corrections, 1);
    assert_eq!(calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn no_candidates_is_deterministic_and_fingerprint_skips_are_not_verdicts() {
    let (phase, calls) = phase(vec![]);
    let mut input = input();
    input.eligible_targets.clear();
    let out = phase.run(input.clone()).await.unwrap();
    assert!(matches!(
        out.decision,
        Some(IdentityResolution::Distinct { .. })
    ));
    input.reconsider = true;
    input.prior_identity_fingerprint = out.identity_fingerprint;
    let out = phase.run(input).await.unwrap();
    assert!(out.decision.is_none());
    assert_eq!(calls.lock().unwrap().len(), 0);
}
#[tokio::test]
async fn budget_refusal_is_not_a_distinct_decision() {
    let (phase, calls) = phase(vec![]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    assert!(
        phase
            .run_with(input(), |run| run.budget(&budget))
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn grounded_merge_returns_a_validated_uncommitted_projection() {
    let verdict = json!({"canonical":"people/candidate","same_as":[],"distinct_because":[],"merge_because":[{"candidate":"people/candidate","reason":"same_grounded_identity","evidence":[{"side":"subject","field":"name","quote":"Casey"},{"side":"candidate","field":"name","quote":"Casey"}]}]});
    let (phase, calls) = phase(vec![verdict]);
    let out = phase.run(input()).await.unwrap();
    assert!(matches!(
        out.decision,
        Some(IdentityResolution::Merge { .. })
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
    let unmerged = out
        .prepared_proposals
        .input_entity("people/candidate")
        .unwrap();
    assert!(!unmerged.source_memory_ids.contains(&"m1".into()));
    assert!(out.prepared_proposals.input_entity("people/test").is_some());
    let survivor = out.proposals.input_entity("people/candidate").unwrap();
    assert!(survivor.source_memory_ids.contains(&"m1".into()));
    assert!(survivor.source_memory_ids.contains(&"m2".into()));
}

#[tokio::test]
async fn cancellation_is_not_an_unresolved_or_distinct_verdict() {
    let (phase, calls) = phase(vec![]);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    assert!(matches!(
        phase.run_with(input(), |run| run.cancellation(token)).await,
        Err(crate::Error::Cancelled(_))
    ));
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn incremental_sweep_banks_new_weak_pairs_without_rejudging() {
    let (phase, calls) = phase(vec![]);
    let mut input = input();
    input.reconsider = true;
    let out = phase.run(input).await.unwrap();
    assert!(
        matches!(out.decision, Some(IdentityResolution::Distinct { ref evidence }) if evidence["reason"] == "no_changed_identity_candidates")
    );
    assert_eq!(out.pair_fingerprints.len(), 1);
    assert!(out.identity_fingerprint.is_some());
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[test]
fn shared_resolution_prompt_contract_validates_input_and_feedback() {
    let prompts = super::ResolvePrompts::default();
    let vars = [
        "path",
        "name",
        "aliases",
        "description",
        "kind",
        "identity_evidence",
        "assertions",
        "candidates",
    ]
    .map(|name| (name, "sample"));
    assert!(prompts.render(&vars).is_ok());
    assert!(prompts.render(&vars[..vars.len() - 1]).is_err());
    let mut duplicate = vars.to_vec();
    duplicate.push(("PATH", "ambiguous"));
    assert!(prompts.render(&duplicate).is_err());
    assert!(
        prompts
            .reject(&[("proposed", "merge"), ("subject", "A"), ("candidates", "B")])
            .is_ok()
    );
    assert!(prompts.reject(&[("proposed", "merge")]).is_err());
}
