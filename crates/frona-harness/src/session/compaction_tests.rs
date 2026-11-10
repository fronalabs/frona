use super::*;
use crate::{
    BudgetLimits, Capabilities, CompactionConfig, CompactionError, DefaultBudget, HistoryEvent,
    HistoryOperation, HistoryView, ModelAdapter, ModelOutput, ToolOutput, UsageReport,
    history::{HistoryRecord, HistoryWriteError, HistoryWritePhase, HistoryWriter},
    inference::{InferenceError, Usage},
    tool,
};
use async_trait::async_trait;
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
enum Summary {
    Text(String),
    Tool,
    Fail,
}
struct State {
    requests: Mutex<Vec<(ModelRequest, bool)>>,
    mains: AtomicUsize,
    summaries: AtomicUsize,
}
struct Fake {
    state: Arc<State>,
    summary: Summary,
    rounds: usize,
    submission: bool,
}
#[async_trait]
impl ModelAdapter for Fake {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        tokens: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        let summary = matches!(request.messages.last(), Some(rig_core::completion::Message::User {content}) if content.iter().any(|c| matches!(c, UserContent::Text(t) if t.text == super::compaction::SUMMARY_INSTRUCTION)));
        self.state
            .requests
            .lock()
            .unwrap()
            .push((request, tokens.is_some()));
        let content = if summary {
            self.state.summaries.fetch_add(1, Ordering::SeqCst);
            match &self.summary {
                Summary::Text(text) => vec![AssistantContent::text(text)],
                Summary::Tool => vec![proposal("summary", "read")],
                Summary::Fail => return Err(InferenceError::ConfigError("summary failed".into())),
            }
        } else {
            let n = self.state.mains.fetch_add(1, Ordering::SeqCst);
            if let Some(tokens) = tokens {
                tokens
                    .send(StreamToken::Text("token".into()))
                    .await
                    .unwrap();
            }
            if self.submission {
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    format!("submit-{n}"),
                    ToolFunction::new("submit".try_into().unwrap(), json!({"value":n})),
                ))]
            } else if n < self.rounds {
                vec![proposal(&format!("call-{n}"), "read")]
            } else {
                vec![AssistantContent::text("finished")]
            }
        };
        Ok(ModelOutput::new(
            content,
            UsageReport::Reported(
                Usage::new()
                    .input_tokens(10)
                    .output_tokens(2)
                    .total_tokens(12),
            ),
        ))
    }
}
struct TextOnly {
    inner: Fake,
    streaming: bool,
}
#[async_trait]
impl ModelAdapter for TextOnly {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: self.streaming,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        tokens: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        let before = self.inner.state.mains.load(Ordering::SeqCst);
        let mut output = self.inner.generate(request, tokens).await?;
        if self.inner.state.mains.load(Ordering::SeqCst) > before {
            output.content = vec![AssistantContent::text("evidence ".repeat(160))];
        }
        Ok(output)
    }
}

