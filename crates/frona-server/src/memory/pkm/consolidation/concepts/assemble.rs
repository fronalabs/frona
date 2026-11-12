//! Prepare portable assembly and atomically publish its plan with a version check.
use super::progress::ConsolidationProgress;
use crate::{
    core::error::AppError,
    db::repo::pkm::AttributeOps,
    memory::pkm::{
        consolidation::{AssembleOutcome, Consolidator},
        model::{KnowledgeConsolidationEntity, KnowledgeEntity},
        ontology::OntologyManager,
    },
};
use frona_pkm::{
    assemble::{
        AdjudicationCheckpoint, Assemble, AssembleInput, AssemblePrompts,
        adjudicate::AdjudicationResult,
    },
    classify::ProposalSet,
};
use std::collections::HashSet;

struct BankAdjudication<'a, 'ctx> {
    progress: tokio::sync::Mutex<&'a mut ConsolidationProgress<'ctx>>,
}
#[async_trait::async_trait]
impl AdjudicationCheckpoint for BankAdjudication<'_, '_> {
    async fn record(&self, result: &AdjudicationResult) -> Result<(), frona_pkm::Error> {
        self.progress
            .lock()
            .await
            .bank_adjudication(result)
            .await
            .map_err(|error| frona_pkm::Error::Internal(error.to_string()))
    }
}

impl Consolidator {
    pub(super) async fn assemble(
        &self,
        ontology: &OntologyManager,
        _entities: &[KnowledgeConsolidationEntity],
        proposals: &ProposalSet,
        _stats: &mut AssembleOutcome,
        _quarantined: &mut HashSet<String>,
        progress: &mut ConsolidationProgress<'_>,
    ) -> Result<usize, AppError> {
        if proposals.by_path.is_empty() {
            return Ok(0);
        }
        let llm = &self.ctx.llm;
        let mut prompts = AssemblePrompts::default();
        prompts.defaults = llm.prompt_defaults();
        prompts.system = llm.prompt_source("pkm/assemble/system.md", &prompts.system)?;
        prompts.input = llm.prompt_source("pkm/assemble/input.md", &prompts.input)?;
        prompts.reject = llm.prompt_source("pkm/assemble/reject.md", &prompts.reject)?;
        prompts.bad_term = llm.prompt_source("pkm/assemble/bad_term.md", &prompts.bad_term)?;
        let phase = Assemble::new(llm.harness())
            .prompts(prompts)
            .submission_attempts(self.config.pkm_adjudication_max_attempts_per_batch)
            .cancellation(llm.cancellation());
        for attempt in 0..3 {
            let mut graph = self.ctx.view.portable_snapshot(&self.ctx.repo).await?;
            // Resolved rows retain source provenance for the publication transaction.
            for path in progress.resolved_into().keys() {
                if let Some(row) = progress.entity_row(path)
                    && !graph.entities.iter().any(|entity| entity.path == row.path)
                {
                    graph.entities.push(row.clone().into());
                }
            }
            let input = AssembleInput {
                graph,
                ontology: ontology.portable(&self.ctx.scope.user_id).await?,
                proposals: proposals.clone(),
                banked_adjudication: progress.adjudication(),
                resolved_into: progress.resolved_into().clone(),
                discarded_paths: progress.discarded_paths().cloned().collect(),
            };
            let checkpoint = BankAdjudication {
                progress: tokio::sync::Mutex::new(&mut *progress),
            };
            let output = phase
                .run_with_checkpoint(input, |run| llm.configure(run), &checkpoint)
                .await?;
            let attributes: Vec<_> = output
                .attributes
                .into_iter()
                .map(|ops| AttributeOps {
                    path: ops.path,
                    rekeys: ops.rekeys,
                    promoted: ops.promoted,
                    retracted: ops.retracted,
                })
                .collect();
            let materialize: Vec<KnowledgeEntity> =
                output.materialize.into_iter().map(Into::into).collect();
            let mut completed = self.ctx.record().await;
            completed.stats.entities_created += output.entities_created;
            completed.state = completed.state.next();
            completed.attempts = 0;
            completed.updated_at = chrono::Utc::now();
            if self
                .ctx
                .repo
                .commit_schema_and_types(
                    &self.ctx.scope.user_id,
                    &output.ontology_delta,
                    crate::memory::pkm::ontology::DELTA_FORMAT,
                    output.expected_ontology_version,
                    &output.types,
                    &output.rekeys,
                    &output.relation_type_renames,
                    &attributes,
                    &materialize,
                    &output.coalesced_sources,
                    &output.coalesced_aliases,
                    &output.working_outcomes,
                    Some(&completed),
                )
                .await?
            {
                self.ctx.adopt_committed_record(completed).await;
                return Ok(output.stamped);
            }
            tracing::warn!(
                attempt = attempt + 1,
                "pkm assemble: schema CAS miss, refreshing phase snapshots"
            );
        }
        Err(AppError::Conflict(
            "assemble: schema commit exceeded its CAS retry budget".into(),
        ))
    }
}
