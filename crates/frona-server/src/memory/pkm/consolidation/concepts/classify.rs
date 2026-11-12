//! Prepare classification snapshots and bank accepted library results.
use super::progress::ConsolidationProgress;
use super::validation::validate_proposal_projection;
use crate::{
    core::error::AppError,
    memory::pkm::{
        consolidation::Consolidator, model::KnowledgeConsolidationEntity, ontology::OntologyManager,
    },
};
use frona_pkm::classify::{Classification, Classify, ClassifyInput, ClassifyPrompts, ProposalSet};
use frona_pkm::projection::projection_rejection_details;

struct RecordDiagnostics<'a, 'ctx> {
    progress: tokio::sync::Mutex<&'a mut ConsolidationProgress<'ctx>>,
    path: &'a str,
}
#[async_trait::async_trait]
impl frona_pkm::classify::ClassificationDiagnostics for RecordDiagnostics<'_, '_> {
    async fn record(&self, diagnostic: &serde_json::Value) -> Result<(), frona_pkm::Error> {
        self.progress
            .lock()
            .await
            .bank_classify_diagnostic(self.path, diagnostic.clone())
            .await
            .map_err(|error| frona_pkm::Error::Internal(error.to_string()))?;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            "classify-diagnostic",
            self.path,
            diagnostic,
        );
        Ok(())
    }
}

impl Consolidator {
    fn classification_phase(&self) -> Result<Classify<'static>, AppError> {
        let mut prompts = ClassifyPrompts::default();
        let llm = &self.ctx.llm;
        prompts.defaults = llm.prompt_defaults();
        prompts.system = llm.prompt_source("pkm/classify/system.md", &prompts.system)?;
        prompts.input = llm.prompt_source("pkm/classify/input.md", &prompts.input)?;
        prompts.reject = llm.prompt_source("pkm/classify/reject.md", &prompts.reject)?;
        prompts.bad_term = llm.prompt_source("pkm/classify/bad_term.md", &prompts.bad_term)?;
        Ok(Classify::new(llm.harness())
            .candidate_retrieval(std::sync::Arc::new(self.ctx.view.clone()))
            .prompts(prompts)
            .submission_attempts(self.config.pkm_consolidation_max_submissions)
            .exploration_turns(self.config.pkm_consolidation_max_tool_turns))
    }

    pub(crate) async fn classify_entity(
        &self,
        ontology: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        minted: &mut Vec<String>,
        initial_rejection: Option<&str>,
    ) -> Result<bool, AppError> {
        let phase = self.classification_phase()?;
        let diagnostics = RecordDiagnostics {
            progress: tokio::sync::Mutex::new(&mut *progress),
            path: &entity.path,
        };
        let output = phase
            .run_with_diagnostics(
                ClassifyInput {
                    path: entity.path.clone(),
                    graph: self.ctx.view.portable_snapshot(&self.ctx.repo).await?,
                    ontology: ontology.portable(&self.ctx.scope.user_id).await?,
                    proposals: proposals.clone(),
                    initial_rejection: initial_rejection.map(str::to_owned),
                },
                |run| self.ctx.llm.configure(run),
                &diagnostics,
            )
            .await?;
        let Some(classification) = output.classification else {
            return Ok(false);
        };
        progress
            .bank_classification(&entity.path, &classification)
            .await?;
        *proposals = output.proposals;
        minted.extend(output.minted_paths);
        progress
            .checkpoint_transition("classify", &entity.path, proposals)
            .await?;
        Ok(true)
    }

    pub(crate) async fn record_entity_proposal(
        &self,
        ontology: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        classification: &Classification,
        proposals: &mut ProposalSet,
        minted: &mut Vec<String>,
    ) -> Result<bool, AppError> {
        let output = self
            .classification_phase()?
            .replay(
                ClassifyInput {
                    path: entity.path.clone(),
                    graph: self.ctx.view.portable_snapshot(&self.ctx.repo).await?,
                    ontology: ontology.portable(&self.ctx.scope.user_id).await?,
                    proposals: proposals.clone(),
                    initial_rejection: None,
                },
                classification.clone(),
            )
            .await?;
        let recorded = output.classification.is_some();
        *proposals = output.proposals;
        minted.extend(output.minted_paths);
        Ok(recorded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClassificationMode {
    Normal,
    ReferencedShell,
}

impl ClassificationMode {
    const fn replay_stage(self) -> &'static str {
        match self {
            Self::Normal => "classify-replay",
            Self::ReferencedShell => "classify-referenced-shell-replay",
        }
    }

    const fn discard_reason(self) -> &'static str {
        match self {
            Self::Normal => "classification budget exhausted without a valid projection",
            Self::ReferencedShell => "referenced identity could not be classified",
        }
    }

    const fn stages_entity(self) -> bool {
        matches!(self, Self::ReferencedShell)
    }

    pub(super) const fn defers_error(self) -> bool {
        matches!(self, Self::Normal)
    }
}

impl Consolidator {
    pub(super) async fn process_classification_job(
        &self,
        ontology_manager: &crate::memory::pkm::ontology::OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        mode: ClassificationMode,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        minted: &mut Vec<String>,
    ) -> Result<(), AppError> {
        if mode.stages_entity() {
            proposals.stage_entity(entity.clone().into());
        }
        // A classification banked by an earlier attempt at this pass is replayed
        // rather than re-derived. Replay must rebuild both the entity and schema
        // proposals because those are not stored in the checkpoint row.
        let mut replay_rejection = None;
        if let Some(banked) = progress.classification(&entity.path) {
            let mut trial = proposals.clone();
            let mut trial_minted = Vec::new();
            if self
                .record_entity_proposal(
                    ontology_manager,
                    entity,
                    &banked,
                    &mut trial,
                    &mut trial_minted,
                )
                .await?
            {
                let validation =
                    validate_proposal_projection(&self.ctx, ontology_manager, &trial).await?;
                if validation.is_valid() {
                    *proposals = trial;
                    for path in trial_minted {
                        if !minted.contains(&path) {
                            minted.push(path);
                        }
                    }
                    progress
                        .checkpoint_transition(mode.replay_stage(), &entity.path, proposals)
                        .await?;
                    return Ok(());
                }
                replay_rejection = Some(projection_rejection_details(&validation));
                progress.discard_classification(&entity.path).await?;
            }
        }
        match self
            .classify_entity(
                ontology_manager,
                entity,
                proposals,
                progress,
                minted,
                replay_rejection.as_deref(),
            )
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                proposals.forget(&entity.path);
                progress.discard(&entity.path, mode.discard_reason()).await
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ClassificationMode;

    #[test]
    fn normal_classification_policy_stays_explicit() {
        let mode = ClassificationMode::Normal;
        assert_eq!(mode.replay_stage(), "classify-replay");
        assert_eq!(
            mode.discard_reason(),
            "classification budget exhausted without a valid projection",
        );
        assert!(!mode.stages_entity());
        assert!(mode.defers_error());
    }

    #[test]
    fn referenced_shell_classification_policy_stays_explicit() {
        let mode = ClassificationMode::ReferencedShell;
        assert_eq!(mode.replay_stage(), "classify-referenced-shell-replay");
        assert_eq!(
            mode.discard_reason(),
            "referenced identity could not be classified",
        );
        assert!(mode.stages_entity());
        assert!(!mode.defers_error());
    }
}
