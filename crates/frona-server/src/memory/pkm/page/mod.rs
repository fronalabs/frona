//! Page Author - author the human-readable markdown article for each reconciled concept
//! page. The model writes the full article (title + prose); the system owns only the
//! frontmatter and the DB-derived `## History` (via `compose_page`).

use std::sync::Arc;

use frona_harness::{Budget, DefaultBudget};
use futures::StreamExt;
use futures::future::BoxFuture;
use tracing::warn;

use crate::core::error::AppError;
use crate::db::repo::pkm::AuthoredPageWrite;

pub(crate) mod projection;
use super::consolidation::ConsolidationContext;
use super::consolidation::PageAuthorOutcome;

pub(crate) struct PageAuthor {
    pub(crate) ctx: Arc<ConsolidationContext>,
    pub(crate) prefixes: frona_pkm::ontology::PrefixMap,
    /// How many per-page model calls run at once.
    pub(crate) concurrency: usize,
}

impl PageAuthor {
    /// Pages are authored concurrently: each is an independent model call writing its
    /// own row and its own file, sharing only admission allowance - unlike classify (which
    /// accumulates a proposal layer) or reconcile (whose worklist grows as it runs).
    /// Caller-owned concurrency and one shared budget bound the batch. A failure
    /// logs and the rest proceed.
    ///
    /// **This stage keeps nothing in the pass record**, and needs nothing: `rendered_at`
    /// is stamped with the exact canonical bytes in one database commit, and the
    /// worklist is `updated_at > rendered_at` re-read at entry. So a pass that dies two
    /// thirds of the way through already resumes having paid for two thirds - the marker
    /// travels with the effect rather than in a second write that a crash could lose.
    /// That is also why author can stay concurrent while the stages that *do* keep state
    /// are sequential.
    pub(crate) async fn run(&self, reconciled: &[String]) -> PageAuthorOutcome {
        let mut out = PageAuthorOutcome::default();
        self.author(reconciled, &mut out, Arc::new(DefaultBudget::default()))
            .await;
        out
    }

    /// Pre-commit failures stay dirty. A failed filesystem mirror after canonical
    /// commit is repaired from durable bytes by recovery without re-authoring.
    async fn author(
        &self,
        reconciled: &[String],
        stats: &mut PageAuthorOutcome,
        budget: Arc<dyn Budget>,
    ) {
        let redirects = match self.ctx.view.redirects().await {
            Ok(redirects) => redirects,
            Err(error) => {
                warn!(%error, "pkm author: required redirects unavailable");
                return;
            }
        };

        let graph = match self.ctx.view.portable_snapshot(&self.ctx.repo).await {
            Ok(graph) => graph,
            Err(error) => {
                warn!(%error, "pkm author: required research snapshot unavailable");
                return;
            }
        };

        // Boxed rather than inline `async move` blocks: a closure returning one makes
        // `&Self` higher-ranked, and the compiler can then only prove `Send` for a
        // specific lifetime - which breaks the scheduler's `Send` bound several layers up.
        let jobs: Vec<BoxFuture<'_, bool>> = reconciled
            .iter()
            .map(|path| {
                let redirects = &redirects;
                let graph = &graph;
                let budget = budget.clone();
                Box::pin(async move {
                    match self.author_page(path, redirects, graph, budget).await {
                        Ok(written) => written,
                        Err(e) => {
                            warn!(error = %e, path = %path, "pkm author: page failed");
                            false
                        }
                    }
                }) as BoxFuture<'_, bool>
            })
            .collect();

        stats.pages_built += futures::stream::iter(jobs)
            .buffer_unordered(self.concurrency.max(1))
            .filter(|written| {
                let written = *written;
                async move { written }
            })
            .count()
            .await;
    }

    /// Returns whether the page was authored - the caller folds the count, since a
    /// shared `&mut stats` would serialize the whole stage.
    async fn author_page(
        &self,
        path: &str,
        redirects: &std::collections::BTreeMap<String, String>,
        graph: &frona_pkm::GraphSnapshot,
        budget: Arc<dyn Budget>,
    ) -> Result<bool, AppError> {
        self.ctx.llm.check_cancelled()?;
        let Some(page) = self.ctx.view.entity_by_path(path).await? else {
            return Ok(false);
        };
        let page = page.as_knowledge_entity();
        let memories = self
            .ctx
            .repo
            .memories_for_entity(&self.ctx.scope.user_id, path)
            .await?;
        let links = self
            .ctx
            .repo
            .links_from_entity(&self.ctx.scope.user_id, path)
            .await?;
        let research_entities = graph.knowledge_entities();
        let research_memories = research_entities
            .iter()
            .map(|entity| {
                let mut memories = std::collections::BTreeMap::new();
                for memory in graph
                    .memories_for_entity(&entity.path)
                    .into_iter()
                    .chain(graph.memories_by_ids(&entity.source_memory_ids))
                {
                    memories.insert(memory.id.clone(), memory);
                }
                (entity.path.clone(), memories.into_values().collect())
            })
            .collect();
        let defaults = frona_pkm::page::PageAuthorPrompts::default();
        let phase = frona_pkm::page::PageAuthor::new(self.ctx.llm.harness())
            .cancellation(self.ctx.llm.cancellation())
            .prompts(frona_pkm::page::PageAuthorPrompts {
                defaults: self.ctx.llm.prompt_defaults(),
                system: self
                    .ctx
                    .llm
                    .prompt_source("pkm/page-author/system.md", &defaults.system)?,
                input: self
                    .ctx
                    .llm
                    .prompt_source("pkm/page-author/input.md", &defaults.input)?,
            });
        let output = phase
            .run_with(
                frona_pkm::page::PageAuthorInput {
                    entity: page.into(),
                    memories: memories.into_iter().map(Into::into).collect(),
                    links: links.into_iter().map(Into::into).collect(),
                    redirects: redirects.clone(),
                    research_entities,
                    research_memories,
                    prefixes: self.prefixes.clone(),
                    layout: frona_pkm::page::PageLayout::new(self.ctx.scope.vault.directory())?,
                    timezone: self.ctx.scope.timezone.clone(),
                },
                |run| self.ctx.llm.configure(run).budget(budget),
            )
            .await?;
        self.ctx.llm.check_cancelled()?;
        let file = output.content;
        self.ctx
            .repo
            .commit_authored_page(
                self.ctx.view.consolidation_id(),
                &self.ctx.scope.user_id,
                &AuthoredPageWrite {
                    path: path.to_string(),
                    name: output.name,
                    description: output.description,
                    attributes: output.attributes,
                    body: output.body,
                    related_playbooks: output.related_playbooks,
                    content: file.clone(),
                    rev: output.rev,
                },
            )
            .await?;
        self.ctx
            .storage
            .write_page(&self.ctx.scope.vault, path, &file)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
