mod helpers;

use async_trait::async_trait;
use frona::{
    agent::models::Agent,
    auth::User,
    chat::{message::models::Reasoning, models::CreateChatRequest},
    core::{error::AppError, repository::Repository},
    db::repo::agents::SurrealAgentRepo,
    inference::{
        hitl::{Hitl, HitlRequest, HitlResponse},
        tool_call::{TaskEvent, ToolStatus},
    },
    tool::{
        AgentTool, InferenceContext, ToolDefinition, ToolOutput,
        persistent_dispatch::{PersistentToolDispatcher, PersistentToolOutcome, project_messages},
        registry::AgentToolRegistry,
    },
};
use frona_harness::{
    ActionResolution, ActionStatus, BudgetLimits, Conversation, ConversationStatus, DefaultBudget,
    ExecutionControl, ExecutionStop, ModelOutput, ToolDispatchError, UsageReport,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction, UserContent},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use surrealdb::{Surreal, engine::local::Mem};
use tokio_util::sync::CancellationToken;

// The server publishes ToolCall synchronously after the required begin write.
// Register a cancellation waker with its real event subscription: publication
// cancels before control returns to the dispatcher, without an executor race.
fn cancel_on_next_event(
    ctx: &mut InferenceContext,
    cancellation: &CancellationToken,
) -> tokio::sync::mpsc::UnboundedReceiver<frona::chat::broadcast::BroadcastEvent> {
    struct CancelOnWake(CancellationToken);
    impl std::task::Wake for CancelOnWake {
        fn wake(self: Arc<Self>) {
            self.0.cancel();
        }
    }
    let broadcast = frona::chat::broadcast::BroadcastService::new();
    let mut events = broadcast.subscribe_raw();
    let waker = std::task::Waker::from(Arc::new(CancelOnWake(cancellation.clone())));
    let mut context = std::task::Context::from_waker(&waker);
    assert!(events.poll_recv(&mut context).is_pending());
    ctx.event_tx =
        broadcast.create_event_sender(&ctx.user.id, &ctx.chat.as_ref().unwrap().id, None);
    events
}

async fn setup() -> (helpers::app_state::Fixture, InferenceContext, String) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let fixture = helpers::app_state::build(&db).await;
    let now = chrono::Utc::now();
    let user = User {
        id: "user-1".into(),
        handle: frona::handle!("user-1"),
        email: "u@example.com".into(),
        name: "User".into(),
        password_hash: String::new(),
        timezone: None,
        groups: vec![],
        deactivated_at: None,
        created_at: now,
        updated_at: now,
    };
    fixture.state.user_service.create(&user).await.unwrap();
    let agent = Agent {
        id: "agent-1".into(),
        user_id: user.id.clone(),
        handle: frona::handle!("agent-1"),
        name: "Agent".into(),
        description: String::new(),
        model_group: "primary".into(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: Default::default(),
        prompt: None,
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: None,
        created_at: now,
        updated_at: now,
    };
    SurrealAgentRepo::new(db).create(&agent).await.unwrap();
    let chat = fixture
        .state
        .chat_service
        .create_chat(
            &user.id,
            CreateChatRequest {
                agent_id: agent.id.clone(),
                space_id: None,
                task_id: None,
                title: None,
                metadata: None,
            },
        )
        .await
        .unwrap();
    let chat = fixture
        .state
        .chat_service
        .get_chat(&user.id, &chat.id)
        .await
        .unwrap();
    let message = fixture
        .state
        .chat_service
        .create_executing_agent_message(&chat.id, &agent.id)
        .await
        .unwrap();
    let tx = fixture
        .state
        .broadcast_service
        .create_event_sender(&user.id, &chat.id, None);
    let ctx = InferenceContext::new(
        user,
        agent,
        chat,
        tx,
        CancellationToken::new(),
        CancellationToken::new(),
    );
    (fixture, ctx, message.id)
}

