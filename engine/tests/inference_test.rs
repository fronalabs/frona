mod helpers;

use std::sync::Arc;

use frona::core::error::AppError;
use frona::inference::error::InferenceError;
use frona::inference::text_inference;
use frona::inference::tool_loop::{run_tool_loop, InferenceEventKind, ToolLoopOutcome};
use frona::tool::registry::AgentToolRegistry;
use helpers::*;
use rig::completion::Message as RigMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// tool_loop tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_tool_loop_simple_text_response() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Hello!".into(),
    )]));
    let registry = test_registry_with_provider("mock", provider.clone());
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "You are a test assistant",
        vec![RigMessage::user("hi")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::Completed { text, .. } => {
            assert!(text.contains("Hello!"));
        }
        other => panic!("Expected Completed, got {other:?}"),
    }

    let mut saw_text = false;
    let mut saw_done = false;
    while let Ok(event) = event_rx.try_recv() {
        match event.kind {
            InferenceEventKind::Text(t) => {
                assert_eq!(t, "Hello!");
                saw_text = true;
            }
            InferenceEventKind::Done(_) => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_text, "Should emit Text event");
    assert!(saw_done, "Should emit Done event");
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
    let mut tool_registry = AgentToolRegistry::new();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "search",
        vec!["search results here".into()],
    )));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("search for rust")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
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

    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    while let Ok(event) = event_rx.try_recv() {
        match event.kind {
            InferenceEventKind::ToolCall { name, .. } => {
                assert_eq!(name, "search");
                saw_tool_call = true;
            }
            InferenceEventKind::ToolResult { name, result } => {
                assert_eq!(name, "search");
                assert_eq!(result, "search results here");
                saw_tool_result = true;
            }
            _ => {}
        }
    }
    assert!(saw_tool_call, "Should emit ToolCall event");
    assert!(saw_tool_result, "Should emit ToolResult event");
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
    let mut tool_registry = AgentToolRegistry::new();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "step_one",
        vec!["step one done".into()],
    )));
    tool_registry.register(Arc::new(MockInternalTool::new(
        "step_two",
        vec!["step two done".into()],
    )));
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("do steps")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
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
    let mut tool_registry = AgentToolRegistry::new();
    tool_registry.register(Arc::new(MockExternalTool::new("ext_tool")));
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("run external")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending { external_tool, .. } => {
            assert_eq!(external_tool.tool_name, "ext_tool");
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
    let mut tool_registry = AgentToolRegistry::new();
    tool_registry.register(Arc::new(MockInternalTool::new(
        "internal",
        vec!["internal done".into()],
    )));
    tool_registry.register(Arc::new(MockExternalTool::new("external")));
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("do both")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
    )
    .await
    .unwrap();

    match outcome {
        ToolLoopOutcome::ExternalToolPending {
            external_tool,
            tool_results,
            ..
        } => {
            assert_eq!(external_tool.tool_name, "external");
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].tool_name, "internal");
        }
        other => panic!("Expected ExternalToolPending, got {other:?}"),
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
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ToolLoopOutcome::Cancelled(_)));
    assert_eq!(provider.calls(), 0);

    let mut saw_cancelled = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.kind, InferenceEventKind::Cancelled(_)) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "Should emit Cancelled event");
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
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
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

    let mut saw_retry = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.kind,
            InferenceEventKind::RateLimitRetry { retry_after_ms: 1 }
        ) {
            saw_retry = true;
        }
    }
    assert!(saw_retry, "Should emit RateLimitRetry event");
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
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let result = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
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
async fn test_tool_loop_tool_execution_failure() {
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
    let mut tool_registry = AgentToolRegistry::new();
    tool_registry.register(Arc::new(MockFailingTool::new("bad_tool")));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let outcome = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("use bad tool")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ToolLoopOutcome::Completed { .. }));
    assert_eq!(provider.calls(), 2);

    let mut saw_error_result = false;
    while let Ok(event) = event_rx.try_recv() {
        if let InferenceEventKind::ToolResult { name, result } = event.kind
            && name == "bad_tool"
        {
            assert!(result.starts_with("Error:"), "Got: {result}");
            saw_error_result = true;
        }
    }
    assert!(saw_error_result, "Should emit tool result with error");
}

