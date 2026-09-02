use std::sync::Arc;

use tracing::warn;

use crate::core::config::MemoryConfig;
use crate::core::error::AppError;

use super::*;

/// **Per user, once** - type, reconcile, author, and GC everything the ingests left
/// dirty. An entity mentioned in three chats is classified, reconciled and authored once
/// here rather than three times.
///
/// The consolidation proper: it is what walks [`ConsolidationStageState`], while mining
/// sits outside the machine and only hands it material.
///
/// Holds `MemoryConfig` and hands each stage only the scalars it reads, so the rule that
/// a stage never reads config directly is structural rather than six hand-copied
/// constructor parameters.
pub struct Consolidator {
    pub(super) ctx: Arc<ConsolidationContext>,
    pub(super) config: MemoryConfig,
    pub(super) ontology: crate::memory::pkm::ontology::OntologyManager,
    pub(super) prefixes: crate::memory::pkm::ontology::PrefixMap,
    tool_calls: Arc<crate::db::repo::tool_calls::SurrealToolCallRepo>,
    messages: crate::db::repo::messages::SurrealMessageRepo,
    pub(super) user_service: crate::auth::user_service::UserService,
}

impl Consolidator {
    pub fn new(
        ctx: Arc<ConsolidationContext>,
        config: MemoryConfig,
        ontology: crate::memory::pkm::ontology::OntologyManager,
        tool_calls: Arc<crate::db::repo::tool_calls::SurrealToolCallRepo>,
        messages: crate::db::repo::messages::SurrealMessageRepo,
        user_service: crate::auth::user_service::UserService,
    ) -> Self {
        let prefixes = ontology.prefixes();
        Self {
            ctx,
            config,
            ontology,
            prefixes,
            tool_calls,
            messages,
            user_service,
        }
    }

    /// Drive the user-scoped stages from wherever the record left off.
    ///
    /// Every stage runs and advances the record. A stage that **fails** stops the pass and
    /// leaves the record where it is: the sweep backs off and resumes at that same stage
    /// next tick, which is what makes a transient model failure cost a retry instead of
    /// the stage's work.
    ///
    /// The driver carries no state of its own. The variant only says *which* stage runs;
    /// a stage that needs to remember anything opens its own progress against the record
    /// through [`ConsolidationContext`] and banks per item as it goes. That is the whole
    /// difference between resuming at a stage boundary and resuming where the pass
    /// actually died.
    ///
    /// Each stage still re-reads its worklist from live state at entry, so a pass that
    /// resumes mid-flight picks up whatever arrived while it was down.
    ///
    /// Returns `Err` when a stage failed (the caller records the attempt); `Ok` once the
    /// record reaches `Done`.
    pub async fn run(&self) -> Result<(), AppError> {
        loop {
            let stage = self.ctx.stage().await;
            let entered = stage.label();
            let outcome: Result<(), AppError> = match stage {
                ConsolidationStageState::Ingest(_) => Ok(()),
                ConsolidationStageState::Classify(_)
                | ConsolidationStageState::Resolve(_)
                | ConsolidationStageState::Reconcile(_)
                | ConsolidationStageState::Assemble(_) => match self.execute().await {
                    Ok((classify, resolve, assemble)) => {
                        self.ctx
                            .absorb(|s| {
                                s.absorb_classify(classify);
                                s.absorb_resolve(resolve);
                                s.absorb_assemble(assemble);
                            })
                            .await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                ConsolidationStageState::PlaybookResolve(_) => self.playbook_resolve().run().await,
                ConsolidationStageState::PlaybookAuthor => match self.playbook_author().run().await
                {
                    Ok(o) => {
                        self.ctx.absorb(|s| s.absorb_playbook_author(o)).await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                ConsolidationStageState::PageAuthor => match self.dirty_concept_entities().await {
                    Ok(reconciled) => {
                        let o = self.page_author().run(&reconciled).await;
                        self.ctx.absorb(|s| s.absorb_page_author(o)).await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                ConsolidationStageState::Cleanup => match self.cleanup().run().await {
                    Ok(o) => {
                        self.ctx.finish_cleanup(o).await?;
                        // Store the effective ontology now that the pass has settled which
                        // concepts survive. Deliberately after cleanup rather than after
                        // classify: until orphans are collected the vault's term set is
                        // still in flux, and anything written mid-pass would be rewritten
                        // moments later. A failure costs nothing - the next load notices
                        // the seed set moved and cuts again.
                        if let Err(e) = self
                            .ontology
                            .save_effective_ontology(&self.ctx.scope.user_id)
                            .await
                        {
                            warn!(error = %e, "pkm ontology: saving the effective ontology failed");
                        }
                        self.ontology
                            .publish_consolidated_graph(&self.ctx.scope.user_id);
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                ConsolidationStageState::Done => return Ok(()),
                ConsolidationStageState::Failed => {
                    return Err(AppError::Database(
                        "pkm consolidation: terminally failed checkpoint".into(),
                    ));
                }
            };
            outcome?;
            if self.ctx.stage().await.label() == entered {
                self.ctx.advance().await?;
            }
        }
    }

    async fn dirty_concept_entities(&self) -> Result<Vec<String>, AppError> {
        self.ctx
            .repo
            .entities_needing_reconciliation_by_category(
                &self.ctx.scope.user_id,
                crate::memory::pkm::model::EntityCategory::Concept,
            )
            .await
    }

    fn playbook_resolve(&self) -> playbook::PlaybookResolve {
        playbook::PlaybookResolve {
            ctx: self.ctx.clone(),
            messages: self.messages.clone(),
            tool_calls: self.tool_calls.clone(),
            max_tool_turns: self.config.pkm_playbook_max_tool_turns,
            max_submissions: self.config.pkm_playbook_max_submissions,
        }
    }

    fn playbook_author(&self) -> playbook::PlaybookAuthor {
        playbook::PlaybookAuthor {
            ctx: self.ctx.clone(),
            prefixes: self.prefixes.clone(),
            tool_calls: self.tool_calls.clone(),
            messages: self.messages.clone(),
            concurrency: self.config.pkm_consolidation_concurrency,
            max_tool_turns: self.config.pkm_playbook_max_tool_turns,
            max_submissions: self.config.pkm_playbook_max_submissions,
        }
    }

    fn page_author(&self) -> PageAuthor {
        PageAuthor {
            ctx: self.ctx.clone(),
            prefixes: self.prefixes.clone(),
            concurrency: self.config.pkm_consolidation_concurrency,
        }
    }

    fn cleanup(&self) -> cleanup::Cleanup {
        cleanup::Cleanup {
            ctx: self.ctx.clone(),
            prefixes: self.prefixes.clone(),
            half_life_secs: self.config.pkm_short_memory_half_life_secs as f32,
            demote_threshold: self.config.pkm_short_memory_demote_threshold,
            keep_records: self.config.pkm_consolidation_keep_records,
        }
    }
}