#[tokio::test]
async fn unsupported_main_request_is_recoverable_without_compaction_side_effects() {
    for (subscribed, supports_streaming) in [(false, true), (true, true), (true, false)] {
        let state = Arc::new(State {
            requests: Mutex::new(vec![]),
            mains: AtomicUsize::new(0),
            summaries: AtomicUsize::new(0),
        });
        let model = Model::from_adapter(
            "fake",
            "text-only",
            TextOnly {
                inner: Fake {
                    state: state.clone(),
                    summary: Summary::Text("Earlier evidence agrees".into()),
                    rounds: 0,
                    submission: false,
                },
                streaming: supports_streaming,
            },
        )
        .unwrap();
        let budget = DefaultBudget::default();
        let run = Harness::new(model)
            .run("Investigate the evidence carefully")
            .tool(
                tool("read", "read evidence", |_: Value| async {
                    Ok(ToolOutput::text("unused"))
                })
                .unwrap(),
            )
            .budget(budget.clone())
            .compaction(config());
        let report = run.report();
        let _progress = subscribed.then(|| run.subscribe());
        let mut session = run.manual();
        for _ in 0..3 {
            session
                .call_model()
                .application_tools(ToolSelection::None)
                .await
                .unwrap();
            session.feedback("continue").unwrap();
        }
        assert_eq!(state.summaries.load(Ordering::SeqCst), 0);
        let before_budget = budget.snapshot();
        let before_history = session.conversation().history().len();
        let before_messages = session.conversation().messages().to_vec();
        let before_report = report.snapshot();
        assert!(matches!(
            session.call_model().await,
            Err(ManualError::Model(ModelError::Unsupported {
                capability: crate::Capability::Tools,
                ..
            }))
        ));
        assert_eq!(state.requests.lock().unwrap().len(), 3);
        assert_eq!(budget.snapshot(), before_budget);
        assert_eq!(
            report.snapshot().attempts.len(),
            before_report.attempts.len()
        );
        assert_eq!(
            report.snapshot().requests.len(),
            before_report.requests.len()
        );
        assert_eq!(session.conversation().history().len(), before_history);
        assert_eq!(session.conversation().messages(), before_messages);
        assert!(session.conversation().compacted_view().is_none());
        assert_eq!(
            session.conversation().status(),
            super::super::ConversationStatus::Active
        );
        session
            .call_model()
            .application_tools(ToolSelection::None)
            .await
            .unwrap();
        assert_eq!(state.summaries.load(Ordering::SeqCst), 1);
        assert_eq!(state.mains.load(Ordering::SeqCst), 4);
        assert_eq!(budget.snapshot().model_attempts, 5);
        assert!(session.conversation().compacted_view().is_some());
        let requests = state.requests.lock().unwrap();
        assert!(!requests[3].1, "summary is not streamed");
        assert_eq!(requests[4].1, subscribed && supports_streaming);
    }
}

fn proposal(id: &str, name: &str) -> AssistantContent {
    let call = AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.try_into().unwrap(), json!({})),
    ));
    if id == "call-0" {
        call.with_native(json!({"signature":"opaque-signature"}))
    } else {
        call
    }
}

fn config() -> CompactionConfig {
    CompactionConfig {
        context_window: 1200,
        max_output_tokens: Some(32),
        ..Default::default()
    }
}
fn run(summary: Summary, payload: String) -> (Run<'static>, Arc<State>) {
    let state = Arc::new(State {
        requests: Mutex::new(vec![]),
        mains: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
    });
    let model = Model::from_adapter(
        "fake",
        "model",
        Fake {
            state: state.clone(),
            summary,
            rounds: 3,
            submission: false,
        },
    )
    .unwrap();
    let run = Harness::new(model)
        .run("Investigate the evidence carefully")
        .tool(
            tool("read", "read evidence", move |_: Value| {
                let payload = payload.clone();
                async move { Ok(ToolOutput::text(payload)) }
            })
            .unwrap(),
        );
    (run, state)
}
fn small(summary: Summary) -> (Run<'static>, Arc<State>) {
    let (run, state) = run(summary, "evidence ".repeat(160));
    (run.compaction(config()), state)
}
async fn three_rounds(session: &mut ManualSession<'_>) {
    for _ in 0..3 {
        let action = session.call_model().await.unwrap().actions.remove(0);
        session.execute(action).await.unwrap();
    }
}

#[tokio::test]
async fn automatic_default_policy_compacts_without_identity_or_database() {
    let (run, state) = run(
        Summary::Text("Earlier evidence agrees".into()),
        "evidence ".repeat(20_000),
    );
    assert_eq!(run.await.unwrap(), "finished");
    assert_eq!(state.summaries.load(Ordering::SeqCst), 1);
    assert_eq!(state.mains.load(Ordering::SeqCst), 4);
    assert!(
        state
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(r, _)| r.max_tokens == Some(8192))
    );
}

