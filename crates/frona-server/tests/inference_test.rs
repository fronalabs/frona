mod helpers;

use std::sync::Arc;

use frona::core::error::AppError;
use frona::inference::error::InferenceError;
use frona::inference::text_inference;
use frona::inference::tool_loop::{ToolLoopOutcome, run_tool_loop};
use frona::tool::registry::AgentToolRegistry;
use helpers::*;
use rig_core::completion::Message as RigMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_tool_loop_simple_text_response() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Hello!".into(),
    )]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "You are a test assistant",
        vec![RigMessage::user("hi")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { text, .. } => {
            assert!(text.contains("Hello!"));
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    // Allow the dispatcher to process queued events
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let frames = drain_sse_frames(&mut sse_rx).await;
    let saw_text = frames
        .iter()
        .any(|f| f.event == "token" && f.data["content"] == "Hello!");
    assert!(saw_text, "Should emit token event with 'Hello!'");
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn test_tool_loop_single_tool_call() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "call-1".into(),
            "search".into(),
            serde_json::json!({"query": "rust"}),
        )]),
        MockResponse::Text("Found results about Rust.".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "search",
        vec!["search results here".into()],
    )));
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("search for rust")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { text, .. } => {
            assert!(text.contains("Found results about Rust."));
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    assert_eq!(provider.calls(), 2);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let frames = drain_sse_frames(&mut sse_rx).await;
    let saw_tool_call = frames
        .iter()
        .any(|f| f.event == "tool_call" && f.data["name"] == "search");
    let saw_tool_result = frames.iter().any(|f| {
        f.event == "tool_result" && f.data["name"] == "search" && f.data["success"] == true
    });
    assert!(saw_tool_call, "Should emit tool_call event");
    assert!(saw_tool_result, "Should emit tool_result event");
}

#[tokio::test]
async fn test_tool_loop_multi_turn() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "c1".into(),
            "step_one".into(),
            serde_json::json!({}),
        )]),
        MockResponse::ToolCalls(vec![(
            "c2".into(),
            "step_two".into(),
            serde_json::json!({}),
        )]),
        MockResponse::Text("All done.".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "step_one",
        vec!["step one done".into()],
    )));
    tool_registry.register(Arc::new(MockInternalTool::new(
        "step_two",
        vec!["step two done".into()],
    )));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("do steps")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ToolLoopOutcome::Completed { .. }));
    assert_eq!(provider.calls(), 3);
}

