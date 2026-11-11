use crate::test_support::{Requests, harness, tool_output};
use crate::{
    Error, GraphSnapshot, Phase,
    classify::ProposalSet,
    classify::{Classify, ClassifyInput, ClassifyOutput, ClassifyPrompts},
    model::{
        EntityCategory, EntityOrigin, KnowledgeConsolidationEntity, KnowledgeEntity,
        KnowledgeOntology,
    },
    ontology::{Ontology, OntologyCatalogue, catalogue::roots::Root},
};
use chrono::Utc;
use frona_harness::{BudgetLimits, DefaultBudget};

use serde_json::{Value, json};

fn phase(values: Vec<Value>) -> (Classify<'static>, Requests) {
    let outputs = values.into_iter().map(|value| {
        let name = value
            .get("__tool")
            .and_then(Value::as_str)
            .unwrap_or("submit")
            .to_string();
        tool_output(&name, value.get("arguments").cloned().unwrap_or(value))
    });
    let (harness, requests) = harness(outputs.map(Ok));
    (Classify::new(harness), requests)
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
fn input() -> ClassifyInput {
    let mut graph = GraphSnapshot {
        scope_id: "u".into(),
        entities: vec![entity("people/test", "Casey", &["old", "new"])],
        links: Vec::new(),
        memory_entities: [
            ("old".into(), vec!["people/test".into()]),
            ("new".into(), vec!["people/test".into()]),
        ]
        .into(),
        memories: vec![
            memory("old", "Casey works at Example Corp"),
            memory("new", "Casey founded Example Corp"),
        ],
    };
    graph.entities[0].kinds.clear();
    graph.entities[0].attributes = json!({});
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("schema.ttl"),
        r#"
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix schema: <https://schema.org/> .
        schema: a owl:Ontology . schema:Person a owl:Class .
        schema:description a owl:DatatypeProperty .
        schema:Organization a owl:Class . schema:worksFor a owl:ObjectProperty .
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
    ClassifyInput {
        path: "people/test".into(),
        graph,
        ontology,
        proposals: ProposalSet::default(),
        initial_rejection: None,
    }
}
async fn invoke<P: Phase<Input = ClassifyInput, Output = ClassifyOutput>>(
    phase: &P,
    input: ClassifyInput,
) -> Result<ClassifyOutput, Error> {
    phase.run(input).await
}

fn memory(id: &str, content: &str) -> crate::model::KnowledgeMemory {
    crate::model::KnowledgeMemory {
        id: id.into(),
        user_id: "u".into(),
        content: content.into(),
        evidence: Vec::new(),
        kind: crate::model::MemoryKind::Fact,
        episode: None,
        relations: Vec::new(),
        disposition: crate::model::Disposition::None,
        ended_at: None,
        comment: None,
        erroneous_at: None,
        created_at: Utc::now(),
    }
}
fn candidate() -> Value {
    json!({"entity":{"name":"Casey","description":"A person","aliases":[]},"classes":[{"class":"schema:Person"}],"relations":[],"attributes":[],"new_entities":[],"declarations":[]})
}
#[tokio::test]
async fn accepted_classification_is_an_uncommitted_plan() {
    let (phase, calls) = phase(vec![candidate()]);
    let input = input();
    let output = invoke(&phase, input.clone()).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(output.classification.is_some());
    assert_eq!(
        output.proposals.by_path["people/test"].classes,
        ["schema:Person"]
    );
    assert!(input.graph.entities[0].kinds.is_empty());
    assert_eq!(output.diagnostic["validation"]["attempts"], 1);
}
#[tokio::test]
async fn invalid_terms_receive_feedback_before_acceptance() {
    let mut invalid = candidate();
    invalid["classes"] = json!([{"class":"unknown:Person"}]);
    let (phase, calls) = phase(vec![invalid, candidate()]);
    let output = phase.run(input()).await.unwrap();
    assert!(output.classification.is_some());
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(output.diagnostic["validation"]["failures"], 1);
}
#[tokio::test]
async fn empty_classes_and_exhausted_semantic_submissions_are_rejections() {
    let mut empty = candidate();
    empty["classes"] = json!([]);
    let (phase, _) = phase(vec![empty]);
    let output = phase.run(input()).await.unwrap();
    assert!(output.classification.is_none());
    assert!(output.proposals.by_path.is_empty());
    let mut invalid = candidate();
    invalid["classes"] = json!([{"class":"unknown:Person"}]);
    let (phase, _) = self::phase(vec![invalid]);
    let output = phase.submission_attempts(1).run(input()).await.unwrap();
    assert!(output.classification.is_none());
    assert!(output.proposals.by_path.is_empty());
}
#[tokio::test]
async fn transport_failure_does_not_return_a_semantic_rejection() {
    let (harness, _) = harness([Err(
        frona_harness::inference::InferenceError::InferenceFailed("transport failed".into()),
    )]);
    let phase = Classify::new(harness);
    assert!(phase.run(input()).await.is_err());
}
#[tokio::test]
async fn accepted_mints_expand_fact_ids_and_promote_only_real_targets() {
    let mut value = candidate();
    value["new_entities"] = json!([{"path":"organizations/example-corp","name":"Example Corp","description":"Employer","class":"schema:Organization","from_facts":["f1"]}]);
    value["attributes"] = json!([{"from":"employer","to":"schema:worksFor","targets":["organizations/example-corp"]}]);
    let (phase, _) = phase(vec![value]);
    let mut source = input();
    source.graph.entities[0].attributes = json!({"employer":"Example Corp"});
    let output = phase.run(source).await.unwrap();
    assert_eq!(output.minted_paths, ["organizations/example-corp"]);
    assert_eq!(
        output.classification.unwrap().new_entities[0].from_facts,
        ["new"]
    );
    let minted = &output.proposals.staged_entities["organizations/example-corp"];
    assert_eq!(minted.source_memory_ids, ["new"]);
    assert!(minted.entity_id.is_none());
    assert_eq!(output.proposals.by_path["people/test"].promoted.len(), 1);
}
#[tokio::test]
async fn cancellation_and_shared_budget_prevent_model_calls() {
    let (phase, calls) = phase(vec![candidate()]);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let error = phase
        .run_with(input(), |run| run.cancellation(token))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled(_)));
    assert_eq!(calls.lock().unwrap().len(), 0);
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
async fn incomplete_snapshots_and_empty_prompts_fail_before_inference() {
    let (phase, calls) = phase(vec![]);
    let mut source = input();
    source.graph.memory_entities.clear();
    assert!(
        phase
            .run(source)
            .await
            .unwrap_err()
            .to_string()
            .contains("missing memory scope")
    );
    assert!(
        phase
            .prompts(ClassifyPrompts {
                input: String::new(),
                ..Default::default()
            })
            .run(input())
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn automatic_identity_discovery_requires_only_one_confirmation() {
    let mut source = input();
    source
        .graph
        .entities
        .push(entity("people/other-one", "Casey", &[]));
    source
        .graph
        .entities
        .push(entity("people/other-two", "Casey", &[]));
    let (phase, calls) = phase(vec![candidate(), candidate()]);
    let output = phase.run(source).await.unwrap();
    assert!(output.classification.is_some());
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(
        output.diagnostic["validation"]["failure_batches"][0]
            .as_str()
            .unwrap()
            .contains("multiple plausible")
    );
}
#[tokio::test]
async fn explicit_identity_research_suppresses_automatic_challenge() {
    let mut source = input();
    source
        .graph
        .entities
        .push(entity("people/other-one", "Casey", &[]));
    source
        .graph
        .entities
        .push(entity("people/other-two", "Casey", &[]));
    let (phase, calls) = phase(vec![
        json!({"__tool":"search_entities","arguments":{"query":"Casey"}}),
        candidate(),
    ]);
    let output = phase.run(source).await.unwrap();
    assert!(output.classification.is_some());
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(output.diagnostic["validation"]["failures"], 0);
}

#[tokio::test]
async fn entity_and_ontology_research_spend_the_same_allowance() {
    let entity = json!({"__tool":"search_entities","arguments":{"query":"Casey"}});
    let ontology = json!({"__tool":"ontology_term_search","arguments":{"term":"Person"}});
    for (first, second) in [(entity.clone(), ontology.clone()), (ontology, entity)] {
        let (phase, requests) = phase(vec![first, second, candidate()]);
        let output = phase.research_tool_budget(1).run(input()).await.unwrap();
        assert!(output.classification.is_some());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let first_observation = serde_json::to_string(&requests[1].messages).unwrap();
        let second_observation = serde_json::to_string(&requests[2].messages).unwrap();
        assert!(!first_observation.contains("Research-tool budget exhausted"));
        assert!(second_observation.contains("Research-tool budget exhausted (1 calls)"));
    }
}

#[tokio::test]
async fn replay_rebuilds_an_accepted_classification_without_model_calls() {
    let (phase, calls) = phase(vec![candidate()]);
    let initial = phase.run(input()).await.unwrap();
    let replay = phase
        .replay(input(), initial.classification.unwrap())
        .await
        .unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(
        replay.proposals.by_path["people/test"].classes,
        ["schema:Person"]
    );
    assert_eq!(
        replay.proposals.input_entity("people/test").unwrap().path,
        "people/test"
    );
}

#[derive(Default)]
struct Diagnostics(std::sync::Mutex<Vec<Value>>);
#[async_trait::async_trait]
impl crate::classify::ClassificationDiagnostics for Diagnostics {
    async fn record(&self, diagnostic: &Value) -> Result<(), Error> {
        self.0.lock().unwrap().push(diagnostic.clone());
        Ok(())
    }
}

#[tokio::test]
async fn interrupted_classification_records_diagnostics_without_an_accepted_output() {
    let mut invalid = candidate();
    invalid["classes"] = json!([{"class":"frona:illegal term"}]);
    let (harness, requests) = harness([
        Ok(tool_output("submit", invalid)),
        Err(frona_harness::inference::InferenceError::InferenceFailed(
            "provider unavailable".into(),
        )),
    ]);
    let phase = Classify::new(harness);
    let diagnostics = Diagnostics::default();
    let original = input();
    let result = phase
        .run_with_diagnostics(original.clone(), |run| run, &diagnostics)
        .await;
    assert!(result.is_err());
    assert!(original.proposals.by_path.is_empty());
    assert_eq!(requests.lock().unwrap().len(), 2);
    let captured = diagnostics.0.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["validation"]["attempts"], 1);
    assert_eq!(captured[0]["validation"]["failures"], 1);
}
