use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use frona_harness::{Decision, FailureKind, RunOutcome};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use crate::chat::message::models::MessageRole;
use crate::chat::message::repository::MessageRepository;
use crate::core::error::AppError;
use crate::db::repo::pkm::AuthoredPageWrite;
use crate::db::repo::tool_calls::ToolCallRepository;
use crate::memory::pkm::model::{EntityCategory, EvidenceSource, KnowledgeMemory};
use crate::memory::pkm::projection::{
    MarkdownPage, canonical_path, canonicalize_wikilinks, compose_page,
};
use crate::tool::AgentTool;
use crate::tool::registry::ToolFilter;

use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::consolidation::{PlaybookAuthorOutcome, PromptSpec, prompt_evidence};

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

pub(crate) struct PlaybookAuthor {
    pub(crate) ctx: Arc<ConsolidationContext>,
    pub(crate) prefixes: crate::memory::pkm::ontology::PrefixMap,
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
        require_requested_path(path, &page.path)?;
        if page.category != EntityCategory::Playbook {
            return Ok(false);
        }
        let memories = self
            .ctx
            .repo
            .memories_for_entity(&self.ctx.scope.user_id, path)
            .await?;
        let procedural: Vec<&KnowledgeMemory> = memories
            .iter()
            .filter(|memory| memory.kind == crate::memory::pkm::model::MemoryKind::Procedural)
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
            self.evidence_context(&procedural).await?;
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
        let vars: [(&str, &str); 7] = [
            ("path", &page.path),
            ("name", &page.name),
            ("description", &page.description),
            ("body", &page.body),
            ("memories", &memory_block),
            ("transcript", &transcript),
            ("invocations", &invocation_block),
        ];
        let allowed = invocations
            .iter()
            .enumerate()
            .map(|(index, invocation)| (format!("c{}", index + 1), invocation.id.clone()))
            .collect();
        let playbook_lookup = Arc::new(super::tools::PlaybookLookup::default());
        let total_tool_budget = Arc::new(std::sync::atomic::AtomicUsize::new(40));
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(super::tools::GetToolOutputTool {
                prompts: self.ctx.llm.prompts().clone(),
                tool_calls: self.tool_calls.clone(),
                allowed,
                remaining: std::sync::atomic::AtomicUsize::new(10),
                total: total_tool_budget.clone(),
            }),
            Arc::new(super::tools::ReadTranscriptTool {
                prompts: self.ctx.llm.prompts().clone(),
                lookup: transcript_lookup,
                remaining: std::sync::atomic::AtomicUsize::new(10),
                total: total_tool_budget.clone(),
            }),
            Arc::new(super::tools::FindPlaybooksTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                view: None,
                eligible_playbook_paths: None,
                subject_path: None,
                total: total_tool_budget.clone(),
            }),
            Arc::new(super::tools::ReadPlaybookTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                memories: self.ctx.repo.clone(),
                user_id: self.ctx.scope.user_id.clone(),
                view: None,
                lookup: playbook_lookup.clone(),
                total: total_tool_budget.clone(),
            }),
            Arc::new(
                crate::memory::pkm::consolidation::tools::SearchEntitiesTool {
                    prompts: self.ctx.llm.prompts().clone(),
                    repo: self.ctx.view.clone(),
                    overlay: None,
                    budget: Some(total_tool_budget.clone()),
                    prefixes: self.prefixes.clone(),
                },
            ),
            Arc::new(crate::memory::pkm::consolidation::tools::ReadEntityTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                memories: self.ctx.repo.clone(),
                user_id: self.ctx.scope.user_id.clone(),
                overlay: None,
                budget: Some(total_tool_budget),
                prefixes: self.prefixes.clone(),
            }),
        ];
        let harness = self
            .ctx
            .llm
            .sdk(
                &self.ctx.scope.agent_id,
                &[ToolFilter::AllowList(&[
                    "get_tool_output",
                    "read_transcript",
                    "find_playbooks",
                    "read_playbook",
                    "search_entities",
                    "read_entity",
                ])],
                &tools,
            )
            .await?;
        let prompts = self.ctx.llm.prompts().clone();
        let authored = harness
            .run(PromptSpec::PLAYBOOK_AUTHOR, &vars)?
            .exploration_turns(self.max_tool_turns)
            .structured::<AuthoredPlaybook>()
            .submission_attempts(self.max_submissions)
            .inspect(move |candidate| {
                let prompts = prompts.clone();
                async move { inspect_submission(candidate, &prompts) }
            })
            .await
            .map_err(|incomplete| {
                tracing::warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    path, "pkm playbook author: refinement incomplete");
                if matches!(
                    incomplete.report.outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Cancelled,
                        ..
                    })
                ) {
                    AppError::Inference(Box::new(
                        frona_harness::inference::InferenceError::Cancelled(
                            incomplete.reason.to_string(),
                        ),
                    ))
                } else {
                    AppError::Internal(format!(
                        "playbook author: `{path}` incomplete: {}",
                        incomplete.reason
                    ))
                }
            })?;

        let mut related = Vec::new();
        for proposed in authored.related_playbooks {
            let Some(candidate) = crate::memory::pkm::storage::normalize_path(&proposed) else {
                continue;
            };
            let candidate = canonical_path(&candidate, redirects);
            if self
                .ctx
                .view
                .entity_by_path(&candidate)
                .await?
                .is_some_and(|page| page.category == EntityCategory::Playbook)
            {
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
        let body = canonicalize_wikilinks(&authored.body, redirects, &self.ctx.scope.vault);
        projected.body = body.clone();
        projected.related_playbooks = related.clone();
        let links = self
            .ctx
            .repo
            .links_from_entity(&self.ctx.scope.user_id, path)
            .await?;
        let article = MarkdownPage::parse(&body);
        let rendered = projected.as_knowledge_entity();
        let file = compose_page(
            &rendered,
            &article,
            &rendered.attributes,
            &links,
            &self.prefixes,
            &self.ctx.scope.vault,
        );
        let rev = crate::memory::pkm::sha256_hex(&file);
        self.ctx
            .repo
            .commit_authored_page(
                self.ctx.view.consolidation_id(),
                &self.ctx.scope.user_id,
                &AuthoredPageWrite {
                    path: path.to_string(),
                    name: authored.name,
                    description: authored.description,
                    attributes: projected.attributes,
                    body,
                    related_playbooks: related,
                    content: file.clone(),
                    rev,
                },
            )
            .await?;
        self.ctx
            .storage
            .write_page(&self.ctx.scope.vault, path, &file)?;
        Ok(true)
    }

    async fn evidence_context(
        &self,
        memories: &[&KnowledgeMemory],
    ) -> Result<
        (
            String,
            Vec<InvocationEvidence>,
            Arc<super::tools::TranscriptLookup>,
        ),
        AppError,
    > {
        let mut anchors = HashMap::<String, BTreeSet<String>>::new();
        for memory in memories {
            for evidence in &memory.evidence {
                match &evidence.source {
                    EvidenceSource::UserMessage {
                        chat_id,
                        message_id,
                        ..
                    }
                    | EvidenceSource::AgentMessage {
                        chat_id,
                        message_id,
                        ..
                    } => {
                        anchors
                            .entry(chat_id.clone())
                            .or_default()
                            .insert(message_id.clone());
                    }
                    _ => {}
                }
            }
        }
        let mut transcript = String::new();
        let mut transcript_tokens = 0usize;
        let mut selected_agents = HashMap::<String, BTreeSet<String>>::new();
        let mut lookup = super::tools::TranscriptLookup::default();
        let mut next_cursor = 1usize;
        for (chat_id, message_ids) in &anchors {
            let messages = self.messages.find_by_chat_id(chat_id).await?;
            let tool_calls = self.tool_calls.find_by_chat_id(chat_id).await?;
            for index in 0..messages.len() {
                lookup
                    .cursors
                    .insert(format!("t{next_cursor}"), (chat_id.clone(), index));
                next_cursor += 1;
            }
            let mut selected = BTreeSet::new();
            for anchor in message_ids {
                let Some(end) = messages.iter().position(|message| &message.id == anchor) else {
                    continue;
                };
                let mut start = end;
                let mut agents = 0usize;
                while start > 0 && agents < 10 {
                    start -= 1;
                    if messages[start].role == MessageRole::Agent {
                        agents += 1;
                    }
                }
                selected.extend(start..=end);
            }
            for index in selected {
                let message = &messages[index];
                let local = lookup
                    .cursors
                    .iter()
                    .find_map(|(local, held)| {
                        (held == &(chat_id.clone(), index)).then_some(local.as_str())
                    })
                    .unwrap_or("?");
                let role = match message.role {
                    MessageRole::User => "user",
                    MessageRole::Agent => "agent",
                    MessageRole::System => "system",
                    _ => "event",
                };
                let line = format!(
                    "[{local} {role}] {}\n",
                    super::tools::redact_text(
                        &super::super::transcript::message_text(
                            &message.id,
                            &message.content,
                            &tool_calls,
                        ),
                        &[],
                    )
                );
                let cost = frona_harness::inference::context::estimate_tokens(&line);
                if transcript_tokens + cost <= 16_000 {
                    transcript_tokens += cost;
                    transcript.push_str(&line);
                }
                if message.role == MessageRole::Agent {
                    selected_agents
                        .entry(chat_id.clone())
                        .or_default()
                        .insert(message.id.clone());
                }
            }
            lookup.tool_calls.insert(chat_id.clone(), tool_calls);
            lookup.chats.insert(chat_id.clone(), messages);
        }
        if transcript.is_empty() {
            transcript.push_str("(source transcript unavailable)");
        }

        let mut out = Vec::new();
        let mut tokens = 0usize;
        for (chat_id, message_ids) in selected_agents {
            for call in lookup.tool_calls.get(&chat_id).into_iter().flatten() {
                if !message_ids.contains(&call.message_id) || out.len() >= 100 {
                    continue;
                }
                let reference = invocation_projection(call);
                let Some(reference) = reference else { continue };
                let cost = frona_harness::inference::context::estimate_tokens(&reference.reference);
                if cost > 2_000 || tokens + cost > 16_000 {
                    continue;
                }
                tokens += cost;
                out.push(reference);
            }
        }
        Ok((transcript, out, Arc::new(lookup)))
    }
}

