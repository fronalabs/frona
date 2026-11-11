use std::collections::HashSet;

use chrono::{TimeZone, Utc};

use crate::memory::pkm::consolidation::ingest::cleanup::{
    remove_multi_entity_candidate_attributes, terminal_cleanup,
};
use crate::memory::pkm::consolidation::ingest::submission::{
    Batch, NewEntity, NewMemory, NewPlaybookCandidate, SourceCitation,
};
use crate::memory::pkm::consolidation::ingest::temporal::resolve_episode;
use crate::memory::pkm::consolidation::ingest::tests::correction::T;
use crate::memory::pkm::consolidation::ingest::tests::evidence::transcript_source;
use crate::memory::pkm::consolidation::ingest::validation::{
    research_coverage_stats, validate_research_coverage,
};
use crate::memory::pkm::consolidation::{
    TemporalSource, TranscriptEvidenceKind, TranscriptEvidenceSource,
};
use crate::memory::pkm::model::{
    Episode, EpisodeStatus, EvidenceStrength, RelativeDuration, TemporalAnchor,
};
use frona_text::prompt::PromptLoader;

pub(super) fn sources(text: &str) -> Vec<TranscriptEvidenceSource> {
    vec![transcript_source(
        "m1",
        text,
        TranscriptEvidenceKind::AgentMessage {
            message_id: "message-1".into(),
            agent_id: "agent-1".into(),
            chat_id: "chat-1".into(),
        },
    )]
}

fn entity(path: &str, name: &str) -> NewEntity {
    NewEntity {
        id: format!("entity-{name}"),
        path: path.into(),
        name: name.into(),
        description: String::new(),
        aliases: Vec::new(),
        sources: vec![SourceCitation {
            message: "m1".into(),
            quote: name.into(),
            strength: EvidenceStrength::Explicit,
            confirmation: false,
        }],
        candidate_attributes: Vec::new(),
    }
}

fn procedural(playbook: Option<&str>) -> NewMemory {
    NewMemory {
        id: "mem-procedure".into(),
        kind: "procedural".into(),
        sources: vec![SourceCitation {
            message: "m1".into(),
            quote: "postgres runs on 5433".into(),
            strength: EvidenceStrength::Explicit,
            confirmation: false,
        }],
        tool_evidence: Vec::new(),
        episode: None,
        content: "postgres runs on 5433".into(),
        entities: vec!["services/postgres".into()],
        playbook: playbook.map(str::to_string),
    }
}

#[test]
fn grounded_procedural_memory_keeps_exactly_its_referenced_playbook_candidate() {
    let mut batch = Batch {
        new_entities: vec![entity("services/postgres", "Postgres")],
        existing_entity_updates: Vec::new(),
        playbooks: vec![
            NewPlaybookCandidate {
                id: "restart".into(),
                path: "playbooks/restart-postgres".into(),
                name: "Restart Postgres".into(),
                description: "Restart it safely".into(),
            },
            NewPlaybookCandidate {
                id: "unused".into(),
                path: "playbooks/unused".into(),
                name: "Unused".into(),
                description: String::new(),
            },
        ],
        memories: vec![procedural(Some("restart"))],
        research_dispositions: Vec::new(),
    };
    terminal_cleanup(&mut batch, &sources(T));
    assert_eq!(batch.memories.len(), 1);
    assert_eq!(batch.playbooks.len(), 1);
    assert_eq!(batch.playbooks[0].id, "restart");
}

#[test]
fn procedural_memory_with_a_dangling_candidate_is_discarded_with_the_candidate() {
    let mut batch = Batch {
        new_entities: vec![entity("services/postgres", "Postgres")],
        existing_entity_updates: Vec::new(),
        playbooks: Vec::new(),
        memories: vec![procedural(Some("missing"))],
        research_dispositions: Vec::new(),
    };
    terminal_cleanup(&mut batch, &sources(T));
    assert!(batch.memories.is_empty());
    assert!(batch.playbooks.is_empty());
}

