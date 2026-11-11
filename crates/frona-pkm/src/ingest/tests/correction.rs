use crate::ToolEvidenceProjection;
use crate::ingest::correction::{
    GroundingFailure, apply_allowed_revision, correction_memory_ids, render_memory_evidence,
    validate_entity_revision_identity,
};
use crate::ingest::evidence::resolve_evidence;
use crate::ingest::submission::Batch;
use crate::ingest::tests::conversion::sources;
use crate::ingest::tests::evidence::selected_evidence_failures;
use crate::ingest::validation::validate_batch;
use crate::model::EvidenceSource;

#[test]
fn correction_may_change_the_coupled_kind_and_episode_fields() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "episode":{"status":"occurred","anchor":{"message":"m1","quote":"postgres runs on 5433"}}
            }]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Episodic", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "episode":{"status":"occurred","anchor":{"message":"m1","quote":"postgres runs on 5433"}}
            }]
        })).unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    assert!(
        selected_evidence_failures(
            merged.clone(),
            &sources(T),
            &ToolEvidenceProjection::default(),
        )
        .is_empty()
    );
    assert_eq!(merged.memories[0].kind, "Episodic");
}

#[test]
fn correction_feedback_separates_retained_and_repair_memory_ids() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [
            {
                "id":"mem-accepted", "kind":"Fact", "content":"The service is active.",
                "entities":["services/example"],
                "sources":[{"message":"m1","quote":"service is active","strength":"explicit"}]
            },
            {
                "id":"mem-repair", "kind":"Fact", "content":"The service uses port 5433.",
                "entities":["services/example"],
                "sources":[{"message":"m1","quote":"wrong quote","strength":"explicit"}]
            }
        ]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "memories[1].sources[0].quote".into(),
        message: "m1".into(),
        submitted: "wrong quote".into(),
        reason: "quote_not_found",
    }];

    let (accepted, repairs) = correction_memory_ids(&batch, &failures);

    assert_eq!(accepted, vec!["mem-accepted"]);
    assert_eq!(repairs, vec!["mem-repair"]);
    assert_eq!(
        render_memory_evidence(&batch, &accepted),
        "- `mem-accepted` -> m1"
    );
    assert_eq!(render_memory_evidence(&batch, &[]), "(none)");
}

#[test]
fn coverage_feedback_keeps_existing_memory_ids_accepted() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "id":"mem-retained", "kind":"Fact", "content":"The service is active.",
            "entities":["services/example"],
            "sources":[{"message":"m1","quote":"service is active","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "research_dispositions.claims".into(),
        message: "m2".into(),
        submitted: "no claim-level coverage".into(),
        reason: "research_claims_required",
    }];

    let (accepted, repairs) = correction_memory_ids(&batch, &failures);

    assert_eq!(accepted, vec!["mem-retained"]);
    assert!(repairs.is_empty());
}

#[test]
fn corrected_memory_evidence_cannot_shrink_to_an_entity_mention() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"Postgres is configured and runs on port 5433","strength":"explicit"}]
            }]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    let failures =
        selected_evidence_failures(merged, &sources(T), &ToolEvidenceProjection::default());
    assert!(failures.iter().any(|failure| {
        failure.reason == "selected_evidence_missing_critical_values"
            && failure.field_path == "memories[0].sources"
    }));
}

#[test]
fn corrected_memory_may_use_shorter_evidence_that_preserves_critical_values() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"Postgres is configured and runs on port 5433","strength":"explicit"}]
            }]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    let failures =
        selected_evidence_failures(merged, &sources(T), &ToolEvidenceProjection::default());

    assert!(
        failures.is_empty(),
        "short exact evidence preserves the complete claim: {failures:?}"
    );
}

#[test]
fn corrected_memory_may_use_multiple_exact_spans_for_one_atomic_claim() {
    let original: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres runs on port 5433","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[
                {"message":"m1","quote":"postgres runs","strength":"explicit"},
                {"message":"m1","quote":"on 5433","strength":"explicit"}
            ]
        }]
    }))
    .unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    assert!(
        selected_evidence_failures(
            merged.clone(),
            &sources(T),
            &ToolEvidenceProjection::default(),
        )
        .is_empty()
    );
    assert!(validate_batch(&merged, &sources(T)).is_empty());
    let evidence = resolve_evidence(&merged.memories[0].sources, &sources(T)).unwrap();
    assert!(
        matches!(&evidence[0].source, EvidenceSource::AgentMessage { quote, .. }
            if quote == "postgres runs")
    );
    assert!(
        matches!(&evidence[1].source, EvidenceSource::AgentMessage { quote, .. }
            if quote == "on 5433")
    );
}

#[test]
fn correction_may_reorder_grounded_sibling_citations() {
    let original: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[
                {"message":"m1","quote":"postgres runs","strength":"explicit"},
                {"message":"m1","quote":"wrong port 5433","strength":"explicit"}
            ]
        }]
    }))
    .unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
            "sources":[
                {"message":"m1","quote":"on 5433","strength":"explicit"},
                {"message":"m1","quote":"postgres runs","strength":"explicit"}
            ]
        }]
    }))
    .unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    assert!(
        selected_evidence_failures(
            merged.clone(),
            &sources(T),
            &ToolEvidenceProjection::default(),
        )
        .is_empty()
    );
    assert!(validate_batch(&merged, &sources(T)).is_empty());
}

