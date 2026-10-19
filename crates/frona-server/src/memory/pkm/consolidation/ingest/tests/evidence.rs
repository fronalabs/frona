use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicUsize;

use chrono::{TimeZone, Utc};

use crate::memory::pkm::consolidation::ingest::cleanup::terminal_cleanup_with_recall;
use crate::memory::pkm::consolidation::ingest::correction::GroundingFailure;
use crate::memory::pkm::consolidation::ingest::evidence::{
    batch_without_failed_contributions, resolve_evidence, resolve_evidence_with_tools,
    validate_agent_tool_grounding,
};
use crate::memory::pkm::consolidation::ingest::submission::{
    Batch, SourceCitation, ToolEvidenceCitation,
};
use crate::memory::pkm::consolidation::ingest::validation::{
    validate_batch_with_recall, validate_extract_submission, validate_selected_evidence,
};
use crate::memory::pkm::consolidation::{
    RecallProjection, ToolEvidenceProjection, TranscriptEvidenceKind, TranscriptEvidenceSource,
};
use crate::memory::pkm::model::{EvidenceSource, EvidenceStrength};

pub(super) fn transcript_source(
    handle: &str,
    text: &str,
    kind: TranscriptEvidenceKind,
) -> TranscriptEvidenceSource {
    TranscriptEvidenceSource {
        handle: handle.into(),
        text: text.into(),
        kind,
    }
}

