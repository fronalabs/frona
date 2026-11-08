use std::{
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use frona_harness::{
    ActionStatus, BudgetLimits, Capabilities, ConversationStatus, Decision, DefaultBudget,
    ExecutionStop, Harness, Incomplete, ManualError, Model, ModelAdapter, ModelOutput,
    ModelRequest, ToolOutput, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
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
            .expect("unexpected provider call")
    }
}
fn scripted_model(
    outputs: Vec<Result<ModelOutput, InferenceError>>,
) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Model::from_adapter(
            "local",
            "refinement",
            Script {
                outputs: Mutex::new(outputs.into()),
                requests: requests.clone(),
            },
        )
        .unwrap(),
        requests,
    )
}
fn call(id: &str, name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), args),
    ))
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
fn submitted(value: u32) -> Result<ModelOutput, InferenceError> {
    Ok(output(vec![call(
        &format!("submit-{value}"),
        "submit",
        json!({"value":value}),
    )]))
}
#[derive(Deserialize, JsonSchema)]
struct Answer {
    value: u32,
}
// Accepted/retained values need neither Clone nor Debug.
struct Plan(u32);

#[tokio::test]
async fn lending_local_inspector_transforms_output_and_preserves_original_submission() {
    let (model, requests) = scripted_model(vec![submitted(1), submitted(7)]);
    let harness = Harness::new(model);
    let local = Rc::new(std::cell::Cell::new(0));
    let mut seen = Vec::new();
    let accepted = harness
        .run("plan")
        .structured::<Answer>()
        .inspect(async |answer| {
            seen.push(answer.value);
            tokio::task::yield_now().await;
            local.set(local.get() + 1);
            Ok::<_, ()>(if answer.value == 7 {
                Decision::accept(Plan(answer.value * 10))
            } else {
                Decision::revise("improve").retain(Plan(answer.value))
            })
        })
        .await
        .unwrap();
    assert_eq!(accepted.0, 70);
    assert_eq!(seen, [1, 7]);
    assert_eq!(local.get(), 2);
    let requests = requests.lock().unwrap();
    let history = serde_json::to_value(&requests[1].messages)
        .unwrap()
        .to_string();
    assert!(history.contains("improve"));
    assert!(history.contains("submit-1"));
    assert!(!history.contains("70"));
}

#[tokio::test]
async fn omitted_retention_keeps_prior_and_explicit_replacement_survives_stop_and_salvage() {
    let (model, _) = scripted_model(vec![
        submitted(10),
        submitted(2),
        submitted(20),
        submitted(1),
    ]);
    let mut incomplete = Harness::new(model)
        .run("rank")
        .structured::<Answer>()
        .inspect(async |answer| {
            Ok::<_, ()>(match answer.value {
                10 | 20 => Decision::revise("continue").retain(Plan(answer.value)),
                2 => Decision::revise("worse; try again"),
                _ => Decision::stop("application finished its search"),
            })
        })
        .await
        .err()
        .expect("stop is incomplete");
    assert_eq!(incomplete.retained.as_ref().unwrap().0, 20);
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::Stopped(_))
    ));
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    assert_eq!(
        incomplete.conversation.responses()[3].actions()[0].status(),
        ActionStatus::Proposed
    );
    let mut salvage = incomplete.retained.take().unwrap();
    salvage.0 -= 1;
    assert_eq!(salvage.0, 19);
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    fn is_error(_: &dyn std::error::Error) {}
    is_error(&incomplete); // No Plan: Debug bound on Incomplete's Error implementation.
}

#[tokio::test]
async fn inspector_failure_stops_without_feedback_or_another_request() {
    let (model, requests) = scripted_model(vec![submitted(4), submitted(8)]);
    let incomplete = Harness::new(model)
        .run("inspect")
        .structured::<Answer>()
        .inspect(async |answer| {
            if answer.value == 4 {
                Ok(Decision::revise("continue").retain(Plan(4)))
            } else {
                Err(Rc::<str>::from("application read failed"))
            }
        })
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&incomplete.reason, ManualError::Inspector(message) if message.contains("application read failed"))
    );
    assert_eq!(incomplete.retained.unwrap().0, 4);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(incomplete.conversation.pending_count(), 1);
    assert_eq!(
        incomplete.conversation.responses()[1].actions()[0].resolution(),
        None
    );
    assert!(
        !serde_json::to_value(incomplete.conversation.messages())
            .unwrap()
            .to_string()
            .contains("application read failed")
    );
}

