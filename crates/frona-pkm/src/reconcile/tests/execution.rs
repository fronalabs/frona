use crate::test_support::{Requests, harness, tool_output};
use crate::{
    Error, GraphSnapshot, Phase,
    classify::ProposalSet,
    model::{
        EntityCategory, EntityOrigin, KnowledgeConsolidationEntity, KnowledgeEntity,
        KnowledgeOntology,
    },
    ontology::{Ontology, OntologyCatalogue, catalogue::roots::Root},
    reconcile::{Reconcile, ReconcileInput, ReconcileOutput, ReconcilePrompts},
};
use chrono::Utc;
use frona_harness::{BudgetLimits, DefaultBudget};

use serde_json::{Value, json};

fn phase(values: Vec<Value>) -> (Reconcile<'static>, Requests) {
    let outputs = values.into_iter().map(|value| tool_output("submit", value));
    let (harness, requests) = harness(outputs.map(Ok));
    (Reconcile::new(harness), requests)
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
fn input() -> ReconcileInput {
    let graph = GraphSnapshot {
        scope_id: "u".into(),
        entities: vec![entity("people/test", "Casey", &["old", "new"])],
        links: Vec::new(),
        memory_entities: [
            ("old".into(), vec!["people/test".into()]),
            ("new".into(), vec!["people/test".into()]),
        ]
        .into(),
        memories: vec![
            super::mem("old", "Casey uses port 123"),
            super::mem("new", "Casey uses port 456"),
        ],
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
    ReconcileInput {
        path: "people/test".into(),
        graph,
        ontology,
        proposals: ProposalSet::default(),
        initial_rejection: None,
    }
}
async fn invoke<P: Phase<Input = ReconcileInput, Output = ReconcileOutput>>(
    phase: &P,
    input: ReconcileInput,
) -> Result<ReconcileOutput, Error> {
    phase.run(input).await
}

#[tokio::test]
async fn accepted_entity_update_is_uncommitted_and_updates_the_proposal_snapshot() {
    let (phase, calls) = phase(vec![json!({"description":"After"})]);
    let input = input();
    let original = input.graph.entities[0].clone();
    let output = invoke(&phase, input.clone()).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(output.entity_update.unwrap().description, "After");
    assert_eq!(
        output
            .proposals
            .input_entity("people/test")
            .unwrap()
            .description,
        "After"
    );
    assert_eq!(input.graph.entities[0].description, original.description);
}

#[tokio::test]
async fn retirement_returns_durable_ids_and_requeues_without_stale_summary() {
    let (phase, _) = phase(vec![
        json!({"description":"Must be deferred","outdated":[{"memory":"m1","note":"No longer current"}]}),
    ]);
    let output = phase.run(input()).await.unwrap();
    assert_eq!(output.changes.outdated_memories[0].memory_id, "old");
    assert!(output.entity_update.is_none());
    assert!(output.reconcile_dirty.contains("people/test"));
}

#[tokio::test]
async fn unsupported_replace_gets_feedback_and_a_second_submission() {
    let invalid = json!({"relations":[{"memory":"m1","links":[{"relation":"replace","to":"m2","was":"invented","now":"456"}]}]});
    let (phase, calls) = phase(vec![invalid, json!({"description":"After"})]);
    let output = phase.run(input()).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(output.changes.memory_relations.is_empty());
}

#[tokio::test]
async fn incomplete_reconciliation_never_returns_a_retained_verdict() {
    let invalid = json!({"relations":[{"memory":"m1","links":[{"relation":"replace","to":"m2","was":"invented","now":"456"}]}]});
    let (phase, _) = phase(vec![invalid]);
    assert!(phase.submission_attempts(1).run(input()).await.is_err());
}

#[tokio::test]
async fn missing_memory_scope_fails_before_inference() {
    let (phase, calls) = phase(vec![]);
    let mut input = input();
    input.graph.memory_entities.remove("old");
    assert!(
        phase
            .run(input)
            .await
            .unwrap_err()
            .to_string()
            .contains("scope")
    );
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn cancellation_and_shared_budget_are_respected() {
    let (phase, calls) = phase(vec![]);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    assert!(matches!(
        phase.run_with(input(), |run| run.cancellation(token)).await,
        Err(Error::Cancelled(_))
    ));
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
async fn empty_prompt_override_fails_before_inference() {
    let (phase, calls) = phase(vec![]);
    let phase = phase.prompts(ReconcilePrompts {
        system: String::new(),
        ..Default::default()
    });
    assert!(phase.run(input()).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn exact_target_advisory_is_sent_once_without_forcing_a_promotion() {
    let verdict = json!({"attributes":{"schema:description":"Acme"},"attribute_sources":[{
        "property":"schema:description","value":"Acme","source_memory_ids":["m1"]
    }]});
    let (phase, calls) = phase(vec![verdict.clone(), verdict]);
    let mut input = input();
    input.graph.memories[0] = super::mem("old", "Casey works at Acme");
    input
        .graph
        .entities
        .push(entity("organizations/acme", "Acme", &[]));
    let output = phase.run(input).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(output.promotions.is_empty());
    assert_eq!(
        output.entity_update.unwrap().attributes["schema:description"],
        "Acme"
    );
}

#[tokio::test]
async fn rename_is_a_plan_and_checks_snapshot_destination_availability() {
    let verdict = json!({"moves":[{"from":"people/test","to":"people/new"}]});
    let (phase, _) = phase(vec![verdict.clone(), verdict]);
    let output = phase.run(input()).await.unwrap();
    assert_eq!(output.move_entity.unwrap().to, "people/new");
    let mut occupied = input();
    occupied
        .graph
        .entities
        .push(entity("people/new", "Someone else", &[]));
    assert!(phase.run(occupied).await.unwrap().move_entity.is_none());
}

#[tokio::test]
async fn accepted_pending_entities_are_validated_and_activated_in_the_output() {
    let (phase, _) = phase(vec![json!({"description":"After"})]);
    let mut input = input();
    input.graph.entities[0].lifecycle = crate::model::ConsolidationEntityLifecycle::Pending;
    input.graph.entities[0].rederive_search();
    let output = phase.run(input).await.unwrap();
    let entity = output.proposals.input_entity("people/test").unwrap();
    assert_eq!(
        entity.lifecycle,
        crate::model::ConsolidationEntityLifecycle::Active
    );
    entity.validate().unwrap();
}