fn recall_projection(message_id: &str, result: &str) -> RecallProjection {
    let call = crate::inference::tool_call::ToolCall {
        id: "call-1".into(),
        chat_id: "chat-1".into(),
        message_id: message_id.into(),
        turn: 1,
        provider_call_id: "provider-1".into(),
        name: "memory_search".into(),
        arguments: serde_json::json!({"query":"phone number"}),
        result: result.into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    RecallProjection::new(&[call], |_| false)
}

pub(super) fn selected_evidence_failures(
    mut batch: Batch,
    sources: &[TranscriptEvidenceSource],
    evidence: &ToolEvidenceProjection,
) -> Vec<GroundingFailure> {
    let mut failures = Vec::new();
    for (index, memory) in batch.memories.iter_mut().enumerate() {
        validate_selected_evidence(
            &format!("memories[{index}]"),
            &memory.content.clone(),
            &mut memory.sources,
            &mut memory.tool_evidence,
            sources,
            evidence,
            &mut failures,
        );
    }
    failures
}

#[test]
fn recall_only_agent_memory_is_rejected_without_non_recall_evidence() {
    let sources = vec![transcript_source(
        "m1",
        "Casey Owner's phone number is 555-0100.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [],
        "memories": [{
            "kind": "Fact", "content": "Casey Owner's phone number is 555-0100.",
            "entities": ["people/casey-owner"],
            "sources": [{"message":"m1","quote":"phone number is 555-0100","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let recall = recall_projection("agent-1", "Casey Owner — phone number is 555-0100");

    let failures = validate_batch_with_recall(&mut batch, &sources, &recall);
    assert!(
        failures
            .iter()
            .any(|failure| failure.reason == "agent_claim_without_tool_evidence")
    );
}

#[test]
fn user_correction_after_recall_is_not_rejected() {
    let sources = vec![
        transcript_source(
            "m1",
            "Casey Owner's phone number is 555-0100.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "agent-1".into(),
                agent_id: "agent".into(),
                chat_id: "chat-1".into(),
            },
        ),
        transcript_source(
            "m2",
            "No, it changed to 555-0199.",
            TranscriptEvidenceKind::UserMessage {
                message_id: "user-1".into(),
                chat_id: "chat-1".into(),
            },
        ),
    ];
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [],
        "memories": [{
            "kind": "Fact", "content": "Casey Owner's phone number changed to 555-0199.",
            "entities": ["people/casey-owner"],
            "sources": [{"message":"m2","quote":"changed to 555-0199","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let recall = recall_projection("agent-1", "Casey Owner — phone number is 555-0100");

    let failures = validate_batch_with_recall(&mut batch, &sources, &recall);
    assert!(
        !failures
            .iter()
            .any(|failure| failure.reason == "agent_claim_recalled")
    );
}

#[test]
fn recalled_agent_answer_cannot_leave_entities_or_attributes_behind() {
    let sources = vec![transcript_source(
        "m1",
        "Casey Owner's phone number is 555-0100.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [{
                "path":"people/casey-owner", "name":"Casey Owner", "description":"Casey Owner has phone number 555-0100",
                "sources":[{"message":"m1","quote":"Casey Owner's phone number is 555-0100","strength":"explicit"}],
                "candidate_attributes":[{
                    "key":"phone number", "value":"555-0100",
                    "sources":[{"message":"m1","quote":"phone number is 555-0100","strength":"explicit"}]
                }]
            }],
            "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Casey Owner's phone number is 555-0100.", "entities":["people/casey-owner"],
                "sources":[{"message":"m1","quote":"phone number is 555-0100","strength":"explicit"}]
            }]
        })).unwrap();
    let recall = recall_projection("agent-1", "Casey Owner — phone number is 555-0100");

    let dropped = terminal_cleanup_with_recall(&mut batch, &sources, &recall);
    assert!(dropped >= 2);
    assert!(batch.new_entities.is_empty());
    assert!(batch.memories.is_empty());
}

#[test]
fn newly_observed_agent_outcome_still_requires_non_recall_evidence() {
    let sources = vec![transcript_source(
        "m1",
        "The Postgres deployment was repaired successfully.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"The Postgres deployment was repaired successfully.",
                "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"Postgres deployment was repaired successfully","strength":"explicit"}]
            }]
        })).unwrap();
    let recall = recall_projection(
        "agent-1",
        "The Postgres deployment was broken before repair.",
    );

    let failures = validate_batch_with_recall(&mut batch, &sources, &recall);
    assert!(
        failures
            .iter()
            .any(|failure| failure.reason == "agent_claim_without_tool_evidence")
    );
}

#[test]
fn critical_values_may_be_covered_across_selected_execution_evidence() {
    let sources = vec![transcript_source(
        "m1",
        "Version 4.2 deployed with failed_checks=0.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = |id: &str, turn: u32, arguments: serde_json::Value, result: &str| {
        crate::inference::tool_call::ToolCall {
            id: id.into(),
            chat_id: "chat-1".into(),
            message_id: "agent-1".into(),
            turn,
            provider_call_id: format!("provider-{id}"),
            name: "shell".into(),
            arguments,
            result: result.into(),
            success: true,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            signature: None,
            turn_reasoning: None,
            created_at: Utc.timestamp_opt(turn as i64, 0).unwrap(),
        }
    };
    let calls = vec![
        call(
            "deploy",
            1,
            serde_json::json!({"version":"4.2"}),
            "deployment accepted",
        ),
        call(
            "health",
            2,
            serde_json::json!({"service":"api"}),
            "failed_checks=0",
        ),
    ];
    let evidence = ToolEvidenceProjection::new(
        &calls,
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "version deployment failed checks", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "Version 4.2 deployed with failed_checks=0".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![
        ToolEvidenceCitation {
            message: "m1".into(),
            evidence_id: "m1:tool1".into(),
            quote: "deployment accepted".into(),
        },
        ToolEvidenceCitation {
            message: "m1".into(),
            evidence_id: "m1:tool2".into(),
            quote: "failed_checks=0".into(),
        },
    ];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "Version 4.2 deployed with failed_checks=0.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "combined selected executions support the claim: {failures:?}"
    );
}

#[test]
fn supplied_tool_evidence_requires_a_matching_agent_assertion_source() {
    let sources = vec![
        transcript_source(
            "m4",
            "Model Alpha V1 is a mixture-of-experts model.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "agent-4".into(),
                agent_id: "agent".into(),
                chat_id: "chat-1".into(),
            },
        ),
        transcript_source(
            "m5",
            "Model Alpha V1 (MoE, the biggest open-weight)",
            TranscriptEvidenceKind::UserMessage {
                message_id: "user-5".into(),
                chat_id: "chat-1".into(),
            },
        ),
    ];
    let call = crate::inference::tool_call::ToolCall {
        id: "model-alpha".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-4".into(),
        turn: 1,
        provider_call_id: "provider-model-alpha".into(),
        name: "web_search".into(),
        arguments: serde_json::json!({"query":"Model Alpha V1 architecture"}),
        result: "Model Alpha V1 has 10B parameters and a mixture-of-experts architecture.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-4".into()],
        &["agent-4".into()],
        10,
        4_000,
        |_| false,
    );
    let search: serde_json::Value = serde_json::from_str(&evidence.search_for_message(
        "m4",
        "agent-4",
        "Model Alpha V1 architecture",
        4_000,
    ))
    .unwrap();
    let evidence_id = search["results"][0]["evidence_id"].as_str().unwrap();
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact",
            "content":"Model Alpha V1 is a 10-billion-parameter mixture-of-experts model.",
            "entities":["models/qwen3-235b"],
            "sources":[{
                "message":"m5", "quote":"Model Alpha V1 (MoE, the biggest open-weight)",
                "strength":"explicit"
            }],
            "tool_evidence":[{
                "message":"m4", "evidence_id":evidence_id,
                "quote":"Model Alpha V1 has 10B parameters and a mixture-of-experts architecture."
            }]
        }]
    }))
    .unwrap();
    let mut recall = RecallProjection::default();
    recall.evidence = evidence;

    let failures = validate_extract_submission(
        &mut batch,
        &sources,
        &[],
        &recall,
        &HashSet::new(),
        &HashSet::new(),
        &AtomicUsize::new(0),
    );

    assert!(
        failures.iter().any(|failure| {
            failure.field_path == "memories[0].tool_evidence"
                && failure.message == "m4"
                && failure.reason == "tool_evidence_without_agent_source"
        }),
        "{failures:?}"
    );
}

