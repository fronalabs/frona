use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use frona_harness::{
    ActionKind, ActionStatus, Budget, BudgetLimit, BudgetLimits, BudgetOperation, BudgetPermit,
    BudgetRefusal, Capabilities, ConversationStatus, DefaultBudget, ExecutionStop, Harness,
    ManualError, Model, ModelAdapter, ModelOutput, ModelRequest, Submission, ToolError,
    UsageReport,
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
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Script {
    outputs: Mutex<VecDeque<Result<ModelOutput, InferenceError>>>,
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
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call")
    }
}
fn model(
    outputs: Vec<Result<ModelOutput, InferenceError>>,
) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Model::from_adapter(
        "local",
        "script",
        Script {
            outputs: Mutex::new(outputs.into()),
            requests: requests.clone(),
        },
    )
    .unwrap();
    (model, requests)
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(
        content,
        UsageReport::Reported(Usage {
            total_tokens: 3,
            ..Usage::new()
        }),
    )
}
fn text(text: &str) -> ModelOutput {
    output(vec![AssistantContent::text(text)])
}
fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), arguments),
    ))
}
fn history(requests: &Arc<Mutex<Vec<ModelRequest>>>) -> Vec<(String, Vec<Message>, Value)> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.system.clone(),
                r.messages.clone(),
                serde_json::to_value(&r.tools).unwrap(),
            )
        })
        .collect()
}
#[derive(Debug, Deserialize, JsonSchema, PartialEq)]
struct Answer {
    value: u32,
}

#[tokio::test]
async fn minimal_runs_are_lazy_independent_and_install_no_application_tools() {
    let (model, requests) = model(vec![Ok(text("first")), Ok(text("second"))]);
    let harness = Harness::new(model);
    let first = harness.run("one");
    let second = harness.run("two");
    assert!(requests.lock().unwrap().is_empty());
    drop(harness);
    assert_eq!(first.await.unwrap(), "first");
    assert_eq!(
        tokio::spawn(async move { second.await })
            .await
            .unwrap()
            .unwrap(),
        "second"
    );
    let requests = requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.tools.is_empty() && r.messages.len() == 1)
    );
}

#[tokio::test]
async fn automatic_and_manual_text_have_identical_requests_pairing_and_admission() {
    fn replies() -> Vec<Result<ModelOutput, InferenceError>> {
        vec![
            Ok(output(vec![
                call("a", "lookup", json!("value")),
                call("b", "unknown", json!({})),
            ])),
            Ok(text("done")),
        ]
    }
    let automatic_budget = DefaultBudget::default();
    let manual_budget = DefaultBudget::default();
    let (automatic, automatic_requests) = model(replies());
    let (manual, manual_requests) = model(replies());
    let run = |model, budget| {
        Harness::new(model)
            .run("input")
            .instructions("instructions")
            .budget(budget)
            .tool(
                tool("lookup", "lookup", |_: String| async {
                    Err::<String, _>(ToolError::failed(std::io::Error::other("ordinary failure")))
                })
                .unwrap(),
            )
    };
    assert_eq!(run(automatic, &automatic_budget).await.unwrap(), "done");
    let mut session = run(manual, &manual_budget).manual();
    for action in session.call_model().await.unwrap().actions {
        session.execute(action).await.unwrap();
    }
    assert!(session.call_model().await.unwrap().actions.is_empty());
    assert_eq!(session.finish().unwrap(), "done");
    assert_eq!(history(&automatic_requests), history(&manual_requests));
    assert_eq!(automatic_budget.snapshot(), manual_budget.snapshot());
    assert_eq!(
        automatic_budget.snapshot().tool_calls,
        1,
        "unoffered tools never run"
    );
}

