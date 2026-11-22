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
        ToolFunction::new(name.try_into().unwrap(), args),
    ))
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Reported(Usage::new().total_tokens(3)))
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
        let frona_harness::Submission::Ready(value) = manual.submission(&action).await.unwrap()
        else {
            panic!("fixture is schema-valid")
        };
        match inspect(value).await.unwrap() {
            Decision::Accept { value, .. } => break manual.accept(action, value).await.unwrap(),
            Decision::Revise {
                feedback,
                retained: candidate,
            } => {
                if candidate.is_some() {
                    retained = candidate;
                }
                manual.revise(action, feedback).await.unwrap();
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

#[tokio::test]
async fn model_response_limit_counts_tool_batches_in_manual_and_fluent_execution() {
    for manual in [false, true] {
        let (model, requests) =
            scripted_model(vec![Ok(output(vec![call("r", "read", json!({}))]))]);
        let harness = Harness::new(model);
        let run = harness.run("input").model_responses(1).tool(
            tool("read", "read", |_: Value| async {
                Ok::<_, frona_harness::ToolError>("evidence")
            })
            .unwrap(),
        );
        let report = run.report();
        if manual {
            let mut session = run.manual();
            let actions = session.call_model().await.unwrap();
            for action in actions.actions {
                session.execute(action).await.unwrap();
            }
            assert!(matches!(
                session.call_model().await,
                Err(ManualError::ModelResponsesExhausted { maximum: 1 })
            ));
        } else {
            let incomplete = run.await.unwrap_err();
            assert!(matches!(
                incomplete.reason,
                ManualError::ModelResponsesExhausted { maximum: 1 }
            ));
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(report.snapshot().actions.len(), 1);
        assert!(matches!(
            report.snapshot().outcome,
            Some(frona_harness::RunOutcome::Incomplete {
                kind: frona_harness::FailureKind::Budget,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn model_response_limit_accepts_the_last_response_and_zero_never_calls() {
    let (model, requests) = scripted_model(vec![Ok(output(vec![AssistantContent::text("done")]))]);
    let harness = Harness::new(model);
    assert!(matches!(
        harness
            .run("zero")
            .model_responses(0)
            .await
            .unwrap_err()
            .reason,
        ManualError::ModelResponsesExhausted { maximum: 0 }
    ));
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        harness.run("last").model_responses(1).await.unwrap(),
        "done"
    );
}

#[derive(Debug, Default, Clone)]
struct RequiredHistory {
    records: Arc<Mutex<Vec<frona_harness::HistoryRecord>>>,
    fail_on: Option<frona_harness::HistoryOperation>,
}
#[async_trait]
impl frona_harness::HistoryWriter for RequiredHistory {
    async fn commit(
        &mut self,
        record: &frona_harness::HistoryRecord,
    ) -> Result<(), frona_harness::HistoryWriteError> {
        tokio::task::yield_now().await;
        if self.fail_on == Some(record.event.operation()) {
            return Err(frona_harness::HistoryWriteError {
                phase: frona_harness::HistoryWritePhase::Append,
                cause: "database unavailable".into(),
            });
        }
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[tokio::test]
async fn required_history_failure_prevents_dependent_model_and_tool_effects() {
    use frona_harness::HistoryOperation as Op;
    for (failure, expected_models, expected_tools) in [
        (Op::Open, 0, 0),
        (Op::Response, 1, 0),
        (Op::ToolStart, 1, 0),
        (Op::ActionResolution, 1, 1),
    ] {
        let (model, requests) = scripted_model(vec![
            Ok(output(vec![call("change", "change", json!({"value":1}))])),
            submitted(2),
        ]);
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = effects.clone();
        let tool = tool("change", "effect", move |_: Answer| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(ToolOutput::text("changed")) }
        })
        .unwrap();
        let sink = RequiredHistory {
            fail_on: Some(failure),
            ..Default::default()
        };
        let error = Harness::new(model)
            .run("change")
            .tool(tool)
            .history_writer(sink)
            .structured::<Answer>()
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error.reason, ManualError::Stop(ExecutionStop::Storage(_))),
            "{failure:?}: {error:?}"
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            expected_models,
            "{failure:?}"
        );
        assert_eq!(
            effects.load(std::sync::atomic::Ordering::SeqCst),
            expected_tools,
            "{failure:?}"
        );
        assert!(matches!(
            error.report.outcome,
            Some(frona_harness::RunOutcome::Incomplete {
                kind: frona_harness::FailureKind::Storage,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn inspection_feedback_and_acceptance_are_durable_before_returning_success() {
    let (model, requests) = scripted_model(vec![submitted(1), submitted(2)]);
    let sink = RequiredHistory::default();
    let accepted = Harness::new(model)
        .run("verify")
        .history_writer(sink.clone())
        .structured::<Answer>()
        .submission_attempts(2)
        .inspect(async |answer| {
            assert!(
                sink.records
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| matches!(r.event, frona_harness::HistoryEvent::Response { .. }))
            );
            Ok::<_, ()>(if answer.value == 1 {
                Decision::revise("health check failed")
            } else {
                Decision::accept(answer.value)
            })
        })
        .await
        .unwrap();
    assert_eq!(accepted, 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
    let history = sink.records.lock().unwrap();
    assert!(
        serde_json::to_string(&*history)
            .unwrap()
            .contains("health check failed")
    );
    assert!(matches!(
        history.last().unwrap().event,
        frona_harness::HistoryEvent::SubmissionCompleted { .. }
    ));
}

#[tokio::test]
async fn accepted_candidate_is_not_success_when_terminal_database_write_fails() {
    let (model, _) = scripted_model(vec![submitted(1)]);
    let sink = RequiredHistory {
        fail_on: Some(frona_harness::HistoryOperation::SubmissionCompletion),
        ..Default::default()
    };
    let error = Harness::new(model)
        .run("verify")
        .history_writer(sink)
        .structured::<Answer>()
        .inspect(async |a| Ok::<_, ()>(Decision::accept(a.value)))
        .await
        .unwrap_err();
    assert!(matches!(
        error.reason,
        ManualError::Stop(ExecutionStop::Storage(_))
    ));
    assert!(matches!(
        error.report.outcome,
        Some(frona_harness::RunOutcome::Incomplete {
            kind: frona_harness::FailureKind::Storage,
            ..
        })
    ));
}

#[derive(Debug)]
struct InterruptedWriter {
    entered: Arc<Notify>,
    operation: frona_harness::HistoryOperation,
}
#[async_trait]
impl frona_harness::HistoryWriter for InterruptedWriter {
    async fn commit(
        &mut self,
        record: &frona_harness::HistoryRecord,
    ) -> Result<(), frona_harness::HistoryWriteError> {
        if record.event.operation() == self.operation {
            self.entered.notify_one();
            std::future::pending().await
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn dropped_async_commit_preserves_uncertainty_and_blocks_later_transitions() {
    use frona_harness::{Conversation, ConversationError, HistoryOperation, PersistenceOutcome};
    for operation in [HistoryOperation::Open, HistoryOperation::Response] {
        let entered = Arc::new(Notify::new());
        let mut conversation = Conversation::new("input");
        conversation
            .attach_history(Box::new(InterruptedWriter {
                entered: entered.clone(),
                operation,
            }))
            .unwrap();
        let mut write = Box::pin(
            conversation.record_response(output(vec![AssistantContent::text("answer")]), vec![]),
        );
        tokio::select! {
            result = &mut write => panic!("unexpected completed write: {result:?}"),
            _ = entered.notified() => {}
        }
        drop(write);
        assert_eq!(conversation.status(), ConversationStatus::Stopped);
        let failure = conversation.history_failure().unwrap();
        assert_eq!(failure.operation, operation);
        assert_eq!(failure.persistence, PersistenceOutcome::Unknown);
        assert_eq!(
            conversation.pending_write().unwrap().event.operation(),
            operation
        );
        assert!(matches!(
            conversation.finish().await,
            Err(ConversationError::Terminal)
        ));
        // A dropped write cannot turn into a silently acknowledged response.
        assert!(conversation.responses().is_empty());
        let mut detached = conversation.into_memory().unwrap();
        detached.adopt_pending_write().await.unwrap();
        assert!(detached.history_failure().is_none());
    }
}

struct AfterRecorded {
    records: Arc<Mutex<Vec<frona_harness::HistoryRecord>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl frona_harness::ToolCallHook for AfterRecorded {
    async fn decide(
        &mut self,
        _: &ToolCall,
    ) -> Result<frona_harness::ToolDecision, frona_harness::ToolFailure> {
        Ok(frona_harness::ToolDecision::Execute)
    }
    async fn after(
        &mut self,
        _: &ToolCall,
        _: &frona_harness::ActionResolution,
    ) -> Result<(), ExecutionStop> {
        assert!(matches!(
            self.records.lock().unwrap().last().unwrap().event,
            frona_harness::HistoryEvent::ActionResolved { .. }
        ));
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ExecutionStop::Stopped(
            "external continuation required".into(),
        ))
    }
}
#[tokio::test]
async fn post_tool_policy_runs_only_after_required_resolution_commit() {
    for fail in [false, true] {
        let (model, requests) = scripted_model(vec![Ok(output(vec![call(
            "probe",
            "probe",
            json!({"value":1}),
        )]))]);
        let writer = RequiredHistory {
            fail_on: fail.then_some(frona_harness::HistoryOperation::ActionResolution),
            ..Default::default()
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = AfterRecorded {
            records: writer.records.clone(),
            calls: calls.clone(),
        };
        let error = Harness::new(model)
            .run("probe")
            .tool(
                tool("probe", "probe", |_: Answer| async {
                    Ok(ToolOutput::text("done"))
                })
                .unwrap(),
            )
            .history_writer(writer)
            .tool_call_hook(hook)
            .await
            .unwrap_err();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(!fail)
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        if fail {
            assert!(matches!(
                error.reason,
                ManualError::Stop(ExecutionStop::Storage(_))
            ));
        } else {
            assert!(matches!(
                error.reason,
                ManualError::Stop(ExecutionStop::Stopped(_))
            ));
        }
    }
}
