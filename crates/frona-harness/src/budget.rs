//! Shared admission and completion accounting, independent of execution drivers.

use std::sync::{Arc, Mutex};

use super::UsageReport;

/// One actual provider attempt or individual tool invocation. Names are borrowed
/// only during admission; no persistent identity or `'static` strings are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetOperation<'a> {
    Model { provider: &'a str, model: &'a str },
    Tool { name: &'a str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLimit {
    ModelAttempts,
    ToolCalls,
    Tokens,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetRefusal {
    #[error("{limit:?} limit reached ({consumed} consumed, maximum {maximum})")]
    LimitReached {
        limit: BudgetLimit,
        consumed: u64,
        maximum: u64,
    },
    /// Application-defined policy rejection, still a terminal control outcome.
    #[error("{0}")]
    Denied(String),
}

impl From<String> for BudgetRefusal {
    fn from(reason: String) -> Self {
        Self::Denied(reason)
    }
}

impl From<&str> for BudgetRefusal {
    fn from(reason: &str) -> Self {
        Self::Denied(reason.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetOutcome {
    Succeeded,
    Failed,
    Cancelled,
    /// The permit was dropped without an explicit completion (including panic
    /// unwinding or a dropped execution future).
    Abandoned,
}

/// Reliable completion accounting. Preserve usage provenance even on failures.
/// Tools supply `Unavailable`; nested model attempts report their own usage via
/// the same budget, rather than charging it again at tool completion.
#[derive(Debug, Clone)]
pub struct BudgetCompletion {
    pub outcome: BudgetOutcome,
    pub usage: UsageReport,
}

/// Replaceable in-process policy. Methods must be short, synchronous, and must
/// not panic. Admission atomically reserves any count allowance before returning.
/// The caller holds the permit across work, then completes it exactly once.
/// Refusal prevents that call from starting; it never cancels existing permits.
///
/// This interface does not intercept calls: execution drivers must invoke it
/// before *each* provider attempt (including retries) and individual tool call.
/// Implementations decide which limits apply and how uncertain usage is charged.
pub trait Budget: Send + Sync {
    fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal>;
}

impl<B: Budget + ?Sized> Budget for Arc<B> {
    fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
        (**self).admit(operation)
    }
}

/// Single-use accounting receipt. No budget lock is held while this exists.
/// Dropping it reports abandoned work with unavailable usage; admitted call
/// counts are never refunded. A custom implementation supplies its completion
/// callback with [`BudgetPermit::new`], which may borrow the budget.
#[must_use = "hold the permit across the admitted call and complete it with its usage"]
pub struct BudgetPermit<'a> {
    complete: Option<Box<dyn FnOnce(BudgetCompletion) + Send + 'a>>,
}

impl<'a> BudgetPermit<'a> {
    pub fn new(complete: impl FnOnce(BudgetCompletion) + Send + 'a) -> Self {
        Self {
            complete: Some(Box::new(complete)),
        }
    }

    pub fn complete(mut self, completion: BudgetCompletion) {
        if let Some(complete) = self.complete.take() {
            complete(completion);
        }
    }
}

impl std::fmt::Debug for BudgetPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetPermit").finish_non_exhaustive()
    }
}

impl Drop for BudgetPermit<'_> {
    fn drop(&mut self) {
        if let Some(complete) = self.complete.take() {
            complete(BudgetCompletion {
                outcome: BudgetOutcome::Abandoned,
                usage: UsageReport::Unavailable,
            });
        }
    }
}

/// Policy for unavailable or legacy-normalized model usage. Reported zero is
/// trusted as zero. Legacy values are uncertain even if they are nonzero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncertainUsagePolicy {
    /// Charge at least this many tokens for each uncertain model completion.
    /// This is a configurable accounting heuristic, not a token upper bound.
    MinimumCharge(u64),
    /// Count known legacy values; unavailable usage adds zero tokens. Finite
    /// call limits still apply unless the developer also disables them.
    KnownOnly,
}

