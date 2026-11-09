use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use frona_harness::{
    ActionResolution, ActionStatus, Budget, BudgetCompletion, BudgetLimits, BudgetOperation,
    BudgetOutcome, Capabilities, Conversation, ConversationError, ConversationStatus,
    DefaultBudget, ExecutionControl, ExecutionStop, Model, ModelAdapter, ModelOutput, ModelRequest,
    Tool, ToolContext, ToolDispatchError, ToolError, ToolOutput, ToolRegistry, ToolResult,
    UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool, tool_with_context,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
    request::ToolDefinition,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, JsonSchema)]
struct Args {
    value: usize,
}
fn call(name: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        name,
        ToolFunction::new(name.into(), json!({"value":1})),
    ))
}
fn record(
    conversation: &mut Conversation,
    names: &[&str],
    tools: &ToolRegistry<'_>,
) -> Vec<frona_harness::PendingAction> {
    conversation
        .record_response(
            ModelOutput::new(
                names.iter().map(|name| call(name)).collect(),
                UsageReport::Unavailable,
            ),
            tools.definitions().cloned().collect(),
        )
        .unwrap()
        .actions
}
fn counting(name: &str, calls: Arc<AtomicUsize>) -> impl Tool + use<> {
    tool(name, "count", move |args: Args| {
        let calls = calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"value":args.value}))
        }
    })
    .unwrap()
}

#[tokio::test]
async fn each_tool_checks_aggregate_limits_and_refusal_stops_unstarted_action() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools.register(counting("work", calls.clone())).unwrap();
    let mut conversation = Conversation::new("go");
    let handles = record(&mut conversation, &["work", "work"], &tools);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(1),
        ..Default::default()
    });
    let control = ExecutionControl::new(&budget);
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, control)
            .await
            .unwrap(),
        ActionResolution::Result(_)
    ));
    assert!(matches!(
        conversation
            .execute_tool(&handles[1], &mut tools, control)
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(budget.snapshot().in_flight, 0);
    assert_eq!(conversation.status(), ConversationStatus::Stopped);
    assert_eq!(
        conversation.responses()[0].actions()[1].status(),
        ActionStatus::Proposed
    );
    assert_eq!(conversation.pending_count(), 1);
}

struct LocalModel {
    calls: Arc<AtomicUsize>,
    tokens: u64,
}
#[async_trait]
impl ModelAdapter for LocalModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput::new(
            vec![AssistantContent::text("nested")],
            UsageReport::Reported(Usage {
                total_tokens: self.tokens,
                ..Usage::new()
            }),
        ))
    }
}
fn nested_model(tokens: u64, calls: Arc<AtomicUsize>) -> Model<'static> {
    Model::from_adapter("local", "fake", LocalModel { calls, tokens }).unwrap()
}

#[tokio::test]
async fn nested_model_tokens_block_next_tool_with_remaining_tool_allowance() {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model = nested_model(12, model_calls.clone());
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool_with_context(
                "nested",
                "nested",
                move |_: Args, context: ToolContext<'_>| {
                    let model = model.clone();
                    Box::pin(async move {
                        context.complete(&model, ModelRequest::new("go")).await?;
                        Ok("done")
                    })
                },
            )
            .unwrap(),
        )
        .unwrap();
    tools
        .register(counting("next", tool_calls.clone()))
        .unwrap();
    let mut conversation = Conversation::new("go");
    let handles = record(&mut conversation, &["nested", "next"], &tools);
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(10),
        ..Default::default()
    });
    let control = ExecutionControl::new(&budget);
    conversation
        .execute_tool(&handles[0], &mut tools, control)
        .await
        .unwrap();
    assert!(matches!(
        conversation
            .execute_tool(&handles[1], &mut tools, control)
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tokens, 12, "nested tokens charged once");
    assert_eq!(budget.snapshot().tool_calls, 1);
}

#[tokio::test]
async fn nested_budget_stop_survives_helper_conversion_without_feedback() {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model = nested_model(1, model_calls.clone());
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool_with_context(
                "nested",
                "nested",
                move |_: Args, context: ToolContext<'_>| {
                    let model = model.clone();
                    Box::pin(async move {
                        context
                            .complete(&model, ModelRequest::new("go"))
                            .await
                            .map_err(ToolError::failed)?;
                        Ok("unreachable")
                    })
                },
            )
            .unwrap(),
        )
        .unwrap();
    let mut conversation = Conversation::new("go");
    let handles = record(&mut conversation, &["nested"], &tools);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, ExecutionControl::new(&budget))
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(
        conversation.messages().len(),
        2,
        "no retryable tool feedback for controls"
    );
    assert_eq!(
        conversation.responses()[0].actions()[0].status(),
        ActionStatus::Started
    );
    assert_eq!(conversation.status(), ConversationStatus::Stopped);
}