#[tokio::test]
async fn test_tool_loop_external_tool_returns_pending() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        (
            "c1".into(),
            "ext_tool".into(),
            serde_json::json!({"action": "run"}),
        ),
    ])]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockExternalTool::new("ext_tool")));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("run external")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "ext_tool");
        }
        other => panic!("Expected ExternalToolPending, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_loop_mixed_internal_external() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "internal".into(), serde_json::json!({})),
        ("c2".into(), "external".into(), serde_json::json!({})),
    ])]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "internal",
        vec!["internal done".into()],
    )));
    tool_registry.register(Arc::new(MockExternalTool::new("external")));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("do both")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "external");
        }
        other => panic!("Expected ExternalToolPending, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_loop_multiple_external_tools_in_same_turn() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "ext1".into(), serde_json::json!({})),
        ("c2".into(), "ext2".into(), serde_json::json!({})),
        ("c3".into(), "ext3".into(), serde_json::json!({})),
    ])]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockExternalTool::new("ext1")));
    tool_registry.register(Arc::new(MockExternalTool::new("ext2")));
    tool_registry.register(Arc::new(MockExternalTool::new("ext3")));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("do all three")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 3);
            assert_eq!(tool_calls[0].name, "ext1");
            assert_eq!(tool_calls[1].name, "ext2");
            assert_eq!(tool_calls[2].name, "ext3");
        }
        other => panic!("Expected ExternalToolPending with 3 tools, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_loop_mixed_internal_and_multiple_external() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "internal".into(), serde_json::json!({})),
        ("c2".into(), "ext1".into(), serde_json::json!({})),
        ("c3".into(), "ext2".into(), serde_json::json!({})),
    ])]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "internal",
        vec!["internal result".into()],
    )));
    tool_registry.register(Arc::new(MockExternalTool::new("ext1")));
    tool_registry.register(Arc::new(MockExternalTool::new("ext2")));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("mixed")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 2);
            assert_eq!(tool_calls[0].name, "ext1");
            assert_eq!(tool_calls[1].name, "ext2");
        }
        other => panic!("Expected ExternalToolPending with 2 external tools, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_loop_cancellation_before_inference() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "should not see this".into(),
    )]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ToolLoopOutcome::Cancelled(_)));
    assert_eq!(provider.calls(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_tool_loop_rate_limit_retry() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::RateLimited {
            retry_after_secs: 5,
        }),
        MockResponse::Text("Success after retry!".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    // SurrealDB schema initialization uses timeout-based retries internally;
    // initialize it with the clock running, then pause again for retry timing.
    tokio::time::resume();
    let chat_service = test_chat_service().await;
    tokio::time::pause();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { text, .. } => {
            assert!(text.contains("Success after retry!"));
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    assert_eq!(provider.calls(), 2);

    // With start_paused, we need to advance time for the dispatcher to run
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let frames = drain_sse_frames(&mut sse_rx).await;
    let saw_retry = frames.iter().any(|f| f.event == "retry");
    assert!(saw_retry, "Should emit retry event");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_tool_loop_rate_limit_exhausted() {
    init_metrics();

    let responses: Vec<MockResponse> = (0..3)
        .map(|_| {
            MockResponse::Error(InferenceError::RateLimited {
                retry_after_secs: 5,
            })
        })
        .collect();
    let provider = Arc::new(MockModelProvider::new(responses));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    // SurrealDB schema initialization uses timeout-based retries internally;
    // initialize it with the clock running, then pause again for retry timing.
    tokio::time::resume();
    let chat_service = test_chat_service().await;
    tokio::time::pause();

    let result = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        AppError::Inference(msg) => {
            assert!(msg.contains("Rate limited"), "Got: {msg}");
        }
        other => panic!("Expected AppError::Inference, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_loop_tool_call_failure() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "c1".into(),
            "bad_tool".into(),
            serde_json::json!({}),
        )]),
        MockResponse::Text("Recovered from tool error.".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockFailingTool::new("bad_tool")));
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("use bad tool")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ToolLoopOutcome::Completed { .. }));
    assert_eq!(provider.calls(), 2);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let frames = drain_sse_frames(&mut sse_rx).await;
    let saw_error_result = frames.iter().any(|f| {
        f.event == "tool_result" && f.data["name"] == "bad_tool" && f.data["success"] == false
    });
    assert!(
        saw_error_result,
        "Should emit tool_result event for bad_tool with success=false"
    );
}

#[tokio::test]
async fn test_tool_loop_provider_error() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("Something broke".into()),
    )]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let result = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        AppError::Inference(msg) => {
            assert!(msg.contains("Something broke"), "Got: {msg}");
        }
        other => panic!("Expected AppError::Inference, got {other:?}"),
    }
}

