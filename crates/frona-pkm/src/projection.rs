use crate::Error;
use crate::GraphSnapshot;
use crate::classify::ProposalSet;
use crate::model::KnowledgeConsolidationEntity;
use crate::ontology::{GraphValidation, Ontology};

/// Validate the exact graph represented by one proposal snapshot.
pub async fn validate_proposal_projection(
    ctx: &GraphSnapshot,
    ontology: &Ontology,
    proposals: &ProposalSet,
) -> Result<GraphValidation, Error> {
    let user_id = &ctx.scope_id;
    let durable = ctx.knowledge_entities();
    let links = ctx.asserted_links();
    let (entities, links) = proposals.project_graph(user_id, durable, links);
    ontology
        .validate_graph(user_id, &entities, &links, &[], &proposals.proposed_edits)
        .await
}

pub fn projection_rejection_details(validation: &GraphValidation) -> String {
    validation
        .grouped()
        .iter()
        .map(|group| {
            format!(
                "- [{}] affected={} axioms={:?} examples={}",
                group.rule,
                group.affected_count,
                group.causal_axioms,
                serde_json::to_string(&group.examples).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_unbacked_shell(entity: &KnowledgeConsolidationEntity) -> bool {
    entity.entity_id.is_none() && entity.source_memory_ids.is_empty()
}

#[cfg(test)]
mod tests;
