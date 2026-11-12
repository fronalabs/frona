use crate::core::error::AppError;
use crate::db::repo::pkm::AuthoredPageWrite;
use crate::memory::pkm::consolidation::{PlaybookAuthorOutcome, context::ConsolidationContext};
use crate::memory::pkm::model::EntityCategory;
use futures::{StreamExt, future::BoxFuture};
use std::sync::Arc;

pub(crate) struct PlaybookAuthor {
    pub(crate) ctx: Arc<ConsolidationContext>,
    pub(crate) prefixes: frona_pkm::ontology::PrefixMap,
    pub(crate) tool_calls: Arc<crate::db::repo::tool_calls::SurrealToolCallRepo>,
    pub(crate) messages: crate::db::repo::messages::SurrealMessageRepo,
    pub(crate) concurrency: usize,
    pub(crate) max_tool_turns: usize,
    pub(crate) max_submissions: usize,
}

impl PlaybookAuthor {
    pub(crate) async fn run(&self) -> Result<PlaybookAuthorOutcome, AppError> {
        let paths = self
            .ctx
            .repo
            .entities_needing_reconciliation_by_category(
                &self.ctx.scope.user_id,
                EntityCategory::Playbook,
            )
            .await?;
        let redirects = self.ctx.view.redirects().await?;
        let jobs: Vec<BoxFuture<'_, Result<bool, AppError>>> = paths
            .iter()
            .map(|path| {
                Box::pin(self.author_one(path, &redirects)) as BoxFuture<'_, Result<bool, AppError>>
            })
            .collect();
        let results: Vec<Result<bool, AppError>> = futures::stream::iter(jobs)
            .buffer_unordered(self.concurrency.max(1))
            .collect()
            .await;
        let built = results
            .iter()
            .filter(|result| matches!(result, Ok(true)))
            .count();
        for result in results {
            if let Err(error) = result {
                tracing::warn!(%error, "pkm playbook author: page remains dirty");
            }
        }
        let remaining = self
            .ctx
            .repo
            .entities_needing_reconciliation_by_category(
                &self.ctx.scope.user_id,
                EntityCategory::Playbook,
            )
            .await?;
        if !remaining.is_empty() {
            return Err(AppError::Internal(format!(
                "playbook author: {} page(s) remain dirty",
                remaining.len()
            )));
        }
        Ok(PlaybookAuthorOutcome {
            playbooks_built: built,
        })
    }

    async fn author_one(
        &self,
        path: &str,
        redirects: &std::collections::BTreeMap<String, String>,
    ) -> Result<bool, AppError> {
        let Some(page) = self.ctx.view.entity_by_path(path).await? else {
            return Ok(false);
        };
        if page.category != EntityCategory::Playbook {
            return Ok(false);
        }
        let memories = self
            .ctx
            .repo
            .memories_for_entity(&self.ctx.scope.user_id, path)
            .await?;
        let entities = self
            .ctx
            .view
            .list_entities()
            .await?
            .into_iter()
            .map(|row| row.as_knowledge_entity())
            .collect();
        let knowledge = super::snapshot::load(
            &self.ctx,
            entities,
            &memories,
            &self.messages,
            &self.tool_calls,
        )
        .await?;
        let links = self
            .ctx
            .repo
            .links_from_entity(&self.ctx.scope.user_id, path)
            .await?;
        let input = frona_pkm::playbook::author::PlaybookAuthorInput {
            path: path.into(),
            page: page.as_knowledge_entity().into(),
            memories: memories.into_iter().map(Into::into).collect(),
            knowledge,
            links: links.into_iter().map(Into::into).collect(),
            redirects: redirects.clone(),
            layout: frona_pkm::page::PageLayout::new(self.ctx.scope.vault.directory())?,
            prefixes: self.prefixes.clone(),
        };
        let mut prompts = frona_pkm::playbook::author::PlaybookAuthorPrompts::default();
        prompts.defaults = self.ctx.llm.prompt_defaults();
        prompts.system = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-author/system.md", &prompts.system)?;
        prompts.input = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-author/input.md", &prompts.input)?;
        prompts.reject = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-author/reject.md", &prompts.reject)?;
        let output = frona_pkm::playbook::author::PlaybookAuthor::new(self.ctx.llm.harness())
            .prompts(prompts)
            .exploration_turns(self.max_tool_turns)
            .submission_attempts(self.max_submissions)
            .run_with(input, |run| self.ctx.llm.configure(run))
            .await?;
        self.ctx.llm.check_cancelled()?;
        self.ctx
            .repo
            .commit_authored_page(
                self.ctx.view.consolidation_id(),
                &self.ctx.scope.user_id,
                &AuthoredPageWrite {
                    path: output.path,
                    name: output.name,
                    description: output.description,
                    attributes: output.attributes,
                    body: output.body,
                    related_playbooks: output.related_playbooks,
                    content: output.content.clone(),
                    rev: output.rev,
                },
            )
            .await?;
        self.ctx
            .storage
            .write_page(&self.ctx.scope.vault, path, &output.content)?;
        Ok(true)
    }
}