/// Limits apply to the lifetime of one shared budget, across all assigned runs.
/// `None` disables a dimension; `Some(0)` refuses its first applicable call.
/// Model/tool counts apply only to that category. Tokens apply to *both*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetLimits {
    pub model_attempts: Option<u64>,
    pub tool_calls: Option<u64>,
    pub tokens: Option<u64>,
    pub uncertain_usage: UncertainUsagePolicy,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            model_attempts: Some(64),
            tool_calls: Some(256),
            tokens: Some(1_000_000),
            uncertain_usage: UncertainUsagePolicy::MinimumCharge(4_096),
        }
    }
}

impl BudgetLimits {
    /// Disable admission limits, retaining consumption accounting.
    pub fn unlimited() -> Self {
        Self {
            model_attempts: None,
            tool_calls: None,
            tokens: None,
            ..Self::default()
        }
    }
}

/// Aggregate counters, not a per-run activity report. Values saturate at u64::MAX.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub model_attempts: u64,
    pub tool_calls: u64,
    /// Settled charges under the selected policy, including uncertain usage.
    pub tokens: u64,
    pub uncertain_model_attempts: u64,
    pub in_flight: u64,
}

/// Finite default policy with shared consumption. Cloning shares state; calling
/// `new` or `default` creates an independent allowance. Counts are reserved at
/// admission, even for failed/abandoned calls. Tokens settle at completion, so
/// simultaneous in-flight work may exceed the threshold. No permit is revoked.
#[derive(Debug, Clone)]
pub struct DefaultBudget {
    limits: BudgetLimits,
    state: Arc<Mutex<BudgetSnapshot>>,
}

impl Default for DefaultBudget {
    fn default() -> Self {
        Self::new(BudgetLimits::default())
    }
}

impl DefaultBudget {
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            limits,
            state: Arc::new(Mutex::new(BudgetSnapshot::default())),
        }
    }

    pub fn limits(&self) -> BudgetLimits {
        self.limits
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Budget for DefaultBudget {
    fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
        let model = matches!(operation, BudgetOperation::Model { .. });
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            check(BudgetLimit::Tokens, self.limits.tokens, state.tokens)?;
            if model {
                check(
                    BudgetLimit::ModelAttempts,
                    self.limits.model_attempts,
                    state.model_attempts,
                )?;
                state.model_attempts = state.model_attempts.saturating_add(1);
            } else {
                check(
                    BudgetLimit::ToolCalls,
                    self.limits.tool_calls,
                    state.tool_calls,
                )?;
                state.tool_calls = state.tool_calls.saturating_add(1);
            }
            state.in_flight = state.in_flight.saturating_add(1);
        }
        Ok(BudgetPermit::new(move |completion| {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.in_flight = state.in_flight.saturating_sub(1);
            if model {
                let (tokens, uncertain) =
                    token_charge(&completion.usage, self.limits.uncertain_usage);
                state.tokens = state.tokens.saturating_add(tokens);
                state.uncertain_model_attempts = state
                    .uncertain_model_attempts
                    .saturating_add(u64::from(uncertain));
            }
        }))
    }
}

fn check(limit: BudgetLimit, maximum: Option<u64>, consumed: u64) -> Result<(), BudgetRefusal> {
    if let Some(maximum) = maximum
        && consumed >= maximum
    {
        return Err(BudgetRefusal::LimitReached {
            limit,
            consumed,
            maximum,
        });
    }
    Ok(())
}

fn token_charge(usage: &UsageReport, policy: UncertainUsagePolicy) -> (u64, bool) {
    let known = match usage {
        UsageReport::Reported(usage) | UsageReport::LegacyNormalized(usage) => {
            // Prefer the larger supplied total; do not add cache/reasoning
            // subsets to their parent input/output counts a second time.
            usage
                .total_tokens
                .max(usage.input_tokens.saturating_add(usage.output_tokens))
        }
        UsageReport::Unavailable => 0,
    };
    let uncertain = !matches!(usage, UsageReport::Reported(_));
    let charge = match (uncertain, policy) {
        (true, UncertainUsagePolicy::MinimumCharge(minimum)) => known.max(minimum),
        _ => known,
    };
    (charge, uncertain)
}
