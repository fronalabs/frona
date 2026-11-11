//! Author one canonical wiki page from a caller-provided knowledge snapshot.
//! Scheduling, dirty markers, transactional commits, and filesystem mirroring stay
//! with the caller. The output pairs the exact canonical bytes with their revision.
pub mod projection;
mod prompts;
mod render;
mod tools;

use crate::{
    Error, Phase,
    model::{KnowledgeEntity, KnowledgeEntityLink, KnowledgeMemory, classify_memories},
    ontology::PrefixMap,
};
use frona_harness::{Harness, Run, Text};
use projection::{MarkdownPage, canonicalize_wikilinks, compose_page, sha256_hex};
pub use prompts::PageAuthorPrompts;
use render::{deterministic_body, list_or, replacement_allowed};
use std::collections::{BTreeMap, HashMap};
use tokio_util::sync::CancellationToken;

/// Pure display settings; no user account or filesystem root is required.
#[derive(Debug, Clone)]
pub struct PageLayout {
    directory: String,
}
impl Default for PageLayout {
    fn default() -> Self {
        Self {
            directory: "Memory".into(),
        }
    }
}
impl PageLayout {
    pub fn new(directory: &str) -> Result<Self, Error> {
        let directory = directory.trim();
        if directory.is_empty()
            || directory.contains('/')
            || directory.contains("..")
            || directory.contains('\0')
        {
            return Err(Error::Internal(
                "memory directory must be a single clean segment (no '/' or '..')".into(),
            ));
        }
        Ok(Self {
            directory: directory.into(),
        })
    }
    pub fn directory(&self) -> &str {
        &self.directory
    }
    pub fn vault_path(&self, path: &str) -> String {
        format!("{}/{path}", self.directory)
    }
}

/// A consistent, authorized snapshot of the page and the view available to its
/// research tools. `research_memories` includes entity-linked and source memories.
pub struct PageAuthorInput {
    pub entity: KnowledgeEntity,
    pub memories: Vec<KnowledgeMemory>,
    pub links: Vec<KnowledgeEntityLink>,
    pub redirects: BTreeMap<String, String>,
    pub research_entities: Vec<KnowledgeEntity>,
    pub research_memories: HashMap<String, Vec<KnowledgeMemory>>,
    pub prefixes: PrefixMap,
    pub layout: PageLayout,
    pub timezone: String,
}

