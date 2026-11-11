use std::collections::HashSet;

use crate::TranscriptEvidenceKind;
use crate::ingest::cleanup::terminal_cleanup;
use crate::ingest::correction::{GroundingFailure, apply_allowed_revision};
use crate::ingest::evidence::{batch_without_failed_contributions, resolve_citation};
use crate::ingest::submission::{Batch, SourceCitation};
use crate::ingest::temporal::extraction_submission_limit;
use crate::ingest::tests::conversion::sources;
use crate::ingest::tests::correction::T;
use crate::ingest::tests::evidence::transcript_source;
use crate::ingest::validation::validate_batch;
use crate::model::EvidenceStrength;

#[test]
fn empty_submission_has_no_optional_sections() {
    let batch: Batch = serde_json::from_value(serde_json::json!({})).unwrap();

    assert!(batch.new_entities.is_empty());
    assert!(batch.existing_entity_updates.is_empty());
    assert!(batch.playbooks.is_empty());
    assert!(batch.memories.is_empty());
    assert!(batch.research_dispositions.is_empty());
}

#[test]
fn submission_limit_scales_and_is_bounded() {
    assert_eq!(extraction_submission_limit(0), 10);
    assert_eq!(extraction_submission_limit(50), 10);
    assert_eq!(extraction_submission_limit(51), 11);
    assert_eq!(extraction_submission_limit(200), 40);
    assert_eq!(extraction_submission_limit(10_000), 40);
}

#[test]
fn terminal_cleanup_drops_bad_entity_citations_individually() {
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "path": "services/postgres",
            "name": "Postgres",
            "sources": [
                {"message": "m1", "quote": "postgres", "strength": "explicit"},
                {"message": "missing", "quote": "postgres", "strength": "explicit"}
            ]
        }],
        "memories": []
    }))
    .unwrap();
    assert_eq!(terminal_cleanup(&mut batch, &sources(T)), 1);
    assert_eq!(batch.new_entities.len(), 1);
    assert_eq!(batch.new_entities[0].sources.len(), 1);
}

#[test]
fn terminal_cleanup_drops_whole_memory_when_any_citation_is_bad() {
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [],
        "memories": [{
            "kind": "Fact",
            "content": "Postgres runs on 5433",
            "entities": ["services/postgres"],
            "sources": [
                {"message": "m1", "quote": "postgres runs on 5433", "strength": "explicit"},
                {"message": "m1", "quote": "runs on 9999", "strength": "explicit"}
            ]
        }]
    }))
    .unwrap();
    terminal_cleanup(&mut batch, &sources(T));
    assert!(batch.memories.is_empty());
}

#[test]
fn validation_reports_precise_field_and_reason() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "path": "services/database",
            "name": "Database",
            "sources": [{"message": "m1", "quote": "translated database", "strength": "explicit"}]
        }],
        "memories": []
    }))
    .unwrap();
    let failures = validate_batch(&batch, &sources(T));
    assert!(failures.iter().any(|failure| {
        failure.field_path == "new_entities[0].sources[0].quote"
            && failure.reason == "quote_not_found"
    }));
}

#[test]
fn ambiguous_agent_quote_feedback_lists_candidate_source_messages() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Compute Box has 64 GB.",
            "entities":["hardware/compute-box"],
            "sources":[{"message":"m3","quote":"Compute Box has 64 GB","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let sources = vec![
        transcript_source(
            "m1",
            "Compute Box has 64 GB.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "a1".into(),
                agent_id: "agent".into(),
                chat_id: "chat".into(),
            },
        ),
        transcript_source(
            "m2",
            "Compute Box has 64 GB.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "a2".into(),
                agent_id: "agent".into(),
                chat_id: "chat".into(),
            },
        ),
        transcript_source(
            "m3",
            "The comparison is complete.",
            TranscriptEvidenceKind::AgentMessage {
                message_id: "a3".into(),
                agent_id: "agent".into(),
                chat_id: "chat".into(),
            },
        ),
    ];

    let failure = validate_batch(&batch, &sources)
        .into_iter()
        .find(|failure| failure.reason == "quote_not_found")
        .unwrap();

    assert!(failure.message.contains("m1"));
    assert!(failure.message.contains("m2"));
}