#[test]
fn correction_uses_memory_id_when_the_model_reorders_siblings() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [
                {
                    "id":"mem-accelerator-a", "kind":"Fact", "content":"Accelerator A has 32 GB.",
                    "entities":["hardware/nvidia"],
                    "sources":[{"message":"m1","quote":"wrong Accelerator A quote","strength":"explicit"}]
                },
                {
                    "id":"mem-accelerator-b", "kind":"Fact", "content":"Accelerator B has 48 GB.",
                    "entities":["hardware/nvidia"],
                    "sources":[{"message":"m1","quote":"Accelerator B has 48 GB","strength":"explicit"}]
                }
            ]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [
            original.memories[1].clone(),
            {
                "id":"mem-accelerator-a", "kind":"Fact", "content":"Accelerator A has 32 GB.",
                "entities":["hardware/nvidia"],
                "sources":[{"message":"m1","quote":"Accelerator A has 32 GB","strength":"explicit"}]
            }
        ]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "memories[0].sources[0].quote".into(),
        message: "m1".into(),
        submitted: "wrong Accelerator A quote".into(),
        reason: "quote_not_found",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(merged.memories[0].id, "mem-accelerator-a");
    assert_eq!(
        merged.memories[0].sources[0].quote,
        "Accelerator A has 32 GB"
    );
    assert_eq!(merged.memories[1].id, "mem-accelerator-b");
    assert_eq!(
        merged.memories[1].sources[0].quote,
        "Accelerator B has 48 GB"
    );
}

#[test]
fn correction_can_replace_one_mixed_memory_with_a_supported_split_by_id() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [
                {
                    "id":"mem-mixed", "kind":"Fact",
                    "content":"Accelerator A has 32 GB and Accelerator B has 48 GB.",
                    "entities":["hardware/nvidia"],
                    "sources":[{"message":"m1","quote":"Accelerator A has 32 GB and Accelerator B has 48 GB","strength":"explicit"}]
                },
                {
                    "id":"mem-kept", "kind":"Fact", "content":"Compute Box has 64 GB.",
                    "entities":["hardware/nvidia"],
                    "sources":[{"message":"m1","quote":"Compute Box has 64 GB","strength":"explicit"}]
                }
            ]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [
            original.memories[1].clone(),
            {
                "id":"mem-accelerator-a", "kind":"Fact", "content":"Accelerator A has 32 GB.",
                "entities":["hardware/nvidia"],
                "sources":[{"message":"m1","quote":"Accelerator A has 32 GB","strength":"explicit"}]
            }
        ]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "memories[0].tool_evidence".into(),
        message: "m1".into(),
        submitted: "Accelerator B has no support".into(),
        reason: "agent_claim_needs_tool_evidence",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(
        merged
            .memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        vec!["mem-kept", "mem-accelerator-a"]
    );
}

#[test]
fn correction_uses_attribute_id_when_the_model_reorders_siblings() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [{
                "path":"services/api", "name":"API", "description":"The API service.",
                "sources":[{"message":"m1","quote":"API","strength":"explicit"}],
                "candidate_attributes":[
                    {"id":"attr-version","key":"version","value":"4.1","sources":[{"message":"m1","quote":"4.1","strength":"derived"}]},
                    {"id":"attr-port","key":"port","value":"8080","sources":[{"message":"m1","quote":"8080","strength":"derived"}]}
                ]
            }],
            "existing_entity_updates": [], "playbooks": [], "memories": []
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [{
                "path":"services/api", "name":"API", "description":"The API service.",
                "sources":[{"message":"m1","quote":"API","strength":"explicit"}],
                "candidate_attributes":[
                    {"id":"attr-port","key":"port","value":"8080","sources":[{"message":"m1","quote":"8080","strength":"derived"}]},
                    {"id":"attr-version","key":"version","value":"4.2","sources":[{"message":"m1","quote":"4.2","strength":"derived"}]}
                ]
            }],
            "existing_entity_updates": [], "playbooks": [], "memories": []
        })).unwrap();
    let failures = vec![GroundingFailure {
        field_path: "new_entities[0].candidate_attributes[0].value".into(),
        message: "m1".into(),
        submitted: "4.1".into(),
        reason: "structured_value_not_found",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(
        merged.new_entities[0].candidate_attributes[0].id,
        "attr-version"
    );
    assert_eq!(merged.new_entities[0].candidate_attributes[0].value, "4.2");
    assert_eq!(
        merged.new_entities[0].candidate_attributes[1].id,
        "attr-port"
    );
}

#[test]
fn correction_uses_entity_id_when_a_sibling_is_omitted_and_the_path_changes() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [
                {
                    "id":"entity-accepted", "path":"products/compute-box", "name":"Compute Box",
                    "description":"A compact AI computer.",
                    "sources":[{"message":"m1","quote":"Compute Box","strength":"explicit"}]
                },
                {
                    "id":"entity-model-alpha", "path":"topics/models/model-alpha-v1", "name":"Model Alpha V1",
                    "description":"A large language model.",
                    "sources":[{"message":"m1","quote":"wrong Model Alpha quote","strength":"explicit"}]
                }
            ],
            "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "id":"mem-model-alpha", "kind":"Fact", "content":"Model Alpha runs on Compute Box.",
                "entities":["topics/models/model-alpha-v1", "products/compute-box"],
                "sources":[{"message":"m1","quote":"Model Alpha runs on Compute Box","strength":"explicit"}]
            }]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "id":"entity-model-alpha", "path":"topics/models/model-alpha", "name":"Model Alpha",
            "description":"A large language model.",
            "sources":[{"message":"m1","quote":"Model Alpha","strength":"explicit"}]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [original.memories[0].clone()]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "new_entities[1].sources[0].quote".into(),
        message: "m1".into(),
        submitted: "wrong Model Alpha quote".into(),
        reason: "quote_not_found",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(
        merged.new_entities.len(),
        2,
        "the accepted sibling remains retained"
    );
    assert_eq!(merged.new_entities[1].path, "topics/models/model-alpha");
    assert_eq!(merged.new_entities[1].name, "Model Alpha");
    assert_eq!(merged.new_entities[1].sources[0].quote, "Model Alpha");
    assert_eq!(
        merged.memories[0].entities,
        vec!["topics/models/model-alpha", "products/compute-box"]
    );
}

