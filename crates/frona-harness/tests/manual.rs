use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use frona_harness::{
    ActionKind, BudgetLimits, Capabilities, ConversationError, ConversationStatus, DefaultBudget,
    ExecutionStop, Harness, ManualError, Model, ModelAdapter, ModelOutput, ModelRequest,
    Submission, ToolSelection, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction, UserContent},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Fake {
    responses: Arc<Mutex<VecDeque<Result<ModelOutput, InferenceError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    capabilities: Capabilities,
}
#[async_trait]
impl ModelAdapter for Fake {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected invocation")
    }
}
fn output(contents: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(
        contents,
        UsageReport::Reported(Usage {
            total_tokens: 3,
            ..Usage::new()
        }),
    )
}
fn text(value: &str) -> ModelOutput {
    output(vec![AssistantContent::text(value)])
}
fn call(id: &str, name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), args),
    ))
}
fn model(
    provider: &str,
    tools: bool,
    outputs: Vec<Result<ModelOutput, InferenceError>>,
) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Model::from_adapter(
        provider,
        "model",
        Fake {
            requests: requests.clone(),
            responses: Arc::new(Mutex::new(outputs.into())),
            capabilities: Capabilities {
                tools,
                ..Capabilities::default()
            },
        },
    )
    .unwrap();
    (model, requests)
}
#[derive(Deserialize, JsonSchema)]
struct Arguments {
    value: String,
}

#[tokio::test]
async fn two_models_keep_history_budget_and_selective_owned_actions() {
    let (first, first_requests) = model(
        "first",
        true,
        vec![Ok(output(vec![
            call("one", "lookup", json!({"value":"keep"})),
            call("two", "lookup", json!({"value":"reject"})),
        ]))],
    );
    let (second, second_requests) = model("second", true, vec![Ok(text("final"))]);
    let effects = AtomicUsize::new(0);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(2),
        ..BudgetLimits::default()
    });
    let harness = Harness::new(first);
    let mut session = harness
        .run("question")
        .instructions("system")
        .budget(&budget)
        .tool(
            tool("lookup", "lookup", |a: Arguments| {
                let effects = &effects;
                async move {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"found":a.value}))
                }
            })
            .unwrap(),
        )
        .manual();
    let turn = session.call_model().await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(turn.actions.len(), 2);
    for action in &turn.actions {
        assert_eq!(session.action(action).unwrap().kind, ActionKind::Tool);
    }
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Conversation(
            ConversationError::PendingActions(2)
        ))
    ));
    assert_eq!(first_requests.lock().unwrap().len(), 1);
    let mut actions = turn.actions.into_iter();
    session.execute(actions.next().unwrap()).await.unwrap();
    session
        .reject(actions.next().unwrap(), "unavailable")
        .unwrap();
    session.call_model().model(&second).await.unwrap();
    assert_eq!(session.finish().unwrap(), "final");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().model_attempts, 2);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(budget.snapshot().tokens, 6);
    let requests = second_requests.lock().unwrap();
    assert_eq!(requests[0].system, "system");
    assert_eq!(requests[0].messages.len(), 3);
    let Message::User { content } = &requests[0].messages[2] else {
        panic!("missing paired batch")
    };
    assert_eq!(content.len(), 2);
    assert!(matches!(&content[0], UserContent::ToolResult(r) if r.call.as_str()=="one"));
    assert!(matches!(&content[1], UserContent::ToolResult(r) if r.call.as_str()=="two"));
}

