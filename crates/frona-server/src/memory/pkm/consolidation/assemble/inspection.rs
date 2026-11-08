//! Application ranking and validation for one in-memory adjudication dialogue.
use super::*;
use frona_harness::Decision as InspectionDecision;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Keep the evaluated plan and original source together even when execution stops.
#[derive(Debug)]
pub(super) struct AdjudicationCandidate {
    pub plan: AssemblePlan,
    pub submission: AdjudicationResult,
}

pub(super) struct AdjudicationInspection {
    consolidator: Consolidator,
    ontology: OntologyManager,
    candidates: Vec<Proposal>,
    projected_abox: Vec<Triple>,
    accepted_edits: Vec<SchemaEdit>,
    most_edits: AtomicUsize,
}

impl AdjudicationInspection {
    pub fn new(
        consolidator: Consolidator,
        ontology: OntologyManager,
        candidates: Vec<Proposal>,
        projected_abox: Vec<Triple>,
        accepted_edits: Vec<SchemaEdit>,
    ) -> Self {
        Self {
            consolidator,
            ontology,
            candidates,
            projected_abox,
            accepted_edits,
            most_edits: AtomicUsize::new(0),
        }
    }

    pub async fn inspect(
        &self,
        submission: AdjudicationResult,
    ) -> Result<InspectionDecision<AdjudicationCandidate>, AppError> {
        let consolidator = &self.consolidator;
        if let Some(feedback) = bad_term_feedback(
            &consolidator.ctx,
            PromptSpec::ASSEMBLE,
            &consolidator.prefixes,
            submission
                .decisions
                .iter()
                .flat_map(|decision| decision.decision.proposed_terms()),
        )? {
            // No replacement: a malformed revision cannot discard prior progress.
            return Ok(InspectionDecision::revise(feedback));
        }
        let (plan, rejected) = consolidator
            .gate_submission(
                &self.ontology,
                &self.candidates,
                &submission,
                &self.projected_abox,
                &self.accepted_edits,
            )
            .await?;
        let improved = plan.edits.len() >= self.most_edits.load(Ordering::Relaxed);
        if improved {
            self.most_edits.store(plan.edits.len(), Ordering::Relaxed);
        }
        let candidate = AdjudicationCandidate { plan, submission };
        if rejected.is_empty() {
            return Ok(if improved {
                InspectionDecision::accept(candidate)
            } else {
                InspectionDecision::abandon("complete revision is worse than retained adjudication")
            });
        }
        let feedback = consolidator.ctx.llm.reject(
            PromptSpec::ASSEMBLE,
            &[("rejections", &rejected.join("\n"))],
        )?;
        let decision = InspectionDecision::revise(feedback);
        Ok(if improved {
            decision.retain(candidate)
        } else {
            decision
        })
    }
}