#[tokio::test]
async fn test_tool_loop_provider_error() {
    init_metrics();

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("Something broke".into()),
    )]));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let result = run_tool_loop(
        &registry,
        &model_group,
        "system",
        vec![RigMessage::user("hello")],
        &tool_registry,
        event_tx,
        cancel,
        &ctx,
        &metrics,
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

// ---------------------------------------------------------------------------
// fallback tests
// ---------------------------------------------------------------------------

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
    )
    .await
    .unwrap();

    assert_eq!(result, "fallback success");
    assert_eq!(main_provider.calls(), 1); // non-retryable, goes straight to fallback
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
    )
    .await
    .unwrap();

    assert_eq!(result, "fb2 ok");
}

// ---------------------------------------------------------------------------
// streaming timing tests
// ---------------------------------------------------------------------------

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
        _model_id: &str,
        _system_prompt: &str,
        _chat_history: Vec<rig::completion::Message>,
        _tools: Vec<rig::completion::request::ToolDefinition>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<(Vec<rig::completion::AssistantContent>, frona::inference::Usage), InferenceError> {
        unreachable!("streaming test should not call non-streaming inference");
    }

    async fn stream_inference(
        &self,
        _model_id: &str,
        _system_prompt: &str,
        _chat_history: Vec<rig::completion::Message>,
        _tools: Vec<rig::completion::request::ToolDefinition>,
        token_tx: mpsc::Sender<String>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<Vec<rig::completion::AssistantContent>, InferenceError> {
        *self.call_count.lock().unwrap() += 1;
        let mut full_text = String::new();
        for token in &self.tokens {
            full_text.push_str(token);
            let _ = token_tx.send(token.clone()).await;
            tokio::time::sleep(self.delay_between).await;
        }
        Ok(vec![rig::completion::AssistantContent::text(full_text)])
    }
}

#[tokio::test]
async fn test_streaming_tokens_arrive_individually() {
    init_metrics();

    let tokens: Vec<&str> = vec![
        "Hello", " ", "world", ",", " ", "this", " ", "is", " ", "a",
        " ", "streaming", " ", "test", " ", "with", " ", "many", " ", "tokens",
    ];
    let provider = Arc::new(StreamingMockProvider::new(
        tokens.clone(),
        std::time::Duration::from_millis(10),
    ));
    let registry = test_registry_with_provider("mock", provider);
    let model_group = test_model_group();
    let tool_registry = AgentToolRegistry::new();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = mock_context();
    let metrics = test_metrics_ctx();

    let handle = tokio::spawn(async move {
        run_tool_loop(
            &registry,
            &model_group,
            "system",
            vec![RigMessage::user("hi")],
            &tool_registry,
            event_tx,
            cancel,
            &ctx,
            &metrics,
        )
        .await
    });

    // Collect text events with timestamps
    let mut received: Vec<(String, std::time::Instant)> = Vec::new();
    let start = std::time::Instant::now();
    while let Some(event) = event_rx.recv().await {
        match event.kind {
            InferenceEventKind::Text(t) => {
                received.push((t, std::time::Instant::now()));
            }
            InferenceEventKind::Done(_) => break,
            _ => {}
        }
    }

    let _ = handle.await;

    // Verify we got individual tokens, not batched chunks
    assert!(
        received.len() >= tokens.len(),
        "Expected at least {} text events, got {} — tokens are being batched",
        tokens.len(),
        received.len(),
    );

    // Verify tokens arrived spread over time, not in bursts.
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

    // Fail if more than 30% of tokens arrive in bursts
    assert!(
        burst_pct < 30.0,
        "{burst_pct:.0}% of tokens arrived in bursts — channel pipeline is batching"
    );
}

