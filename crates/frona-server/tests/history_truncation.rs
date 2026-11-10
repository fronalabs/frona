use frona::inference::streaming::ModelGroupStreaming;
mod helpers;

use std::sync::Arc;

use frona_harness::inference::compaction::summary_message;
use frona_harness::inference::{
    ModelGroup, ModelRequest,
    context::{estimate_message_tokens, estimate_tokens},
    error::InferenceError,
};
use helpers::{
    MockModelProvider, MockResponse, test_metrics_ctx, test_model_group, test_providers,
    test_usage_ctx,
};
use rig_core::completion::message::{CallId, ToolResult, ToolResultContent, UserContent};
use rig_core::completion::{AssistantContent, Message};

const SYSTEM: &str = "fixture system";
const OUTPUT: usize = 10;

fn cost(history: &[Message]) -> usize {
    history.iter().map(estimate_message_tokens).sum()
}

// Provider capture now happens after Rig's wire preparation. Keep canonical
// truncation expectations separate from that provider-facing normalization.
fn native_history(history: &[Message], model: &str, has_tools: bool) -> Vec<Message> {
    use rig_core::{
        operation::Completion,
        test_utils::MockScript,
        wire::{Operation, Wire},
    };
    let mut request = rig_core::completion::CompletionRequest::new("");
    request.chat_history = std::iter::once(Arc::new(Message::system(SYSTEM)))
        .chain(history.iter().cloned().map(Arc::new))
        .collect();
    if has_tools {
        request
            .tools
            .push(Arc::new(rig_core::completion::ToolDefinition {
                name: "read".try_into().unwrap(),
                description: "Read".into(),
                parameters: serde_json::json!({"type":"object"}),
                annotations: Default::default(),
            }));
    }
    let wire = MockScript::new("fixture").with_id(model);
    let prepared = Completion::prepare(request, &wire.describe()).unwrap();
    prepared
        .chat_history
        .into_iter()
        .skip(1)
        .map(Arc::unwrap_or_clone)
        .collect()
}

fn selected_history(history: &[Message], budget: usize) -> Vec<Message> {
    frona_harness::inference::context::truncate_history(
        history.to_vec(),
        SYSTEM,
        budget + estimate_tokens(SYSTEM) + OUTPUT,
        OUTPUT,
        100,
    )
    .unwrap()
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
    let observer = usage.scoped(&context);
    let request = ModelRequest {
        input_token_estimate: None,
        system_prompt: SYSTEM,
        history: history.to_vec(),
        tools: vec![],
        observations: frona_harness::inference::observation::Observations::new(&observer),
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
                UserContent::ToolResult(ToolResult {
                    call: CallId::from_wire(*id),
                    name: "read".try_into().unwrap(),
                    is_error: false,
                    content: vec![ToolResultContent::text(format!("result for {id}"))],
                })
            })
            .collect(),
    }
}

fn calls(ids: &[&str]) -> Message {
    Message::Assistant(rig_core::message::AssistantMessage::new(
        std::iter::once(AssistantContent::text("checking"))
            .chain(ids.iter().map(|id| {
                AssistantContent::tool_call(*id, "read".try_into().unwrap(), serde_json::json!({}))
            }))
            .collect(),
    ))
}

