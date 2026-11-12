use crate::core::error::AppError;
use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::ontology::OntologyManager;
use frona_pkm::{classify::ProposalSet, ontology::GraphValidation};

/// Revalidate banked work against the current durable view before committing it.
pub(super) async fn validate_proposal_projection(
    ctx: &ConsolidationContext,
    ontology: &OntologyManager,
    proposals: &ProposalSet,
) -> Result<GraphValidation, AppError> {
    let graph = ctx.view.portable_snapshot(&ctx.repo).await?;
    let ontology = ontology.portable(&ctx.scope.user_id).await?;
    Ok(frona_pkm::projection::validate_proposal_projection(&graph, &ontology, proposals).await?)
}