#[tokio::test]
async fn rejected_unknown_and_unoffered_calls_never_execute_or_consume_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools.register(counting("allowed", calls.clone())).unwrap();
    tools.register(counting("hidden", calls.clone())).unwrap();
    let mut conversation = Conversation::new("go");
    let offered: Vec<_> = tools
        .definitions()
        .filter(|tool| tool.name == "allowed")
        .cloned()
        .chain([ToolDefinition {
            name: "missing".into(),
            description: String::new(),
            parameters: json!({}),
        }])
        .collect();
    let handles = conversation
        .record_response(
            ModelOutput::new(
                vec![call("allowed"), call("hidden"), call("missing")],
                UsageReport::Unavailable,
            ),
            offered,
        )
        .unwrap()
        .actions;
    conversation
        .reject_tool(&handles[0], ToolOutput::text("application declined"))
        .unwrap();
    let budget = DefaultBudget::default();
    for handle in &handles[1..] {
        assert!(matches!(
            conversation
                .execute_tool(handle, &mut tools, ExecutionControl::new(&budget))
                .await
                .unwrap(),
            ActionResolution::Rejected(_)
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 0);
    assert_eq!(conversation.pending_count(), 0);
    assert_eq!(conversation.messages().len(), 3);
}

#[tokio::test]
async fn ordinary_errors_are_paired_feedback_but_typed_storage_and_cancellation_stop() {
    for control_stop in [false, true] {
        let mut tools = ToolRegistry::default();
        tools
            .register(
                tool("work", "work", move |_: Args| async move {
                    Err::<Value, _>(if control_stop {
                        ToolError::Failed(Box::new(ExecutionStop::Storage(
                            "required append failed".into(),
                        )))
                    } else {
                        ToolError::failed("cancelled is ordinary error text")
                    })
                })
                .unwrap(),
            )
            .unwrap();
        let mut conversation = Conversation::new("go");
        let handles = record(&mut conversation, &["work"], &tools);
        let budget = DefaultBudget::default();
        let result = conversation
            .execute_tool(&handles[0], &mut tools, ExecutionControl::new(&budget))
            .await;
        if control_stop {
            assert!(matches!(
                result,
                Err(ToolDispatchError::Stop(ExecutionStop::Storage(_)))
            ));
            assert_eq!(conversation.status(), ConversationStatus::Stopped);
            assert_eq!(conversation.pending_count(), 1);
        } else {
            assert!(matches!(result, Ok(ActionResolution::Failed(_))));
            assert_eq!(conversation.pending_count(), 0);
            assert_eq!(conversation.status(), ConversationStatus::Active);
        }
        assert_eq!(budget.snapshot().in_flight, 0);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools.register(counting("work", calls.clone())).unwrap();
    let mut conversation = Conversation::new("go");
    let handles = record(&mut conversation, &["work"], &tools);
    let budget = DefaultBudget::default();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        conversation
            .execute_tool(
                &handles[0],
                &mut tools,
                ExecutionControl::new(&budget).with_cancellation(&cancellation)
            )
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 0);
}

#[tokio::test]
async fn invalid_handles_and_submission_never_reach_effects_or_admission() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools.register(counting("work", calls.clone())).unwrap();
    let mut conversation = Conversation::new("go");
    let mut foreign = Conversation::new("other");
    let other = record(&mut foreign, &["work"], &tools);
    let handles = record(&mut conversation, &["work", "submit"], &tools);
    let budget = DefaultBudget::default();
    let control = ExecutionControl::new(&budget);
    assert!(matches!(
        conversation
            .execute_tool(&other[0], &mut tools, control)
            .await,
        Err(ToolDispatchError::Conversation(
            ConversationError::ForeignAction
        ))
    ));
    assert!(matches!(
        conversation
            .execute_tool(&handles[1], &mut tools, control)
            .await,
        Err(ToolDispatchError::Submission)
    ));
    conversation.start(&handles[0]).unwrap();
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, control)
            .await,
        Err(ToolDispatchError::Conversation(
            ConversationError::AlreadyStarted
        ))
    ));
    assert!(matches!(
        conversation.reject_tool(&handles[0], ToolOutput::text("late")),
        Err(ToolDispatchError::Conversation(
            ConversationError::AlreadyStarted
        ))
    ));
    conversation
        .resolve(
            &handles[0],
            ActionResolution::Result(ToolOutput::text("external result")),
        )
        .unwrap();
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, control)
            .await,
        Err(ToolDispatchError::Conversation(
            ConversationError::AlreadyResolved
        ))
    ));
    conversation
        .resolve(
            &handles[1],
            ActionResolution::Rejected(ToolOutput::text("repair")),
        )
        .unwrap();
    record(&mut conversation, &[], &tools);
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, control)
            .await,
        Err(ToolDispatchError::Conversation(
            ConversationError::StaleAction
        ))
    ));
    conversation.stop().unwrap();
    assert!(matches!(
        conversation
            .execute_tool(&handles[0], &mut tools, control)
            .await,
        Err(ToolDispatchError::Conversation(ConversationError::Terminal))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 0);
}