#[test]
fn research_coverage_records_supported_and_unsupported_claims_separately() {
    let batch: Batch = serde_json::from_value(serde_json::json!({
            "new_entities": [], "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "id":"mem-accelerator-a", "kind":"Fact", "content":"Accelerator A has 32 GB of memory.",
                "entities":["hardware/accelerator-a"],
                "sources":[{"message":"m1","quote":"Accelerator A has 32 GB","strength":"explicit"}]
            }],
            "research_dispositions": [{
                "message":"m1", "result":"extracted",
                "reason":"The Accelerator A claim was extracted and the Accelerator B claim lacked evidence.",
                "claims":[
                    {
                        "claim":"Accelerator A has 32 GB of memory.", "result":"extracted",
                        "contribution_ids":["mem-accelerator-a"], "reason":"Supported by selected evidence."
                    },
                    {
                        "claim":"Accelerator B has 48 GB of memory.", "result":"unsupported",
                        "contribution_ids":[], "reason":"No supporting execution was available."
                    }
                ]
            }]
        })).unwrap();
    let sources = vec![transcript_source(
        "m1",
        "Accelerator A has 32 GB. Accelerator B has 48 GB.",
        TranscriptEvidenceKind::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat".into(),
        },
    )];
    let research = HashSet::from(["m1".to_string()]);

    assert!(validate_research_coverage(&batch, &sources, &research).is_empty());
    let stats = research_coverage_stats(&batch, &sources, &research);
    assert_eq!(stats.claims, 2);
    assert_eq!(stats.claims_extracted, 1);
    assert_eq!(stats.claims_unsupported, 1);
}

#[test]
fn multi_entity_memory_evidence_cannot_also_be_a_candidate_attribute() {
    let mut batch: Batch = serde_json::from_value(serde_json::json!({
        "new_entities": [],
        "existing_entity_updates": [{
            "path": "people/me",
            "candidate_attributes": [{
                "key": "assistant name",
                "value": "Example Assistant",
                "sources": [{
                    "message": "m4",
                    "quote": "you named me Example Assistant",
                    "strength": "explicit"
                }]
            }]
        }],
        "memories": [{
            "kind": "Fact",
            "content": "Casey Owner named the assistant Example Assistant.",
            "entities": ["people/me", "assistants/example-assistant"],
            "sources": [{
                "message": "m4",
                "quote": "you named me Example Assistant",
                "strength": "explicit"
            }]
        }]
    }))
    .unwrap();

    remove_multi_entity_candidate_attributes(&mut batch);

    assert!(batch.existing_entity_updates.is_empty());
}

fn prompts() -> PromptLoader {
    PromptLoader::new(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("resources")
            .join("prompts"),
    )
}

/// Regression. Template rendering is **strict**: one placeholder the caller does not
/// supply fails the whole render, and the old `unwrap_or_default()` turned that into
/// an empty user prompt. Combined with `tool_choice: Required`, the model was obliged
/// to answer with nothing to answer from - and invented entire sessions, which the
/// pipeline then classified, typed, and assembled.
///
/// This pins the exact variables `extract_and_link` passes. Add a placeholder to the
/// template without wiring it here and this fails, instead of the extractor silently
/// going blind in production.
#[test]
fn ingest_prompt_renders_with_exactly_the_vars_the_caller_supplies() {
    let transcript = "User: my postgres runs on 5433.";
    let rendered = prompts()
        .read_with_vars(
            "pkm/ingest/input.md",
            &[
                ("owner_name", "Casey Owner"),
                ("handle", "testuser"),
                ("self_path", "people/me"),
                ("existing_entities", "- `people/me` — Casey Owner"),
                ("research_messages", "- `m2`"),
                ("transcript", transcript),
            ],
        )
        .expect("the template renders with the caller's variables");

    assert!(
        rendered.contains(transcript),
        "the transcript must survive into the prompt — it is the entire input:\n{rendered}"
    );
    assert!(
        rendered.contains("- `people/me` — Casey Owner"),
        "the existing entity list must survive into the model input:\n{rendered}"
    );
    assert!(!rendered.trim().is_empty());
}