#[test]
fn critical_value_rejection_reports_all_missing_values_at_once() {
    let sources = vec![transcript_source(
        "m1",
        "Flights EX101 and EX202 depart at 9:00 AM.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "flights".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-flights".into(),
        name: "web_search".into(),
        arguments: serde_json::json!({"query":"SFO SEA flights"}),
        result: "EXA101 and EXA202 depart at 09:00AM.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "flights depart", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "Flights EX101 and EX202 depart at 9:00 AM".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: "m1:tool1".into(),
        quote: "EXA101 and EXA202 depart at 09:00AM".into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "Flights EX101 and EX202 depart at 9:00 AM.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );
    validate_selected_evidence(
        "memories[0]",
        "Flights EX101 and EX202 depart at 9:00 AM.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert_eq!(failures.len(), 1);
    assert!(failures[0].submitted.contains("EX101"), "{failures:?}");
    assert!(failures[0].submitted.contains("EX202"), "{failures:?}");
    assert!(
        !failures[0]
            .submitted
            .contains("missing critical value: 6:28"),
        "{failures:?}"
    );
    let rendered = failures[0].render_with_allowed(None);
    assert!(
        rendered.contains("EX101") && rendered.contains("EX202"),
        "{rendered}"
    );
}

#[test]
fn critical_value_rejection_identifies_only_the_unsupported_clause() {
    let sources = vec![transcript_source(
        "m1",
        "Accelerator A has 32 GB of memory. Accelerator B has 48 GB of memory.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "accelerator-a".into(),
        chat_id: "task-chat".into(),
        message_id: "task-agent".into(),
        turn: 1,
        provider_call_id: "provider-accelerator-a".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":"https://example.test/accelerator-a"}),
        result: "The Accelerator A accelerator has 32 GB of memory.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let mut task_evidence = HashMap::new();
    task_evidence.insert("agent-1".into(), vec![call]);
    let evidence = ToolEvidenceProjection::new_with_task_evidence(
        &[],
        &["agent-1".into()],
        &["agent-1".into()],
        &task_evidence,
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "Accelerator A 32 GB", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "Accelerator A has 32 GB of memory. Accelerator B has 48 GB of memory".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: "m1:tool1".into(),
        quote: "Accelerator A accelerator has 32 GB of memory".into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "Accelerator A has 32 GB of memory. Accelerator B has 48 GB of memory.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );
    validate_selected_evidence(
        "memories[0]",
        "Accelerator A has 32 GB of memory. Accelerator B has 48 GB of memory.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].reason, "tool_evidence_clause_mismatch");
    assert!(
        failures[0].submitted.contains("Accelerator B has 48 GB"),
        "{failures:?}"
    );
    assert!(
        !failures[0]
            .submitted
            .contains("unsupported clause: Accelerator A"),
        "{failures:?}"
    );
}

#[test]
fn terminal_cleanup_preserves_supported_clauses() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "id":"mem-accelerators", "kind":"Fact",
                "content":"Accelerator A has 32 GB of memory. Accelerator B has 48 GB of memory.",
                "entities":["hardware/accelerator-a", "hardware/accelerator-b"],
                "sources":[{"message":"m1","quote":"Accelerator A and Accelerator B specifications","strength":"derived"}],
                "tool_evidence":[{"message":"m1","evidence_id":"m1:tool1","quote":"Accelerator A has 32 GB"}]
            }]
        })).unwrap();
    let failures = vec![GroundingFailure {
        field_path: "memories[0].tool_evidence".into(),
        message: "Accelerator B has 48 GB of memory.".into(),
        submitted: "unsupported clause: Accelerator B has 48 GB of memory.".into(),
        reason: "tool_evidence_clause_mismatch",
    }];

    let cleaned = batch_without_failed_contributions(&batch, &failures);

    assert_eq!(cleaned.memories.len(), 1);
    assert_eq!(cleaned.memories[0].id, "mem-accelerators");
    assert_eq!(
        cleaned.memories[0].content,
        "Accelerator A has 32 GB of memory."
    );
}

#[test]
fn corrected_agent_evidence_checks_the_complete_selected_execution() {
    let sources = vec![transcript_source(
        "m1",
        "Version 4.2 deployed with failed_checks=0.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "deploy".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-deploy".into(),
        name: "shell".into(),
        arguments: serde_json::json!({"command":"deploy 4.2"}),
        result: "deployment accepted; version=4.2 failed_checks=0. A previous deployment failed."
            .into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "deployment version failed checks", 4_000);
    let revised: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Version 4.2 deployed with failed_checks=0.",
                "entities":["services/api"],
                "sources":[{"message":"m1","quote":"Version 4.2 deployed with failed_checks=0","strength":"derived"}],
                "tool_evidence":[{"message":"m1","evidence_id":"m1:tool1","quote":"deployment accepted"}]
            }]
        })).unwrap();
    let failures = selected_evidence_failures(revised, &sources, &evidence);

    assert!(
        failures.is_empty(),
        "critical values come from the complete selected execution, not only its narrow quote: {failures:?}"
    );
}

