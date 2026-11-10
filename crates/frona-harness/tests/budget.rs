use std::sync::{Arc, Barrier, Mutex};

use frona_harness::{
    Budget, BudgetCompletion, BudgetLimit, BudgetLimits, BudgetOperation, BudgetOutcome,
    BudgetPermit, BudgetRefusal, DefaultBudget, UncertainUsagePolicy, UsageReport,
    inference::Usage,
};

fn model() -> BudgetOperation<'static> {
    BudgetOperation::Model {
        provider: "local",
        model: "fake",
    }
}

fn tool() -> BudgetOperation<'static> {
    BudgetOperation::Tool { name: "lookup" }
}

fn reported(tokens: u64) -> BudgetCompletion {
    BudgetCompletion {
        outcome: BudgetOutcome::Succeeded,
        usage: UsageReport::Reported(Usage::new().total_tokens(tokens)),
    }
}

#[test]
fn clones_share_consumption_across_loops_and_new_budgets_do_not() {
    fn run_loop(budget: &dyn Budget) {
        budget.admit(model()).unwrap().complete(reported(11));
        budget.admit(tool()).unwrap().complete(reported(99));
    }
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(2),
        ..BudgetLimits::default()
    });
    run_loop(&budget);
    run_loop(&budget.clone());
    assert_eq!(budget.snapshot().model_attempts, 2);
    assert_eq!(budget.snapshot().tool_calls, 2);
    // Tools do not double-charge usage from nested model calls.
    assert_eq!(budget.snapshot().tokens, 22);
    assert!(matches!(
        budget.admit(model()),
        Err(BudgetRefusal::LimitReached {
            limit: BudgetLimit::ModelAttempts,
            consumed: 2,
            maximum: 2,
        })
    ));
    let independent = DefaultBudget::new(budget.limits());
    run_loop(&independent);
    assert_eq!(independent.snapshot().model_attempts, 1);
    // Exhausted model count does not block a different category.
    budget.admit(tool()).unwrap().complete(reported(0));
}

#[test]
fn tokens_block_each_category_without_debiting_refused_calls() {
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(10),
        ..BudgetLimits::default()
    });
    budget.admit(model()).unwrap().complete(reported(12));
    for operation in [model(), tool()] {
        assert_eq!(
            budget.admit(operation).unwrap_err(),
            BudgetRefusal::LimitReached {
                limit: BudgetLimit::Tokens,
                consumed: 12,
                maximum: 10,
            }
        );
    }
    assert_eq!(budget.snapshot().tool_calls, 0);
    assert_eq!(budget.snapshot().model_attempts, 1);
}

#[test]
fn finite_defaults_zero_limits_and_explicit_disablement() {
    let defaults = DefaultBudget::default();
    assert_eq!(defaults.limits().model_attempts, Some(64));
    assert_eq!(defaults.limits().tool_calls, Some(256));
    assert_eq!(defaults.limits().tokens, Some(1_000_000));
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        tool_calls: Some(0),
        tokens: None,
        ..BudgetLimits::default()
    });
    assert!(matches!(
        budget.admit(model()),
        Err(BudgetRefusal::LimitReached {
            limit: BudgetLimit::ModelAttempts,
            ..
        })
    ));
    assert!(matches!(
        budget.admit(tool()),
        Err(BudgetRefusal::LimitReached {
            limit: BudgetLimit::ToolCalls,
            ..
        })
    ));
    let no_tokens = DefaultBudget::new(BudgetLimits {
        tokens: Some(0),
        ..BudgetLimits::unlimited()
    });
    assert!(matches!(
        no_tokens.admit(tool()),
        Err(BudgetRefusal::LimitReached {
            limit: BudgetLimit::Tokens,
            ..
        })
    ));
    let unlimited = DefaultBudget::new(BudgetLimits::unlimited());
    for _ in 0..300 {
        unlimited.admit(model()).unwrap().complete(reported(10_000));
        unlimited.admit(tool()).unwrap().complete(reported(0));
    }
    assert_eq!(unlimited.snapshot().tokens, 3_000_000);
    assert_eq!(unlimited.snapshot().tool_calls, 300);
}