#[tokio::test]
async fn manual_compaction_commits_raw_summary_and_exact_report_inputs_with_streaming_parity() {
    for streaming in [false, true] {
        let (run, state) = small(Summary::Text("Earlier evidence agrees".into()));
        let budget = DefaultBudget::default();
        let run = run.budget(budget.clone());
        let report = run.report();
        let _progress = streaming.then(|| run.subscribe());
        let mut session = run.manual();
        three_rounds(&mut session).await;
        let originals = session.conversation().messages().to_vec();
        let actions = session.call_model().await.unwrap();
        assert!(actions.actions.is_empty());
        assert_eq!(
            session.conversation().request_messages().unwrap(),
            session.conversation().model_view().unwrap().messages()
        );
        assert!(
            session.conversation().request_messages().unwrap().len()
                < session.conversation().messages().len()
        );
        assert_eq!(session.finish().unwrap(), "finished");
        assert_eq!(
            &session.conversation().messages()[..originals.len()],
            originals
        );
        let (saved, output) = session
            .conversation()
            .history()
            .iter()
            .find_map(|r| match &r.event {
                HistoryEvent::Compacted { view, output, .. } => Some((view, output)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            output.usage,
            UsageReport::Reported(
                Usage::new()
                    .input_tokens(10)
                    .output_tokens(2)
                    .total_tokens(12)
            )
        );
        let restored = HistoryView::from_compacted(&originals, saved).unwrap();
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests[4].0.messages, restored.messages());
        assert!(
            !requests[3].1,
            "summary is never streamed into response progress"
        );
        assert_eq!(requests[4].1, streaming);
        assert!(requests[3].0.tools.is_empty());
        let report = report.snapshot();
        assert_eq!(report.attempts.len(), 5);
        assert_eq!(
            report.requests[4].input.as_ref().unwrap().messages,
            restored.original_positions()
        );
        assert!(
            report.requests[3]
                .input
                .as_ref()
                .unwrap()
                .messages
                .contains(&2)
        );
        assert!(
            !report.requests[4]
                .input
                .as_ref()
                .unwrap()
                .messages
                .contains(&2)
        );
        let old_feedback = report
            .feedback
            .iter()
            .find(|f| f.message == Some(2))
            .unwrap();
        assert!(old_feedback.issued_in.contains(&crate::AttemptId(3)));
        assert!(!old_feedback.issued_in.contains(&crate::AttemptId(4)));
        assert_eq!(budget.snapshot().model_attempts, 5);
        assert_eq!(budget.snapshot().tool_calls, 3);
        assert_eq!(budget.snapshot().tokens, 60);
    }
}

#[tokio::test]
async fn disabled_suppresses_summaries_and_reports_overflow_without_trimming() {
    let (run, state) = small(Summary::Text("never".into()));
    let mut session = run.disable_compaction().manual();
    three_rounds(&mut session).await;
    // Disabling allows unchanged requests while they fit, but never trims a
    // later overflowing request merely to avoid a summary call.
    session.call_model().await.unwrap();
    session
        .feedback("additional constraints ".repeat(100))
        .unwrap();
    let before = session.conversation().messages().to_vec();
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Compaction(CompactionError::ContextOverflow { .. })
    ));
    assert_eq!(state.summaries.load(Ordering::SeqCst), 0);
    assert_eq!(session.conversation().messages(), before);
}

#[tokio::test]
async fn every_summary_and_main_call_requires_fresh_budget_admission() {
    for limit in [3, 4] {
        let (run, state) = small(Summary::Text("Earlier evidence".into()));
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(limit),
            ..Default::default()
        });
        let mut session = run.budget(budget.clone()).manual();
        three_rounds(&mut session).await;
        assert!(matches!(
            session.call_model().await.unwrap_err(),
            ManualError::Stop(ExecutionStop::BudgetExhausted(_))
        ));
        assert_eq!(state.mains.load(Ordering::SeqCst), 3);
        assert_eq!(
            state.summaries.load(Ordering::SeqCst),
            usize::from(limit == 4)
        );
        assert_eq!(
            session.conversation().compacted_view().is_some(),
            limit == 4
        );
        assert_eq!(budget.snapshot().model_attempts, limit);
    }
}