struct Probe {
    calls: Arc<AtomicUsize>,
    kind: &'static str,
}
#[async_trait]
impl AgentTool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: "probe".into(),
            provider_id: String::new(),
            description: "probe".into(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn execute(
        &self,
        _: &str,
        arguments: Value,
        _: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        assert!(arguments.get("description").is_none());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(match self.kind {
            "hitl" => ToolOutput::text("").with_hitl(Hitl {
                prompt: "Region?".into(),
                url: String::new(),
                request: HitlRequest::Question {
                    options: vec!["us".into(), "eu".into()],
                },
                status: ToolStatus::Pending,
                response: None,
                delivery: None,
            }),
            "task" => ToolOutput::text("deferred").with_task_event(TaskEvent::Deferred {
                task_id: "task-1".into(),
                delay_minutes: 5,
                reason: "wait".into(),
            }),
            "callback" => ToolOutput::text("waiting").as_pending_external(),
            kind => {
                let output = if kind == "failure" {
                    ToolOutput::error("exact failure")
                } else {
                    ToolOutput::text("exact success")
                };
                output
                    .with_images(vec![frona_harness::tools::files::image::ImageData {
                        bytes: vec![1, 2, 3],
                        media_type: "image/png".into(),
                    }])
                    .with_attachment(frona::storage::Attachment {
                        filename: "a.png".into(),
                        content_type: "image/png".into(),
                        size_bytes: 3,
                        owner: "user-1".into(),
                        path: "a.png".into(),
                        url: None,
                    })
                    .with_system_prompt("follow-up instructions")
            }
        })
    }
}
fn registry(kind: &'static str) -> (AgentToolRegistry, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = AgentToolRegistry::empty();
    registry.register(Arc::new(Probe {
        calls: calls.clone(),
        kind,
    }));
    (registry, calls)
}
fn record(
    conversation: &mut Conversation,
    names: &[&str],
    registry: &AgentToolRegistry,
) -> Vec<frona_harness::PendingAction> {
    let definitions = registry
        .definitions()
        .iter()
        .map(|d| rig_core::completion::request::ToolDefinition {
            name: rig_core::message::ToolName::new(d.id.clone()).unwrap(),
            description: d.description.clone(),
            parameters: d.parameters.clone(),
            annotations: Default::default(),
        })
        .collect();
    conversation
        .record_response(
            ModelOutput::new(
                names
                    .iter()
                    .map(|name| {
                        AssistantContent::ToolCall(ToolCall::from_wire(
                            "duplicate",
                            ToolFunction::new(
                                (*name).try_into().unwrap(),
                                json!({"description":"Working","value":1}),
                            ),
                        ))
                        .with_native(json!({"id":"item", "thoughtSignature":"signature"}))
                    })
                    .collect(),
                UsageReport::Unavailable,
            ),
            definitions,
        )
        .unwrap()
        .actions
}

#[tokio::test]
async fn rich_success_and_failure_keep_raw_calls_but_project_canonical_requests() {
    for kind in ["success", "failure"] {
        let (fixture, ctx, message) = setup().await;
        let service = &fixture.state.chat_service;
        let (registry, calls) = registry(kind);
        let mut conversation = Conversation::default();
        let actions = record(&mut conversation, &["probe", "probe"], &registry);
        let budget = DefaultBudget::default();
        let mut dispatcher = PersistentToolDispatcher::new(
            service,
            &registry,
            &ctx,
            &message,
            4,
            Some("turn text".into()),
            Some(Reasoning {
                id: None,
                content: "thought".into(),
                signature: Some("sig".into()),
                raw: None,
            }),
        )
        .await
        .unwrap();
        let first = dispatcher
            .execute(
                &mut conversation,
                &actions[0],
                ExecutionControl::new(&budget),
            )
            .await
            .unwrap();
        let second = dispatcher
            .execute(
                &mut conversation,
                &actions[1],
                ExecutionControl::new(&budget),
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_ne!(first.result().record.id, second.result().record.id);
        assert_eq!(
            first.result().record.turn_text.as_deref(),
            Some("turn text")
        );
        assert!(second.result().record.turn_text.is_none());
        assert!(second.result().record.turn_reasoning.is_none());
        assert_eq!(first.result().attachments.len(), 1);
        let output = first.result().resolution.output();
        assert_eq!(output.content.len(), 2);
        assert_eq!(
            matches!(first.result().resolution, ActionResolution::Failed(_)),
            kind == "failure"
        );
        assert_eq!(
            first.result().record.result,
            if kind == "failure" {
                "exact failure"
            } else {
                "exact success"
            }
        );
        let rows = service.get_tool_calls_by_message(&message).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.provider_item_id.as_deref() == Some("item")
                    && r.signature.as_deref() == Some("signature")
                    && r.arguments.get("description").is_none()
                    && r.description.as_deref() == Some("Working"))
        );
        let raw = conversation.messages();
        let Message::Assistant(rig_core::message::AssistantMessage { content, .. }) = &raw[0]
        else {
            panic!()
        };
        let AssistantContent::ToolCall(call) = &content[0] else {
            panic!()
        };
        assert_eq!(call.function.arguments["description"], "Working");
        let projected = project_messages(raw);
        let Message::Assistant(rig_core::message::AssistantMessage { content, .. }) = &projected[0]
        else {
            panic!()
        };
        let AssistantContent::ToolCall(call) = &content[0] else {
            panic!()
        };
        assert!(call.function.arguments.get("description").is_none());
        let Message::User { content } = &projected[1] else {
            panic!()
        };
        assert_eq!(content.len(), 4);
        assert!(matches!(&content[1], UserContent::Image(_)));
        assert!(matches!(&content[3], UserContent::Image(_)));
    }
}