impl PageAuthorInput {
    pub fn new(entity: KnowledgeEntity, memories: Vec<KnowledgeMemory>) -> Self {
        Self {
            entity,
            memories,
            links: Vec::new(),
            redirects: BTreeMap::new(),
            research_entities: Vec::new(),
            research_memories: HashMap::new(),
            prefixes: PrefixMap::default(),
            layout: PageLayout::default(),
            timezone: "UTC".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageAuthorOutput {
    pub path: String,
    pub name: String,
    pub description: String,
    pub attributes: serde_json::Value,
    pub body: String,
    pub related_playbooks: Vec<String>,
    pub content: String,
    pub rev: String,
}

pub struct PageAuthor<'a> {
    harness: Harness<'a>,
    prompts: PageAuthorPrompts,
    cancellation: CancellationToken,
}
impl<'a> PageAuthor<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: PageAuthorPrompts::default(),
            cancellation: CancellationToken::new(),
        }
    }
    pub fn prompts(mut self, prompts: PageAuthorPrompts) -> Self {
        self.prompts = prompts;
        self
    }
    /// Applies even when there are no current memories and no model call is needed.
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }
    pub async fn run(&self, input: PageAuthorInput) -> Result<PageAuthorOutput, Error> {
        self.run_with(input, |run| run).await
    }
    pub async fn run_with<'run, F>(
        &'run self,
        input: PageAuthorInput,
        configure: F,
    ) -> Result<PageAuthorOutput, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        self.check_cancelled()?;
        let (current, history) = classify_memories(&input.memories);
        let article = if current.is_empty() {
            MarkdownPage::parse(&deterministic_body(
                &input.entity,
                &current,
                &input.timezone,
            ))
        } else {
            self.authored_body(&input, &current, &history, configure)
                .await?
                .unwrap_or_else(|| {
                    MarkdownPage::parse(&deterministic_body(
                        &input.entity,
                        &current,
                        &input.timezone,
                    ))
                })
        };
        let article = MarkdownPage::parse(&canonicalize_wikilinks(
            &article.body,
            &input.redirects,
            &input.layout,
        ));
        let content = compose_page(
            &input.entity,
            &article,
            &input.entity.attributes,
            &input.links,
            &input.prefixes,
            &input.layout,
        );
        self.check_cancelled()?;
        let rev = sha256_hex(&content);
        Ok(PageAuthorOutput {
            path: input.entity.path,
            name: input.entity.name,
            description: input.entity.description,
            attributes: input.entity.attributes,
            body: article.body,
            related_playbooks: input.entity.related_playbooks,
            content,
            rev,
        })
    }
    fn check_cancelled(&self) -> Result<(), Error> {
        if self.cancellation.is_cancelled() {
            Err(Error::Cancelled("page author cancelled".into()))
        } else {
            Ok(())
        }
    }
    async fn authored_body<'run, F>(
        &'run self,
        input: &PageAuthorInput,
        current: &[&KnowledgeMemory],
        history: &[&KnowledgeMemory],
        configure: F,
    ) -> Result<Option<MarkdownPage>, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        let cur = list_or(current, "(none)", &input.timezone);
        let old = list_or(history, "(none — nothing has changed)", &input.timezone);
        let mut attrs = String::new();
        if let Some(map) = input
            .entity
            .attributes
            .as_object()
            .filter(|m| !m.is_empty())
        {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                let v = match &map[k] {
                    serde_json::Value::String(v) => v.clone(),
                    other => other.to_string(),
                };
                attrs.push_str(&format!("- {k}: {v}\n"));
            }
        } else {
            attrs.push_str("(none)\n");
        }
        let kinds = input.prefixes.display_joined(&input.entity.kinds);
        let (system,prompt)=self.prompts.render(&[
            ("name",&input.entity.name),("kind",&kinds),("path",&input.entity.path),("description",&input.entity.description),
            ("current",&cur),("superseded",&old),("attributes",&attrs),
            ("related","Use `search_entities` for entities worth linking, then inspect the exact path with `read_entity`."),
        ])?;
        let tools = tools::registry(input)?;
        let run = configure(
            self.harness
                .run(prompt)
                .instructions(system)
                .tools(tools)
                .cancellation(self.cancellation.clone()),
        )
        .model_responses(20);
        let raw = match run.await {
            Ok(raw) => raw,
            Err(incomplete)
                if replacement_allowed(&incomplete.reason, &incomplete.report.outcome) =>
            {
                tracing::warn!(error=%incomplete.reason,name=%input.entity.name,"pkm author: using deterministic replacement");
                return Ok(None);
            }
            Err(incomplete) => {
                if matches!(
                    incomplete.report.outcome,
                    Some(frona_harness::RunOutcome::Incomplete {
                        kind: frona_harness::FailureKind::Cancelled,
                        ..
                    })
                ) {
                    return Err(Error::Cancelled(incomplete.reason.to_string()));
                }
                return Err(Error::Internal(format!(
                    "pkm author stopped: {}",
                    incomplete.reason
                )));
            }
        };
        let article = MarkdownPage::parse(&raw);
        Ok((!article.body.trim().is_empty()).then_some(article))
    }
}
impl Phase for PageAuthor<'_> {
    type Input = PageAuthorInput;
    type Output = PageAuthorOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        PageAuthor::run(self, input).await
    }
}

#[cfg(test)]
mod tests;