#[tokio::test]
async fn automatic_and_manual_structured_repair_and_multiple_submissions_match() {
    fn replies() -> Vec<Result<ModelOutput, InferenceError>> {
        vec![
            Ok(text("prose without submission")),
            Ok(output(vec![
                call("bad", "submit", json!({})),
                call("lookup", "lookup", json!("query")),
            ])),
            Ok(output(vec![
                call("chosen", "submit", json!({"value":7})),
                call("extra", "submit", json!({"value":9})),
            ])),
        ]
    }
    let automatic_budget = DefaultBudget::default();
    let manual_budget = DefaultBudget::default();
    let (automatic, automatic_requests) = model(replies());
    let (manual, manual_requests) = model(replies());
    let run = |model, budget| {
        Harness::new(model)
            .run("input")
            .budget(budget)
            .tool(tool("lookup", "lookup", |_: String| async { Ok("found") }).unwrap())
            .structured::<Answer>()
    };
    assert_eq!(
        run(automatic, &automatic_budget).await.unwrap(),
        Answer { value: 7 }
    );
    let mut session = run(manual, &manual_budget).manual();
    assert!(session.call_model().await.unwrap().actions.is_empty());
    session.repair_missing_submission().unwrap();
    let mut turn = session.call_model().await.unwrap().actions.into_iter();
    let bad = turn.next().unwrap();
    session.execute(turn.next().unwrap()).await.unwrap();
    assert!(matches!(
        session.submission(&bad).unwrap(),
        Submission::Repaired { .. }
    ));
    let mut turn = session.call_model().await.unwrap().actions.into_iter();
    let chosen = turn.next().unwrap();
    let Submission::Ready(value) = session.submission(&chosen).unwrap() else {
        panic!("valid submission rejected")
    };
    session.reject(turn.next().unwrap(), "An earlier valid submission was selected. This additional submission was not selected.").unwrap();
    assert_eq!(session.accept(chosen, value).unwrap(), Answer { value: 7 });
    assert_eq!(history(&automatic_requests), history(&manual_requests));
    assert_eq!(automatic_budget.snapshot(), manual_budget.snapshot());
    assert!(
        automatic_requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.tools.iter().any(|t| t.name == "submit"))
    );
    let final_record = session.conversation().responses().last().unwrap();
    assert_eq!(final_record.actions().len(), 2);
    assert_eq!(final_record.actions()[1].status(), ActionStatus::Rejected);
}

#[tokio::test]
async fn unexpected_submit_in_text_mode_is_paired_and_never_accepted() {
    let (model, requests) = model(vec![
        Ok(output(vec![call(
            "unexpected",
            "submit",
            json!({"value":1}),
        )])),
        Ok(text("plain answer")),
    ]);
    assert_eq!(
        Harness::new(model).run("input").await.unwrap(),
        "plain answer"
    );
    let requests = requests.lock().unwrap();
    assert!(requests[0].tools.is_empty());
    let encoded = serde_json::to_value(&requests[1].messages)
        .unwrap()
        .to_string();
    assert!(encoded.contains("Structured submission was not offered"));
    assert!(encoded.contains("unexpected"));
}

#[tokio::test]
async fn default_budget_terminates_unproductive_structured_generation_after_64_calls() {
    let (model, requests) = model((0..64).map(|_| Ok(text("still no submit"))).collect());
    let incomplete = Harness::new(model)
        .run("input")
        .structured::<Answer>()
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(
            BudgetRefusal::LimitReached {
                limit: BudgetLimit::ModelAttempts,
                consumed: 64,
                maximum: 64,
            }
        ))
    ));
    assert_eq!(requests.lock().unwrap().len(), 64);
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    assert_eq!(incomplete.conversation.responses().len(), 64);
}