#[tokio::test]
async fn pending_hitl_is_completed_prompt_invocation_and_answer_stays_in_canonical_row() {
    let (fixture, ctx, message) = setup().await;
    let service = &fixture.state.chat_service;
    let (registry, calls) = registry("hitl");
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["probe"], &registry);
    let budget = DefaultBudget::default();
    let mut dispatcher =
        PersistentToolDispatcher::new(service, &registry, &ctx, &message, 0, None, None)
            .await
            .unwrap();
    let outcome = dispatcher
        .execute(
            &mut conversation,
            &actions[0],
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PersistentToolOutcome::Pending(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        conversation.responses()[0].actions()[0].status(),
        ActionStatus::Resolved
    );
    let id = &outcome.result().record.id;
    service
        .resolve_tool_call_with_hitl_response(
            id,
            Some("eu".into()),
            Some(HitlResponse::Choice("eu".into())),
        )
        .await
        .unwrap();
    let answered = service.get_tool_call(id).await.unwrap().unwrap();
    assert_eq!(answered.result, "eu");
    assert_eq!(answered.hitl.unwrap().status, ToolStatus::Resolved);
    // Resume reloads this row. No SDK interrupted-tool recovery is requested.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn task_and_external_callback_are_explicit_host_outcomes() {
    for kind in ["task", "callback"] {
        let (fixture, ctx, message) = setup().await;
        let (registry, _) = registry(kind);
        let mut conversation = Conversation::default();
        let actions = record(&mut conversation, &["probe"], &registry);
        let budget = DefaultBudget::default();
        let mut dispatcher = PersistentToolDispatcher::new(
            &fixture.state.chat_service,
            &registry,
            &ctx,
            &message,
            0,
            None,
            None,
        )
        .await
        .unwrap();
        let outcome = dispatcher
            .execute(
                &mut conversation,
                &actions[0],
                ExecutionControl::new(&budget),
            )
            .await
            .unwrap();
        assert_eq!(
            matches!(outcome, PersistentToolOutcome::Task(_)),
            kind == "task"
        );
        assert_eq!(
            matches!(outcome, PersistentToolOutcome::Pending(_)),
            kind == "callback"
        );
        let row = fixture
            .state
            .chat_service
            .get_tool_call(&outcome.result().record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.task_event.is_some(), kind == "task");
    }
}