#[test]
fn structural_grounding_failures_name_the_broken_invariant() {
    let cases = [
        (
            serde_json::json!({
                "kind":"Fact", "content":"Postgres ran yesterday", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "episode":{"status":"occurred","anchor":{"message":"m1","quote":"postgres runs on 5433"}}
            }),
            "episode_for_non_episodic",
        ),
        (
            serde_json::json!({
                "kind":"Episodic", "content":"Postgres ran yesterday", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
            }),
            "episodic_missing_episode",
        ),
        (
            serde_json::json!({
                "kind":"Episodic", "content":"Postgres ran yesterday", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "episode":{
                    "status":"occurred", "anchor":{"message":"m1","quote":"postgres runs on 5433"},
                    "duration":{"direction":"past","amount":1,"unit":"day","semantics":"elapsed"},
                    "absolute":{"year":2026,"month":8,"day":8,"hour":null,"minute":null}
                }
            }),
            "episode_has_duration_and_absolute",
        ),
        (
            serde_json::json!({
                "kind":"Procedural", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[
                    {"message":"m1","quote":"postgres runs on 5433","strength":"explicit"},
                    {"message":"m2","quote":"postgres runs on 5433","strength":"explicit"}
                ],
                "playbook":"missing"
            }),
            "procedural_requires_one_assertion_source",
        ),
        (
            serde_json::json!({
                "kind":"Procedural", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
            }),
            "procedural_playbook_missing",
        ),
        (
            serde_json::json!({
                "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "playbook":"restart"
            }),
            "non_procedural_playbook_present",
        ),
    ];
    let user_sources = vec![
        transcript_source(
            "m1",
            T,
            TranscriptEvidenceKind::UserMessage {
                message_id: "message-1".into(),
                chat_id: "chat-1".into(),
            },
        ),
        transcript_source(
            "m2",
            T,
            TranscriptEvidenceKind::UserMessage {
                message_id: "message-2".into(),
                chat_id: "chat-1".into(),
            },
        ),
    ];
    for (memory, expected) in cases {
        let batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [memory]
        }))
        .unwrap();
        let failures = validate_batch(&batch, &user_sources);
        assert!(
            failures.iter().any(|failure| failure.reason == expected),
            "missing {expected}: {failures:?}"
        );
    }

    let unknown_playbook: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [],
            "playbooks": [{"id":"other","path":"operations/other","name":"Other","description":"Other"}],
            "memories": [{
                "kind":"Procedural", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}],
                "playbook":"missing"
            }]
        })).unwrap();
    assert!(
        validate_batch(&unknown_playbook, &sources(T))
            .iter()
            .any(|failure| failure.reason == "unknown_playbook_candidate")
    );
}

#[test]
fn invalid_playbook_candidates_are_rejected_before_their_memories_can_be_lost() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [],
        "playbooks": [{
            "id":"restart", "path":"procedures/restart-postgres",
            "name":"Restart Postgres", "description":""
        }],
        "memories": [{
            "kind":"procedural", "content":"Postgres runs on 5433",
            "entities":["services/postgres"], "playbook":"restart",
            "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
        }]
    }))
    .unwrap();

    let failures = validate_batch(&batch, &sources(T));

    assert!(
        failures.iter().any(|failure| {
            failure.field_path == "playbooks[0].description"
                && failure.reason == "playbook_description_required"
        }),
        "the invalid candidate must be corrected before conversion: {failures:?}"
    );
    assert!(
        failures.iter().any(|failure| {
            failure.field_path == "memories[0].playbook"
                && failure.reason == "unknown_playbook_candidate"
        }),
        "the dependent memory must be part of the same grouped feedback: {failures:?}"
    );
}

#[test]
fn playbook_identity_is_not_duplicated_as_an_entity_or_memory_subject() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [{
            "path":"tools/yfinance/fetch-stock-close-price",
            "name":"Fetch Stock Close Price",
            "description":"Fetch a stock close price.",
            "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
        }],
        "existing_entity_updates": [],
        "playbooks": [{
            "id":"fetch-close", "path":"tools/yfinance/fetch-stock-close-price",
            "name":"Fetch Stock Close Price", "description":"Fetch a stock close price."
        }],
        "memories": [{
            "kind":"Procedural", "content":"Postgres runs on 5433",
            "entities":["tools/yfinance", "tools/yfinance/fetch-stock-close-price"],
            "playbook":"fetch-close",
            "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
        }]
    }))
    .unwrap();

    let failures = validate_batch(&batch, &sources(T));

    assert!(
        failures.iter().any(|failure| {
            failure.field_path == "new_entities[0].path"
                && failure.reason == "entity_path_duplicates_playbook_candidate"
        }),
        "the entity and Playbook collision must be rejected: {failures:?}"
    );
    assert!(
        failures.iter().any(|failure| {
            failure.field_path == "memories[0].entities"
                && failure.reason == "procedural_page_duplicates_playbook_candidate"
        }),
        "the Playbook association must use only the playbook field: {failures:?}"
    );

    let cleaned = batch_without_failed_contributions(&batch, &failures);
    assert!(cleaned.new_entities.is_empty());
    assert_eq!(cleaned.memories.len(), 1);
    assert_eq!(cleaned.memories[0].entities, ["tools/yfinance"]);
    assert_eq!(cleaned.playbooks.len(), 1);
}