fn mixed_history() -> Vec<Message> {
    vec![
        summary_message("Stable rolling summary", None),
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
                    vec![native_history(&expected, "test-model", structured)],
                    "structured={structured}, budget={budget}"
                );
                assert_eq!(selected_history(&history, budget), expected);
                assert!(cost(&expected) <= budget);
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
                history.insert(0, summary_message("baseline", None));
                expected.insert(0, history[0].clone());
            }
            let provider = provider(vec![]);
            send(&group(provider.clone(), 1000), &history, structured)
                .await
                .unwrap();
            assert_eq!(
                provider.histories(),
                vec![native_history(&expected, "test-model", structured)]
            );
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
                    summary_message("baseline", None),
                    Message::assistant("no live user"),
                    result(&["orphan"]),
                ],
                1000,
            ),
            (
                vec![
                    summary_message(&"large".repeat(100), None),
                    Message::user("small"),
                ],
                50,
            ),
            (
                vec![summary_message("baseline", None)],
                cost(&[summary_message("baseline", None)]) - 1,
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
        for history in [vec![], vec![summary_message("baseline", None)]] {
            let provider = provider(vec![]);
            send(
                &group(provider.clone(), cost(&history).max(4)),
                &history,
                structured,
            )
            .await
            .unwrap();
            assert_eq!(
                provider.histories(),
                vec![native_history(&history, "test-model", structured)]
            );
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
                native_history(&first_expected, "test-model", structured),
                native_history(&first_expected, "test-model", structured),
                native_history(&last_expected, "fallback", structured)
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
        let history = vec![summary_message("baseline", None), Message::user("request")];
        let provider = provider(vec![]);
        let mut group = group(provider.clone(), cost(&history) - 1);
        let mut fallback = group.main.clone();
        fallback.model_id = "fallback".into();
        fallback.request_settings.max_tokens = Some((OUTPUT - 1) as u64);
        group.fallbacks.push(fallback);
        send(&group, &history, structured).await.unwrap();
        assert_eq!(
            provider.histories(),
            vec![native_history(&history, "fallback", structured)]
        );
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
        summary_message("baseline", None),
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
            assert_eq!(
                provider.histories(),
                vec![native_history(&expected, "test-model", structured)]
            );
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
        assert_eq!(
            provider.histories(),
            vec![native_history(&history, "test-model", structured)]
        );
    }
}

#[tokio::test]
async fn streaming_rejects_oversized_input_without_sending_a_truncated_request() {
    let history = mixed_history();
    let provider = Arc::new(MockModelProvider::new(vec![]));
    let group = group(provider.clone(), cost(&history[8..]));
    let (events, _, _) = helpers::test_event_sender().await;
    let error = group
        .stream_inference(
            ModelRequest {
                input_token_estimate: None,
                system_prompt: SYSTEM,
                history,
                tools: vec![],
                observations: frona_harness::inference::observation::Observations::new(
                    &test_metrics_ctx().scoped(&test_usage_ctx()),
                ),
                overrides: Default::default(),
            },
            &events,
            frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default())
                .with_cancellation(&tokio_util::sync::CancellationToken::new()),
            &mut String::new(),
        )
        .await
        .err()
        .expect("oversized complete input must fail");
    assert!(
        error
            .to_string()
            .contains("Complete input exceeds context limit")
    );
    assert!(provider.histories().is_empty());
}

fn active_history() -> Vec<Message> {
    let mut history = mixed_history();
    if let Message::Assistant(assistant) = &mut history[4] {
        let mut origin =
            rig_core::message::Origin::new("anthropic.messages", "anthropic", "fixture");
        origin.response_id = Some("assistant-original".into());
        assistant.origin = Some(origin);
        assistant.content.insert(0, AssistantContent::Reasoning(
            rig_core::message::Reasoning::new("Original reasoning")
        ).with_native(serde_json::json!({"type":"thinking", "thinking":"Original reasoning", "signature":"opaque-signature+/="})));
    }
    *history.last_mut().unwrap() = Message::User {
        content: vec![
            UserContent::text("Original task: analyze the attached report and image"),
            UserContent::image_base64("ZXhhY3QtaW1hZ2U=", None, None),
            UserContent::document_text("Exact report attachment", None),
        ],
    };
    history
}