#[tokio::test]
async fn rejection_pairs_without_effect_and_stopped_conversation_writes_nothing() {
    let (fixture, ctx, message) = setup().await;
    let (registry, calls) = registry("success");
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["unknown", "probe"], &registry);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    let mut dispatcher = PersistentToolDispatcher::new(
        &fixture.state.chat_service,
        &registry,
        &ctx,
        &message,
        0,
        None,
        None,
    )
    .await
    .unwrap();
    let rejected = dispatcher
        .execute(
            &mut conversation,
            &actions[0],
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap();
    assert!(matches!(
        rejected.result().resolution,
        ActionResolution::Rejected(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    conversation.stop().unwrap();
    assert!(
        dispatcher
            .reject(
                &mut conversation,
                &actions[1],
                ExecutionControl::new(&budget),
                "no".into()
            )
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .state
            .chat_service
            .get_tool_calls_by_message(&message)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn admission_and_ownership_refusal_precede_any_record_or_effect() {
    let (fixture, mut ctx, message) = setup().await;
    let (registry, calls) = registry("success");
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["probe"], &registry);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    let mut dispatcher = PersistentToolDispatcher::new(
        &fixture.state.chat_service,
        &registry,
        &ctx,
        &message,
        0,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        dispatcher
            .execute(
                &mut conversation,
                &actions[0],
                ExecutionControl::new(&budget)
            )
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    drop(dispatcher);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .state
            .chat_service
            .get_tool_calls_by_message(&message)
            .await
            .unwrap()
            .is_empty()
    );
    ctx.agent.id = "other-agent".into();
    assert!(
        PersistentToolDispatcher::new(
            &fixture.state.chat_service,
            &registry,
            &ctx,
            &message,
            0,
            None,
            None
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn failed_required_begin_and_finish_stop_without_feedback_or_later_effects() {
    for event in ["CREATE", "UPDATE"] {
        let (fixture, ctx, message) = setup().await;
        let (registry, calls) = registry("success");
        let mut conversation = Conversation::default();
        let actions = record(&mut conversation, &["probe", "probe"], &registry);
        let budget = DefaultBudget::default();
        let mut dispatcher = PersistentToolDispatcher::new(
            &fixture.state.chat_service,
            &registry,
            &ctx,
            &message,
            0,
            None,
            None,
        )
        .await
        .unwrap();
        fixture.state.db.query(format!("DEFINE EVENT refuse ON TABLE tool_call WHEN $event = '{event}' THEN {{ THROW 'required storage unavailable'; }};")).await.unwrap().check().unwrap();
        let error = dispatcher
            .execute(
                &mut conversation,
                &actions[0],
                ExecutionControl::new(&budget),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolDispatchError::Stop(ExecutionStop::Storage(_))),
            "{error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(event == "UPDATE"));
        assert_eq!(conversation.status(), ConversationStatus::Stopped);
        assert_eq!(
            conversation.responses()[0].actions()[0].status(),
            ActionStatus::Started
        );
        assert!(
            dispatcher
                .execute(
                    &mut conversation,
                    &actions[1],
                    ExecutionControl::new(&budget)
                )
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(event == "UPDATE"));
    }
}

#[tokio::test]
async fn authorization_is_rechecked_at_controlled_invocation_after_registry_creation() {
    let (fixture, ctx, message) = setup().await;
    let calls = Arc::new(AtomicUsize::new(0));
    fixture
        .state
        .tool_manager
        .register_user_tool(
            &ctx.user.id,
            Arc::new(Probe {
                calls: calls.clone(),
                kind: "success",
            }),
        )
        .await;
    fixture.state.policy_service.create_policy(&ctx.user.id, "@id(\"allow-tools\") permit(principal, action == Policy::Action::\"invoke_tool\", resource);").await.unwrap();
    let registry = fixture
        .state
        .tool_manager
        .build_agent_registry(
            &ctx.user.id,
            &ctx.agent,
            &fixture.state.policy_service,
            None,
        )
        .await;
    assert!(registry.definitions().iter().any(|d| d.id == "probe"));
    fixture.state.policy_service.create_policy(&ctx.user.id,"@id(\"deny-now\") forbid(principal, action == Policy::Action::\"invoke_tool\", resource);").await.unwrap();
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["probe"], &registry);
    let budget = DefaultBudget::default();
    let mut dispatcher = PersistentToolDispatcher::new(
        &fixture.state.chat_service,
        &registry,
        &ctx,
        &message,
        0,
        None,
        None,
    )
    .await
    .unwrap();
    let outcome = dispatcher
        .execute(
            &mut conversation,
            &actions[0],
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.result().resolution,
        ActionResolution::Failed(_)
    ));
    assert!(
        outcome
            .result()
            .record
            .result
            .contains("Authorization denied")
    );
    assert!(!outcome.result().record.success);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejection_cancelled_during_required_writes_retains_evidence_and_stops() {
    for fail_finish in [false, true] {
        let (fixture, mut ctx, message) = setup().await;
        let (registry, calls) = registry("success");
        let cancellation = CancellationToken::new();
        let _events = cancel_on_next_event(&mut ctx, &cancellation);
        if fail_finish {
            fixture.state.db.query("DEFINE EVENT refuse ON TABLE tool_call WHEN $event = 'UPDATE' THEN { THROW 'required storage unavailable'; };").await.unwrap().check().unwrap();
        }
        let mut conversation = Conversation::default();
        let actions = record(&mut conversation, &["probe"], &registry);
        // Zero allowance proves rejection performs no tool admission.
        let budget = DefaultBudget::new(BudgetLimits {
            tool_calls: Some(0),
            ..Default::default()
        });
        let mut dispatcher = PersistentToolDispatcher::new(
            &fixture.state.chat_service,
            &registry,
            &ctx,
            &message,
            0,
            None,
            None,
        )
        .await
        .unwrap();
        let error = dispatcher
            .reject(
                &mut conversation,
                &actions[0],
                ExecutionControl::new(&budget).with_cancellation(&cancellation),
                "application rejected".into(),
            )
            .await
            .unwrap_err();
        if fail_finish {
            assert!(
                matches!(error, ToolDispatchError::Stop(ExecutionStop::Storage(_))),
                "{error:?}"
            );
        } else {
            assert!(
                matches!(error, ToolDispatchError::Stop(ExecutionStop::Cancelled(_))),
                "{error:?}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(conversation.status(), ConversationStatus::Stopped);
        assert_eq!(
            conversation.responses()[0].actions()[0].status(),
            ActionStatus::Proposed
        );
        let rows = fixture
            .state
            .chat_service
            .get_tool_calls_by_message(&message)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].result,
            if fail_finish {
                ""
            } else {
                "application rejected"
            }
        );
        assert!(!rows[0].success);
    }
}

#[tokio::test]
async fn stale_and_foreign_proposals_never_write_canonical_rows_or_invoke_tools() {
    use frona_harness::ConversationError;

    for foreign in [false, true] {
        let (fixture, ctx, message) = setup().await;
        let (registry, calls) = registry("success");
        let mut conversation = Conversation::default();
        let action = if foreign {
            let mut other = Conversation::default();
            let action = record(&mut other, &["probe"], &registry).remove(0);
            record(&mut conversation, &["probe"], &registry);
            action
        } else {
            let action = record(&mut conversation, &["probe"], &registry).remove(0);
            conversation
                .resolve(
                    &action,
                    ActionResolution::Result(frona_harness::ToolOutput::text("already resolved")),
                )
                .unwrap();
            record(&mut conversation, &["probe"], &registry);
            action
        };
        let expected = if foreign {
            ConversationError::ForeignAction
        } else {
            ConversationError::StaleAction
        };
        let before = conversation.messages().to_vec();
        let history_len = conversation.history().len();
        let budget = DefaultBudget::default();
        let mut dispatcher = PersistentToolDispatcher::new(
            &fixture.state.chat_service,
            &registry,
            &ctx,
            &message,
            0,
            None,
            None,
        )
        .await
        .unwrap();
        for reject in [false, true] {
            let error = if reject {
                dispatcher
                    .reject(
                        &mut conversation,
                        &action,
                        ExecutionControl::new(&budget),
                        "invalid proposal".into(),
                    )
                    .await
            } else {
                dispatcher
                    .execute(&mut conversation, &action, ExecutionControl::new(&budget))
                    .await
            }
            .unwrap_err();
            assert!(
                matches!(&error, ToolDispatchError::Conversation(actual) if actual == &expected),
                "{error:?}"
            );
            assert_eq!(conversation.status(), ConversationStatus::Active);
            assert_eq!(conversation.messages(), before);
            assert_eq!(conversation.history().len(), history_len);
            assert_eq!(conversation.pending_count(), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(budget.snapshot().tool_calls, 0);
            assert!(
                fixture
                    .state
                    .chat_service
                    .get_tool_calls_by_message(&message)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn cancellation_after_required_begin_preserves_row_without_invoking_tool() {
    let (fixture, mut ctx, message) = setup().await;
    let (registry, calls) = registry("success");
    let cancellation = CancellationToken::new();
    let mut events = cancel_on_next_event(&mut ctx, &cancellation);
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &["probe", "probe"], &registry);
    let budget = DefaultBudget::default();
    let mut dispatcher = PersistentToolDispatcher::new(
        &fixture.state.chat_service,
        &registry,
        &ctx,
        &message,
        0,
        None,
        None,
    )
    .await
    .unwrap();
    let error = dispatcher
        .execute(
            &mut conversation,
            &actions[0],
            ExecutionControl::new(&budget).with_cancellation(&cancellation),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolDispatchError::Stop(ExecutionStop::Cancelled(_))),
        "{error:?}"
    );
    assert!(matches!(
        events.try_recv().unwrap().kind,
        frona::chat::broadcast::BroadcastEventKind::Inference(
            frona::inference::InferenceEventKind::ToolCall { .. }
        )
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(budget.snapshot().in_flight, 0);
    assert_eq!(conversation.status(), ConversationStatus::Stopped);
    let records = conversation.responses()[0].actions();
    assert_eq!(records[0].status(), ActionStatus::Started);
    assert!(records[0].resolution().is_none());
    assert_eq!(records[1].status(), ActionStatus::Proposed);
    assert!(records[1].resolution().is_none());
    assert_eq!(conversation.messages().len(), 1);
    assert!(
        dispatcher
            .execute(
                &mut conversation,
                &actions[1],
                ExecutionControl::new(&budget)
            )
            .await
            .is_err()
    );
    let rows = fixture
        .state
        .chat_service
        .get_tool_calls_by_message(&message)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "probe");
    assert_eq!(rows[0].result, "");
    assert!(!rows[0].success);
    assert!(events.try_recv().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
