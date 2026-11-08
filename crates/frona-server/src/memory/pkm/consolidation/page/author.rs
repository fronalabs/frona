//! Page Author - author the human-readable markdown article for each reconciled concept
//! page. The model writes the full article (title + prose); the system owns only the
//! frontmatter and the DB-derived `## History` (via `compose_page`).

use std::sync::Arc;

use frona_harness::inference::InferenceError;
use frona_harness::{Budget, DefaultBudget, FailureKind, ManualError, ModelError, RunOutcome};
use futures::StreamExt;
use futures::future::BoxFuture;
use tracing::warn;

use crate::core::error::AppError;
use crate::db::repo::pkm::AuthoredPageWrite;
use crate::memory::pkm::model::{KnowledgeEntity, KnowledgeMemory, classify_memories};
use crate::memory::pkm::projection::{MarkdownPage, canonicalize_wikilinks, compose_page};
use crate::tool::AgentTool;
use crate::tool::registry::ToolFilter;

use super::super::context::ConsolidationContext;
use super::super::{PageAuthorOutcome, PromptSpec, prompt_evidence};

pub(crate) struct PageAuthor {
    pub(crate) ctx: Arc<ConsolidationContext>,
    pub(crate) prefixes: crate::memory::pkm::ontology::PrefixMap,
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

