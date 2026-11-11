//! Application-owned correctness and one-shot advisory inspection.
use super::run::ReconcileLogic;
use super::{
    EntityVerdict,
    projection::{
        close_property_replacements, memory_replacement_property_rejections,
        property_replacement_rejections, unsupported_scope_relations,
    },
    validation::{
        RelationPromotionValidation, assertion_provenance_rejections, promotion_suggestions,
        reconcile_declaration_rejections, relation_promotion_rejections,
        render_promotion_suggestions, unsupported_replaces,
    },
};
use crate::Error;
use crate::prompt::PromptIds;
use crate::{
    classify::ProposalSet,
    model::{
        AttributeSource, KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeEntityLink,
        KnowledgeMemory,
    },
    projection::{projection_rejection_details, validate_proposal_projection},
};
use frona_harness::Decision;
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
};

/// Own the captured inputs so each inspection remains Send at server task boundaries.
/// Prompt-local values are validated here; only an accepted result is expanded for application.
pub(super) struct ReconciliationInspection {
    pub reconciler: ReconcileLogic,
    pub path: String,
    pub current: Vec<KnowledgeMemory>,
    pub scopes: HashMap<String, Vec<String>>,
    pub attribute_sources: Vec<AttributeSource>,
    pub prompt_links: Vec<KnowledgeEntityLink>,
    pub known_object_properties: HashSet<String>,
    pub known_data_properties: HashSet<String>,
    pub suggestion_entities: Vec<KnowledgeEntity>,
    pub suggestions_sent: AtomicBool,
    pub proposals: ProposalSet,
    pub page: KnowledgeConsolidationEntity,
    pub links: Vec<KnowledgeEntityLink>,
    pub memories: Vec<KnowledgeMemory>,
    pub ids: PromptIds,
}

impl ReconciliationInspection {
    pub async fn inspect(
        &self,
        mut candidate: EntityVerdict,
    ) -> Result<Decision<EntityVerdict>, Error> {
        let reconciler = &self.reconciler;
        let ctx = &reconciler.input;
        let prefixes = &reconciler.prefixes;
        let current = &self.current;
        let source_path = self.path.as_str();
        let scopes = &self.scopes;
        let existing_attribute_sources = &self.attribute_sources;
        let existing_links = &self.prompt_links;
        let known_object_properties = &self.known_object_properties;
        let known_data_properties = &self.known_data_properties;
        let suggestion_entities = &self.suggestion_entities;
        let suggestion_links = &self.links;
        let sent = &self.suggestions_sent;
        let proposal_snapshot = &self.proposals;
        let validation_page = &self.page;
        let validation_links = &self.links;
        let validation_memories = &self.memories;
        let validation_ids = &self.ids;
        close_property_replacements(&mut candidate);
        let mut rejections = reconcile_declaration_rejections(
            &candidate,
            known_object_properties,
            known_data_properties,
            prefixes,
        );
        rejections.extend(unsupported_replaces(&candidate, current));
        rejections.extend(property_replacement_rejections(
            &candidate,
            current,
            existing_attribute_sources,
            existing_links,
        ));
        rejections.extend(memory_replacement_property_rejections(
            &candidate,
            existing_attribute_sources,
            existing_links,
        ));
        rejections.extend(unsupported_scope_relations(&candidate, scopes, source_path));
        rejections.extend(assertion_provenance_rejections(
            &candidate,
            current,
            existing_attribute_sources,
            existing_links,
            scopes,
            source_path,
        ));
        rejections.extend(
            relation_promotion_rejections(
                &candidate,
                RelationPromotionValidation {
                    ctx,
                    proposals: proposal_snapshot,
                    source_path,
                    prefixes,
                    memories: current,
                    known_data_properties,
                    existing: proposal_snapshot.promotions_for(source_path),
                },
            )
            .await?,
        );
        let advisory = if !sent.swap(true, Ordering::Relaxed) {
            let suggestions = promotion_suggestions(
                source_path,
                &candidate,
                suggestion_entities,
                suggestion_links,
            );
            if !suggestions.is_empty() {
                Some(render_promotion_suggestions(
                    &reconciler.prompts,
                    &suggestions,
                )?)
            } else {
                None
            }
        } else {
            None
        };
        let mut expanded = candidate.clone();
        expanded.expand_prompt_ids(validation_ids)?;
        let trial = reconciler
            .reconcile_trial(
                source_path,
                validation_page,
                validation_links,
                validation_memories,
                &expanded,
                proposal_snapshot,
            )
            .await?;
        let validation = validate_proposal_projection(
            &reconciler.input.graph,
            &reconciler.input.ontology,
            &trial,
        )
        .await?;
        if !validation.is_valid() {
            rejections.push(projection_rejection_details(&validation));
        }
        if rejections.is_empty() && advisory.is_none() {
            return Ok(Decision::accept(candidate));
        }
        let mut feedback = if rejections.is_empty() {
            String::new()
        } else {
            reconciler.prompts.reject(&rejections.join("\n"))?
        };
        if let Some(advisory) = advisory {
            if !feedback.is_empty() {
                feedback.push_str("\n\n");
            }
            feedback.push_str(&advisory);
        }
        Ok(Decision::revise(feedback))
    }
}