#[tokio::test]
async fn sibling_tool_exhaustion_never_promotes_a_valid_looking_submission() {
    let response = output(vec![
        call("candidate", "submit", json!({"value":42})),
        call("one", "effect", json!(null)),
        call("two", "effect", json!(null)),
    ]);
    let (automatic, _) = model(vec![Ok(response.clone())]);
    let effects = AtomicUsize::new(0);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(1),
        ..BudgetLimits::default()
    });
    let incomplete = Harness::new(automatic)
        .run("input")
        .budget(&budget)
        .tool(
            tool("effect", "effect", |_: ()| {
                let effects = &effects;
                async move {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap(),
        )
        .structured::<Answer>()
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(incomplete.conversation.pending_count(), 2);
    assert_eq!(
        incomplete.conversation.responses()[0].actions()[0]
            .call()
            .function
            .arguments,
        json!({"value":42})
    );
    assert!(
        incomplete.conversation.responses()[0].actions()[0]
            .resolution()
            .is_none()
    );
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );

    // Compare the final partial state too, not only requests emitted before it.
    let (manual, _) = model(vec![Ok(response)]);
    let manual_budget = DefaultBudget::new(budget.limits());
    let mut session = Harness::new(manual)
        .run("input")
        .budget(&manual_budget)
        .tool(tool("effect", "effect", |_: ()| async { Ok(()) }).unwrap())
        .structured::<Answer>()
        .manual();
    let mut actions = session.call_model().await.unwrap().actions.into_iter();
    let _candidate = actions.next().unwrap();
    session.execute(actions.next().unwrap()).await.unwrap();
    assert!(matches!(
        session.execute(actions.next().unwrap()).await,
        Err(ManualError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(
        session.conversation().messages(),
        incomplete.conversation.messages()
    );
    assert_eq!(manual_budget.snapshot(), budget.snapshot());
    for (manual, automatic) in session.conversation().responses()[0]
        .actions()
        .iter()
        .zip(incomplete.conversation.responses()[0].actions())
    {
        assert_eq!(manual.status(), automatic.status());
        assert_eq!(manual.resolution(), automatic.resolution());
    }
}

struct CancellingBudget {
    inner: DefaultBudget,
    token: CancellationToken,
    tools: bool,
}
impl Budget for CancellingBudget {
    fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
        let permit = self.inner.admit(operation)?;
        let cancel = matches!(operation, BudgetOperation::Tool { .. }) == self.tools;
        Ok(BudgetPermit::new(move |completion| {
            permit.complete(completion);
            if cancel {
                self.token.cancel();
            }
        }))
    }
}
#[tokio::test]
async fn cancellation_at_completion_cannot_produce_text_or_typed_success() {
    for tools in [false, true] {
        let token = CancellationToken::new();
        let budget = CancellingBudget {
            inner: DefaultBudget::default(),
            token: token.clone(),
            tools,
        };
        let response = if tools {
            output(vec![
                call("answer", "submit", json!({"value":1})),
                call("effect", "effect", json!(null)),
            ])
        } else {
            text("looks complete")
        };
        let (model, _) = model(vec![Ok(response)]);
        let run = Harness::new(model)
            .run("input")
            .budget(&budget)
            .cancellation(token);
        let incomplete = if tools {
            run.tool(tool("effect", "effect", |_: ()| async { Ok(()) }).unwrap())
                .structured::<Answer>()
                .await
                .unwrap_err()
        } else {
            run.await.unwrap_err()
        };
        assert!(matches!(
            incomplete.reason,
            ManualError::Stop(ExecutionStop::Cancelled(_))
        ));
        assert_eq!(
            incomplete.conversation.status(),
            ConversationStatus::Stopped
        );
        if tools {
            assert_eq!(
                incomplete.conversation.responses()[0].actions()[0].kind(),
                ActionKind::Submission
            );
            assert!(
                incomplete.conversation.responses()[0].actions()[0]
                    .resolution()
                    .is_none()
            );
        }
    }
    let token = CancellationToken::new();
    token.cancel();
    let (model, requests) = model(vec![]);
    assert!(
        Harness::new(model)
            .run("input")
            .cancellation(token)
            .await
            .is_err()
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn setup_and_provider_failures_are_typed_incomplete_without_fallback_output() {
    let (provider, _) = model(vec![Err(InferenceError::InferenceFailed(
        "provider failed".into(),
    ))]);
    let incomplete = Harness::new(provider).run("input").await.unwrap_err();
    assert!(matches!(incomplete.reason, ManualError::Model(_)));
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    let (model, requests) = model(vec![]);
    let incomplete = Harness::new(model)
        .run("input")
        .tool(tool("same", "first", |_: ()| async { Ok(()) }).unwrap())
        .tool(tool("same", "second", |_: ()| async { Ok(()) }).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(incomplete.reason, ManualError::Setup(_)));
    assert!(requests.lock().unwrap().is_empty());
}
