//! Plan memory changes from a human page edit. The caller owns revision checks and
//! commits the returned operations together with the adopted page content.
mod diff;
mod output;
mod prompts;
mod submission;
mod validation;

use crate::prompt::PromptIds;
use crate::{
    Error, Phase,
    model::{Disposition, KnowledgeMemory},
};
pub use diff::writeback_diff;
use frona_harness::{Harness, Run, Text};
pub use output::PageEditMemoryOp;
pub use prompts::WritebackPrompts;
use std::collections::HashSet;
use submission::WriteBackOps;
use validation::validated_writeback_ops;

pub struct WritebackInput {
    pub path: String,
    pub head: String,
    pub incoming: String,
    pub memories: Vec<KnowledgeMemory>,
}

pub struct Writeback<'a> {
    harness: Harness<'a>,
    prompts: WritebackPrompts,
}
impl<'a> Writeback<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: WritebackPrompts::default(),
        }
    }
    pub fn prompts(mut self, prompts: WritebackPrompts) -> Self {
        self.prompts = prompts;
        self
    }
    pub async fn run(&self, input: WritebackInput) -> Result<Vec<PageEditMemoryOp>, Error> {
        self.run_with(input, |run| run).await
    }
    pub async fn run_with<'run, F>(
        &'run self,
        input: WritebackInput,
        configure: F,
    ) -> Result<Vec<PageEditMemoryOp>, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        let diff = writeback_diff(&input.head, &input.incoming);
        if diff.trim().is_empty() {
            return Ok(Vec::new());
        }
        let memories = input
            .memories
            .iter()
            .filter(|m| m.disposition != Disposition::Erroneous)
            .collect::<Vec<_>>();
        let valid_ids: HashSet<String> = memories.iter().map(|m| m.id.clone()).collect();
        let ids = PromptIds::new("m", memories.iter().map(|m| m.id.clone()));
        let mut lines = String::new();
        for memory in memories {
            lines.push_str(&format!(
                "{} | {:?} | {}\n",
                ids.local(&memory.id),
                memory.kind,
                memory.content
            ));
        }
        if lines.is_empty() {
            lines.push_str("(none)");
        }
        let (system, prompt) = self
            .prompts
            .render(&[("memories", lines.trim()), ("diff", diff.trim())])?;
        let mut parsed = configure(self.harness.run(prompt).instructions(system))
            .structured::<WriteBackOps>()
            .submission_attempts(1)
            .await
            .map_err(|incomplete| {
                if matches!(
                    incomplete.report.outcome,
                    Some(frona_harness::RunOutcome::Incomplete {
                        kind: frona_harness::FailureKind::Cancelled,
                        ..
                    })
                ) {
                    Error::Cancelled(incomplete.reason.to_string())
                } else {
                    Error::Internal(format!(
                        "pkm writeback planning incomplete: {}",
                        incomplete.reason
                    ))
                }
            })?;
        for operation in &mut parsed.ops {
            if !operation.memory_id.trim().is_empty() {
                operation.memory_id = ids.expand(&operation.memory_id)?;
            }
        }
        Ok(validated_writeback_ops(&input.path, parsed, &valid_ids))
    }
}
impl Phase for Writeback<'_> {
    type Input = WritebackInput;
    type Output = Vec<PageEditMemoryOp>;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Writeback::run(self, input).await
    }
}

#[cfg(test)]
mod tests;
