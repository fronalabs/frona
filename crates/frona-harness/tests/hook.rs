#[path = "helpers/native.rs"]
mod native;

use std::{
    cell::Cell,
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use frona_harness::{
    ActionResolution, ActionStatus, Budget, BudgetCompletion, BudgetLimits, BudgetOperation,
    BudgetOutcome, Capabilities, ConversationStatus, Decision, DefaultBudget, ExecutionStop,
    Harness, ManualError, Model, ModelAdapter, ModelOutput, ModelRequest, ToolCallHook,
    ToolDecision, ToolFailure, ToolSelection, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct Script {
    outputs: Mutex<VecDeque<ModelOutput>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

#[async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Capabilities::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request"))
    }
}

fn scripted_model(outputs: Vec<ModelOutput>) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Model::from_adapter(
            "local",
            "hooks",
            Script {
                outputs: Mutex::new(outputs.into()),
                requests: requests.clone(),
            },
        )
        .unwrap(),
        requests,
    )
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Reported(Usage::new().total_tokens(1)))
}
fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.try_into().unwrap(), arguments),
    ))
}
fn text() -> ModelOutput {
    output(vec![AssistantContent::text("done")])
}
fn effect_tool(name: &str, effects: Arc<AtomicUsize>) -> impl frona_harness::Tool {
    tool(name, "Record invocation", move |_: Value| {
        effects.fetch_add(1, Ordering::SeqCst);
        async { Ok("effect") }
    })
    .unwrap()
}
fn histories(requests: &Arc<Mutex<Vec<ModelRequest>>>) -> Vec<Vec<Message>> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.messages.clone())
        .collect()
}

#[tokio::test]
async fn automatic_hook_matches_manual_execute_and_reject_with_one_paired_result() {
    let outputs = vec![
        output(vec![
            call("a", "allow", json!({})),
            call("d", "deny", json!({})),
        ]),
        text(),
    ];
    let (automatic, automatic_requests) = scripted_model(outputs.clone());
    let (manual, manual_requests) = scripted_model(outputs);
    let automatic_effects = Arc::new(AtomicUsize::new(0));
    let manual_effects = Arc::new(AtomicUsize::new(0));
    let auto_budget = DefaultBudget::default();
    let manual_budget = DefaultBudget::default();
    let decision = |call: ToolCall| async move {
        Ok::<_, ()>(if call.function.name == "deny" {
            ToolDecision::Reject("not for this operation".into())
        } else {
            ToolDecision::Execute
        })
    };
    assert_eq!(
        Harness::new(automatic)
            .run("input")
            .tool(effect_tool("allow", automatic_effects.clone()))
            .tool(effect_tool("deny", automatic_effects.clone()))
            .budget(&auto_budget)
            .before_tool(decision)
            .await
            .unwrap(),
        "done"
    );
    let mut session = Harness::new(manual)
        .run("input")
        .tool(effect_tool("allow", manual_effects.clone()))
        .tool(effect_tool("deny", manual_effects.clone()))
        .budget(&manual_budget)
        .manual();
    for action in session.call_model().await.unwrap().actions {
        if session.action(&action).unwrap().call.function.name == "deny" {
            session.reject(action, "not for this operation").unwrap();
        } else {
            session.execute(action).await.unwrap();
        }
    }
    session.call_model().await.unwrap();
    assert_eq!(session.finish().unwrap(), "done");
    assert_eq!(histories(&automatic_requests), histories(&manual_requests));
    assert_eq!(automatic_effects.load(Ordering::SeqCst), 1);
    assert_eq!(manual_effects.load(Ordering::SeqCst), 1);
    assert_eq!(auto_budget.snapshot(), manual_budget.snapshot());
    assert_eq!(auto_budget.snapshot().tool_calls, 1);
}

