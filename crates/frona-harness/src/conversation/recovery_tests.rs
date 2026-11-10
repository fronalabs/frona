use super::*;
use crate::{
    DefaultBudget, ExecutionControl, ExecutionStop, HistoryWritePhase, RecoveryError,
    ToolDispatchError, ToolRegistry, UsageReport, history::HistoryWriteError, tool,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct RetryFault {
    result: bool,
    fail: bool,
    cancel: Option<CancellationToken>,
}
impl HistoryWriter for RetryFault {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        let targeted = match &record.event {
            HistoryEvent::ToolStarted { ordinal, .. } => !self.result && *ordinal == 2,
            HistoryEvent::ActionResolved { .. } => self.result,
            _ => false,
        };
        if targeted {
            if let Some(token) = &self.cancel {
                token.cancel();
            }
            if self.fail {
                return Err(HistoryWriteError {
                    phase: HistoryWritePhase::Synchronize,
                    cause: "retry write failed".into(),
                });
            }
        }
        Ok(())
    }
}
fn fixture(writer: Option<RetryFault>) -> (Conversation, ToolRegistry<'static>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let effects = calls.clone();
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool("work", "effect", move |_: Value| {
                effects.fetch_add(1, Ordering::SeqCst);
                async { Ok("returned effect") }
            })
            .unwrap(),
        )
        .unwrap();
    let mut conversation = Conversation::new("input");
    if let Some(writer) = writer {
        conversation.attach_history(Box::new(writer));
    }
    let actions = conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "id",
                    rig_core::completion::message::ToolFunction::new(
                        "work".try_into().unwrap(),
                        json!({}),
                    ),
                ))],
                UsageReport::Unavailable,
            ),
            tools.definitions().cloned().collect(),
        )
        .unwrap();
    conversation.start(&actions.actions[0]).unwrap();
    (conversation, tools, calls)
}

#[tokio::test]
async fn failed_retry_start_or_result_blocks_recovery_until_explicit_reconciliation() {
    for result in [false, true] {
        let (mut conversation, mut tools, calls) = fixture(Some(RetryFault {
            result,
            fail: true,
            cancel: None,
        }));
        let budget = DefaultBudget::default();
        let error = conversation
            .recover_tools(&mut tools, ExecutionControl::new(&budget))
            .await
            .unwrap_err();
        let ToolDispatchError::Stop(ExecutionStop::Storage(error)) = error else {
            panic!("storage stop")
        };
        let failure = error.downcast_ref::<HistoryFailure>().unwrap();
        assert_eq!(
            failure.effect,
            if result {
                EffectState::Returned
            } else {
                EffectState::Prevented
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(result));
        assert!(conversation.pending_write().is_some());
        assert!(matches!(
            conversation
                .recover_tools(&mut tools, ExecutionControl::new(&budget))
                .await,
            Err(ToolDispatchError::Stop(ExecutionStop::Storage(_)))
        ));
        let mut detached = conversation.into_memory().unwrap();
        assert!(matches!(
            detached
                .recover_tools(&mut tools, ExecutionControl::new(&budget))
                .await,
            Err(ToolDispatchError::Conversation(
                ConversationError::PendingWrite
            ))
        ));
        if result {
            assert_eq!(
                detached.discard_pending_write(),
                Err(ConversationError::CannotDiscardWrite)
            );
            detached.adopt_pending_write().unwrap();
            detached
                .recover_tools(&mut tools, ExecutionControl::new(&budget))
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            detached.discard_pending_write().unwrap();
            detached
                .recover_tools(&mut tools, ExecutionControl::new(&budget))
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(detached.responses()[0].actions()[0].invocation_ordinal(), 2);
        }
    }
}

#[tokio::test]
async fn cancellation_from_successful_retry_writes_preserves_saved_state_and_stops() {
    for result in [false, true] {
        let cancel = CancellationToken::new();
        let (mut conversation, mut tools, calls) = fixture(Some(RetryFault {
            result,
            fail: false,
            cancel: Some(cancel.clone()),
        }));
        assert!(matches!(
            conversation
                .recover_tools(
                    &mut tools,
                    ExecutionControl::new(&DefaultBudget::default()).with_cancellation(&cancel),
                )
                .await,
            Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(result));
        assert_eq!(
            conversation.responses()[0].actions()[0].invocation_ordinal(),
            2
        );
        assert_eq!(
            conversation.responses()[0].actions()[0].status(),
            if result {
                ActionStatus::Resolved
            } else {
                ActionStatus::Started
            }
        );
        assert_eq!(conversation.status(), ConversationStatus::Stopped);
    }
}

#[tokio::test]
async fn overflow_and_unoffered_interrupted_calls_are_effect_free_errors() {
    for overflow in [false, true] {
        let (mut conversation, mut tools, calls) = fixture(None);
        if overflow {
            conversation.responses[0].actions[0].invocation_ordinal = u64::MAX;
        } else {
            conversation.responses[0].offered_tools.clear();
        }
        let budget = DefaultBudget::default();
        let result = conversation
            .recover_tools(&mut tools, ExecutionControl::new(&budget))
            .await;
        if overflow {
            assert!(matches!(
                result,
                Err(ToolDispatchError::Conversation(
                    ConversationError::InvocationOverflow
                ))
            ));
        } else {
            assert!(matches!(
                result,
                Err(ToolDispatchError::Recovery(
                    RecoveryError::UnofferedTool { .. }
                ))
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(budget.snapshot().tool_calls, 0);
        assert_eq!(
            conversation.responses()[0].actions()[0].status(),
            ActionStatus::Started
        );
    }
}