#[tokio::test]
async fn test_fallback_main_succeeds() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "main success".into(),
    )]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await
    .unwrap();

    assert_eq!(result, "main success");
    assert_eq!(provider.calls(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_fallback_main_fails_fallback_succeeds() {
    init_metrics();

    let main_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("main down".into()),
    )]));
    let fallback_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "fallback success".into(),
    )]));

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "mock".to_string(),
        main_provider.clone() as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    providers.insert(
        "fallback".to_string(),
        fallback_provider.clone() as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    let registry = frona::inference::registry::ModelProviderRegistry::for_testing(
        providers,
        std::collections::HashMap::new(),
    );

    let model_group = test_model_group_with_fallback("fallback", "fallback-model");
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await
    .unwrap();

    assert_eq!(result, "fallback success");
    assert_eq!(main_provider.calls(), 1);
    assert_eq!(fallback_provider.calls(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_fallback_all_fail() {
    init_metrics();

    let main_provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("main err".into())),
        MockResponse::Error(InferenceError::InferenceFailed("main err retry".into())),
    ]));
    let fallback_provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("fallback err".into())),
        MockResponse::Error(InferenceError::InferenceFailed("fallback err retry".into())),
    ]));

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "mock".to_string(),
        main_provider as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    providers.insert(
        "fallback".to_string(),
        fallback_provider as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    let registry = frona::inference::registry::ModelProviderRegistry::for_testing(
        providers,
        std::collections::HashMap::new(),
    );

    let model_group = test_model_group_with_fallback("fallback", "fallback-model");
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        InferenceError::AllFallbacksFailed(errors) => {
            assert_eq!(errors.len(), 2);
        }
        other => panic!("Expected AllFallbacksFailed, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_fallback_retryable_error_retried() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("timeout".into())),
        MockResponse::Text("retry succeeded".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await
    .unwrap();

    assert_eq!(result, "retry succeeded");
    assert_eq!(provider.calls(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_fallback_non_retryable_skips_retry() {
    init_metrics();

    let main_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::ProviderNotConfigured("missing".into()),
    )]));
    let fallback_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "fallback ok".into(),
    )]));

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "mock".to_string(),
        main_provider.clone() as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    providers.insert(
        "fallback".to_string(),
        fallback_provider.clone() as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    let registry = frona::inference::registry::ModelProviderRegistry::for_testing(
        providers,
        std::collections::HashMap::new(),
    );

    let model_group = test_model_group_with_fallback("fallback", "fallback-model");
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await
    .unwrap();

    assert_eq!(result, "fallback ok");
    assert_eq!(main_provider.calls(), 1);
    assert_eq!(fallback_provider.calls(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn test_fallback_multiple_fallbacks_order() {
    init_metrics();

    let main_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("main err".into()),
    )]));
    let fb1_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("fb1 err".into()),
    )]));
    let fb2_provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "fb2 ok".into(),
    )]));

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "mock".to_string(),
        main_provider as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    providers.insert(
        "fb1".to_string(),
        fb1_provider as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    providers.insert(
        "fb2".to_string(),
        fb2_provider as Arc<dyn frona::inference::provider::ModelProvider>,
    );
    let registry = frona::inference::registry::ModelProviderRegistry::for_testing(
        providers,
        std::collections::HashMap::new(),
    );

    let mut model_group = test_model_group();
    model_group.fallbacks = vec![
        frona::inference::ModelRef {
            provider: "fb1".into(),
            model_id: "fb1-model".into(),
        },
        frona::inference::ModelRef {
            provider: "fb2".into(),
            model_id: "fb2-model".into(),
        },
    ];
    let metrics = test_metrics_ctx();

    let result = text_inference(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hi")],
        &metrics,
        &test_usage_ctx(),
    )
    .await
    .unwrap();

    assert_eq!(result, "fb2 ok");
}

/// A mock provider that sends tokens one-by-one with a delay between each,
/// simulating realistic LLM streaming behavior.
struct StreamingMockProvider {
    tokens: Vec<String>,
    delay_between: std::time::Duration,
    call_count: std::sync::Mutex<usize>,
}