/// Domain content inspection; only explicit acceptance can enter page projection.
fn inspect_submission(
    candidate: AuthoredPlaybook,
    prompts: &frona_text::prompt::PromptLoader,
) -> Result<Decision<AuthoredPlaybook>, AppError> {
    let mut missing = Vec::new();
    if candidate.name.trim().is_empty() {
        missing.push("name");
    }
    if candidate.description.trim().is_empty() {
        missing.push("description");
    }
    if candidate.body.trim().is_empty() {
        missing.push("body");
    }
    if missing.is_empty() {
        Ok(Decision::accept(candidate))
    } else {
        let feedback = PromptSpec::PLAYBOOK_AUTHOR.reject(
            prompts,
            &[("rejections", &format!("missing: {}", missing.join(", ")))],
        )?;
        Ok(Decision::revise(feedback))
    }
}

fn require_requested_path(requested: &str, resolved: &str) -> Result<(), AppError> {
    if requested == resolved {
        return Ok(());
    }
    Err(AppError::Internal(format!(
        "playbook author: requested `{requested}` resolved to `{resolved}`; refusing to write the canonical page under a stale path",
    )))
}

fn invocation_projection(
    call: &crate::inference::tool_call::ToolCall,
) -> Option<InvocationEvidence> {
    let name = call.name.to_ascii_lowercase();
    let value = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| {
                call.arguments
                    .get(*key)
                    .and_then(|value| value.as_str())
                    .map(str::trim)
            })
            .filter(|value| !value.is_empty())
    };
    let reference = if name.contains("browser") {
        serde_json::json!({"url": value(&["url"])?}).to_string()
    } else if name.contains("search") {
        serde_json::json!({"query": value(&["query", "q"])?}).to_string()
    } else if name.contains("python") || name == "node" || name.contains("nodejs") {
        serde_json::json!({"code": value(&["code", "script"])?}).to_string()
    } else if name == "shell" || name.contains("exec_command") || name.contains("terminal") {
        serde_json::json!({"command": value(&["command", "cmd"])?}).to_string()
    } else {
        serde_json::json!({
            "tool": call.name,
            "parameters": super::tools::redact_json(&call.arguments, &[]),
        })
        .to_string()
    };
    Some(InvocationEvidence {
        id: call.id.clone(),
        tool: call.name.clone(),
        reference: super::tools::redact_text(&reference, &[]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod refinement;

    #[test]
    fn author_rejects_an_entity_redirected_away_from_the_requested_output_path() {
        let error = require_requested_path(
            "tools/yfinance/fetch-stock-close-price",
            "tools/yfinance/diagnose-secure-yahoo-connectivity",
        )
        .unwrap_err();

        assert!(error.to_string().contains("fetch-stock-close-price"));
        assert!(
            error
                .to_string()
                .contains("diagnose-secure-yahoo-connectivity")
        );
    }
}