#[test]
fn concurrent_count_admission_never_overbooks() {
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(1),
        ..BudgetLimits::default()
    });
    let start = Arc::new(Barrier::new(3));
    let admitted = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let budget = budget.clone();
                let start = start.clone();
                let admitted = admitted.clone();
                scope.spawn(move || {
                    start.wait();
                    let permit = budget.admit(tool());
                    admitted.wait();
                    match permit {
                        Ok(permit) => {
                            permit.complete(reported(0));
                            true
                        }
                        Err(BudgetRefusal::LimitReached {
                            limit: BudgetLimit::ToolCalls,
                            ..
                        }) => false,
                        Err(error) => panic!("unexpected refusal: {error}"),
                    }
                })
            })
            .collect();
        start.wait();
        admitted.wait();
        let successes = workers
            .into_iter()
            .map(|worker| u32::from(worker.join().unwrap()))
            .sum::<u32>();
        assert_eq!(successes, 1);
    });
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(budget.snapshot().in_flight, 0);
    // The other category remains usable despite the exhausted tool allowance.
    budget.admit(model()).unwrap().complete(reported(0));
}

#[tokio::test]
async fn admitted_work_finishes_after_another_call_exhausts_tokens() {
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(10),
        ..BudgetLimits::default()
    });
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (release, wait) = tokio::sync::oneshot::channel();
    let worker = {
        let budget = budget.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            let permit = budget.admit(model()).unwrap();
            barrier.wait().await;
            wait.await.unwrap();
            permit.complete(reported(7));
            "completed"
        })
    };
    barrier.wait().await;
    assert_eq!(budget.snapshot().in_flight, 1);
    budget.admit(model()).unwrap().complete(reported(12));
    assert!(budget.admit(tool()).is_err());
    release.send(()).unwrap();
    assert_eq!(worker.await.unwrap(), "completed");
    assert_eq!(budget.snapshot().tokens, 19);
    assert_eq!(budget.snapshot().in_flight, 0);
}

#[test]
fn uncertain_failed_and_abandoned_usage_is_charged_once() {
    let budget = DefaultBudget::default();
    budget.admit(model()).unwrap().complete(reported(0));
    assert_eq!(budget.snapshot().tokens, 0);
    assert_eq!(budget.snapshot().uncertain_model_attempts, 0);
    for usage in [
        UsageReport::Unavailable,
        UsageReport::LegacyNormalized(Usage::new()),
    ] {
        budget.admit(model()).unwrap().complete(BudgetCompletion {
            outcome: BudgetOutcome::Failed,
            usage,
        });
    }
    drop(budget.admit(model()).unwrap());
    drop(budget.admit(tool()).unwrap());
    assert_eq!(budget.snapshot().tokens, 3 * 4096);
    assert_eq!(budget.snapshot().uncertain_model_attempts, 3);
    assert_eq!(budget.snapshot().model_attempts, 4);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(budget.snapshot().in_flight, 0);
}

#[test]
fn known_only_policy_and_large_legacy_values_preserve_known_counts() {
    let legacy = UsageReport::LegacyNormalized(
        Usage::new()
            .input_tokens(3000)
            .output_tokens(2000)
            .cached_input_tokens(1000)
            .reasoning_tokens(500),
    );
    for policy in [
        UncertainUsagePolicy::KnownOnly,
        UncertainUsagePolicy::MinimumCharge(4096),
    ] {
        let budget = DefaultBudget::new(BudgetLimits {
            uncertain_usage: policy,
            ..BudgetLimits::default()
        });
        budget.admit(model()).unwrap().complete(BudgetCompletion {
            outcome: BudgetOutcome::Succeeded,
            usage: legacy.clone(),
        });
        assert_eq!(budget.snapshot().tokens, 5000);
    }
    let budget = DefaultBudget::new(BudgetLimits {
        uncertain_usage: UncertainUsagePolicy::KnownOnly,
        ..BudgetLimits::default()
    });
    drop(budget.admit(model()).unwrap());
    assert_eq!(budget.snapshot().tokens, 0);
    assert_eq!(budget.snapshot().uncertain_model_attempts, 1);
    assert_eq!(budget.snapshot().model_attempts, 1);
}