impl StreamingMockProvider {
    fn new(tokens: Vec<&str>, delay: std::time::Duration) -> Self {
        Self {
            tokens: tokens.into_iter().map(String::from).collect(),
            delay_between: delay,
            call_count: std::sync::Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl frona::inference::provider::ModelProvider for StreamingMockProvider {
    async fn inference(
        &self,
        _model: &frona::inference::ModelRef,
        _system_prompt: &str,
        _chat_history: Vec<rig_core::completion::Message>,
        _tools: Vec<rig_core::completion::request::ToolDefinition>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<frona::inference::provider::InferenceOutput, InferenceError> {
        unreachable!("streaming test should not call non-streaming inference");
    }

    async fn stream_inference(
        &self,
        _model: &frona::inference::ModelRef,
        _system_prompt: &str,
        _chat_history: Vec<rig_core::completion::Message>,
        _tools: Vec<rig_core::completion::request::ToolDefinition>,
        token_tx: mpsc::Sender<frona::inference::provider::StreamToken>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<frona::inference::provider::InferenceOutput, InferenceError> {
        *self.call_count.lock().unwrap() += 1;
        let mut full_text = String::new();
        for token in &self.tokens {
            full_text.push_str(token);
            let _ = token_tx
                .send(frona::inference::provider::StreamToken::Text(token.clone()))
                .await;
            tokio::time::sleep(self.delay_between).await;
        }
        Ok(frona::inference::provider::InferenceOutput::new(
            vec![rig_core::completion::AssistantContent::text(full_text)],
            frona::inference::Usage::default(),
        ))
    }

    async fn structured_inference(
        &self,
        _model: &frona::inference::ModelRef,
        _system_prompt: &str,
        _chat_history: Vec<rig_core::completion::Message>,
        _schema: serde_json::Value,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        unreachable!("streaming test should not call structured_inference");
    }
}

#[tokio::test]
async fn test_streaming_tokens_arrive_individually() {
    init_metrics();

    let tokens: Vec<&str> = vec![
        "Hello",
        " ",
        "world",
        ",",
        " ",
        "this",
        " ",
        "is",
        " ",
        "a",
        " ",
        "streaming",
        " ",
        "test",
        " ",
        "with",
        " ",
        "many",
        " ",
        "tokens",
    ];
    let provider = Arc::new(StreamingMockProvider::new(
        tokens.clone(),
        std::time::Duration::from_millis(10),
    ));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let handle = tokio::spawn(async move {
        run_tool_loop(
            &registry,
            &model_group,
            "system",
            vec![RigMessage::user("hi")],
            &tool_registry,
            event_sender,
            cancel,
            &ctx,
            &metrics,
            &chat_service,
            "test-msg",
        )
        .await
    });

    // Collect token events with timestamps by polling the SSE receiver.
    // The dispatcher forwards pre-serialized SSE frames; we parse them back.
    let mut received: Vec<(String, std::time::Instant)> = Vec::new();
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline.into(), sse_rx.recv()).await {
            Ok(Some(Ok(event))) => {
                if let Some(frame) = parse_sse_frame(event).await
                    && frame.event == "token"
                    && let Some(content) = frame.data["content"].as_str()
                {
                    received.push((content.to_string(), std::time::Instant::now()));
                }
            }
            Ok(Some(Err(_))) => continue,
            Ok(None) => break, // channel closed
            Err(_) => break,   // timeout
        }
    }

    let _ = handle.await;

    assert!(
        received.len() >= tokens.len(),
        "Expected at least {} text events, got {} — tokens are being batched",
        tokens.len(),
        received.len(),
    );

    // With 10ms delay per token, 20 tokens should take ~200ms.
    // If they all arrive within <50ms, something is batching them.
    let total_elapsed = received.last().unwrap().1.duration_since(start);
    assert!(
        total_elapsed.as_millis() >= 100,
        "All {} tokens arrived in {:?} — likely batched, expected spread over ~200ms",
        received.len(),
        total_elapsed,
    );

    // Check that consecutive tokens have reasonable gaps (not all arriving at once)
    let mut burst_count = 0;
    for window in received.windows(2) {
        let gap = window[1].1.duration_since(window[0].1);
        if gap.as_millis() < 2 {
            burst_count += 1;
        }
    }
    let burst_pct = (burst_count as f64 / (received.len() - 1) as f64) * 100.0;
    println!(
        "Streaming stats: {} tokens, {:.0}% arrived in bursts (<2ms gap), total time: {:?}",
        received.len(),
        burst_pct,
        total_elapsed,
    );

    assert!(
        burst_pct < 30.0,
        "{burst_pct:.0}% of tokens arrived in bursts — channel pipeline is batching"
    );
}

#[tokio::test]
async fn test_tool_loop_reasoning_in_completed_outcome() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::TextWithReasoning(
            "The answer is 42.".into(),
            "Let me think step by step...".into(),
        ),
    ]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("what is the meaning of life?")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed {
            text, reasoning, ..
        } => {
            assert!(text.contains("The answer is 42."));
            let r = reasoning.expect("reasoning should be present");
            assert_eq!(r.content, "Let me think step by step...");
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let frames = drain_sse_frames(&mut sse_rx).await;

    let saw_reasoning = frames
        .iter()
        .any(|f| f.event == "reasoning" && f.data["content"] == "Let me think step by step...");
    assert!(saw_reasoning, "Should emit reasoning SSE event");

    let saw_text = frames
        .iter()
        .any(|f| f.event == "token" && f.data["content"] == "The answer is 42.");
    assert!(saw_text, "Should emit token SSE event");
}

#[tokio::test]
async fn test_tool_loop_reasoning_with_tool_calls() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "c1".into(),
            "search".into(),
            serde_json::json!({"query": "test"}),
        )]),
        MockResponse::TextWithReasoning(
            "Found it.".into(),
            "Based on the search results...".into(),
        ),
    ]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "search",
        vec!["search results".into()],
    )));
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("search for something")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed {
            text, reasoning, ..
        } => {
            assert!(text.contains("Found it."));
            let r = reasoning.expect("reasoning should be present from last turn");
            assert_eq!(r.content, "Based on the search results...");
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let frames = drain_sse_frames(&mut sse_rx).await;

    let saw_reasoning = frames.iter().any(|f| f.event == "reasoning");
    assert!(
        saw_reasoning,
        "Should emit reasoning SSE event after tool call"
    );
}

