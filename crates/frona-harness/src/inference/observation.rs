//! Optional execution telemetry, independent of persistent identity and history.
//!
//! Errors returned by inference observers are logged and never become an
//! inference or required history-storage failure. They are not budget admission.
//!
//! `Observations::default()` disables telemetry. A caller can attach a local
//! observer and arbitrary attribution tags without creating identity records.
//! Persistent callers bind their existing usage service/context through
//! `UsageService::scoped`; only that adapter knows their stored identity schema.
//!
//! Attempt callbacks describe each provider invocation. Usage callbacks retain
//! the existing successful-operation recording boundary and retry metadata.
//! Direct structured inference emits unavailable usage. The remaining legacy
//! adapter normalization (omitted wire usage becoming zero) must be addressed
//! at the provider contract in HSDK-04, not guessed from numeric token values.
//! These callbacks are not the reliable history or final report mechanisms.

use crate::inference::{Usage, error::InferenceError, provider::ModelConfig};
use async_trait::async_trait;

/// Timing of the recorded successful model operation, including prior retries.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyMetrics {
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub retry_overhead_ms: u64,
    pub retry_count: u32,
}

pub type ObservationError = Box<dyn std::error::Error + Send + Sync>;

/// A completed model operation. `None` means usage was unavailable, not zero.
/// `Some` is the adapter's normalized usage; legacy adapters may normalize
/// omitted wire usage to zero. This is not proof of wire-level presence.
pub struct UsageObservation<'a> {
    pub model: &'a ModelConfig,
    pub usage: Option<&'a Usage>,
    pub fallback_index: u8,
    pub latency: LatencyMetrics,
    pub attribution: &'a [(&'a str, &'a str)],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl AttemptOutcome {
    pub fn from_result<T>(result: &Result<T, InferenceError>) -> Self {
        match result {
            Ok(_) => Self::Succeeded,
            Err(InferenceError::Cancelled(_)) => Self::Cancelled,
            Err(_) => Self::Failed,
        }
    }
}

/// One actual provider invocation, before retry/fallback classification.
pub struct AttemptObservation<'a> {
    pub model: &'a ModelConfig,
    pub outcome: AttemptOutcome,
    pub usage: Option<&'a Usage>,
    pub duration_ms: u64,
    pub attribution: &'a [(&'a str, &'a str)],
}

#[async_trait]
pub trait InferenceObserver: Send + Sync {
    async fn usage(&self, _observation: UsageObservation<'_>) -> Result<(), ObservationError> {
        Ok(())
    }

    async fn attempt(&self, _observation: AttemptObservation<'_>) -> Result<(), ObservationError> {
        Ok(())
    }
}

/// Borrowed, optional observation configuration. No identity records are needed.
#[derive(Clone, Copy, Default)]
pub struct Observations<'a> {
    observer: Option<&'a dyn InferenceObserver>,
    attribution: &'a [(&'a str, &'a str)],
}

impl<'a> Observations<'a> {
    pub fn new(observer: &'a dyn InferenceObserver) -> Self {
        Self {
            observer: Some(observer),
            attribution: &[],
        }
    }

    pub fn with_attribution(mut self, attribution: &'a [(&'a str, &'a str)]) -> Self {
        self.attribution = attribution;
        self
    }

    pub async fn usage(
        self,
        model: &ModelConfig,
        usage: Option<&Usage>,
        fallback_index: u8,
        latency: LatencyMetrics,
    ) {
        if let Some(observer) = self.observer
            && let Err(error) = observer
                .usage(UsageObservation {
                    model,
                    usage,
                    fallback_index,
                    latency,
                    attribution: self.attribution,
                })
                .await
        {
            tracing::warn!(%error, "inference usage observer failed");
        }
    }

    pub async fn attempt(
        self,
        model: &ModelConfig,
        outcome: AttemptOutcome,
        usage: Option<&Usage>,
        duration_ms: u64,
    ) {
        if let Some(observer) = self.observer
            && let Err(error) = observer
                .attempt(AttemptObservation {
                    model,
                    outcome,
                    usage,
                    duration_ms,
                    attribution: self.attribution,
                })
                .await
        {
            tracing::warn!(%error, "inference attempt observer failed");
        }
    }
}

/// Optional activity transport. The persistent integration supplies broadcasts.
pub trait ActivityObserver: Send + Sync {
    fn active_requests(&self, count: usize);
}

impl<F: Fn(usize) + Send + Sync> ActivityObserver for F {
    fn active_requests(&self, count: usize) {
        self(count);
    }
}