#[tokio::test]
async fn override_is_per_call_and_budget_refusal_does_not_reset_history() {
    let (first, first_requests) = model("first", false, vec![Ok(text("first"))]);
    let (second, second_requests) = model("second", false, vec![Ok(text("second"))]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(2),
        ..BudgetLimits::default()
    });
    let harness = Harness::new(first);
    let mut session = harness.run("input").budget(budget.clone()).manual();
    session.call_model().model(&second).await.unwrap();
    session.call_model().await.unwrap();
    let history = session.conversation().messages().to_vec();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(session.conversation().status(), ConversationStatus::Stopped);
    assert_eq!(session.conversation().messages(), history);
    assert_eq!(first_requests.lock().unwrap().len(), 1);
    assert_eq!(second_requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn incompatible_switch_fails_before_admission_and_original_model_still_works() {
    let signed = ToolCall::from_wire(
        "signed",
        ToolFunction::new("lookup".into(), json!({"value":"a"})),
    )
    .with_signature(Some("opaque".into()));
    let (first, first_requests) = model(
        "first",
        true,
        vec![
            Ok(output(vec![AssistantContent::ToolCall(signed)])),
            Ok(text("continued")),
        ],
    );
    let (other, other_requests) = model("other", true, vec![]);
    let (no_tools, no_tool_requests) = model("no-tools", false, vec![]);
    let budget = DefaultBudget::default();
    let harness = Harness::new(first);
    let mut session = harness.run("input").budget(&budget).manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    session.reject(action, "not available").unwrap();
    let original = session.conversation().messages().to_vec();
    for target in [&other, &no_tools] {
        assert!(matches!(
            session.call_model().model(target).await,
            Err(ManualError::Incompatible { .. })
        ));
        assert_eq!(session.conversation().messages(), original);
        assert_eq!(session.conversation().status(), ConversationStatus::Active);
        assert_eq!(budget.snapshot().model_attempts, 1);
    }
    session.call_model().await.unwrap();
    assert_eq!(session.finish().unwrap(), "continued");
    assert_eq!(first_requests.lock().unwrap().len(), 2);
    assert!(other_requests.lock().unwrap().is_empty());
    assert!(no_tool_requests.lock().unwrap().is_empty());
}

#[derive(Deserialize, JsonSchema)]
struct Answer {
    value: String,
}

#[tokio::test]
async fn typed_manual_repairs_and_recovers_consumed_handle_after_premature_acceptance() {
    let (model, requests) = model(
        "structured",
        true,
        vec![
            Ok(text("forgot to submit")),
            Ok(output(vec![call("bad", "submit", json!({"value":7}))])),
            Ok(output(vec![
                call("good", "submit", json!({"value":"original"})),
                call("sibling", "lookup", json!({"value":"x"})),
            ])),
        ],
    );
    let harness = Harness::new(model);
    let mut session = harness.run("input").structured::<Answer>().manual();
    session.call_model().await.unwrap();
    session.repair_missing_submission().unwrap();
    let bad = session.call_model().await.unwrap().actions.pop().unwrap();
    assert!(matches!(
        session.submission(&bad).unwrap(),
        Submission::Repaired { .. }
    ));
    let mut actions = session
        .call_model()
        .application_tools(ToolSelection::None)
        .await
        .unwrap()
        .actions
        .into_iter();
    let good = actions.next().unwrap();
    let sibling = actions.next().unwrap();
    let Submission::Ready(value) = session.submission(&good).unwrap() else {
        panic!("expected ready")
    };
    assert!(matches!(
        session.accept(good, value.value.clone()),
        Err(ManualError::Submission(_))
    ));
    let recovered = session
        .pending_actions()
        .unwrap()
        .actions
        .into_iter()
        .find(|handle| session.action(handle).unwrap().kind == ActionKind::Submission)
        .unwrap();
    session.reject(sibling, "unoffered").unwrap();
    assert_eq!(
        session
            .accept(recovered, value.value.to_uppercase())
            .unwrap(),
        "ORIGINAL"
    );
    assert_eq!(
        session.conversation().responses()[2].output().content[0],
        call("good", "submit", json!({"value":"original"}))
    );
    assert_eq!(requests.lock().unwrap()[2].tools.len(), 1);
    assert_eq!(requests.lock().unwrap()[2].tools[0].name, "submit");
    assert!(session.pending_actions().is_err());
}

#[tokio::test]
async fn reissued_handles_and_wrong_kind_cannot_repeat_or_strand_effects() {
    let (model, _) = model(
        "structured",
        true,
        vec![Ok(output(vec![
            call("submit", "submit", json!({"value":"a"})),
            call("lookup", "lookup", json!({"value":"x"})),
        ]))],
    );
    let effects = Arc::new(AtomicUsize::new(0));
    let captured = effects.clone();
    let harness = Harness::new(model);
    let mut session = harness
        .run("input")
        .structured::<Answer>()
        .tool(
            tool("lookup", "lookup", move |_: Arguments| {
                let effects = captured.clone();
                async move {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok("done")
                }
            })
            .unwrap(),
        )
        .manual();
    let mut actions = session.call_model().await.unwrap().actions.into_iter();
    assert!(matches!(
        session.execute(actions.next().unwrap()).await,
        Err(ManualError::Dispatch(_))
    ));
    let duplicate = session.pending_actions().unwrap().actions.pop().unwrap();
    session.execute(actions.next().unwrap()).await.unwrap();
    assert!(session.execute(duplicate).await.is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let submit = session.pending_actions().unwrap().actions.pop().unwrap();
    let Submission::Ready(answer) = session.submission(&submit).unwrap() else {
        panic!()
    };
    assert_eq!(session.accept(submit, answer.value).unwrap(), "a");
}

#[tokio::test]
async fn setup_cancellation_and_provider_failures_are_terminal_before_success() {
    let (model, requests) = model(
        "failure",
        false,
        vec![Err(InferenceError::InferenceFailed(
            "provider failed".into(),
        ))],
    );
    let harness = Harness::new(model);
    let mut failed = harness.run("input").manual();
    assert!(matches!(
        failed.call_model().await,
        Err(ManualError::Model(_))
    ));
    assert_eq!(failed.conversation().status(), ConversationStatus::Stopped);
    assert!(failed.finish().is_err());
    let cancellation = CancellationToken::new();
    let mut cancelled = harness
        .run("input")
        .cancellation(cancellation.clone())
        .manual();
    cancellation.cancel();
    assert!(matches!(
        cancelled.call_model().await,
        Err(ManualError::Stop(ExecutionStop::Cancelled(_)))
    ));
    let mut invalid = harness
        .run("input")
        .tool(tool("same", "one", |_: Arguments| async { Ok("one") }).unwrap())
        .tool(tool("same", "two", |_: Arguments| async { Ok("two") }).unwrap())
        .manual();
    assert!(matches!(
        invalid.call_model().await,
        Err(ManualError::Setup(_))
    ));
    assert_eq!(invalid.conversation().status(), ConversationStatus::Stopped);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn independent_runs_and_owned_session_are_spawnable() {
    let (model, _) = model("owned", false, vec![Ok(text("one")), Ok(text("two"))]);
    let harness = Harness::new(model);
    let mut first = harness.run("one").manual();
    let mut second = harness.run("two").manual();
    drop(harness);
    tokio::spawn(async move {
        first.call_model().await.unwrap();
        second.call_model().await.unwrap();
        assert_eq!(first.finish().unwrap(), "one");
        assert_eq!(second.finish().unwrap(), "two");
        assert_eq!(first.conversation().messages().len(), 2);
        assert_eq!(second.conversation().messages().len(), 2);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn borrowed_model_and_policy_work_without_spawn_or_static_bounds() {
    struct Borrowed<'a>(&'a str);
    #[async_trait]
    impl ModelAdapter for Borrowed<'_> {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn generate(
            &self,
            _: ModelRequest,
            _: Option<mpsc::Sender<StreamToken>>,
        ) -> Result<ModelOutput, InferenceError> {
            tokio::task::yield_now().await;
            Ok(text(self.0))
        }
    }
    let application = String::from("borrowed application result");
    let budget = DefaultBudget::default();
    let model = Model::from_adapter("borrowed", "local", Borrowed(&application)).unwrap();
    let harness = Harness::new(model);
    let mut session = harness.run("input").budget(&budget).manual();
    session.call_model().await.unwrap();
    assert_eq!(session.finish().unwrap(), application);
    assert_eq!(budget.snapshot().model_attempts, 1);
}

#[tokio::test]
async fn unsupported_offered_tools_can_be_disabled_without_losing_session() {
    let (model, requests) = model("text-only", false, vec![Ok(text("done"))]);
    let harness = Harness::new(model);
    let budget = DefaultBudget::default();
    let mut session = harness
        .run("input")
        .budget(&budget)
        .tool(
            tool("lookup", "lookup", |_: Arguments| async {
                Ok("not called")
            })
            .unwrap(),
        )
        .manual();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Model(
            frona_harness::ModelError::Unsupported { .. }
        ))
    ));
    assert_eq!(budget.snapshot().model_attempts, 0);
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(session.conversation().status(), ConversationStatus::Active);
    session
        .call_model()
        .application_tools(ToolSelection::None)
        .await
        .unwrap();
    assert_eq!(session.finish().unwrap(), "done");
}

#[tokio::test]
async fn text_mode_can_explicitly_reject_an_unoffered_submission() {
    let (model, requests) = model(
        "unexpected",
        true,
        vec![
            Ok(output(vec![call(
                "unexpected",
                "submit",
                json!({"value":"a"}),
            )])),
            Ok(text("proper answer")),
        ],
    );
    let harness = Harness::new(model);
    let mut session = harness.run("input").manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    assert!(requests.lock().unwrap()[0].tools.is_empty());
    session
        .reject(action, "submission tool was not offered")
        .unwrap();
    session.call_model().await.unwrap();
    assert_eq!(session.finish().unwrap(), "proper answer");
    assert_eq!(requests.lock().unwrap()[1].messages.len(), 3);
}
