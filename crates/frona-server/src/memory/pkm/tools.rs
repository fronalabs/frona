//! Foreground tool surface for the knowledge service: `memory_search`,
//! `memory_remember`, `memory_cite`. Reading a page is the general `read`
//! tool (pages are self-describing `.md` files). Tool definitions live in
//! `resources/prompts/tools/pkm/<name>.md` (loaded by the `#[agent_tool]` macro).

use std::sync::Arc;

use serde_json::Value;

use frona_derive::agent_tool;

use crate::agent::prompt::PromptLoader;
use crate::auth::user_service::UserService;
use crate::core::error::AppError;
use crate::tool::{InferenceContext, ToolOutput, active_chat, str_arg};

use super::ontology::OntologyManager;
use super::search::MemorySearch;
use super::storage::PkmStorage;
use super::vault::VaultScope;
use crate::db::repo::pkm::PkmRepo;

pub fn all(
    repo: Arc<PkmRepo>,
    storage: PkmStorage,
    ontology: OntologyManager,
    prompts: PromptLoader,
    user_service: UserService,
) -> Vec<Arc<dyn crate::tool::AgentTool>> {
    let vault = VaultResolver {
        storage,
        user_service,
    };
    vec![
        Arc::new(RememberTool {
            repo: repo.clone(),
            prompts: prompts.clone(),
        }),
        Arc::new(SearchTool {
            search: MemorySearch::new(repo.clone(), ontology.clone()),
            prompts: prompts.clone(),
            vault: vault.clone(),
        }),
        Arc::new(CitePageTool {
            repo,
            prompts,
            vault,
        }),
    ]
}

/// The two dependencies [`VaultScope::resolve`] needs, as one collaborator.
///
/// `memory_search` and `memory_cite` both work in page paths, so both have to know where
/// this user's files live - and neither wants `PkmStorage` or `UserService` for anything
/// else. Carried loose, they were two fields apiece whose reason for existing was a call
/// spelled out identically in both, which is one edit away from two answers to "which
/// directory is the vault?".
#[derive(Clone)]
struct VaultResolver {
    storage: PkmStorage,
    user_service: UserService,
}

impl VaultResolver {
    async fn for_caller(&self, ctx: &InferenceContext) -> Result<VaultScope, AppError> {
        VaultScope::resolve(
            &self.user_service,
            &self.storage,
            &ctx.user.id,
            &ctx.user.handle,
        )
        .await
    }
}

/// [`str_arg`] plus this surface's policy: a missing argument is a validation error the
/// tool loop reports back, rather than a message the tool composes itself.
fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, AppError> {
    str_arg(args, key).ok_or_else(|| AppError::Validation(format!("missing '{key}'")))
}

pub struct RememberTool {
    repo: Arc<PkmRepo>,
    prompts: PromptLoader,
}

#[agent_tool(name = "memory_remember", dir = "pkm")]
impl RememberTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let content = arg(&arguments, "content")?;
        let chat = active_chat(ctx)?;
        self.repo.remember(&ctx.user.id, &chat.id, content).await?;
        Ok(ToolOutput::text(format!("Remembered: {content}")))
    }
}

pub struct SearchTool {
    search: MemorySearch,
    prompts: PromptLoader,
    vault: VaultResolver,
}

#[agent_tool(name = "memory_search", dir = "pkm")]
impl SearchTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let query = arg(&arguments, "query")?;
        let vault = self.vault.for_caller(ctx).await?;
        let result = self.search.execute(&ctx.user.id, query, &vault).await?;
        Ok(ToolOutput::text(serde_json::to_string_pretty(&result)?))
    }
}

pub struct CitePageTool {
    repo: Arc<PkmRepo>,
    prompts: PromptLoader,
    vault: VaultResolver,
}

#[agent_tool(name = "memory_cite", dir = "pkm")]
impl CitePageTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let raw = arg(&arguments, "path")?;
        let vault = self.vault.for_caller(ctx).await?;
        // Accept the vault-relative path memory_search returned (or an absolute
        // one); fall back to a bare page path for resilience.
        let page_path = vault
            .page_from_any(raw)
            .unwrap_or_else(|| raw.trim_end_matches(".md").to_string());
        match self.repo.bump_entity_use(&ctx.user.id, &page_path).await {
            Ok(n) => Ok(ToolOutput::text(format!(
                "Cited '{page_path}' (total uses: {n})."
            ))),
            Err(AppError::Validation(msg)) => Ok(ToolOutput::text(format!(
                "Couldn't cite: {msg}. Use the absolute path returned by memory_search."
            ))),
            Err(e) => Err(e),
        }
    }
}