#[tokio::test]
async fn exhaustion_and_provider_failure_preserve_explicit_retention() {
    for exhaust in [true, false] {
        let (model, requests) = scripted_model(vec![
            submitted(9),
            Err(InferenceError::InferenceFailed("provider failed".into())),
        ]);
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(if exhaust { 1 } else { 2 }),
            ..BudgetLimits::default()
        });
        let incomplete = Harness::new(model)
            .run("continue")
            .budget(budget)
            .structured::<Answer>()
            .inspect(async |answer| {
                Ok::<_, ()>(Decision::revise("more evidence").retain(Plan(answer.value)))
            })
            .await
            .err()
            .unwrap();
        assert_eq!(incomplete.retained.unwrap().0, 9);
        if exhaust {
            assert!(matches!(
                incomplete.reason,
                ManualError::Stop(ExecutionStop::BudgetExhausted(_))
            ));
            assert_eq!(requests.lock().unwrap().len(), 1);
        } else {
            assert!(matches!(incomplete.reason, ManualError::Model(_)));
            assert_eq!(requests.lock().unwrap().len(), 2);
        }
        assert_eq!(
            incomplete.conversation.status(),
            ConversationStatus::Stopped
        );
    }
}

#[tokio::test]
async fn omitted_retention_survives_later_failure_and_replacement_is_application_owned() {
    for replace in [false, true] {
        let (model, _) = scripted_model(vec![submitted(10), submitted(2)]);
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(2),
            ..BudgetLimits::default()
        });
        let incomplete = Harness::new(model)
            .run("rank")
            .budget(budget)
            .structured::<Answer>()
            .inspect(async |answer| {
                let decision = Decision::revise("try again");
                Ok::<_, ()>(if answer.value == 10 || replace {
                    decision.retain(Plan(answer.value))
                } else {
                    decision
                })
            })
            .await
            .err()
            .unwrap();
        assert_eq!(incomplete.retained.unwrap().0, if replace { 2 } else { 10 });
        assert!(matches!(
            incomplete.reason,
            ManualError::Stop(ExecutionStop::BudgetExhausted(_))
        ));
    }
}

#[tokio::test]
async fn nested_storage_stop_preserves_prior_candidate_without_inspecting_new_submission() {
    let (model, _) = scripted_model(vec![
        submitted(5),
        Ok(output(vec![
            call("submit-next", "submit", json!({"value":99})),
            call("required-storage", "persist", json!({})),
        ])),
    ]);
    let failing = tool("persist", "Required execution storage", |_: Value| async {
        Err::<ToolOutput, _>(
            ExecutionStop::Storage(Box::new(std::io::Error::other("required write failed"))).into(),
        )
    })
    .unwrap();
    let mut inspections = 0;
    let incomplete = Harness::new(model)
        .run("persist")
        .tool(failing)
        .structured::<Answer>()
        .inspect(async |answer| {
            inspections += 1;
            Ok::<_, ()>(Decision::revise("continue").retain(Plan(answer.value)))
        })
        .await
        .err()
        .unwrap();
    assert_eq!(inspections, 1);
    assert_eq!(incomplete.retained.unwrap().0, 5);
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::Storage(_))
    ));
}

#[tokio::test]
async fn cancellation_interrupts_pending_inspection_and_keeps_previous_candidate() {
    let (model, requests) = scripted_model(vec![submitted(6), submitted(7)]);
    let cancellation = CancellationToken::new();
    let inspecting = Notify::new();
    let run = Harness::new(model)
        .run("inspect")
        .cancellation(cancellation.clone())
        .structured::<Answer>();
    let inspection = run.inspect(async |answer| {
        if answer.value == 6 {
            return Ok::<_, ()>(Decision::revise("continue").retain(Plan(6)));
        }
        inspecting.notify_one();
        std::future::pending::<()>().await;
        Ok(Decision::accept(Plan(7)))
    });
    let (outcome, ()) = tokio::join!(inspection, async {
        inspecting.notified().await;
        cancellation.cancel();
    });
    let incomplete = outcome.err().unwrap();
    assert_eq!(incomplete.retained.unwrap().0, 6);
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(incomplete.conversation.pending_count(), 1);
}