#[tokio::test]
async fn manual_execute_honors_hook_but_explicit_reject_and_invalid_actions_do_not() {
    let (model, _) = scripted_model(vec![output(vec![
        call("a", "known", json!({})),
        call("b", "known", json!({})),
    ])]);
    let hooks = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let hook_calls = hooks.clone();
    let mut session = Harness::new(model)
        .run("input")
        .tool(effect_tool("known", effects.clone()))
        .before_tool(move |_| {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, ()>(ToolDecision::Execute) }
        })
        .manual();
    let mut actions = session.call_model().await.unwrap().actions;
    let repeated = session.pending_actions().unwrap().actions.remove(0);
    session.execute(actions.remove(0)).await.unwrap();
    assert!(session.execute(repeated).await.is_err());
    session
        .reject(actions.remove(0), "caller rejected")
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unavailable_tools_and_submission_protocol_never_reach_hook() {
    let (model, _) = scripted_model(vec![output(vec![
        call("a", "known", json!({})),
        call("b", "unknown", json!({})),
    ])]);
    let effects = Arc::new(AtomicUsize::new(0));
    let mut session = Harness::new(model)
        .run("input")
        .tool(effect_tool("known", effects.clone()))
        .before_tool(|_| async { Err::<ToolDecision, _>("unoffered tools must not reach policy") })
        .manual();
    let turn = session
        .call_model()
        .application_tools(ToolSelection::None)
        .await
        .unwrap();
    for action in turn.actions {
        assert!(matches!(
            session.execute(action).await.unwrap(),
            ActionResolution::Rejected(_)
        ));
    }
    assert_eq!(effects.load(Ordering::SeqCst), 0);

    let (model, _) = scripted_model(vec![output(vec![call("s", "submit", json!({}))]), text()]);
    assert_eq!(
        Harness::new(model)
            .run("input")
            .before_tool(|_| async {
                Err::<ToolDecision, _>("submission is not an application tool")
            })
            .await
            .unwrap(),
        "done"
    );
}

#[tokio::test]
async fn hook_snapshot_cannot_rewrite_canonical_arguments_or_signatures() {
    let signed = native::call(
        ToolCall::from_wire(
            "a",
            ToolFunction::new(
                "known".try_into().unwrap(),
                json!({"description":"original"}),
            ),
        ),
        json!({"signature":"opaque+/="}),
    );
    let (model, requests) = scripted_model(vec![
        output(vec![AssistantContent::ToolCall(signed.clone())]),
        text(),
    ]);
    let seen = Arc::new(Mutex::new(Value::Null));
    let captured = seen.clone();
    Harness::new(model)
        .run("input")
        .tool(
            tool("known", "Keep arguments", move |args: Value| {
                *captured.lock().unwrap() = args;
                async { Ok("done") }
            })
            .unwrap(),
        )
        .before_tool(|mut snapshot| async move {
            assert_eq!(snapshot.function.arguments["description"], "original");
            assert_eq!(
                snapshot.native.as_ref().unwrap().item["signature"],
                "opaque+/="
            );
            snapshot.function.arguments = json!({"description":"modified"})
                .as_object()
                .unwrap()
                .clone();
            snapshot.native = None;
            assert_eq!(snapshot.function.arguments["description"], "modified");
            assert!(snapshot.native.is_none());
            Ok::<_, ()>(ToolDecision::Execute)
        })
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), json!({"description":"original"}));
    let requests = requests.lock().unwrap();
    let Message::Assistant(rig_core::completion::message::AssistantMessage { content, .. }) =
        &requests[1].messages[1]
    else {
        panic!("missing response")
    };
    assert_eq!(content, &vec![AssistantContent::ToolCall(signed)]);
}

#[derive(Deserialize, JsonSchema)]
struct Answer {
    value: u32,
}

