mod helpers;

use std::sync::Arc;

use frona::inference::{
    ModelGroup, ModelRequest,
    context::{estimate_message_tokens, estimate_tokens},
    conversation::summary_message,
    error::InferenceError,
};
use helpers::{
    MockModelProvider, MockResponse, test_metrics_ctx, test_model_group, test_providers,
    test_usage_ctx,
};
use rig_core::completion::message::{
    ProviderCallId, ToolCallId, ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, Message};

const SYSTEM: &str = "fixture system";
const OUTPUT: usize = 10;

fn cost(history: &[Message]) -> usize {
    history.iter().map(estimate_message_tokens).sum()
}

fn provider(mut responses: Vec<MockResponse>) -> Arc<MockModelProvider> {
    responses.push(MockResponse::ToolCalls(vec![(
        "submit".into(),
        "submit".into(),
        serde_json::json!({}),
    )]));
    Arc::new(MockModelProvider::new(responses))
}

fn group(provider: Arc<MockModelProvider>, budget: usize) -> ModelGroup {
    let mut group = test_model_group();
    group.providers = test_providers("mock", provider);
    group.context_window = budget + estimate_tokens(SYSTEM) + OUTPUT;
    group.main.request_settings.max_tokens = Some(OUTPUT as u64);
    group.inference.history_truncation_pct = 100;
    group
}

async fn send(
    group: &ModelGroup,
    history: &[Message],
    structured: bool,
) -> Result<(), InferenceError> {
    let usage = test_metrics_ctx();
    let context = test_usage_ctx();
    let request = ModelRequest {
        input_token_estimate: None,
        system_prompt: SYSTEM,
        history: history.to_vec(),
        tools: vec![],
        usage_service: &usage,
        usage_context: &context,
        overrides: Default::default(),
    };
    if structured {
        group
            .structured_inference(request, serde_json::json!({"type":"object"}))
            .await
            .map(|_| ())
    } else {
        group.inference(request).await.map(|_| ())
    }
}

fn result(ids: &[&str]) -> Message {
    Message::User {
        content: ids
            .iter()
            .map(|id| {
                let provider = ProviderCallId::new(*id);
                UserContent::ToolResult(ToolResult {
                    call: ToolCallId::for_provider(provider.as_ref()),
                    provider,
                    name: "read".into(),
                    content: vec![ToolResultContent::text(format!("result for {id}"))],
                })
            })
            .collect(),
    }
}

fn calls(ids: &[&str]) -> Message {
    Message::Assistant {
        id: None,
        content: std::iter::once(AssistantContent::text("checking"))
            .chain(
                ids.iter()
                    .map(|id| AssistantContent::tool_call(*id, "read", serde_json::json!({}))),
            )
            .collect(),
    }
}

fn mixed_history() -> Vec<Message> {
    vec![
        summary_message("Stable rolling summary"),
        Message::user("Old question"),
        Message::assistant("Old answer"),
        Message::user("Look at two files"),
        calls(&["a", "b"]),
        result(&["a"]),
        result(&["b"]),
        Message::assistant("Both files checked"),
        Message::user("Now answer"),
    ]
}

#[tokio::test]
async fn nonstreaming_preserves_summary_and_complete_tool_groups_at_smaller_budgets() {
    let history = mixed_history();
    let original = history.clone();
    for structured in [false, true] {
        // Exact fit; a budget cutting inside the old exchange; a budget cutting
        // inside the multi-call group; and one below summary + newest user.
        for (budget, expected) in [
            (cost(&history), Some(history.clone())),
            (
                cost(&history) - estimate_message_tokens(&history[1]),
                Some([history[..1].to_vec(), history[3..].to_vec()].concat()),
            ),
            (
                cost(&history[..1]) + cost(&history[5..]),
                Some(vec![history[0].clone(), history[8].clone()]),
            ),
            (
                cost(&[history[0].clone(), history[8].clone()]),
                Some(vec![history[0].clone(), history[8].clone()]),
            ),
            (cost(&[history[0].clone(), history[8].clone()]) - 1, None),
        ] {
            let provider = provider(vec![]);
            let outcome = send(&group(provider.clone(), budget), &history, structured).await;
            if let Some(expected) = expected {
                outcome.unwrap();
                assert_eq!(
                    provider.histories(),
                    vec![expected],
                    "structured={structured}, budget={budget}"
                );
                assert!(cost(&provider.last_history()) <= budget);
            } else {
                assert!(outcome.is_err(), "must reject losing the active user input");
                assert_eq!(provider.calls(), 0);
            }
            assert_eq!(history, original);
        }
    }
}