#[test]
fn revised_evidence_validation_does_not_use_array_position_as_identity() {
    let sources = vec![transcript_source(
        "m1",
        "Accelerator A has 32 GB. Accelerator B has 48 GB.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "accelerator-a".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-accelerator-a".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":"https://example.test/accelerator-a"}),
        result: "Accelerator A has 32 GB.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "Accelerator A 32 GB", 4_000);
    let revised: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [
                {
                    "id":"mem-accelerator-a", "kind":"Fact", "content":"Accelerator A has 32 GB.",
                    "entities":["hardware/accelerator-a"],
                    "sources":[{"message":"m1","quote":"Accelerator A has 32 GB","strength":"derived"}],
                    "tool_evidence":[{"message":"m1","evidence_id":"m1:tool1","quote":"Accelerator A has 32 GB"}]
                },
                {
                    "id":"mem-accelerator-b", "kind":"Fact", "content":"Accelerator B has 48 GB.",
                    "entities":["hardware/accelerator-b"],
                    "sources":[{"message":"m1","quote":"Accelerator B has 48 GB","strength":"derived"}],
                    "tool_evidence":[{"message":"m1","evidence_id":"m1:tool1","quote":"Accelerator A has 32 GB"}]
                }
            ]
        })).unwrap();
    let failures = selected_evidence_failures(revised, &sources, &evidence);

    assert!(
        failures
            .iter()
            .any(|failure| failure.submitted.contains("Accelerator B")
                || failure.submitted.contains("141")),
        "{failures:?}"
    );
}