#[tokio::test]
async fn streaming_tool_result_retries_and_fallback_preserve_the_complete_active_turn() {
    use frona::inference::tool_loop::{ToolLoopOutcome, run_tool_loop};
    use frona::tool::registry::AgentToolRegistry;

    let history = active_history();
    let primary = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "large-call".into(),
            "read".into(),
            serde_json::json!({}),
        )]),
        MockResponse::Error(InferenceError::RateLimited {
            retry_after_secs: 0,
        }),
        MockResponse::Error(InferenceError::InferenceFailed(
            "primary unavailable".into(),
        )),
    ]));
    let fallback = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Complete answer".into(),
    )]));
    let mut group = group(primary.clone(), 20_000);
    Arc::make_mut(&mut group.providers).insert("backup".into(), fallback.clone());
    let mut candidate = group.main.clone();
    candidate.provider_handle = frona::core::Handle::try_new("backup").unwrap().into();
    candidate.model_id = "backup-model".into();
    candidate.request_settings.max_tokens = Some(100);
    group.fallbacks.push(candidate);
    // The obsolete streaming trim would drop the entire post-tool history at
    // this percentage, even though the complete request fits the real window.
    group.inference.history_truncation_pct = 5;
    let large_result = "exact large result ".repeat(500);
    let mut registry = AgentToolRegistry::empty();
    registry.register(Arc::new(helpers::MockInternalTool::new(
        "read",
        vec![large_result.clone()],
    )));
    let (events, _, _) = helpers::test_event_sender().await;
    let outcome = run_tool_loop(
        &group,
        SYSTEM,
        history.clone(),
        &registry,
        events,
        tokio_util::sync::CancellationToken::new(),
        &helpers::mock_context(),
        &test_metrics_ctx(),
        &helpers::test_chat_service_for_turn("active-turn").await,
        "active-turn",
        None,
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, ToolLoopOutcome::Completed { text, .. } if text == "Complete answer")
    );
    let requests = primary.histories();
    assert_eq!(requests.len(), 3);
    let prefix = native_history(&history, "test-model", true);
    assert_eq!(requests[0], prefix);
    assert_eq!(requests[1][..prefix.len()], prefix);
    assert_eq!(requests[1].len(), prefix.len() + 2);
    assert_eq!(history, active_history());
    assert_eq!(requests[1], requests[2]);
    assert_eq!(fallback.histories(), vec![requests[1].clone()]);
    let Message::Assistant(rig_core::message::AssistantMessage { content, .. }) =
        &requests[1][prefix.len()]
    else {
        panic!("tool call must precede its result")
    };
    let AssistantContent::ToolCall(call) = &content[0] else {
        panic!("missing tool call")
    };
    let Message::User { content } = requests[1].last().unwrap() else {
        panic!("missing tool result")
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!("missing tool result")
    };
    assert_eq!(result.call, call.id);
    assert_eq!(result.content, vec![ToolResultContent::text(large_result)]);
}

#[tokio::test]
async fn streaming_smaller_fallback_limits_fail_explicitly_without_losing_history() {
    for local_limit in [false, true] {
        let history = active_history();
        let primary = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
            InferenceError::InferenceFailed("primary unavailable".into()),
        )]));
        let fallback = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
            InferenceError::from(rig_core::error::ProviderError::Provider(
                "400 context limit exceeded".into(),
            )),
        )]));
        let mut group = group(primary.clone(), cost(&history));
        Arc::make_mut(&mut group.providers).insert("backup".into(), fallback.clone());
        let mut candidate = group.main.clone();
        candidate.provider_handle = frona::core::Handle::try_new("backup").unwrap().into();
        candidate.model_id = "small-context-model".into();
        if local_limit {
            candidate.request_settings.max_tokens = Some((OUTPUT + 1) as u64);
        }
        group.fallbacks.push(candidate);
        let (events, _, _) = helpers::test_event_sender().await;
        let error = group
            .stream_inference(
                ModelRequest {
                    input_token_estimate: None,
                    system_prompt: SYSTEM,
                    history: history.clone(),
                    tools: vec![],
                    observations: frona_harness::inference::observation::Observations::new(
                        &test_metrics_ctx().scoped(&test_usage_ctx()),
                    ),
                    overrides: Default::default(),
                },
                &events,
                frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default())
                    .with_cancellation(&tokio_util::sync::CancellationToken::new()),
                &mut String::new(),
            )
            .await
            .err()
            .expect("context limit must surface");
        assert_eq!(
            primary.histories(),
            vec![native_history(&history, "test-model", false)]
        );
        if local_limit {
            assert!(fallback.histories().is_empty());
            assert!(
                error
                    .to_string()
                    .contains("Complete input exceeds context limit")
            );
        } else {
            assert_eq!(
                fallback.histories(),
                vec![native_history(&history, "small-context-model", false)]
            );
            assert!(error.to_string().contains("400 context limit exceeded"));
        }
        assert!(error.to_string().contains("backup/small-context-model"));
    }
}