#[tokio::test]
async fn cancellation_before_acceptance_never_promotes_accept_or_prior_candidate() {
    let (model, _) = scripted_model(vec![submitted(1), submitted(2)]);
    let cancellation = CancellationToken::new();
    let incomplete = Harness::new(model)
        .run("accept")
        .cancellation(cancellation.clone())
        .structured::<Answer>()
        .inspect(async |answer| {
            Ok::<_, ()>(if answer.value == 1 {
                Decision::revise("continue").retain(Plan(1))
            } else {
                cancellation.cancel();
                Decision::accept(Plan(2))
            })
        })
        .await
        .err()
        .unwrap();
    assert_eq!(incomplete.retained.unwrap().0, 1);
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert_eq!(incomplete.conversation.pending_count(), 1);
}

#[tokio::test]
async fn schema_repair_skips_inspector_and_tools_share_application_observations() {
    let (model, requests) = scripted_model(vec![
        Ok(output(vec![call("invalid", "submit", json!({"wrong":1}))])),
        Ok(output(vec![
            call("record", "remember", json!({"seen":true})),
            call("valid", "submit", json!({"value":3})),
        ])),
    ]);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let tool_observations = observations.clone();
    let remembering = tool("remember", "Record evidence", move |args: Value| {
        tool_observations.lock().unwrap().push(args);
        async { Ok("recorded") }
    })
    .unwrap();
    let mut inspections = 0;
    let accepted = Harness::new(model)
        .run("repair")
        .tool(remembering)
        .structured::<Answer>()
        .inspect(async |answer| {
            inspections += 1;
            tokio::task::yield_now().await;
            assert_eq!(
                observations.lock().unwrap().as_slice(),
                [json!({"seen":true})]
            );
            Ok::<_, ()>(Decision::accept(Plan(answer.value)))
        })
        .await
        .unwrap();
    assert_eq!(accepted.0, 3);
    assert_eq!(inspections, 1);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn owned_inspection_is_spawnable_and_stop_can_explicitly_retain() {
    let (model, _) = scripted_model(vec![submitted(8)]);
    let run = Harness::new(model)
        .run("owned")
        .budget(DefaultBudget::new(BudgetLimits {
            model_attempts: Some(1),
            tokens: Some(3),
            ..BudgetLimits::default()
        }))
        .structured::<Answer>();
    let accepted = tokio::spawn(async move {
        run.inspect(async move |answer| {
            tokio::task::yield_now().await;
            Ok::<_, ()>(Decision::accept(Plan(answer.value)))
        })
        .await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(accepted.0, 8);
    let (model, _) = scripted_model(vec![submitted(11)]);
    let incomplete: Incomplete<Plan> = Harness::new(model)
        .run("retain at stop")
        .structured::<Answer>()
        .inspect(async |answer| Ok::<_, ()>(Decision::stop("enough").retain(Plan(answer.value))))
        .await
        .err()
        .unwrap();
    assert_eq!(incomplete.retained.unwrap().0, 11);
}

#[tokio::test]
async fn manual_loop_can_reuse_the_same_matchable_inspector_decisions() {
    async fn inspect(answer: Answer) -> Result<Decision<Plan>, ()> {
        tokio::task::yield_now().await;
        Ok(if answer.value == 1 {
            Decision::revise("improve").retain(Plan(1))
        } else {
            Decision::accept(Plan(answer.value))
        })
    }
    let (model, _) = scripted_model(vec![submitted(1), submitted(2)]);
    let mut manual = Harness::new(model)
        .run("manual")
        .structured::<Answer>()
        .manual();
    let mut retained = None;
    let accepted = loop {
        let action = manual.call_model().await.unwrap().actions.pop().unwrap();
        let frona_harness::Submission::Ready(value) = manual.submission(&action).unwrap() else {
            panic!("fixture is schema-valid")
        };
        match inspect(value).await.unwrap() {
            Decision::Accept { value, .. } => break manual.accept(action, value).unwrap(),
            Decision::Revise {
                feedback,
                retained: candidate,
            } => {
                if candidate.is_some() {
                    retained = candidate;
                }
                manual.revise(action, feedback).unwrap();
            }
            Decision::Stop { .. } | Decision::Abandon { .. } => panic!("fixture does not stop"),
        }
    };
    assert_eq!(accepted.0, 2);
    assert_eq!(retained.unwrap().0, 1);
    assert_eq!(manual.conversation().status(), ConversationStatus::Finished);
}

#[tokio::test]
async fn response_attempt_cap_excludes_exploration_and_accepts_the_last_mixed_submission() {
    let (model, requests) = scripted_model(vec![
        Ok(output(vec![call("explore", "read", json!({}))])),
        Ok(output(vec![AssistantContent::text("missing submission")])),
        Ok(output(vec![call(
            "malformed",
            "submit",
            json!({"value":"bad"}),
        )])),
        Ok(output(vec![
            call("last-read", "read", json!({})),
            call("valid", "submit", json!({"value":7})),
        ])),
    ]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(4),
        tool_calls: Some(2),
        ..Default::default()
    });
    let harness = Harness::new(model);
    let run = harness
        .run("input")
        .tool(tool("read", "Read", |_: Value| async { Ok("evidence") }).unwrap())
        .budget(&budget)
        .structured::<Answer>()
        .submission_attempts(3);
    let report = run.report();
    let answer = run
        .inspect(async |answer| Ok::<_, ()>(Decision::accept(answer.value)))
        .await
        .unwrap();
    assert_eq!(answer, 7);
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert_eq!(budget.snapshot().tool_calls, 2);
    assert_eq!(
        report.snapshot().outcome,
        Some(frona_harness::RunOutcome::Completed)
    );
}

#[tokio::test]
async fn response_attempt_cap_stops_missing_submissions_and_is_fresh_on_manual_resume() {
    let (model, requests) = scripted_model(vec![
        Ok(output(vec![AssistantContent::text("not yet")])),
        submitted(9),
    ]);
    let harness = Harness::new(model);
    let mut session = harness
        .run("input")
        .structured::<Answer>()
        .submission_attempts(1)
        .manual();
    session.call_model().await.unwrap();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::SubmissionAttemptsExhausted { maximum: 1 })
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
    let answer = harness
        .resume(session.into_conversation())
        .structured::<Answer>()
        .submission_attempts(1)
        .await
        .unwrap();
    assert_eq!(answer.value, 9);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn closed_exploration_counts_unoffered_tool_only_responses_as_missing_submissions() {
    let (model, requests) = scripted_model(vec![
        Ok(output(vec![call("explore", "read", json!({}))])),
        Ok(output(vec![call("unoffered", "read", json!({}))])),
    ]);
    let harness = Harness::new(model);
    let incomplete = harness
        .run("input")
        .tool(tool("read", "Read", |_: Value| async { Ok("evidence") }).unwrap())
        .exploration_turns(1)
        .structured::<Answer>()
        .submission_attempts(1)
        .inspect(async |answer| Ok::<_, ()>(Decision::accept(answer.value)))
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::SubmissionAttemptsExhausted { maximum: 1 }
    ));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(
        incomplete
            .report
            .actions
            .iter()
            .filter(|action| action.started)
            .count(),
        1
    );
}

#[tokio::test]
async fn last_allowed_response_still_requires_shared_budget_admission_for_its_tool() {
    let (model, _) = scripted_model(vec![Ok(output(vec![
        call("read", "read", json!({})),
        call("valid", "submit", json!({"value":7})),
    ]))]);
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(3),
        ..Default::default()
    });
    let incomplete = Harness::new(model)
        .run("input")
        .tool(
            tool("read", "Read", |_: Value| async {
                panic!("budget must prevent this effect");
                #[allow(unreachable_code)]
                Ok("never")
            })
            .unwrap(),
        )
        .budget(&budget)
        .structured::<Answer>()
        .submission_attempts(1)
        .inspect(async |answer| Ok::<_, ()>(Decision::accept(answer.value)))
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Dispatch(frona_harness::ToolDispatchError::Stop(
            ExecutionStop::BudgetExhausted(_)
        )) | ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(budget.snapshot().tool_calls, 0);
}
