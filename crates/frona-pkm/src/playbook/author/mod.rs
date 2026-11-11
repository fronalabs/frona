//! Author one canonical playbook projection from supplied knowledge and evidence.
//! The returned page is uncommitted; the host owns CAS, dirty flags, and file mirrors.
use super::{KnowledgeSnapshot, MessageRole};
use crate::prompt::prompt_evidence;
use crate::{
    Error, Phase,
    model::{
        EntityCategory, EvidenceSource, KnowledgeEntity, KnowledgeEntityLink, KnowledgeMemory,
        MemoryKind,
    },
    ontology::PrefixMap,
    page::{
        PageLayout,
        projection::{
            MarkdownPage, canonical_path, canonicalize_wikilinks, compose_page, sha256_hex,
        },
    },
};
use frona_harness::{Decision, FailureKind, Harness, Run, RunOutcome, Text};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
mod evidence;
mod inspection;
mod prompts;
use inspection::{inspect_submission, require_requested_path};
pub use prompts::PlaybookAuthorPrompts;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AuthoredPlaybook {
    name: String,
    description: String,
    body: String,
    #[serde(default)]
    related_playbooks: Vec<String>,
}
struct InvocationEvidence {
    id: String,
    tool: String,
    reference: String,
}

#[derive(Debug, Clone)]
pub struct PlaybookAuthorInput {
    /// The requested output identity, checked against `page.path` to prevent stale
    /// redirects from writing a canonical page under the old identity.
    pub path: String,
    pub page: KnowledgeEntity,
    pub memories: Vec<KnowledgeMemory>,
    pub knowledge: KnowledgeSnapshot,
    pub links: Vec<KnowledgeEntityLink>,
    pub redirects: BTreeMap<String, String>,
    pub layout: PageLayout,
    pub prefixes: PrefixMap,
}

/// Canonical authored bytes and their revision, ready for one host-owned commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookAuthorOutput {
    pub path: String,
    pub name: String,
    pub description: String,
    pub attributes: serde_json::Value,
    pub body: String,
    pub related_playbooks: Vec<String>,
    pub content: String,
    pub rev: String,
}

pub struct PlaybookAuthor<'a> {
    harness: Harness<'a>,
    prompts: PlaybookAuthorPrompts,
    max_tool_turns: usize,
    max_submissions: usize,
}
impl<'a> PlaybookAuthor<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: PlaybookAuthorPrompts::default(),
            max_tool_turns: 20,
            max_submissions: 20,
        }
    }
    pub fn prompts(mut self, prompts: PlaybookAuthorPrompts) -> Self {
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
    pub async fn run(&self, input: PlaybookAuthorInput) -> Result<PlaybookAuthorOutput, Error> {
        self.run_with(input, |run| run).await
    }
    pub async fn run_with<'run, F>(
        &'run self,
        mut input: PlaybookAuthorInput,
        configure: F,
    ) -> Result<PlaybookAuthorOutput, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        input.knowledge.redirects.extend(input.redirects.clone());
        let page = &input.page;
        require_requested_path(&input.path, &page.path)?;
        if page.category != EntityCategory::Playbook {
            return Err(Error::Internal(
                "playbook author requires a Playbook entity".into(),
            ));
        }
        // The requested page and its supplied memories are authoritative within
        // the otherwise caller-prepared effective snapshot.
        input
            .knowledge
            .entities
            .retain(|entity| entity.path != page.path);
        input.knowledge.entities.push(page.clone());
        let source_memories = input
            .knowledge
            .memories_by_path
            .entry(page.path.clone())
            .or_default();
        for memory in &input.memories {
            if !source_memories.iter().any(|known| known.id == memory.id) {
                source_memories.push(memory.clone());
            }
        }
        let procedural: Vec<_> = input
            .memories
            .iter()
            .filter(|memory| memory.kind == MemoryKind::Procedural)
            .collect();
        let memory_block = if procedural.is_empty() {
            "(none)".to_string()
        } else {
            procedural
                .iter()
                .map(|memory| {
                    format!(
                        "- {}\n  evidence: {}",
                        memory.content,
                        prompt_evidence(&memory.evidence)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (transcript, invocations, transcript_lookup) =
            evidence::evidence_context(&input.knowledge, &procedural)?;
        let invocation_block = if invocations.is_empty() {
            "(none)".to_string()
        } else {
            invocations
                .iter()
                .enumerate()
                .map(|(index, invocation)| {
                    format!(
                        "- c{}: {} {}",
                        index + 1,
                        invocation.tool,
                        invocation.reference
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let vars = [
            ("path", page.path.as_str()),
            ("name", page.name.as_str()),
            ("description", page.description.as_str()),
            ("body", page.body.as_str()),
            ("memories", memory_block.as_str()),
            ("transcript", transcript.as_str()),
            ("invocations", invocation_block.as_str()),
        ];
        let allowed = invocations
            .iter()
            .enumerate()
            .map(|(index, invocation)| (format!("c{}", index + 1), invocation.id.clone()))
            .collect();
        let lookup = Arc::new(super::tools::PlaybookLookup::default());
        let tools = super::tools::author_registry(
            Arc::new(input.knowledge.clone()),
            lookup,
            transcript_lookup,
            allowed,
            input.prefixes.clone(),
        )?;
        let prompts = self.prompts.clone();
        let (system, prompt) = self.prompts.render(&vars)?;
        let authored = configure(self.harness.run(prompt).instructions(system).tools(tools))
            .exploration_turns(self.max_tool_turns)
            .structured::<AuthoredPlaybook>()
            .submission_attempts(self.max_submissions)
            .inspect(move |candidate| {
                let prompts = prompts.clone();
                async move { inspect_submission(candidate, &prompts) }
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
                        "playbook author: `{}` incomplete: {}",
                        input.path, incomplete.reason
                    ))
                }
            })?;
        let mut related = Vec::new();
        for proposed in authored.related_playbooks {
            let Some(candidate) = crate::path::normalize_path(&proposed) else {
                continue;
            };
            let candidate = canonical_path(&candidate, &input.redirects);
            if input.knowledge.entities.iter().any(|entity| {
                entity.path == candidate && entity.category == EntityCategory::Playbook
            }) {
                related.push(candidate);
            }
        }
        related.sort();
        related.dedup();
        let mut projected = page.clone();
        if projected.name != authored.name {
            projected.aliases.insert(projected.name.clone());
            projected.name = authored.name.clone();
        }
        projected.description = authored.description.clone();
        let body = canonicalize_wikilinks(&authored.body, &input.redirects, &input.layout);
        projected.body = body.clone();
        projected.related_playbooks = related.clone();
        let article = MarkdownPage::parse(&body);
        let file = compose_page(
            &projected,
            &article,
            &projected.attributes,
            &input.links,
            &input.prefixes,
            &input.layout,
        );
        let rev = sha256_hex(&file);
        Ok(PlaybookAuthorOutput {
            path: input.path,
            name: authored.name,
            description: authored.description,
            attributes: projected.attributes,
            body,
            related_playbooks: related,
            content: file,
            rev,
        })
    }
}
impl Phase for PlaybookAuthor<'_> {
    type Input = PlaybookAuthorInput;
    type Output = PlaybookAuthorOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        PlaybookAuthor::run(self, input).await
    }
}
#[cfg(test)]
mod tests;