#[test]
fn new_entity_ids_are_required_and_unique() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [
            {
                "id":"", "path":"topics/one", "name":"One", "description":"One topic.",
                "sources":[{"message":"m1","quote":"One","strength":"explicit"}]
            },
            {
                "id":"entity-two", "path":"topics/two", "name":"Two", "description":"Two topic.",
                "sources":[{"message":"m1","quote":"Two","strength":"explicit"}]
            },
            {
                "id":"entity-two", "path":"topics/other-two", "name":"Other Two",
                "description":"Another topic.",
                "sources":[{"message":"m1","quote":"Other Two","strength":"explicit"}]
            }
        ],
        "existing_entity_updates": [], "playbooks": [], "memories": []
    }))
    .unwrap();

    let failures = validate_batch(&batch, &sources("One. Two. Other Two."));

    assert!(
        failures
            .iter()
            .any(|failure| failure.reason == "entity_id_required"
                && failure.field_path == "new_entities[0].id")
    );
    assert_eq!(
        failures
            .iter()
            .filter(|failure| failure.reason == "duplicate_entity_id")
            .count(),
        2
    );
}

#[test]
fn correction_rejects_a_changed_entity_id_and_retains_the_original_candidate() {
    let original: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "id":"entity-model-alpha", "path":"topics/model-alpha-v1", "name":"Model Alpha V1",
            "description":"A model.",
            "sources":[{"message":"m1","quote":"wrong quote","strength":"explicit"}]
        }],
        "existing_entity_updates": [], "playbooks": [], "memories": []
    }))
    .unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "id":"entity-model-alpha-fixed", "path":"topics/model-alpha", "name":"Model Alpha",
            "description":"A model.",
            "sources":[{"message":"m1","quote":"Model Alpha","strength":"explicit"}]
        }],
        "existing_entity_updates": [], "playbooks": [], "memories": []
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "new_entities[0].sources[0].quote".into(),
        message: "m1".into(),
        submitted: "wrong quote".into(),
        reason: "quote_not_found",
    }];

    let protocol_failures = validate_entity_revision_identity(&original, &revised, &failures);
    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(protocol_failures.len(), 1);
    assert_eq!(protocol_failures[0].reason, "entity_id_changed");
    assert_eq!(
        protocol_failures[0].message,
        "expected retained entity ID: entity-model-alpha"
    );
    assert_eq!(merged.new_entities[0].id, "entity-model-alpha");
    assert_eq!(merged.new_entities[0].sources[0].quote, "wrong quote");
}

#[test]
fn correction_can_assign_an_initially_missing_entity_id() {
    let original: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "path":"topics/model-alpha", "name":"Model Alpha", "description":"A model.",
            "sources":[{"message":"m1","quote":"Model Alpha","strength":"explicit"}]
        }],
        "existing_entity_updates": [], "playbooks": [], "memories": []
    }))
    .unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [{
                "id":"entity-model-alpha", "path":"topics/model-alpha", "name":"Model Alpha", "description":"A model.",
                "sources":[{"message":"m1","quote":"Model Alpha","strength":"explicit"}]
            }],
            "existing_entity_updates": [], "playbooks": [], "memories": []
        })).unwrap();
    let failures = vec![GroundingFailure {
        field_path: "new_entities[0].id".into(),
        message: String::new(),
        submitted: String::new(),
        reason: "entity_id_required",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(merged.new_entities[0].id, "entity-model-alpha");
}

/// Postgres and Alice are anchored; "Hyprland" appears nowhere.
pub(super) const T: &str = "User: my postgres runs on 5433. Alice manages the database.";
