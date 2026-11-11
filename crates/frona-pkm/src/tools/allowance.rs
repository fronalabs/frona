use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// A phase-local research allowance shared by related tools. This does not replace
/// harness execution budgets. Attempts include refused calls for research diagnostics.
#[derive(Debug, Clone)]
pub(crate) struct ResearchAllowance {
    limit: usize,
    attempts: Arc<AtomicUsize>,
}
impl ResearchAllowance {
    pub(crate) fn new(limit: usize) -> Self {
        Self::with_counter(limit, Arc::new(AtomicUsize::new(0)))
    }
    pub(crate) fn with_counter(limit: usize, attempts: Arc<AtomicUsize>) -> Self {
        Self { limit, attempts }
    }
    pub(crate) fn try_acquire(&self) -> bool {
        let before = self
            .attempts
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            })
            .expect("counter update always succeeds");
        before < self.limit
    }
    pub(crate) fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.attempts())
    }
    pub(crate) fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }
}

#[cfg(test)]
mod tests;