        // Boxed rather than inline `async move` blocks: a closure returning one makes
        // `&Self` higher-ranked, and the compiler can then only prove `Send` for a
        // specific lifetime - which breaks the scheduler's `Send` bound several layers up.
        let jobs: Vec<BoxFuture<'_, bool>> = reconciled
            .iter()
            .map(|path| {
                let redirects = &redirects;
                let budget = budget.clone();
                Box::pin(async move {
                    match self.author_page(path, redirects, budget).await {
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
        // Current heads vs superseded/outdated history - disposition-aware
        // (erroneous excluded from both; an erroneous superseder is ignored, so
        // what it wrongly superseded is restored). See `classify_memories`.
        let (current, history) = classify_memories(&memories);

        let article = if current.is_empty() {
            MarkdownPage::parse(&deterministic_body(
                &page,
                &current,
                &self.ctx.scope.timezone,
            ))
        } else {
            self.authored_body(&page, &page.attributes, &current, &history, budget)
                .await?
                .unwrap_or_else(|| {
                    MarkdownPage::parse(&deterministic_body(
                        &page,
                        &current,
                        &self.ctx.scope.timezone,
                    ))
                })
        };
        let article = MarkdownPage::parse(&canonicalize_wikilinks(
            &article.body,
            redirects,
            &self.ctx.scope.vault,
        ));

        let file = compose_page(
            &page,
            &article,
            &page.attributes,
            &links,
            &self.prefixes,
            &self.ctx.scope.vault,
        );
        self.ctx.llm.check_cancelled()?;
        let rev = crate::memory::pkm::sha256_hex(&file);
        self.ctx
            .repo
            .commit_authored_page(
                self.ctx.view.consolidation_id(),
                &self.ctx.scope.user_id,
                &AuthoredPageWrite {
                    path: path.to_string(),
                    name: page.name,
                    description: page.description,
                    attributes: page.attributes,
                    body: article.body,
                    related_playbooks: page.related_playbooks,
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

    async fn authored_body(
        &self,
        page: &KnowledgeEntity,
        attributes: &serde_json::Value,
        current: &[&KnowledgeMemory],
        history: &[&KnowledgeMemory],
        shared_budget: Arc<dyn Budget>,
    ) -> Result<Option<MarkdownPage>, AppError> {
        let cur = list_or(current, "(none)", &self.ctx.scope.timezone);
        let old = list_or(
            history,
            "(none — nothing has changed)",
            &self.ctx.scope.timezone,
        );
        let mut attrs = String::new();
        if let Some(map) = attributes.as_object().filter(|m| !m.is_empty()) {
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
        let related_block = "Use `search_entities` for entities worth linking, then inspect the exact path with `read_entity`.";
        // The model reads and writes CURIEs; the database holds IRIs.
        let kinds = self.prefixes.display_joined(&page.kinds);
        // The model authors the full article (title + prose). `MarkdownPage::parse`
        // applies lenient, language-agnostic syntax cleanup (unwrap a stray whole-doc
        // code fence, strip leading frontmatter) - we do NOT police headings/sections
        // (locale-fragile). Empty output → None → deterministic fallback.
        let budget = Arc::new(std::sync::atomic::AtomicUsize::new(20));
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(
                crate::memory::pkm::consolidation::tools::SearchEntitiesTool {
                    prompts: self.ctx.llm.prompts().clone(),
                    repo: self.ctx.view.clone(),
                    overlay: None,
                    budget: Some(budget.clone()),
                    prefixes: self.prefixes.clone(),
                },
            ),
            Arc::new(crate::memory::pkm::consolidation::tools::ReadEntityTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                memories: self.ctx.repo.clone(),
                prefixes: self.prefixes.clone(),
                user_id: self.ctx.scope.user_id.clone(),
                overlay: None,
                budget: Some(budget),
            }),
        ];
        let harness = self
            .ctx
            .llm
            .sdk(
                &self.ctx.scope.agent_id,
                &[ToolFilter::AllowList(&[])],
                &tools,
            )
            .await?;
        let run = harness
            .run(
                PromptSpec::PAGE_AUTHOR,
                &[
                    ("name", &page.name),
                    ("kind", &kinds),
                    ("path", &page.path),
                    ("description", &page.description),
                    ("current", &cur),
                    ("superseded", &old),
                    ("attributes", &attrs),
                    ("related", related_block),
                ],
            )?
            .budget(shared_budget)
            // Preserve the former 20-response loop, including tool-bearing responses.
            .model_responses(20);
        let raw = match run.await {
            Ok(raw) => raw,
            Err(incomplete)
                if replacement_allowed(&incomplete.reason, &incomplete.report.outcome) =>
            {
                warn!(error = %incomplete.reason, name = %page.name, "pkm author: using deterministic replacement");
                return Ok(None);
            }
            Err(incomplete) => {
                return Err(AppError::Internal(format!(
                    "pkm author stopped: {}",
                    incomplete.reason
                )));
            }
        };
        let article = MarkdownPage::parse(&raw);
        if article.body.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(article))
        }
    }
}

/// Replacement is a domain choice for an unavailable provider or empty answer.
/// Typed execution controls (including nested provider stops), setup, compaction,
/// and protocol failures never become a successful deterministic page.
fn replacement_allowed(reason: &ManualError, outcome: &Option<RunOutcome>) -> bool {
    matches!(reason, ManualError::NoText)
        || (matches!(reason, ManualError::Model(ModelError::Provider(error))
            if provider_replacement_allowed(error))
            && matches!(
                outcome,
                Some(RunOutcome::Incomplete {
                    kind: FailureKind::Model,
                    ..
                })
            ))
}

/// Prepared groups wrap both operational and setup failures with provider/fallback
/// provenance. Only an aggregate of operational failures permits replacement.
fn provider_replacement_allowed(error: &InferenceError) -> bool {
    match error {
        InferenceError::InferenceFailed(_)
        | InferenceError::StreamingFailed(_)
        | InferenceError::CompletionFailed(_)
        | InferenceError::RateLimited { .. }
        | InferenceError::EmptyResponse => true,
        InferenceError::ModelFailed { source, .. } => provider_replacement_allowed(source),
        InferenceError::AllFallbacksFailed(errors) => {
            !errors.is_empty() && errors.iter().all(provider_replacement_allowed)
        }
        InferenceError::ProviderNotConfigured(_)
        | InferenceError::ModelGroupNotFound(_)
        | InferenceError::ConfigError(_)
        | InferenceError::Stop(_)
        | InferenceError::Cancelled(_) => false,
    }
}

fn deterministic_body(
    page: &KnowledgeEntity,
    current: &[&KnowledgeMemory],
    timezone: &str,
) -> String {
    let mut s = String::new();
    if !page.description.trim().is_empty() {
        s.push_str(page.description.trim());
        s.push_str("\n\n");
    }
    if !current.is_empty() {
        s.push_str("## What we know\n\n");
        for m in current {
            s.push_str(&author_memory_bullet(m, timezone));
        }
    }
    s
}

fn list_or(memories: &[&KnowledgeMemory], empty: &str, timezone: &str) -> String {
    if memories.is_empty() {
        return format!("{empty}\n");
    }
    let mut s = String::new();
    for m in memories {
        s.push_str(&author_memory_bullet(m, timezone));
    }
    s
}

fn author_memory_bullet(memory: &KnowledgeMemory, timezone: &str) -> String {
    let evidence = prompt_evidence(&memory.evidence);
    let mut rendered = format!(
        "- ({:?}) {}\n  evidence: {}\n",
        memory.kind, memory.content, evidence,
    );
    if let Some(episode) = &memory.episode {
        let status = format!("{:?}", episode.status).to_ascii_lowercase();
        let mut fields = vec![format!("status={status}")];
        if let Some(start) = episode.resolved_start {
            fields.push(format!(
                "start_utc={}",
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
            fields.push(format!(
                "start_local={} timezone={timezone}",
                start
                    .with_timezone(&tz)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            ));
        } else if let Some(absolute) = &episode.absolute {
            fields.push(format!(
                "absolute_utc={}",
                serde_json::to_string(absolute).unwrap_or_else(|_| "null".into()),
            ));
        }
        if let Some(end) = episode.resolved_end {
            fields.push(format!(
                "end_utc={}",
                end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
            fields.push(format!(
                "end_local={} timezone={timezone}",
                end.with_timezone(&tz)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            ));
        }
        if let Some(duration) = &episode.duration {
            fields.push(format!(
                "duration={}",
                serde_json::to_string(duration).unwrap_or_else(|_| "null".into()),
            ));
        }
        rendered.push_str(&format!("  episode: {}\n", fields.join("; ")));
    }
    rendered
}

#[cfg(test)]
mod execution;

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::memory::pkm::model::{
        AbsoluteTime, Disposition, Episode, EpisodeStatus, MemoryKind, TemporalAnchor,
    };

    #[test]
    fn author_input_includes_episode_status_and_utc_and_local_dates() {
        let memory = KnowledgeMemory {
            id: "episode-1".into(),
            user_id: "user-1".into(),
            created_at: Utc.with_ymd_and_hms(2026, 7, 18, 20, 48, 27).unwrap(),
            kind: MemoryKind::Episodic,
            episode: Some(Episode {
                status: EpisodeStatus::Planned,
                anchor: TemporalAnchor {
                    message: "m23".into(),
                    quote: String::new(),
                },
                duration: None,
                absolute: Some(AbsoluteTime {
                    year: Some(2026),
                    month: Some(7),
                    day: Some(20),
                    hour: Some(2),
                    minute: Some(0),
                }),
                resolved_start: Some(Utc.with_ymd_and_hms(2026, 7, 20, 2, 0, 0).unwrap()),
                resolved_end: None,
            }),
            content: "A Sunday-evening reminder was scheduled.".into(),
            relations: Vec::new(),
            disposition: Disposition::None,
            ended_at: None,
            comment: None,
            erroneous_at: None,
            evidence: Vec::new(),
        };

        let rendered = list_or(&[&memory], "(none)", "America/Los_Angeles");

        assert!(rendered.contains("status=planned"), "{rendered}");
        assert!(
            rendered.contains("start_utc=2026-07-20T02:00:00Z"),
            "{rendered}"
        );
        assert!(
            rendered.contains("start_local=2026-07-19T19:00:00-07:00 timezone=America/Los_Angeles"),
            "{rendered}",
        );
    }
}
