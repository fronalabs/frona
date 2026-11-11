//! Classify one effective entity and validate its complete ontology proposal.
//! The host owns worklists, checkpoints, and persistence of accepted results. Research
//! reads the supplied snapshot; no schema, entities, links, or facts are written.
mod logic;
mod prompts;
mod proposal;
mod run;

use crate::{Error, GraphSnapshot, Phase, ontology::Ontology};
use frona_harness::{Harness, Run, Text};
pub use prompts::ClassifyPrompts;
pub use proposal::{
    AttributeMapping, ClassChoice, Classification, EntityProposal, EntityShape, HasKeyMarker,
    NewEntity, OntologyDeclaration, ProposalSet, RelationMapping,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct ClassifyInput {
    pub path: String,
    /// Effective entities/links and complete memory attachment scopes. Include
    /// pending source memories even if they are not yet attached to the entity.
    pub graph: GraphSnapshot,
    pub ontology: Ontology,
    pub proposals: ProposalSet,
    pub initial_rejection: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClassifyOutput {
    /// `None` means semantic rejection (empty classification or exhausted
    /// validation submissions). Interrupted execution returns an error instead.
    pub classification: Option<Classification>,
    pub proposals: ProposalSet,
    pub minted_paths: Vec<String>,
    pub diagnostic: serde_json::Value,
}

pub struct Classify<'a> {
    harness: Harness<'a>,
    prompts: ClassifyPrompts,
    max_submissions: usize,
    max_tool_turns: usize,
    research_tool_budget: usize,
}
impl<'a> Classify<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: ClassifyPrompts::default(),
            max_submissions: 10,
            max_tool_turns: 10,
            research_tool_budget: usize::MAX,
        }
    }
    pub fn prompts(mut self, prompts: ClassifyPrompts) -> Self {
        self.prompts = prompts;
        self
    }
    pub fn submission_attempts(mut self, maximum: usize) -> Self {
        self.max_submissions = maximum;
        self
    }
    pub fn exploration_turns(mut self, maximum: usize) -> Self {
        self.max_tool_turns = maximum;
        self
    }
    pub fn research_tool_budget(mut self, maximum: usize) -> Self {
        self.research_tool_budget = maximum;
        self
    }
    pub async fn run(&self, input: ClassifyInput) -> Result<ClassifyOutput, Error> {
        self.run_with(input, |run| run).await
    }
    /// Apply shared budgets, cancellation, tracing, or compaction policy.
    pub async fn run_with<'run, F>(
        &'run self,
        input: ClassifyInput,
        configure: F,
    ) -> Result<ClassifyOutput, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        run::run(self, input, configure).await
    }
}
impl Phase for Classify<'_> {
    type Input = ClassifyInput;
    type Output = ClassifyOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Classify::run(self, input).await
    }
}
fn claim_automatic_identity_discovery(discovery_calls: usize, challenged: &AtomicBool) -> bool {
    discovery_calls == 0 && !challenged.swap(true, Ordering::Relaxed)
}
#[cfg(test)]
mod tests;