#[test]
fn tool_operation_name_is_available_for_critical_value_validation() {
    let sources = vec![transcript_source(
        "m1",
        "A retry of web_fetch succeeded.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "fetch".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-fetch".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":"https://example.test"}),
        result: "The retry completed successfully.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "retry completed successfully", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "retry of web_fetch succeeded".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: "m1:tool1".into(),
        quote: "retry completed successfully".into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "A retry of web_fetch succeeded.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "the selected execution includes its operation name: {failures:?}"
    );
}

#[test]
fn local_negative_constraint_does_not_negate_a_positive_procedure() {
    let sources = vec![transcript_source(
        "m1",
        "Enter DFU mode from the home screen by holding SET.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "instructions".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-instructions".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":"https://example.test/instructions"}),
        result: "From the home screen, not in any menu, hold SET to enter DFU mode.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "home screen DFU mode SET", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "Enter DFU mode from the home screen by holding SET".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: "m1:tool1".into(),
        quote: "From the home screen, not in any menu, hold SET to enter DFU mode".into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "Enter DFU mode from the home screen by holding SET.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "an unrelated local constraint must not negate the procedure: {failures:?}"
    );
}

#[test]
fn tool_evidence_resolves_against_its_own_agent_citation() {
    let sources = vec![
        transcript_source(
            "m1",
            "I checked the release feed.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "agent-1".into(),
                agent_id: "agent".into(),
                chat_id: "chat-1".into(),
            },
        ),
        transcript_source(
            "m2",
            "Acme released version 4.2.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "agent-2".into(),
                agent_id: "agent".into(),
                chat_id: "chat-1".into(),
            },
        ),
    ];
    let call = crate::inference::tool_call::ToolCall {
        id: "release".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-2".into(),
        turn: 1,
        provider_call_id: "provider-release".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":"https://example.test/releases"}),
        result: "Acme released version 4.2.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into(), "agent-2".into()],
        &["agent-1".into(), "agent-2".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m2", "agent-2", "Acme version 4.2", 4_000);
    let mut citations = vec![
        SourceCitation {
            message: "m1".into(),
            quote: "I checked the release feed".into(),
            strength: EvidenceStrength::Explicit,
            confirmation: false,
        },
        SourceCitation {
            message: "m2".into(),
            quote: "Acme released version 4.2".into(),
            strength: EvidenceStrength::Explicit,
            confirmation: false,
        },
    ];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m2".into(),
        evidence_id: "m2:tool1".into(),
        quote: "Acme released version 4.2".into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "Acme released version 4.2.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "the selected ID belongs to m2, not the first Agent citation: {failures:?}"
    );
}

#[test]
fn tool_quote_is_resolved_against_the_sanitized_execution_shown_during_ingest() {
    let sources = vec![transcript_source(
        "m1",
        "The request status was ok.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "request".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-request".into(),
        name: "http".into(),
        arguments: serde_json::json!({"a":"one", "authorization":"Bearer secret", "z":"two"}),
        result: "request completed".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let evidence = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    evidence.search_for_message("m1", "agent-1", "request completed", 4_000);
    let mut citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "request status was ok".into(),
        strength: EvidenceStrength::Derived,
        confirmation: false,
    }];
    let mut tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: "m1:tool1".into(),
        quote: r#"{"a":"one","z":"two"}"#.into(),
    }];
    let mut failures = Vec::new();

    validate_agent_tool_grounding(
        "memories[0]",
        "The request status was ok.",
        &mut citations,
        &mut tool_citations,
        &sources,
        &evidence,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "the sanitized execution is the model-visible citation surface: {failures:?}"
    );
}

