use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use frona_harness::{
    ActionResolution, ActionStatus, BudgetLimits, Conversation, DefaultBudget, ExecutionControl,
    ExecutionStop, ModelOutput, ToolDispatchError, ToolError, ToolOutput, ToolRegistry,
    UsageReport, tool,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction, UserContent},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, JsonSchema)]
struct Args {
    id: usize,
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Probe {
    started: mpsc::UnboundedReceiver<usize>,
    release: Arc<Vec<Semaphore>>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

fn setup(names: &[&str], fail: Option<usize>) -> (ToolRegistry<'static>, Probe) {
    let (tx, started) = mpsc::unbounded_channel();
    let release = Arc::new(
        (0..names.len())
            .map(|_| Semaphore::new(0))
            .collect::<Vec<_>>(),
    );
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    for name in names
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (tx, release, active, peak) =
            (tx.clone(), release.clone(), active.clone(), peak.clone());
        tools
            .register(
                tool(name, "probe", move |args: Args| {
                    let (tx, release, active, peak) =
                        (tx.clone(), release.clone(), active.clone(), peak.clone());
                    async move {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        let _guard = Active(active);
                        peak.fetch_max(count, Ordering::SeqCst);
                        tx.send(args.id).unwrap();
                        release[args.id].acquire().await.unwrap().forget();
                        if fail == Some(args.id) {
                            return Err(ToolError::failed("ordinary failure"));
                        }
                        Ok(ToolOutput::text(args.id.to_string()))
                    }
                })
                .unwrap(),
            )
            .unwrap();
    }
    (
        tools,
        Probe {
            started,
            release,
            active,
            peak,
        },
    )
}

fn record(
    conversation: &mut Conversation,
    names: &[&str],
    tools: &ToolRegistry<'_>,
) -> Vec<frona_harness::PendingAction> {
    conversation
        .record_response(
            ModelOutput::new(
                names
                    .iter()
                    .enumerate()
                    .map(|(id, name)| {
                        AssistantContent::ToolCall(ToolCall::from_wire(
                            id.to_string(),
                            ToolFunction::new((*name).try_into().unwrap(), json!({"id":id})),
                        ))
                    })
                    .collect(),
                UsageReport::Unavailable,
            ),
            tools.definitions().cloned().collect(),
        )
        .unwrap()
        .actions
}

#[tokio::test]
async fn bounded_dispatch_refills_slots_and_preserves_proposal_order() {
    let names = ["a", "a", "b", "c"];
    let (mut tools, mut probe) = setup(&names, None);
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &names, &tools);
    let budget = DefaultBudget::default();
    let (results, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            conversation.execute_tools(
                &actions,
                &mut tools,
                ExecutionControl::new(&budget),
                NonZeroUsize::new(2).unwrap()
            ),
            async {
                assert_eq!(probe.started.recv().await, Some(0));
                // The second call to a must not block an available slot for b.
                assert_eq!(probe.started.recv().await, Some(2));
                probe.release[2].add_permits(1);
                assert_eq!(probe.started.recv().await, Some(3));
                probe.release[3].add_permits(1);
                probe.release[0].add_permits(1);
                assert_eq!(probe.started.recv().await, Some(1));
                probe.release[1].add_permits(1);
            }
        )
    })
    .await
    .unwrap();
    let results = results.unwrap();
    assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result.output(), &ToolOutput::text(index.to_string()));
    }
    let Message::User { content } = conversation.messages().last().unwrap() else {
        panic!("tool result message")
    };
    let ids: Vec<_> = content
        .iter()
        .map(|item| match item {
            UserContent::ToolResult(result) => result.call.to_string(),
            _ => panic!("tool result"),
        })
        .collect();
    assert_eq!(ids, ["0", "1", "2", "3"]);
    assert_eq!(budget.snapshot().tool_calls, 4);
    assert_eq!(budget.snapshot().in_flight, 0);
}

#[tokio::test]
async fn one_is_sequential_and_ordinary_failures_do_not_stop_the_batch() {
    let names = ["a", "b", "c"];
    let (mut tools, mut probe) = setup(&names, Some(1));
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &names, &tools);
    let budget = DefaultBudget::default();
    let (results, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            conversation.execute_tools(
                &actions,
                &mut tools,
                ExecutionControl::new(&budget),
                NonZeroUsize::MIN
            ),
            async {
                for id in 0..3 {
                    assert_eq!(probe.started.recv().await, Some(id));
                    assert!(probe.started.try_recv().is_err());
                    probe.release[id].add_permits(1);
                }
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(results.unwrap()[1], ActionResolution::Failed(_)));
    assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
    assert_eq!(conversation.pending_count(), 0);
}