#[tokio::test]
async fn failed_empty_or_tool_summary_stops_without_replacing_view_or_retrying() {
    for summary in [
        Summary::Fail,
        Summary::Tool,
        Summary::Text(" ".into()),
        Summary::Text("too large ".repeat(1000)),
    ] {
        let (run, state) = small(summary);
        let mut session = run.manual();
        three_rounds(&mut session).await;
        let before = session.conversation().messages().to_vec();
        assert!(session.call_model().await.is_err());
        assert!(session.call_model().await.is_err());
        assert_eq!(state.summaries.load(Ordering::SeqCst), 1);
        assert_eq!(state.mains.load(Ordering::SeqCst), 3);
        assert!(session.conversation().compacted_view().is_none());
        assert_eq!(session.conversation().messages(), before);
    }
}

#[derive(Debug)]
struct FailCompaction;
impl HistoryWriter for FailCompaction {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if record.event.operation() == HistoryOperation::Compact {
            Err(HistoryWriteError {
                phase: HistoryWritePhase::Synchronize,
                cause: "sync failed".into(),
            })
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn failed_compaction_write_preserves_old_view_and_is_explicitly_discardable_only() {
    let (mut run, state) = small(Summary::Text("Earlier evidence".into()));
    run.core
        .conversation
        .attach_history(Box::new(FailCompaction));
    let mut session = run.manual();
    three_rounds(&mut session).await;
    let before = session.conversation().messages().to_vec();
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Stop(ExecutionStop::Storage(_))
    ));
    assert_eq!(state.mains.load(Ordering::SeqCst), 3);
    assert!(session.conversation().compacted_view().is_none());
    assert!(matches!(
        session.conversation().pending_write().unwrap().event,
        HistoryEvent::Compacted { .. }
    ));
    let mut session = session.into_memory().unwrap();
    assert!(matches!(
        session.adopt_pending_write().unwrap_err(),
        ManualError::Conversation(ConversationError::CannotAdoptWrite)
    ));
    assert_eq!(session.conversation().messages(), before);
    session.discard_pending_write().unwrap();
    assert!(session.conversation().compacted_view().is_none());
    session.call_model().await.unwrap();
    assert_eq!(state.summaries.load(Ordering::SeqCst), 2);
    assert_eq!(state.mains.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn non_reducing_summary_is_not_committed_or_retried() {
    let (run, state) = small(Summary::Text("summary ".repeat(220)));
    let mut session = run
        .compaction(CompactionConfig {
            trigger_percent: 50,
            target_percent: 40,
            ..config()
        })
        .manual();
    for _ in 0..2 {
        let action = session.call_model().await.unwrap().actions.remove(0);
        session.execute(action).await.unwrap();
    }
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Compaction(CompactionError::NoReduction { .. })
    ));
    assert!(session.call_model().await.is_err());
    assert_eq!(state.summaries.load(Ordering::SeqCst), 1);
    assert_eq!(state.mains.load(Ordering::SeqCst), 2);
    assert!(session.conversation().compacted_view().is_none());
}