#[tokio::test]
async fn nonstreaming_removes_invalid_openers_even_when_within_budget() {
    for structured in [false, true] {
        for summary in [false, true] {
            let mut history = vec![
                Message::assistant("orphan"),
                result(&["missing"]),
                Message::user("valid request"),
            ];
            let mut expected = vec![history[2].clone()];
            if summary {
                history.insert(0, summary_message("baseline"));
                expected.insert(0, history[0].clone());
            }
            let provider = provider(vec![]);
            send(&group(provider.clone(), 1000), &history, structured)
                .await
                .unwrap();
            assert_eq!(provider.histories(), vec![expected]);
        }
    }
}

#[tokio::test]
async fn nonstreaming_rejects_all_invalid_tails_and_unfit_summaries_without_provider_calls() {
    for structured in [false, true] {
        for (history, budget) in [
            (vec![], 3),
            (vec![Message::assistant("no user")], 1000),
            (vec![result(&["orphan"])], 1000),
            (
                vec![
                    summary_message("baseline"),
                    Message::assistant("no live user"),
                    result(&["orphan"]),
                ],
                1000,
            ),
            (
                vec![
                    summary_message(&"large".repeat(100)),
                    Message::user("small"),
                ],
                50,
            ),
            (
                vec![summary_message("baseline")],
                cost(&[summary_message("baseline")]) - 1,
            ),
        ] {
            let provider = provider(vec![]);
            assert!(
                send(&group(provider.clone(), budget), &history, structured)
                    .await
                    .is_err()
            );
            assert_eq!(provider.calls(), 0);
        }
    }
}

#[tokio::test]
async fn nonstreaming_supports_empty_history_and_an_exactly_fitting_summary() {
    for structured in [false, true] {
        for history in [vec![], vec![summary_message("baseline")]] {
            let provider = provider(vec![]);
            send(
                &group(provider.clone(), cost(&history).max(4)),
                &history,
                structured,
            )
            .await
            .unwrap();
            assert_eq!(provider.histories(), vec![history]);
        }
    }
}

#[tokio::test]
async fn nonstreaming_retries_and_fallbacks_rebuild_from_the_unchanged_source() {
    let history = mixed_history();
    let original = history.clone();
    let first_expected = [history[..1].to_vec(), history[3..].to_vec()].concat();
    let last_expected = vec![history[0].clone(), history[8].clone()];
    let first_budget = cost(&first_expected);
    let last_budget = cost(&last_expected);
    for structured in [false, true] {
        let provider = provider(vec![
            MockResponse::Error(InferenceError::RateLimited {
                retry_after_secs: 0,
            }),
            MockResponse::Error(InferenceError::InferenceFailed("try fallback".into())),
        ]);
        let mut group = group(provider.clone(), first_budget);
        let mut fallback = group.main.clone();
        fallback.model_id = "fallback".into();
        fallback.request_settings.max_tokens = Some((OUTPUT + first_budget - last_budget) as u64);
        group.fallbacks.push(fallback);
        send(&group, &history, structured).await.unwrap();
        assert_eq!(
            provider.histories(),
            vec![
                first_expected.clone(),
                first_expected.clone(),
                last_expected.clone()
            ]
        );
        assert_eq!(
            provider.model_names(),
            vec!["test-model", "test-model", "fallback"]
        );
        assert_eq!(history, original);
    }
}

#[tokio::test]
async fn nonstreaming_local_budget_failure_still_allows_a_larger_fallback_budget() {
    for structured in [false, true] {
        let history = vec![summary_message("baseline"), Message::user("request")];
        let provider = provider(vec![]);
        let mut group = group(provider.clone(), cost(&history) - 1);
        let mut fallback = group.main.clone();
        fallback.model_id = "fallback".into();
        fallback.request_settings.max_tokens = Some((OUTPUT - 1) as u64);
        group.fallbacks.push(fallback);
        send(&group, &history, structured).await.unwrap();
        assert_eq!(provider.histories(), vec![history]);
        assert_eq!(provider.model_names(), vec!["fallback"]);
    }
}

#[tokio::test]
async fn nonstreaming_rejects_incomplete_or_mismatched_retained_tool_groups() {
    for structured in [false, true] {
        for tail in [
            vec![calls(&["a", "a"]), result(&["a"])],
            vec![calls(&["a", "b"]), result(&["a"])],
            vec![calls(&["a"]), result(&["other"])],
            vec![calls(&["a"]), result(&["a", "a"])],
            vec![
                calls(&["a"]),
                Message::assistant("interruption"),
                result(&["a"]),
            ],
        ] {
            let history = [vec![Message::user("request")], tail].concat();
            let provider = provider(vec![]);
            assert!(
                send(&group(provider.clone(), 1000), &history, structured)
                    .await
                    .is_err()
            );
            assert_eq!(provider.calls(), 0);
        }
    }
}