#[test]
fn token_accumulation_saturates_instead_of_reopening_allowance() {
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(u64::MAX),
        ..BudgetLimits::default()
    });
    let first = budget.admit(model()).unwrap();
    let second = budget.admit(model()).unwrap();
    first.complete(reported(u64::MAX - 1));
    second.complete(reported(100));
    assert_eq!(budget.snapshot().tokens, u64::MAX);
    assert!(budget.admit(tool()).is_err());
}

#[test]
fn custom_trait_policy_receives_original_completion_once_and_can_borrow() {
    struct Custom<'a>(&'a Mutex<Vec<BudgetCompletion>>);
    impl Budget for Custom<'_> {
        fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
            if let BudgetOperation::Tool { name: "forbidden" } = operation {
                return Err(BudgetRefusal::Denied("application policy".into()));
            }
            Ok(BudgetPermit::new(|completion| {
                self.0.lock().unwrap().push(completion)
            }))
        }
    }
    let records = Mutex::new(Vec::new());
    let budget: Arc<dyn Budget + '_> = Arc::new(Custom(&records));
    let name = String::from("local model");
    budget
        .admit(BudgetOperation::Model {
            provider: "local",
            model: &name,
        })
        .unwrap()
        .complete(BudgetCompletion {
            outcome: BudgetOutcome::Cancelled,
            usage: UsageReport::LegacyNormalized(Usage::new().total_tokens(13)),
        });
    drop(budget.admit(tool()).unwrap());
    assert_eq!(
        budget
            .admit(BudgetOperation::Tool { name: "forbidden" })
            .unwrap_err(),
        BudgetRefusal::Denied("application policy".into())
    );
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].outcome, BudgetOutcome::Cancelled);
    assert!(
        matches!(&records[0].usage, UsageReport::LegacyNormalized(usage) if usage.total_tokens == Some(13))
    );
    assert_eq!(records[1].outcome, BudgetOutcome::Abandoned);
    assert!(matches!(records[1].usage, UsageReport::Unavailable));
}

#[test]
fn reported_partial_usage_retains_known_counts_and_applies_uncertainty_policy() {
    for (policy, expected) in [
        (UncertainUsagePolicy::KnownOnly, 100),
        (UncertainUsagePolicy::MinimumCharge(4096), 4096),
    ] {
        let budget = DefaultBudget::new(BudgetLimits {
            uncertain_usage: policy,
            ..BudgetLimits::default()
        });
        budget.admit(model()).unwrap().complete(BudgetCompletion {
            outcome: BudgetOutcome::Succeeded,
            usage: UsageReport::Reported(Usage::new().input_tokens(100)),
        });
        assert_eq!(budget.snapshot().tokens, expected);
        assert_eq!(budget.snapshot().uncertain_model_attempts, 1);
    }
    for usage in [Usage::new(), Usage::new().input_tokens(0)] {
        let budget = DefaultBudget::default();
        budget.admit(model()).unwrap().complete(BudgetCompletion {
            outcome: BudgetOutcome::Succeeded,
            usage: UsageReport::Reported(usage),
        });
        assert_eq!(budget.snapshot().tokens, 4096);
        assert_eq!(budget.snapshot().uncertain_model_attempts, 1);
    }
    for usage in [
        Usage::new().total_tokens(0),
        Usage::new().input_tokens(0).output_tokens(0),
    ] {
        let budget = DefaultBudget::default();
        budget.admit(model()).unwrap().complete(BudgetCompletion {
            outcome: BudgetOutcome::Succeeded,
            usage: UsageReport::Reported(usage),
        });
        assert_eq!(budget.snapshot().tokens, 0);
        assert_eq!(budget.snapshot().uncertain_model_attempts, 0);
    }
}
