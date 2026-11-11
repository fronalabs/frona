//! Reconcile one effective entity against a host-prepared graph snapshot.
//! The host owns the outer worklist, re-resolution after identity changes, atomic
//! persistence, profile updates, and retries against a refreshed snapshot.
mod inspection;
mod output;
mod projection;
mod prompts;
mod run;
mod submission;
mod validation;

use crate::{Error, GraphSnapshot, Phase, classify::ProposalSet, ontology::Ontology};
use frona_harness::{Harness, Run, Text};
pub use output::*;
pub use prompts::ReconcilePrompts;
use submission::*;

/// All reads needed for a single reconciliation. `graph` must include every
/// entity/edge participating in validation and all active/historical memories for
/// `path`, including source memories staged by classification. `graph.memory_entities`
/// must contain the complete scopes of those memories; scope equality determines
/// whether a global duplicate/absorption is safe.
#[derive(Clone)]
pub struct ReconcileInput {
    pub path: String,
    pub graph: GraphSnapshot,
    pub ontology: Ontology,
    pub proposals: ProposalSet,
    pub initial_rejection: Option<String>,
}

pub struct Reconcile<'a> {
    harness: Harness<'a>,
    prompts: ReconcilePrompts,
    max_submissions: usize,
}

impl<'a> Reconcile<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: ReconcilePrompts::default(),
            max_submissions: 10,
        }
    }
    pub fn prompts(mut self, prompts: ReconcilePrompts) -> Self {
        self.prompts = prompts;
        self
    }
    pub fn submission_attempts(mut self, maximum: usize) -> Self {
        self.max_submissions = maximum;
        self
    }
    pub async fn run(&self, input: ReconcileInput) -> Result<ReconcileOutput, Error> {
        self.run_with(input, |run| run).await
    }
    /// Apply caller budgets, cancellation, telemetry, or compaction policy.
    pub async fn run_with<'run, F>(
        &'run self,
        input: ReconcileInput,
        configure: F,
    ) -> Result<ReconcileOutput, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        let path = input.path.clone();
        let mut output = run::ReconcileRun {
            prefixes: input.ontology.prefixes(),
            input: std::sync::Arc::new(input),
            prompts: self.prompts.clone(),
            harness: &self.harness,
            max_submissions: self.max_submissions,
        }
        .run(configure)
        .await?;
        if output.entity_update.is_some()
            && let Some(mut entity) = output.proposals.input_entity(&path)
        {
            entity.lifecycle = crate::model::ConsolidationEntityLifecycle::Active;
            entity.rederive_search();
            entity.validate()?;
            output.proposals.stage_input_entity(entity);
        }
        Ok(output)
    }
}

impl Phase for Reconcile<'_> {
    type Input = ReconcileInput;
    type Output = ReconcileOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Reconcile::run(self, input).await
    }
}

fn comparison_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests;