#[tokio::test]
async fn borrowed_policy_and_lending_non_sync_tool_keep_metadata_and_raw_arguments() {
    use frona_harness::{BudgetPermit, BudgetRefusal, ToolContent};
    use rig_core::completion::message::{DocumentSourceKind, Image, ImageMediaType};
    use std::cell::Cell;
    struct BorrowedPolicy<'a>(&'a DefaultBudget);
    impl Budget for BorrowedPolicy<'_> {
        fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
            self.0.admit(operation)
        }
    }
    struct Lending<'a> {
        definition: ToolDefinition,
        model: &'a Model<'a>,
        state: &'a mut Vec<String>,
        non_sync: Cell<usize>,
    }
    #[async_trait]
    impl Tool for Lending<'_> {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }
        async fn call(&mut self, arguments: Value, context: &ToolContext<'_>) -> ToolResult {
            assert_eq!(arguments["description"], "keep original");
            context
                .complete(self.model, ModelRequest::new("nested"))
                .await?;
            self.state.push(context.call_id.clone().unwrap());
            self.non_sync.set(1);
            Ok(ToolOutput::content(vec![ToolContent::Image(Image {
                data: DocumentSourceKind::Raw(vec![1, 2]),
                media_type: Some(ImageMediaType::PNG),
                ..Default::default()
            })])
            .with_metadata(json!({"private":"retained"})))
        }
    }
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model = nested_model(3, model_calls.clone());
    let budget = DefaultBudget::default();
    let borrowed = BorrowedPolicy(&budget);
    let mut state = vec![];
    {
        let mut tools = ToolRegistry::default();
        tools
            .register(Lending {
                definition: ToolDefinition {
                    name: "lending".into(),
                    description: String::new(),
                    parameters: json!({}),
                },
                model: &model,
                state: &mut state,
                non_sync: Cell::new(0),
            })
            .unwrap();
        let original = ToolCall::from_dual_wire(
            "item-id",
            "call-id",
            ToolFunction::new("lending".into(), json!({"description":"keep original"})),
        )
        .with_signature(Some("opaque+/=".into()))
        .with_additional_params(Some(json!({"raw":"metadata"})));
        let mut conversation = Conversation::new("go");
        let handles = conversation
            .record_response(
                ModelOutput::new(
                    vec![AssistantContent::ToolCall(original.clone())],
                    UsageReport::Unavailable,
                ),
                tools.definitions().cloned().collect(),
            )
            .unwrap()
            .actions;
        let result = conversation
            .execute_tool(&handles[0], &mut tools, ExecutionControl::new(&borrowed))
            .await
            .unwrap();
        assert_eq!(result.output().metadata, json!({"private":"retained"}));
        assert!(matches!(&result.output().content[0], ToolContent::Image(_)));
        assert_eq!(conversation.responses()[0].actions()[0].call(), &original);
        assert_eq!(
            conversation.responses()[0].actions()[0].resolution(),
            Some(&result)
        );
    }
    assert_eq!(state, ["call-id"]);
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().tokens, 3);
}