#[test]
fn evidence_resolves_message_ids_roles_quotes_and_strengths() {
    let sources = vec![transcript_source(
        "m1",
        "It finished 1–1.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "message-1".into(),
            agent_id: "agent-1".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "finished 1–1".into(),
        strength: EvidenceStrength::Explicit,
        confirmation: false,
    }];
    let evidence = resolve_evidence(&citations, &sources).expect("valid evidence");
    assert_eq!(evidence[0].strength, EvidenceStrength::Explicit);
    assert!(matches!(&evidence[0].source,
            EvidenceSource::AgentMessage { message_id, agent_id, chat_id, quote }
            if message_id == "message-1" && agent_id == "agent-1"
                && chat_id == "chat-1" && quote == "finished 1–1"));
}

#[test]
fn requested_url_is_stored_in_web_page_memory_evidence() {
    let sources = vec![transcript_source(
        "m1",
        "Acme released version 4.2.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let call = crate::inference::tool_call::ToolCall {
        id: "release".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        provider_call_id: "provider-release".into(),
        name: "web_fetch".into(),
        arguments: serde_json::json!({
            "url":"https://example.test/releases?version=4.2&channel=stable"
        }),
        result: "Acme released version 4.2.".into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc.timestamp_opt(1, 0).unwrap(),
    };
    let projection = ToolEvidenceProjection::new(
        &[call],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let search = projection.search_for_message("m1", "agent-1", "Acme version 4.2", 4_000);
    let search: serde_json::Value = serde_json::from_str(&search).unwrap();
    let evidence_id = search["results"][0]["evidence_id"].as_str().unwrap();
    let citations = vec![SourceCitation {
        message: "m1".into(),
        quote: "Acme released version 4.2".into(),
        strength: EvidenceStrength::Explicit,
        confirmation: false,
    }];
    let tool_citations = vec![ToolEvidenceCitation {
        message: "m1".into(),
        evidence_id: evidence_id.into(),
        quote: "Acme released version 4.2".into(),
    }];

    let evidence = resolve_evidence_with_tools(&citations, &tool_citations, &sources, &projection)
        .expect("valid web-page evidence");

    assert!(matches!(
        &evidence[1].source,
        EvidenceSource::WebPage { url: Some(url), .. }
            if url == "https://example.test/releases?version=4.2&channel=stable"
    ));
}

#[test]
fn confirmation_requires_the_preceding_agent_claim_in_the_same_citation_set() {
    let sources = vec![
        transcript_source(
            "m1",
            "You live in Exampletown.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "a".into(),
                agent_id: "agent".into(),
                chat_id: "chat".into(),
            },
        ),
        transcript_source(
            "m2",
            "Yes, all of that is correct.",
            TranscriptEvidenceKind::UserMessage {
                message_id: "u".into(),
                chat_id: "chat".into(),
            },
        ),
    ];
    let confirmation = SourceCitation {
        message: "m2".into(),
        quote: "all of that is correct".into(),
        strength: EvidenceStrength::Explicit,
        confirmation: true,
    };
    assert!(resolve_evidence(std::slice::from_ref(&confirmation), &sources).is_none());
    let agent = SourceCitation {
        message: "m1".into(),
        quote: "live in Exampletown".into(),
        strength: EvidenceStrength::Explicit,
        confirmation: false,
    };
    let evidence = resolve_evidence(&[agent, confirmation], &sources).expect("paired confirmation");
    assert!(matches!(
        evidence[1].source,
        EvidenceSource::UserConfirmation { .. }
    ));
}