/// The failure mode itself: a missing variable yields `None`, not a usable prompt.
/// `render()` logs and the extract guard refuses to call the model.
#[test]
fn missing_variable_fails_the_render_rather_than_emptying_it() {
    assert!(
        prompts()
            .read_with_vars("pkm/ingest/input.md", &[("owner_name", "Casey Owner")])
            .is_none(),
        "strict rendering must reject an incomplete variable set"
    );
}

#[test]
fn named_weekday_anchor_remains_unresolved() {
    let at = chrono::DateTime::parse_from_rfc3339("2030-01-05T17:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut episode = Episode {
        status: EpisodeStatus::Planned,
        anchor: TemporalAnchor {
            message: "m1".into(),
            quote: "next Sunday".into(),
        },
        duration: None,
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    };
    resolve_episode(
        &mut episode,
        &[TemporalSource {
            handle: "m1".into(),
            text: "Give Buddy medication next Sunday".into(),
            created_at: at,
            task_event_at: None,
            task_target_at: None,
        }],
        "America/Los_Angeles",
    );
    assert_eq!(episode.resolved_start, None);
    assert_eq!(episode.anchor.quote, "next Sunday");
}

#[test]
fn source_specific_anchor_uses_its_declared_message() {
    let at = Utc::now();
    let mut episode = Episode {
        status: EpisodeStatus::Planned,
        anchor: TemporalAnchor {
            message: "m1".into(),
            quote: "next week".into(),
        },
        duration: Some(RelativeDuration {
            direction: crate::memory::pkm::model::TemporalDirection::Future,
            amount: 1,
            unit: crate::memory::pkm::model::TemporalUnit::Week,
            semantics: crate::memory::pkm::model::TemporalSemantics::Calendar,
        }),
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    };
    let sources = vec![
        TemporalSource {
            handle: "m1".into(),
            text: "London next week".into(),
            created_at: at,
            task_event_at: None,
            task_target_at: None,
        },
        TemporalSource {
            handle: "m2".into(),
            text: "Paris next week".into(),
            created_at: at,
            task_event_at: None,
            task_target_at: None,
        },
    ];
    resolve_episode(&mut episode, &sources, "UTC");
    assert!(episode.resolved_start.is_some());
}

#[test]
fn planned_task_episode_resolves_to_its_target_time() {
    let event_at = Utc.timestamp_opt(100, 0).unwrap();
    let target_at = Utc.timestamp_opt(200, 0).unwrap();
    let mut episode = Episode {
        status: EpisodeStatus::Planned,
        anchor: TemporalAnchor {
            message: "m1".into(),
            quote: String::new(),
        },
        duration: None,
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    };

    resolve_episode(
            &mut episode,
            &[TemporalSource {
                handle: "m1".into(),
                text: "[task scheduled event_at=1970-01-01T00:01:40.000Z target_at=1970-01-01T00:03:20.000Z] Review report".into(),
                created_at: event_at,
                task_event_at: Some(event_at),
                task_target_at: Some(target_at),
            }],
            "UTC",
        );

    assert_eq!(episode.resolved_start, Some(target_at));
}

#[test]
fn completed_task_episode_resolves_to_its_event_time() {
    let event_at = Utc.timestamp_opt(300, 0).unwrap();
    let target_at = Utc.timestamp_opt(200, 0).unwrap();
    let mut episode = Episode {
        status: EpisodeStatus::Occurred,
        anchor: TemporalAnchor {
            message: "m1".into(),
            quote: String::new(),
        },
        duration: None,
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    };

    resolve_episode(
            &mut episode,
            &[TemporalSource {
                handle: "m1".into(),
                text: "[task completed event_at=1970-01-01T00:05:00.000Z target_at=1970-01-01T00:03:20.000Z] Review report".into(),
                created_at: event_at,
                task_event_at: Some(event_at),
                task_target_at: Some(target_at),
            }],
            "UTC",
        );

    assert_eq!(episode.resolved_start, Some(event_at));
}