#[tokio::test]
async fn summary_request_itself_must_fit_before_dispatch() {
    let (mut run, state) = small(Summary::Text("never invoked".into()));
    run.core.compaction.context_window = 700;
    // A restored original history may exceed a newly chosen window. Populate
    // owned protocol state directly; no provider or tool is needed to load it.
    for i in 0..4 {
        let action = run
            .core
            .conversation
            .record_response(
                ModelOutput::new(
                    vec![proposal(&format!("saved-{i}"), "read")],
                    UsageReport::Unavailable,
                ),
                vec![],
            )
            .unwrap()
            .actions
            .remove(0);
        run.core
            .conversation
            .resolve(
                &action,
                ActionResolution::Result(ToolOutput::text("old evidence ".repeat(100))),
            )
            .unwrap();
    }
    let mut session = run.manual();
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Compaction(CompactionError::ContextOverflow { .. })
    ));
    assert_eq!(state.requests.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn compatibility_uses_retained_content_without_erasing_old_signed_response() {
    let (run, _) = small(Summary::Text("Earlier evidence".into()));
    let mut session = run.manual();
    three_rounds(&mut session).await;
    session.call_model().await.unwrap();
    let state = Arc::new(State {
        requests: Mutex::new(vec![]),
        mains: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
    });
    let other = Model::from_adapter(
        "other",
        "model",
        Fake {
            state: state.clone(),
            summary: Summary::Text("summary".into()),
            rounds: 0,
            submission: false,
        },
    )
    .unwrap();
    session.call_model().model(&other).await.unwrap();
    let AssistantContent::ToolCall(first) =
        &session.conversation().responses()[0].output().content[0]
    else {
        panic!("missing original call")
    };
    assert_eq!(
        first.native.as_ref().unwrap().item["signature"],
        "opaque-signature"
    );
    assert_eq!(
        session.conversation().responses()[0].origin(),
        Some("fake/model")
    );
    assert_eq!(state.mains.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn configured_output_setting_and_invalid_configuration_precede_effects() {
    let (run, state) = small(Summary::Text("unused".into()));
    let mut session = run
        .compaction(CompactionConfig {
            context_window: 32,
            max_output_tokens: Some(32),
            ..Default::default()
        })
        .manual();
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Compaction(CompactionError::InvalidConfiguration)
    ));
    assert!(state.requests.lock().unwrap().is_empty());
}

#[derive(Debug)]
struct CancelCompaction(CancellationToken);
impl HistoryWriter for CancelCompaction {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if matches!(record.event, HistoryEvent::Compacted { .. }) {
            self.0.cancel();
        }
        Ok(())
    }
}
#[tokio::test]
async fn cancellation_during_successful_compaction_commit_keeps_view_without_main_dispatch() {
    let token = CancellationToken::new();
    let (run, state) = small(Summary::Text("Earlier evidence".into()));
    let mut run = run.cancellation(token.clone());
    run.core
        .conversation
        .attach_history(Box::new(CancelCompaction(token)));
    let mut session = run.manual();
    three_rounds(&mut session).await;
    assert!(matches!(
        session.call_model().await.unwrap_err(),
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert!(session.conversation().compacted_view().is_some());
    assert_eq!(state.mains.load(Ordering::SeqCst), 3);
    assert_eq!(state.summaries.load(Ordering::SeqCst), 1);
    assert!(
        session
            .conversation()
            .history()
            .iter()
            .any(|r| matches!(r.event, HistoryEvent::Compacted { .. }))
    );
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Answer {
    value: usize,
}
#[tokio::test]
async fn single_input_stateful_refinement_compacts_without_accepting_old_candidates() {
    let state = Arc::new(State {
        requests: Mutex::new(vec![]),
        mains: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
    });
    let model = Model::from_adapter(
        "fake",
        "model",
        Fake {
            state: state.clone(),
            summary: Summary::Text("Earlier candidates needed stronger evidence".into()),
            rounds: 0,
            submission: true,
        },
    )
    .unwrap();
    let run = Harness::new(model)
        .run("Find the best evidence")
        .structured::<Answer>()
        .compaction(config());
    let mut inspected = Vec::new();
    let value = run
        .inspect(async |answer| {
            inspected.push(answer.value);
            Ok::<_, ()>(if answer.value < 3 {
                crate::Decision::revise("More evidence ".repeat(140)).retain(answer.value)
            } else {
                crate::Decision::accept(answer.value)
            })
        })
        .await
        .unwrap();
    assert_eq!(inspected, [0, 1, 2, 3]);
    assert_eq!(value, 3);
    assert!(state.summaries.load(Ordering::SeqCst) > 0);
    assert_eq!(state.mains.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn saved_view_still_materializes_when_new_response_has_unresolved_actions() {
    let (run, _) = small(Summary::Text("Earlier evidence".into()));
    let mut session = run.manual();
    three_rounds(&mut session).await;
    session.call_model().await.unwrap();
    let saved = session.conversation().compacted_view().unwrap().clone();
    session
        .core
        .conversation
        .record_response(
            ModelOutput::new(vec![proposal("pending", "read")], UsageReport::Unavailable),
            vec![],
        )
        .unwrap();
    let view = HistoryView::from_compacted(session.conversation().messages(), &saved).unwrap();
    assert_eq!(
        view.messages().last(),
        session.conversation().messages().last()
    );
    assert_eq!(session.conversation().pending_count(), 1);
    assert!(
        view.plan(
            "",
            &[],
            crate::CompactionLimits {
                available_input_tokens: 1168,
                target_input_tokens: 800
            }
        )
        .is_err()
    );
}
