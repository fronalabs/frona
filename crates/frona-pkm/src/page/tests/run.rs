use super::support::*;
use crate::test_support::{harness, tool_output};
use crate::{
    Error, Phase,
    model::KnowledgeEntity,
    page::{PageAuthor, PageAuthorInput, PageAuthorPrompts, projection::sha256_hex},
};
use frona_harness::{
    BudgetLimits, DefaultBudget, ExecutionStop, ModelOutput, UsageReport, inference::InferenceError,
};
use rig_core::completion::AssistantContent;
use serde_json::json;

fn entity() -> KnowledgeEntity {
    serde_json::from_value(json!({"id":"entity","user_id":"user","path":"services/postgres","origin":"internal","category":"concept",
        "kinds":[],"name":"Postgres","description":"Database service","identity_evidence":[],"attribute_sources":[],"source_memory_ids":[],
        "body":"","related_playbooks":[],"search_text":"Postgres","search_names":[],"search_name_tokens":[],"search_assertions":[],
        "attributes":{},"use_count":0,"aliases":[],"updated_at":"2026-11-11T00:00:00Z","rendered_at":"2026-11-11T00:00:00Z"})).unwrap()
}
fn article(value: &str) -> Result<ModelOutput, InferenceError> {
    Ok(ModelOutput::new(
        vec![AssistantContent::text(value)],
        UsageReport::Unavailable,
    ))
}

#[tokio::test]
async fn returns_canonical_bytes_and_their_exact_revision_without_persisting() {
    let (harness, requests) = harness(vec![article(
        "```markdown\n# Postgres\nUse [[old/path|old name]].\n```",
    )]);
    let phase = PageAuthor::new(harness);
    let mut input = PageAuthorInput::new(entity(), vec![memory("memory-1")]);
    input.redirects.insert("old/path".into(), "new/path".into());
    let out = Phase::run(&phase, input).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(out.body.contains("[[new/path|old name]]"));
    assert!(!out.body.contains("```"));
    assert!(out.content.starts_with("---\nuid: entity\n"));
    assert_eq!(out.rev, sha256_hex(&out.content));
    assert_eq!(out.path, "services/postgres");
}

#[tokio::test]
async fn empty_current_memories_use_deterministic_content_without_model() {
    let (harness, requests) = harness(vec![]);
    let out = PageAuthor::new(harness)
        .run(PageAuthorInput::new(entity(), vec![]))
        .await
        .unwrap();
    assert!(out.body.contains("Database service"));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_no_memory_page_does_not_return_deterministic_success() {
    let (harness, requests) = harness(vec![]);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let result = PageAuthor::new(harness)
        .cancellation(cancel)
        .run(PageAuthorInput::new(entity(), vec![]))
        .await;
    assert!(matches!(result, Err(Error::Cancelled(_))));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn provider_failure_and_empty_output_allow_deterministic_replacement() {
    for response in [
        Err(InferenceError::InferenceFailed("unavailable".into())),
        article("  "),
    ] {
        let (harness, _) = harness(vec![response]);
        let out = PageAuthor::new(harness)
            .run(PageAuthorInput::new(entity(), vec![memory("m1")]))
            .await
            .unwrap();
        assert!(out.body.contains("Postgres runs on 5433."));
    }
}

#[tokio::test]
async fn execution_and_setup_failures_are_not_replaced() {
    for error in [
        InferenceError::Cancelled("stop".into()),
        InferenceError::ConfigError("bad config".into()),
        InferenceError::Stop(ExecutionStop::BudgetExhausted("limit".into())),
    ] {
        let (harness, _) = harness(vec![Err(error)]);
        assert!(
            PageAuthor::new(harness)
                .run(PageAuthorInput::new(entity(), vec![memory("m1")]))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn caller_budget_refusal_cannot_become_a_replacement_page() {
    let (harness, requests) = harness(vec![]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    let result = PageAuthor::new(harness)
        .run_with(PageAuthorInput::new(entity(), vec![memory("m1")]), |run| {
            run.budget(&budget)
        })
        .await;
    assert!(result.is_err());
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn research_tools_read_only_the_supplied_entity_view() {
    let (harness, requests) = harness(vec![
        Ok(tool_output(
            "read_entity",
            json!({"path":"services/postgres"}),
        )),
        article("# Postgres\nKnown facts"),
    ]);
    let out = PageAuthor::new(harness)
        .run(PageAuthorInput::new(entity(), vec![memory("m1")]))
        .await
        .unwrap();
    assert!(out.body.contains("Known facts"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let dialogue = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(dialogue.contains("Postgres runs on 5433."));
    assert!(dialogue.contains("services/postgres"));
}

#[tokio::test]
async fn malformed_prompt_override_fails_before_calling_model() {
    let (harness, requests) = harness(vec![]);
    let phase = PageAuthor::new(harness).prompts(PageAuthorPrompts {
        input: "{{unknown}}".into(),
        ..Default::default()
    });
    assert!(
        phase
            .run(PageAuthorInput::new(entity(), vec![memory("m1")]))
            .await
            .is_err()
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn research_response_cap_is_terminal_instead_of_deterministic_replacement() {
    let responses = (0..20)
        .map(|_| {
            Ok(tool_output(
                "read_entity",
                json!({"path":"services/postgres"}),
            ))
        })
        .collect::<Vec<_>>();
    let (harness, requests) = harness(responses);
    let phase = PageAuthor::new(harness);
    let budget = DefaultBudget::new(BudgetLimits::unlimited());
    let result = phase
        .run_with(PageAuthorInput::new(entity(), vec![memory("m1")]), |run| {
            run.budget(&budget)
        })
        .await;
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 20);
    assert_eq!(budget.snapshot().tool_calls, 20);
}

#[tokio::test]
async fn research_tool_budget_refusal_is_terminal() {
    let (harness, requests) = harness(vec![Ok(tool_output(
        "read_entity",
        json!({"path":"services/postgres"}),
    ))]);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    let result = PageAuthor::new(harness)
        .run_with(PageAuthorInput::new(entity(), vec![memory("m1")]), |run| {
            run.budget(&budget)
        })
        .await;
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn research_read_follows_redirect_chains_and_terminates_cycles() {
    for cyclic in [false, true] {
        let (harness, requests) = harness(vec![
            Ok(tool_output("read_entity", json!({"path":"legacy"}))),
            article("# Postgres\nKnown facts"),
        ]);
        let mut input = PageAuthorInput::new(entity(), vec![memory("m1")]);
        input
            .redirects
            .insert("legacy".into(), "intermediate".into());
        input.redirects.insert(
            "intermediate".into(),
            if cyclic {
                "legacy"
            } else {
                "services/postgres"
            }
            .into(),
        );
        if cyclic {
            let mut misleading = entity();
            misleading.path = "legacy".into();
            misleading.body = "CYCLIC_ENTITY_MUST_NOT_BE_EXPOSED".into();
            input.research_entities.push(misleading);
        }
        PageAuthor::new(harness).run(input).await.unwrap();
        let requests = requests.lock().unwrap();
        let dialogue = serde_json::to_string(&requests[1].messages).unwrap();
        if cyclic {
            assert!(dialogue.contains("entity redirect cycle"));
            assert!(!dialogue.contains("CYCLIC_ENTITY_MUST_NOT_BE_EXPOSED"));
        } else {
            assert!(dialogue.contains("Postgres runs on 5433."));
            assert!(dialogue.contains("path=services/postgres"));
        }
    }
}