#[tokio::test]
async fn admitted_tool_finishes_after_exhaustion_but_cancellation_interrupts_it() {
    for cancel in [false, true] {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut channels = Some((started_tx, release_rx));
        let finished = Arc::new(AtomicUsize::new(0));
        let observed = finished.clone();
        let mut tools = ToolRegistry::default();
        tools
            .register(
                tool("wait", "wait", move |_: Args| {
                    let (started, release) = channels.take().unwrap();
                    let finished = finished.clone();
                    async move {
                        started.send(()).unwrap();
                        release.await.map_err(ToolError::failed)?;
                        finished.fetch_add(1, Ordering::SeqCst);
                        Ok("done")
                    }
                })
                .unwrap(),
            )
            .unwrap();
        let budget = DefaultBudget::new(BudgetLimits {
            tokens: Some(10),
            ..Default::default()
        });
        let token = CancellationToken::new();
        let mut conversation = Conversation::new("go");
        let handles = record(&mut conversation, &["wait"], &tools);
        let (result, ()) = tokio::join!(
            conversation.execute_tool(
                &handles[0],
                &mut tools,
                ExecutionControl::new(&budget).with_cancellation(&token)
            ),
            async {
                started_rx.await.unwrap();
                if cancel {
                    token.cancel();
                } else {
                    budget
                        .admit(BudgetOperation::Model {
                            provider: "other",
                            model: "other",
                        })
                        .unwrap()
                        .complete(BudgetCompletion {
                            outcome: BudgetOutcome::Succeeded,
                            usage: UsageReport::Reported(Usage {
                                total_tokens: 12,
                                ..Usage::new()
                            }),
                        });
                }
                // Cancellation wins over an already-ready tool result as specified.
                let _ = release_tx.send(());
            }
        );
        assert_eq!(budget.snapshot().in_flight, 0);
        if cancel {
            assert!(matches!(
                result,
                Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
            ));
            assert_eq!(observed.load(Ordering::SeqCst), 0);
            assert_eq!(conversation.status(), ConversationStatus::Stopped);
        } else {
            assert!(matches!(result, Ok(ActionResolution::Result(_))));
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            assert_eq!(budget.snapshot().tokens, 12);
        }
    }
}

#[test]
fn model_error_conversions_preserve_recursive_controls_and_default_context_cannot_reset_budget() {
    use frona_harness::ModelError;
    let nested = InferenceError::AllFallbacksFailed(vec![InferenceError::ModelFailed {
        provider: "provider".into(),
        model: "model".into(),
        retry_count: 1,
        source: Box::new(InferenceError::Stop(ExecutionStop::BudgetExhausted(
            "shared".into(),
        ))),
    }]);
    assert!(matches!(
        ToolError::failed(ModelError::Provider(nested)),
        ToolError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert!(matches!(
        ToolContext::default().execution(),
        Err(ToolError::Stop(ExecutionStop::Stopped(_)))
    ));
}

#[tokio::test]
async fn cancellation_from_budget_completion_stops_before_pairing_success() {
    use frona_harness::{BudgetPermit, BudgetRefusal};
    struct CancelOnCompletion<'a> {
        cancellation: &'a CancellationToken,
        completions: AtomicUsize,
    }
    impl Budget for CancelOnCompletion<'_> {
        fn admit(&self, _: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
            Ok(BudgetPermit::new(|completion| {
                assert_eq!(completion.outcome, BudgetOutcome::Succeeded);
                self.completions.fetch_add(1, Ordering::SeqCst);
                self.cancellation.cancel();
            }))
        }
    }
    let token = CancellationToken::new();
    let budget = CancelOnCompletion {
        cancellation: &token,
        completions: AtomicUsize::new(0),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    tools.register(counting("work", calls.clone())).unwrap();
    let mut conversation = Conversation::new("go");
    let handles = record(&mut conversation, &["work"], &tools);
    let result = conversation
        .execute_tool(
            &handles[0],
            &mut tools,
            ExecutionControl::new(&budget).with_cancellation(&token),
        )
        .await;
    assert!(matches!(
        result,
        Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.completions.load(Ordering::SeqCst), 1);
    assert_eq!(conversation.status(), ConversationStatus::Stopped);
    assert_eq!(conversation.pending_count(), 1);
    assert_eq!(
        conversation.messages().len(),
        2,
        "cancelled work must not pair success"
    );
    let action = &conversation.responses()[0].actions()[0];
    assert_eq!(action.status(), ActionStatus::Started);
    assert!(action.resolution().is_none());
}

#[tokio::test]
async fn unsuccessful_rich_feedback_preserves_content_and_metadata() {
    use frona_harness::ToolContent;
    use rig_core::completion::message::{DocumentSourceKind, Image, ImageMediaType};
    let output = ToolOutput::content(vec![
        ToolContent::text("exact unsuccessful feedback"),
        ToolContent::Image(Image {
            data: DocumentSourceKind::Raw(vec![1, 2, 3]),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        }),
    ])
    .with_metadata(json!({"source":"application"}));
    let returned = output.clone();
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool(
                "rich_failure",
                "return unsuccessful content",
                move |_: Args| {
                    let output = returned.clone();
                    async move { Err::<ToolOutput, _>(ToolError::Feedback(output)) }
                },
            )
            .unwrap(),
        )
        .unwrap();
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["rich_failure"], &tools);
    let budget = DefaultBudget::default();
    let resolution = conversation
        .execute_tool(&actions[0], &mut tools, ExecutionControl::new(&budget))
        .await
        .unwrap();
    assert_eq!(resolution, ActionResolution::Failed(output));
    assert_eq!(conversation.status(), ConversationStatus::Active);
}