#[tokio::test]
async fn hook_stop_or_operational_failure_preserves_retention_without_feedback_or_later_effects() {
    for fail in [false, true] {
        let (model, requests) = scripted_model(vec![
            output(vec![call("s1", "submit", json!({"value": 4}))]),
            output(vec![
                call("a", "known", json!({})),
                call("b", "known", json!({})),
                call("s2", "submit", json!({"value": 9})),
            ]),
        ]);
        let effects = Arc::new(AtomicUsize::new(0));
        let mut inspections = 0;
        let incomplete = Harness::new(model)
            .run("input")
            .tool(effect_tool("known", effects.clone()))
            .before_tool(move |_| async move {
                tokio::task::yield_now().await;
                if fail {
                    Err("policy service unavailable")
                } else {
                    Ok(ToolDecision::Stop("policy stopped".into()))
                }
            })
            .structured::<Answer>()
            .inspect(async |answer| {
                inspections += 1;
                Ok::<_, ()>(Decision::revise("improve").retain(answer.value))
            })
            .await
            .unwrap_err();
        assert_eq!(incomplete.retained, Some(4));
        if fail {
            assert!(
                matches!(&incomplete.reason, ManualError::ToolHook(message) if message.contains("policy service unavailable"))
            );
        } else {
            assert!(matches!(
                incomplete.reason,
                ManualError::Stop(ExecutionStop::Stopped(_))
            ));
        }
        assert_eq!(
            incomplete.conversation.status(),
            ConversationStatus::Stopped
        );
        assert_eq!(incomplete.conversation.pending_count(), 3);
        assert!(
            incomplete.conversation.responses()[1]
                .actions()
                .iter()
                .all(|a| a.resolution().is_none() && a.status() == ActionStatus::Proposed)
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(inspections, 1);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn admission_rechecks_tokens_consumed_while_async_hook_waits() {
    let (model, _) = scripted_model(vec![output(vec![call("a", "known", json!({}))])]);
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(5),
        ..BudgetLimits::default()
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let started = Notify::new();
    let release = Notify::new();
    let run = Harness::new(model)
        .run("input")
        .budget(&budget)
        .tool(effect_tool("known", effects.clone()))
        .before_tool(|_| async {
            started.notify_one();
            release.notified().await;
            Ok::<_, ()>(ToolDecision::Execute)
        });
    let other_loop = async {
        started.notified().await;
        budget
            .admit(BudgetOperation::Model {
                provider: "local",
                model: "other",
            })
            .unwrap()
            .complete(BudgetCompletion {
                outcome: BudgetOutcome::Succeeded,
                usage: UsageReport::Reported(Usage::new().total_tokens(4)),
            });
        release.notify_one();
    };
    let (result, ()) = tokio::join!(std::future::IntoFuture::into_future(run), other_loop);
    let incomplete = result.unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 0);
    assert_eq!(
        incomplete.conversation.responses()[0].actions()[0].status(),
        ActionStatus::Proposed
    );
}

#[tokio::test]
async fn cancellation_after_hook_decision_prevents_execution_and_rejection_pairing() {
    for reject in [false, true] {
        let (model, _) = scripted_model(vec![output(vec![call("a", "known", json!({}))])]);
        let cancellation = CancellationToken::new();
        let effects = Arc::new(AtomicUsize::new(0));
        let incomplete = Harness::new(model)
            .run("input")
            .cancellation(cancellation.clone())
            .tool(effect_tool("known", effects.clone()))
            .before_tool(|_| async {
                tokio::task::yield_now().await;
                cancellation.cancel();
                Ok::<_, ()>(if reject {
                    ToolDecision::Reject("reject".into())
                } else {
                    ToolDecision::Execute
                })
            })
            .await
            .unwrap_err();
        assert!(matches!(
            incomplete.reason,
            ManualError::Stop(ExecutionStop::Cancelled(_))
        ));
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert!(
            incomplete.conversation.responses()[0].actions()[0]
                .resolution()
                .is_none()
        );
    }
}

#[tokio::test]
async fn cancellation_interrupts_pending_hook_without_effects() {
    let (model, _) = scripted_model(vec![output(vec![call("a", "known", json!({}))])]);
    let cancellation = CancellationToken::new();
    let started = Notify::new();
    let effects = Arc::new(AtomicUsize::new(0));
    let run = Harness::new(model)
        .run("input")
        .cancellation(cancellation.clone())
        .tool(effect_tool("known", effects.clone()))
        .before_tool(|_| async {
            started.notify_one();
            std::future::pending::<()>().await;
            Ok::<_, ()>(ToolDecision::Execute)
        });
    let cancel = async {
        started.notified().await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(std::future::IntoFuture::into_future(run), cancel);
    assert!(matches!(
        result.unwrap_err().reason,
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

struct LendingHook<'a> {
    calls: &'a mut Vec<String>,
    count: Cell<usize>,
}
#[async_trait]
impl ToolCallHook for LendingHook<'_> {
    async fn decide(&mut self, call: &ToolCall) -> Result<ToolDecision, ToolFailure> {
        self.calls.push(call.function.name.to_string());
        tokio::task::yield_now().await;
        self.count.set(self.count.get() + 1);
        Ok(ToolDecision::Execute)
    }
}

#[tokio::test]
async fn lending_non_sync_hook_and_owned_spawned_callback_are_supported() {
    let (model, _) = scripted_model(vec![output(vec![call("a", "known", json!({}))]), text()]);
    let mut calls = Vec::new();
    Harness::new(model)
        .run("input")
        .tool(effect_tool("known", Arc::new(AtomicUsize::new(0))))
        .tool_call_hook(LendingHook {
            calls: &mut calls,
            count: Cell::new(0),
        })
        .await
        .unwrap();
    assert_eq!(calls, ["known"]);

    let (model, _) = scripted_model(vec![output(vec![call("a", "known", json!({}))]), text()]);
    let run = Harness::new(model)
        .run("input")
        .tool(effect_tool("known", Arc::new(AtomicUsize::new(0))))
        .before_tool(|_| async { Ok::<_, ()>(ToolDecision::Execute) });
    assert_eq!(
        tokio::spawn(async move { run.await })
            .await
            .unwrap()
            .unwrap(),
        "done"
    );
}