#[tokio::test]
async fn cancellation_abandons_active_calls_without_starting_queued_calls() {
    let names = ["a", "b", "c"];
    let (mut tools, mut probe) = setup(&names, None);
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &names, &tools);
    let budget = DefaultBudget::default();
    let cancellation = CancellationToken::new();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            conversation.execute_tools(
                &actions,
                &mut tools,
                ExecutionControl::new(&budget).with_cancellation(&cancellation),
                NonZeroUsize::new(2).unwrap()
            ),
            async {
                assert_eq!(probe.started.recv().await, Some(0));
                assert_eq!(probe.started.recv().await, Some(1));
                cancellation.cancel();
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
    ));
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 2);
    assert_eq!(budget.snapshot().in_flight, 0);
    let states: Vec<_> = conversation.responses()[0]
        .actions()
        .iter()
        .map(|action| action.status())
        .collect();
    assert_eq!(
        states,
        [
            ActionStatus::Started,
            ActionStatus::Started,
            ActionStatus::Proposed
        ]
    );
}

#[tokio::test]
async fn budget_refusal_drains_admitted_calls_and_leaves_remaining_proposals() {
    let names = ["a", "b", "c", "d"];
    let (mut tools, mut probe) = setup(&names, None);
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &names, &tools);
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(2),
        ..BudgetLimits::default()
    });
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            conversation.execute_tools(
                &actions,
                &mut tools,
                ExecutionControl::new(&budget),
                NonZeroUsize::new(3).unwrap()
            ),
            async {
                assert_eq!(probe.started.recv().await, Some(0));
                assert_eq!(probe.started.recv().await, Some(1));
                probe.release[1].add_permits(1);
                probe.release[0].add_permits(1);
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(budget.snapshot().in_flight, 0);
    let states: Vec<_> = conversation.responses()[0]
        .actions()
        .iter()
        .map(|action| action.status())
        .collect();
    assert_eq!(
        states,
        [
            ActionStatus::Resolved,
            ActionStatus::Resolved,
            ActionStatus::Proposed,
            ActionStatus::Proposed
        ]
    );
}

#[test]
fn configuration_defaults_to_cpu_based_concurrency_and_rejects_zero() {
    let config: frona_harness::config::InferenceConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        config.max_concurrent_tool_calls,
        frona_harness::config::default_tool_concurrency()
    );
    assert!((2..=8).contains(&config.max_concurrent_tool_calls.get()));
    assert!(
        serde_json::from_value::<frona_harness::config::InferenceConfig>(
            json!({"max_concurrent_tool_calls":0})
        )
        .is_err()
    );
    let config: frona_harness::config::InferenceConfig =
        serde_json::from_value(json!({"max_concurrent_tool_calls":1})).unwrap();
    assert_eq!(config.max_concurrent_tool_calls.get(), 1);
}

#[tokio::test]
async fn refusal_does_not_admit_later_calls_even_if_their_budget_would_allow_them() {
    struct RefuseB(DefaultBudget);
    impl frona_harness::Budget for RefuseB {
        fn admit(
            &self,
            operation: frona_harness::BudgetOperation<'_>,
        ) -> Result<frona_harness::BudgetPermit<'_>, frona_harness::BudgetRefusal> {
            if matches!(
                operation,
                frona_harness::BudgetOperation::Tool { name: "b" }
            ) {
                return Err("b is refused".into());
            }
            frona_harness::Budget::admit(&self.0, operation)
        }
    }
    let names = ["a", "b", "c"];
    let (mut tools, mut probe) = setup(&names, None);
    let mut conversation = Conversation::default();
    let actions = record(&mut conversation, &names, &tools);
    let budget = RefuseB(DefaultBudget::default());
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            conversation.execute_tools(
                &actions,
                &mut tools,
                ExecutionControl::new(&budget),
                NonZeroUsize::new(3).unwrap()
            ),
            async {
                assert_eq!(probe.started.recv().await, Some(0));
                probe.release[0].add_permits(1);
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(budget.0.snapshot().tool_calls, 1);
    assert_eq!(budget.0.snapshot().in_flight, 0);
    assert!(probe.started.try_recv().is_err());
}
