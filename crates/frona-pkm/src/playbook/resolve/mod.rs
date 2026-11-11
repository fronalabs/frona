//! Resolve one provisional procedural-memory group. The caller drives the queue,
//! includes accepted staged targets in each next snapshot, and commits resolutions.
use super::KnowledgeSnapshot;
use crate::prompt::PromptIds;
use crate::{
    Error, Phase,
    ingest::PendingPlaybookCandidate,
    model::{EntityCategory, KnowledgeConsolidationEntity, KnowledgeMemory, PLAYBOOK_KIND_IRI},
    path::normalize_path,
};
use frona_harness::{FailureKind, Harness, Run, RunOutcome, Text};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};
mod inspection;
mod prompts;
use inspection::PlaybookInspection;
pub use prompts::PlaybookResolvePrompts;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolvedPlaybook {
    pub existing_path: Option<String>,
    #[serde(default)]
    pub merge_from: Vec<String>,
    pub path: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub memory_ids: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct Verdict {
    #[serde(default)]
    playbooks: Vec<ResolvedPlaybook>,
}

#[derive(Debug, Clone)]
pub struct PlaybookResolveInput {
    pub candidate: PendingPlaybookCandidate,
    pub memories: Vec<KnowledgeMemory>,
    /// Effective snapshot after any previously accepted resolutions. Unresolved
    /// provisional groups must be excluded, except for this candidate.
    pub knowledge: KnowledgeSnapshot,
    /// Targets already inspected and accepted in this caller-owned run.
    pub staged_paths: BTreeSet<String>,
}

pub struct PlaybookResolve<'a> {
    harness: Harness<'a>,
    prompts: PlaybookResolvePrompts,
    max_tool_turns: usize,
    max_submissions: usize,
}
impl<'a> PlaybookResolve<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: PlaybookResolvePrompts::default(),
            max_tool_turns: 20,
            max_submissions: 20,
        }
    }
    pub fn prompts(mut self, prompts: PlaybookResolvePrompts) -> Self {
        self.prompts = prompts;
        self
    }
    pub fn exploration_turns(mut self, maximum: usize) -> Self {
        self.max_tool_turns = maximum;
        self
    }
    pub fn submission_attempts(mut self, maximum: usize) -> Self {
        self.max_submissions = maximum;
        self
    }
    pub async fn run(&self, input: PlaybookResolveInput) -> Result<Vec<ResolvedPlaybook>, Error> {
        self.run_with(input, |run| run).await
    }
    pub async fn run_with<'run, F>(
        &'run self,
        mut input: PlaybookResolveInput,
        configure: F,
    ) -> Result<Vec<ResolvedPlaybook>, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        let candidate = &input.candidate;
        let memories: Vec<_> = input
            .memories
            .into_iter()
            .filter(|memory| candidate.source_memory_ids.contains(&memory.id))
            .collect();
        let ids = PromptIds::new("m", memories.iter().map(|memory| memory.id.clone()));
        let memory_block = memories
            .iter()
            .map(|memory| format!("- {}: {}", ids.local(&memory.id), memory.content))
            .collect::<Vec<_>>()
            .join("\n");
        let existing_by_path: BTreeMap<String, EntityCategory> = input
            .knowledge
            .entities
            .iter()
            .map(|entity| (entity.path.clone(), entity.category))
            .collect();
        if let Some(entity) = input
            .knowledge
            .entities
            .iter_mut()
            .find(|entity| entity.path == candidate.path)
        {
            entity.category = EntityCategory::Playbook;
            entity.kinds = vec![PLAYBOOK_KIND_IRI.into()];
            entity.name = candidate.name.clone();
            entity.description = candidate.description.clone();
            entity
                .source_memory_ids
                .extend(memories.iter().map(|memory| memory.id.clone()));
            entity.source_memory_ids.sort();
            entity.source_memory_ids.dedup();
        } else {
            let mut entity = KnowledgeConsolidationEntity::pending(
                "",
                "",
                &candidate.path,
                EntityCategory::Playbook,
                vec![],
                candidate.source_memory_ids.clone(),
            );
            entity.kinds = vec![PLAYBOOK_KIND_IRI.into()];
            entity.name = candidate.name.clone();
            entity.description = candidate.description.clone();
            input.knowledge.entities.push(entity.as_knowledge_entity());
        }
        // Only a newly staged candidate has a complete memory view supplied by
        // this input alone. A pre-existing entity still requires its host snapshot.
        if !existing_by_path.contains_key(&candidate.path) {
            input
                .knowledge
                .memories_by_path
                .entry(candidate.path.clone())
                .or_default();
        }
        if let Some(entry) = input.knowledge.memories_by_path.get_mut(&candidate.path) {
            for memory in &memories {
                if !entry.iter().any(|known| known.id == memory.id) {
                    entry.push(memory.clone());
                }
            }
        }
        let existing = super::tools::ranked_playbooks(
            &input.knowledge,
            &candidate.path,
            &candidate.name,
            None,
        );
        let existing_lines=existing.iter().map(|candidate|format!(
            "- path={} name={} description={} exact_name={} token_containment={} similarity={:.3}",
            candidate.entity.path,candidate.entity.name,candidate.entity.description,candidate.evidence.exact_name,candidate.evidence.token_containment,
            candidate.evidence.ordered_similarity.max(candidate.evidence.token_order_similarity))).collect::<Vec<_>>();
        let existing_block = if existing_lines.is_empty() {
            "(none)".into()
        } else {
            existing_lines.join("\n")
        };
        let candidate_block = format!(
            "path={}\nname={}\ndescription={}",
            candidate.path, candidate.name, candidate.description
        );
        let vars = [
            ("candidate", candidate_block.as_str()),
            ("memories", memory_block.as_str()),
            ("existing_playbooks", existing_block.as_str()),
        ];
        let lookup = Arc::new(super::tools::PlaybookLookup::default());
        let memory_lookup: HashMap<_, _> = memories
            .iter()
            .map(|memory| (ids.local(&memory.id).to_string(), memory.clone()))
            .collect();
        let tools = super::tools::resolve_registry(
            Arc::new(input.knowledge),
            memory_lookup,
            lookup.clone(),
            candidate.path.clone(),
        )?;
        let inspection = Arc::new(PlaybookInspection {
            prompts: self.prompts.clone(),
            ids,
            allowed: memories.iter().map(|memory| memory.id.clone()).collect(),
            existing_by_path,
            playbook_lookup: lookup,
            staged_paths: input.staged_paths,
        });
        let (system, prompt) = self.prompts.render(&vars)?;
        configure(self.harness.run(prompt).instructions(system).tools(tools))
            .exploration_turns(self.max_tool_turns)
            .structured::<Verdict>()
            .submission_attempts(self.max_submissions)
            .inspect(move |verdict| {
                let inspection = inspection.clone();
                async move { inspection.inspect(verdict) }
            })
            .await
            .map_err(|incomplete| {
                if matches!(
                    incomplete.report.outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Cancelled,
                        ..
                    })
                ) {
                    Error::Cancelled(incomplete.reason.to_string())
                } else {
                    Error::Internal(format!(
                        "playbook resolution incomplete: {}",
                        incomplete.reason
                    ))
                }
            })
    }
}
impl Phase for PlaybookResolve<'_> {
    type Input = PlaybookResolveInput;
    type Output = Vec<ResolvedPlaybook>;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        PlaybookResolve::run(self, input).await
    }
}

/// Fold accepted proposals in the caller's deterministic candidate order. Latest
/// metadata wins; every earlier memory assignment and merge source is preserved.
pub fn coalesced_targets(
    proposals: &BTreeMap<String, Vec<ResolvedPlaybook>>,
) -> Vec<ResolvedPlaybook> {
    let mut targets = BTreeMap::<String, ResolvedPlaybook>::new();
    for proposal in proposals.values() {
        for mut target in proposal.iter().cloned() {
            if let Some(earlier) = targets.remove(&target.path) {
                target.memory_ids.extend(earlier.memory_ids);
                target.memory_ids.sort();
                target.memory_ids.dedup();
                target.merge_from.extend(earlier.merge_from);
                target.merge_from.sort();
                target.merge_from.dedup();
                if target.existing_path.is_none() {
                    target.existing_path = earlier.existing_path;
                }
            }
            targets.insert(target.path.clone(), target);
        }
    }
    targets.into_values().collect()
}

#[cfg(test)]
mod tests;