#[tokio::test]
async fn nonstreaming_retains_mixed_tool_results_only_with_their_calls() {
    let mut mixed = result(&["a", "b"]);
    if let Message::User { content } = &mut mixed {
        content.insert(0, UserContent::text("Additional context"));
    }
    let history = vec![
        summary_message("baseline"),
        Message::user("request"),
        calls(&["a", "b"]),
        mixed,
        Message::user("followup"),
    ];
    for structured in [false, true] {
        for (budget, expected) in [
            (cost(&history), history.clone()),
            (
                cost(&history[..1]) + cost(&history[3..]),
                vec![history[0].clone(), history[4].clone()],
            ),
        ] {
            let provider = provider(vec![]);
            send(&group(provider.clone(), budget), &history, structured)
                .await
                .unwrap();
            assert_eq!(provider.histories(), vec![expected]);
        }
    }
}

#[tokio::test]
async fn nonstreaming_cannot_substitute_an_older_small_turn_for_an_oversized_newest_turn() {
    let history = vec![
        Message::user("old"),
        Message::assistant("answer"),
        Message::user("latest".repeat(100)),
    ];
    for structured in [false, true] {
        let provider = provider(vec![]);
        assert!(
            send(
                &group(provider.clone(), cost(&history[..2])),
                &history,
                structured
            )
            .await
            .is_err()
        );
        assert_eq!(provider.calls(), 0);
    }
}

#[tokio::test]
async fn nonstreaming_keeps_a_text_assistant_followup_with_its_user_input() {
    let history = vec![
        Message::user("request"),
        Message::assistant("partial answer"),
    ];
    for structured in [false, true] {
        let provider = provider(vec![]);
        send(
            &group(provider.clone(), cost(&history)),
            &history,
            structured,
        )
        .await
        .unwrap();
        assert_eq!(provider.histories(), vec![history.clone()]);
    }
}

#[tokio::test]
async fn streaming_keeps_its_existing_truncation_policy() {
    let history = mixed_history();
    let budget = cost(&history[8..]);
    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "streamed answer".into(),
    )]));
    let group = group(provider.clone(), budget);
    let (events, _, _) = helpers::test_event_sender().await;
    group
        .stream_inference(
            ModelRequest {
                input_token_estimate: None,
                system_prompt: SYSTEM,
                history: history.clone(),
                tools: vec![],
                usage_service: &test_metrics_ctx(),
                usage_context: &test_usage_ctx(),
                overrides: Default::default(),
            },
            &events,
            &tokio_util::sync::CancellationToken::new(),
            &mut String::new(),
        )
        .await
        .unwrap();
    assert_eq!(provider.histories(), vec![history[8..].to_vec()]);
}

#[tokio::test]
async fn nonstreaming_cannot_use_text_mixed_with_orphan_results_as_an_opening() {
    let mut mixed = result(&["orphan"]);
    if let Message::User { content } = &mut mixed {
        content.insert(0, UserContent::text("Looks like user input"));
    }
    for structured in [false, true] {
        for valid_followup in [false, true] {
            let mut history = vec![summary_message("baseline"), mixed.clone()];
            if valid_followup {
                history.push(Message::user("valid followup"));
            }
            let provider = provider(vec![]);
            let outcome = send(&group(provider.clone(), 1000), &history, structured).await;
            if valid_followup {
                outcome.unwrap();
                assert_eq!(
                    provider.histories(),
                    vec![vec![history[0].clone(), history[2].clone()]]
                );
            } else {
                assert!(outcome.is_err());
                assert_eq!(provider.calls(), 0);
            }
        }
    }
}

#[tokio::test]
async fn repeatedly_truncated_requests_keep_one_summary_and_stable_order() {
    let original = mixed_history();
    for structured in [false, true] {
        let mut history = original.clone();
        for expected in [
            original.clone(),
            [original[..1].to_vec(), original[3..].to_vec()].concat(),
            vec![original[0].clone(), original[8].clone()],
        ] {
            let provider = provider(vec![]);
            send(
                &group(provider.clone(), cost(&expected)),
                &history,
                structured,
            )
            .await
            .unwrap();
            assert_eq!(provider.histories(), vec![expected]);
            history = provider.last_history();
        }
    }
}