#[tokio::test]
async fn test_tool_loop_no_reasoning_when_absent() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Plain text.".into(),
    )]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::empty();
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { reasoning, .. } => {
            assert!(reasoning.is_none(), "No reasoning expected for plain text");
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let frames = drain_sse_frames(&mut sse_rx).await;
    let saw_reasoning = frames.iter().any(|f| f.event == "reasoning");
    assert!(
        !saw_reasoning,
        "Should not emit reasoning event for plain text"
    );
}

#[tokio::test]
async fn test_tool_result_sse_includes_summary() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "lookup".into(), serde_json::json!({}))]),
        MockResponse::Text("Done.".into()),
    ]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "lookup",
        vec!["detailed result data here".into()],
    )));
    let (event_sender, mut sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let _outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("lookup something")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let frames = drain_sse_frames(&mut sse_rx).await;

    let tool_result_frame = frames
        .iter()
        .find(|f| f.event == "tool_result" && f.data["name"] == "lookup")
        .expect("Should emit tool_result event");

    assert!(
        tool_result_frame.data.get("summary").is_some(),
        "tool_result SSE should include summary field"
    );
    assert_eq!(
        tool_result_frame.data["summary"].as_str().unwrap(),
        "detailed result data here"
    );
}

#[tokio::test]
async fn test_tool_loop_deduplicates_attachments_before_lifecycle_return() {
    init_metrics();

    let attachment = frona::storage::Attachment {
        filename: "report.md".to_string(),
        content_type: "text/markdown".to_string(),
        size_bytes: 1234,
        owner: "agent:test".to_string(),
        path: "report.md".to_string(),
        url: None,
    };

    // The completion event takes the early lifecycle return that used to skip
    // bottom-of-loop attachment deduplication.
    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "produce_file".into(), serde_json::json!({})),
        ("c2".into(), "complete_task".into(), serde_json::json!({})),
    ])]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let mut tool_registry = AgentToolRegistry::empty();
    tool_registry.register(Arc::new(MockAttachmentTool::new(
        "produce_file",
        attachment.clone(),
    )));
    tool_registry.register(Arc::new(
        MockAttachmentTool::new("complete_task", attachment.clone()).with_task_event(
            frona::inference::tool_call::TaskEvent::Completion {
                task_id: "task-1".into(),
                chat_id: Some("chat-1".into()),
                status: frona::agent::task::models::TaskStatus::Completed,
                summary: Some("done".into()),
                deliverables: vec![attachment],
            },
        ),
    ));
    let (event_sender, _sse_rx, _broadcast) = test_event_sender().await;
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();
    let chat_service = test_chat_service().await;

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("produce and complete")],
        &tool_registry,
        event_sender,
        cancel,
        &ctx,
        &metrics,
        &chat_service,
        "test-msg",
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { attachments, .. } => {
            assert_eq!(
                attachments.len(),
                1,
                "Same attachment from two tools should be deduplicated, got {} attachments",
                attachments.len()
            );
            assert_eq!(attachments[0].path, "report.md");
        }
        other => panic!("Expected Completed, got {other:?}"),
    }
}