#[test]
fn procedural_memory_may_cite_multiple_passages_from_one_agent_message() {
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [],
            "playbooks": [{"id":"restart","path":"operations/restart","name":"Restart","description":"Restart safely"}],
            "memories": [{
                "kind":"Procedural", "content":"Install the client and run the restart command.",
                "entities":["services/postgres"], "playbook":"restart",
                "sources":[
                    {"message":"m1","quote":"postgres runs","strength":"explicit"},
                    {"message":"m1","quote":"on 5433","strength":"explicit"}
                ]
            }]
        })).unwrap();

    let failures = validate_batch(&batch, &sources(T));

    assert!(
        !failures
            .iter()
            .any(|failure| failure.reason == "procedural_requires_one_assertion_source"),
        "two passages from one Agent message are one assertion source: {failures:?}"
    );
    terminal_cleanup(&mut batch, &sources(T));
    assert_eq!(
        batch.memories.len(),
        1,
        "terminal cleanup uses the same distinct-message invariant as admission"
    );
}

#[test]
fn confirmation_failures_distinguish_missing_claim_from_ineligible_source() {
    let user_sources = vec![transcript_source(
        "m1",
        "Yes, Postgres runs on 5433.",
        TranscriptEvidenceKind::UserMessage {
            message_id: "message-1".into(),
            chat_id: "chat-1".into(),
        },
    )];
    let confirmation = SourceCitation {
        message: "m1".into(),
        quote: "Postgres runs on 5433".into(),
        strength: EvidenceStrength::Explicit,
        confirmation: true,
    };
    let handles = HashSet::from(["m1"]);
    assert_eq!(
        resolve_citation(&confirmation, &user_sources, &handles).unwrap_err(),
        "confirmation_without_cited_agent_claim"
    );

    let agent_sources = sources(T);
    assert_eq!(
        resolve_citation(&confirmation, &agent_sources, &handles).unwrap_err(),
        "confirmation_on_ineligible_source"
    );
}

#[test]
fn correction_cannot_rewrite_an_unrejected_memory_field() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"Fact", "content":"Postgres is a database", "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":"Postgres is a relational database","strength":"explicit"}]
            }]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"Fact", "content":"Postgres is very reliable", "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres is a database","strength":"explicit"}]
        }]
    }))
    .unwrap();
    let original_failures = validate_batch(&original, &sources(T));
    let merged = apply_allowed_revision(&original, &revised, &original_failures);
    assert_eq!(merged.memories[0].content, "Postgres is a database");
    assert_eq!(
        merged.memories[0].sources[0].quote,
        "Postgres is a database"
    );
}

#[test]
fn correction_may_drop_a_rejected_memory_without_shifting_an_accepted_sibling() {
    let original: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [
                {
                    "kind":"Fact", "content":"Unsupported Agent claim", "entities":["topics/unsupported"],
                    "sources":[{"message":"m1","quote":"postgres runs","strength":"explicit"}]
                },
                {
                    "kind":"Fact", "content":"Postgres runs on 5433", "entities":["services/postgres"],
                    "sources":[{"message":"m1","quote":"postgres runs on 5433","strength":"explicit"}]
                }
            ]
        })).unwrap();
    let revised: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [original.memories[1].clone()]
    }))
    .unwrap();
    let failures = vec![GroundingFailure {
        field_path: "memories[0]".into(),
        message: "m1".into(),
        submitted: "Unsupported Agent claim".into(),
        reason: "agent_claim_needs_tool_evidence",
    }];

    let merged = apply_allowed_revision(&original, &revised, &failures);

    assert_eq!(merged.memories.len(), 1);
    assert_eq!(merged.memories[0].content, "Postgres runs on 5433");
}