#[tokio::test]
async fn streaming_in_flight_cancellation_keeps_the_original_request() {
    let history = active_history();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Barrier(
        barrier.clone(),
        Box::new(MockResponse::Pending),
    )]));
    let group = group(provider.clone(), cost(&history));
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel = cancellation.clone();
    let canceller = tokio::spawn(async move {
        barrier.wait().await;
        cancel.cancel();
    });
    let (events, _, _) = helpers::test_event_sender().await;
    let result = group
        .stream_inference(
            ModelRequest {
                input_token_estimate: None,
                system_prompt: SYSTEM,
                history: history.clone(),
                tools: vec![],
                observations: frona_harness::inference::observation::Observations::new(
                    &test_metrics_ctx().scoped(&test_usage_ctx()),
                ),
                overrides: Default::default(),
            },
            &events,
            frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default())
                .with_cancellation(&cancellation),
            &mut String::new(),
        )
        .await
        .unwrap();
    canceller.await.unwrap();
    assert!(matches!(
        result,
        frona::inference::streaming::StreamResult::Cancelled
    ));
    assert_eq!(
        provider.histories(),
        vec![native_history(&history, "test-model", false)]
    );
    assert_eq!(history, active_history());
}

#[tokio::test]
async fn nonstreaming_cannot_use_text_mixed_with_orphan_results_as_an_opening() {
    let mut mixed = result(&["orphan"]);
    if let Message::User { content } = &mut mixed {
        content.insert(0, UserContent::text("Looks like user input"));
    }
    for structured in [false, true] {
        for valid_followup in [false, true] {
            let mut history = vec![summary_message("baseline", None), mixed.clone()];
            if valid_followup {
                history.push(Message::user("valid followup"));
            }
            let provider = provider(vec![]);
            let outcome = send(&group(provider.clone(), 1000), &history, structured).await;
            if valid_followup {
                outcome.unwrap();
                assert_eq!(
                    provider.histories(),
                    vec![native_history(
                        &[history[0].clone(), history[2].clone()],
                        "test-model",
                        structured
                    )]
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
            assert_eq!(
                provider.histories(),
                vec![native_history(&expected, "test-model", structured)]
            );
            assert_eq!(selected_history(&history, cost(&expected)), expected);
            // The next call starts from canonical conversation state, never the
            // provider-specific request view.
            history = expected;
        }
    }
}

#[tokio::test]
async fn canonical_truncation_preserves_signed_reasoning_for_the_same_provider_wire() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let history = active_history()[..8].to_vec();
    let original = history.clone();
    let retained = selected_history(&history, cost(&history));
    assert_eq!(retained, history);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"reply", "type":"message", "role":"assistant", "model":"fixture",
            "content":[{"type":"text","text":"done"}], "stop_reason":"end_turn",
            "usage":{"input_tokens":3,"output_tokens":2}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let model = frona_harness::Model::builder("anthropic", "fixture")
        .api_key("fixture")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = frona_harness::ModelRequest::new("unused");
    request.system = SYSTEM.into();
    request.messages = retained;
    request.max_tokens = Some(100);
    request.tools = vec![rig_core::completion::ToolDefinition {
        name: "read".try_into().unwrap(),
        description: "Read".into(),
        parameters: serde_json::json!({"type":"object"}),
        annotations: Default::default(),
    }];
    model.complete(request).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = requests[0].body_json().unwrap();
    let thinking = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|part| part["type"] == "thinking")
        .expect("same-provider signed reasoning must reach the native wire");
    assert_eq!(
        thinking,
        &serde_json::json!({
            "type":"thinking", "thinking":"Original reasoning", "signature":"opaque-signature+/="
        })
    );
    assert_eq!(
        history, original,
        "native preparation cannot rewrite the archive"
    );
}
