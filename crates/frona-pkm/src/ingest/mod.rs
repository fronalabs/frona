//! Extract grounded memories and untyped entity mentions from one transcript.
//! The caller seeds the owner entity, scopes evidence, and commits the returned
//! batch together with its watermark. This phase performs no persistence.
mod cleanup;
mod correction;
mod evidence;
mod input;
mod output;
mod prepare;
mod prompts;
mod run;
mod submission;
mod temporal;
mod tools;
mod validation;

pub use input::{EntitySummary, IngestInput};
pub use output::{
    IngestBatch, PendingEntity, PendingEntityUpdate, PendingMemory, PendingPlaybookCandidate,
};
pub use prompts::IngestPrompts;

use crate::{Error, Phase};
use frona_harness::{Harness, Run, Text};

/// Reusable extraction phase. Every invocation starts an independent dialogue.
pub struct Ingest<'a> {
    harness: Harness<'a>,
    prompts: IngestPrompts,
}

impl<'a> Ingest<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: IngestPrompts::default(),
        }
    }

    pub fn prompts(mut self, prompts: IngestPrompts) -> Self {
        self.prompts = prompts;
        self
    }

    pub async fn run(&self, input: IngestInput) -> Result<IngestBatch, Error> {
        self.run_with(input, |run| run).await
    }

    /// Configure SDK execution directly: shared budgets, cancellation, observers,
    /// or compaction. Phase-owned submission and exploration limits are applied
    /// afterward. No server runtime is required.
    pub async fn run_with<'run, F>(
        &'run self,
        input: IngestInput,
        configure: F,
    ) -> Result<IngestBatch, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        run::IngestRun {
            input: std::sync::Arc::new(input),
            prompts: self.prompts.clone(),
            harness: &self.harness,
        }
        .run(configure)
        .await
    }
}

impl Phase for Ingest<'_> {
    type Input = IngestInput;
    type Output = IngestBatch;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Ingest::run(self, input).await
    }
}

#[cfg(test)]
mod tests;
