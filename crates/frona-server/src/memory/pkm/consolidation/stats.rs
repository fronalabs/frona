use surrealdb::types::SurrealValue;

use crate::db::repo::pkm::{IngestBatch, IngestCounts};

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ResearchCoverageStats {
    pub messages: usize,
    pub extracted: usize,
    pub no_durable_claim: usize,
    pub duplicate: usize,
    pub unsupported: usize,
    pub memories_added_by_repair: usize,
    pub citation_repairs: usize,
    pub mixed_claim_splits: usize,
    pub claims: usize,
    pub claims_extracted: usize,
    pub claims_no_durable_claim: usize,
    pub claims_duplicate: usize,
    pub claims_unsupported: usize,
}

impl ResearchCoverageStats {
    pub fn add(&mut self, other: &Self) {
        self.messages += other.messages;
        self.extracted += other.extracted;
        self.no_durable_claim += other.no_durable_claim;
        self.duplicate += other.duplicate;
        self.unsupported += other.unsupported;
        self.memories_added_by_repair += other.memories_added_by_repair;
        self.citation_repairs += other.citation_repairs;
        self.mixed_claim_splits += other.mixed_claim_splits;
        self.claims += other.claims;
        self.claims_extracted += other.claims_extracted;
        self.claims_no_durable_claim += other.claims_no_durable_claim;
        self.claims_duplicate += other.claims_duplicate;
        self.claims_unsupported += other.claims_unsupported;
    }
}

/// What a pass counted, folded in as each stage completes.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ConsolidationStats {
    pub memories_added: usize,
    pub entities_created: usize,
    pub entities_reconciled: usize,
    pub supersessions_recorded: usize,
    pub moves_applied: usize,
    pub playbooks_built: usize,
    pub pages_built: usize,
    pub short_memory_dropped: usize,
    pub orphans_gced: usize,
    pub dropped_gced: usize,
    pub entities_gced: usize,
    /// Cleanup stage: entities re-created for memories whose entity had gone missing.
    pub entities_rehomed: usize,
    /// Classify stage: entities typed against the OWL schema this pass.
    pub entities_typed: usize,
    /// Resolve stage: duplicate mention-entities merged into a canonical entity.
    pub entities_merged: usize,
    /// Resolve passes: one initial sweep plus one incremental sweep when Reconcile first
    /// changes identity-visible state.
    pub resolve_sweeps: usize,
    /// Candidate searches completed across all Resolve sweeps.
    pub resolve_candidate_evaluations: usize,
    /// Candidate searches attributable to incremental checks after the first sweep.
    pub resolve_candidate_evaluations_after_first_sweep: usize,
    /// Fingerprint changes that required a fresh distinct-or-merge decision. This also
    /// counts decisions with no candidates, which require no model conversation.
    pub resolve_decision_attempts: usize,
    /// Candidate evaluations skipped because the previously banked input was unchanged.
    pub resolve_fingerprint_skips: usize,
    /// Model conversations started for decision attempts with at least one candidate.
    pub resolve_conversations: usize,
    /// Decision attempts after the first sweep because the visible identity input changed.
    pub resolve_reconsiderations: usize,
    /// Reconsiderations that had candidates and therefore started a model conversation.
    pub resolve_reconsideration_conversations: usize,
    /// Losing entity paths merged by decisions made after the first sweep.
    pub resolve_merges_after_first_sweep: usize,
    /// Reconciled entities whose narrow identity state changed after the initial sweep.
    pub resolve_identity_state_changes: usize,
    /// Candidate pairs re-submitted because identity evidence strengthened or a name/class changed.
    pub resolve_identity_pair_changes: usize,
    /// Candidate-pair evidence removals banked without spending a judge conversation.
    pub resolve_identity_pair_weakenings: usize,
    /// Candidate pairs merged with validated Resolve evidence.
    pub resolve_merges_with_evidence: usize,
    /// Strong candidates kept distinct with validated Resolve evidence.
    pub resolve_distinct_with_evidence: usize,
    /// Resolve submissions rejected by the evidence validator.
    pub resolve_evidence_corrections: usize,
    /// Candidate pairs left separate after the correction budget was exhausted.
    pub resolve_unresolved_pairs: usize,
    /// Classify stage: entities created for an entity an attribute value named and
    /// nothing had an entity for. Distinct from `entities_created`, which is extract's count of
    /// mentions the transcript talked *about* - these are entities it only mentioned in
    /// passing, and before this they stayed literal strings forever.
    pub entities_minted: usize,
    /// Assemble stage: facts quarantined `Suspect` after a reasoning clash.
    pub facts_quarantined: usize,
    /// Assemble stage: quarantined facts released once their entity stopped violating
    /// anything - the reverse of `facts_quarantined`.
    pub facts_reinstated: usize,
    pub grounding_corrections: usize,
    pub grounding_items_dropped: usize,
    pub recall_result_lookups: usize,
    pub agent_evidence_no_tool_drops: usize,
    pub agent_evidence_strong_matches: usize,
    pub agent_evidence_fallback_reviews: usize,
    pub agent_evidence_fallback_retains: usize,
    pub agent_evidence_invalid_submissions: usize,
    pub agent_evidence_lookup_calls: usize,
    pub agent_evidence_terminal_drops: usize,
    pub research_coverage: ResearchCoverageStats,
}

#[derive(Debug, Default)]
pub struct ClassifyOutcome {
    pub entities_minted: usize,
}

#[derive(Debug, Default)]
pub struct ResolveOutcome {
    pub entities_merged: usize,
    pub resolve_sweeps: usize,
    pub resolve_candidate_evaluations: usize,
    pub resolve_candidate_evaluations_after_first_sweep: usize,
    pub resolve_decision_attempts: usize,
    pub resolve_fingerprint_skips: usize,
    pub resolve_conversations: usize,
    pub resolve_reconsiderations: usize,
    pub resolve_reconsideration_conversations: usize,
    pub resolve_merges_after_first_sweep: usize,
    pub resolve_identity_state_changes: usize,
    pub resolve_identity_pair_changes: usize,
    pub resolve_identity_pair_weakenings: usize,
    pub resolve_merges_with_evidence: usize,
    pub resolve_distinct_with_evidence: usize,
    pub resolve_evidence_corrections: usize,
    pub resolve_unresolved_pairs: usize,
}

#[derive(Debug, Default)]
pub struct AssembleOutcome {
    pub entities_typed: usize,
    pub facts_quarantined: usize,
    pub facts_reinstated: usize,
}

#[derive(Debug, Default)]
pub struct ReconcileOutcome {
    pub entities_reconciled: usize,
    pub supersessions_recorded: usize,
    pub moves_applied: usize,
}

#[derive(Debug, Default)]
pub struct PlaybookAuthorOutcome {
    pub playbooks_built: usize,
}

#[derive(Debug, Default)]
pub struct PageAuthorOutcome {
    pub pages_built: usize,
}

#[derive(Debug, Default)]
pub struct CleanupOutcome {
    pub short_memory_dropped: usize,
    pub orphans_gced: usize,
    pub dropped_gced: usize,
    pub entities_gced: usize,
    pub entities_rehomed: usize,
}

impl ConsolidationStats {
    /// Bank diagnostics from committed extraction windows in the flat checkpoint fields.
    pub fn absorb_ingest_batch(&mut self, batch: &IngestBatch) {
        self.grounding_corrections += batch.grounding_corrections;
        self.grounding_items_dropped += batch.grounding_items_dropped;
        self.recall_result_lookups += batch.recall_result_lookups;
        self.agent_evidence_no_tool_drops += batch.agent_evidence_no_tool_drops;
        self.agent_evidence_strong_matches += batch.agent_evidence_strong_matches;
        self.agent_evidence_fallback_reviews += batch.agent_evidence_fallback_reviews;
        self.agent_evidence_fallback_retains += batch.agent_evidence_fallback_retains;
        self.agent_evidence_invalid_submissions += batch.agent_evidence_invalid_submissions;
        self.agent_evidence_lookup_calls += batch.agent_evidence_lookup_calls;
        self.agent_evidence_terminal_drops += batch.agent_evidence_terminal_drops;
        self.research_coverage.add(&batch.research_coverage);
    }

    /// Bank the row counts that only the extraction transaction can know.
    pub fn absorb_ingest_counts(&mut self, counts: &IngestCounts) {
        self.memories_added += counts.memories_added;
        self.entities_created += counts.entities_created;
    }

    pub fn absorb_classify(&mut self, o: ClassifyOutcome) {
        self.entities_minted += o.entities_minted;
    }

    pub fn absorb_resolve(&mut self, o: ResolveOutcome) {
        self.entities_merged += o.entities_merged;
        self.resolve_sweeps += o.resolve_sweeps;
        self.resolve_candidate_evaluations += o.resolve_candidate_evaluations;
        self.resolve_candidate_evaluations_after_first_sweep +=
            o.resolve_candidate_evaluations_after_first_sweep;
        self.resolve_decision_attempts += o.resolve_decision_attempts;
        self.resolve_fingerprint_skips += o.resolve_fingerprint_skips;
        self.resolve_conversations += o.resolve_conversations;
        self.resolve_reconsiderations += o.resolve_reconsiderations;
        self.resolve_reconsideration_conversations += o.resolve_reconsideration_conversations;
        self.resolve_merges_after_first_sweep += o.resolve_merges_after_first_sweep;
        self.resolve_identity_state_changes += o.resolve_identity_state_changes;
        self.resolve_identity_pair_changes += o.resolve_identity_pair_changes;
        self.resolve_identity_pair_weakenings += o.resolve_identity_pair_weakenings;
        self.resolve_merges_with_evidence += o.resolve_merges_with_evidence;
        self.resolve_distinct_with_evidence += o.resolve_distinct_with_evidence;
        self.resolve_evidence_corrections += o.resolve_evidence_corrections;
        self.resolve_unresolved_pairs += o.resolve_unresolved_pairs;
    }

    pub fn absorb_assemble(&mut self, o: AssembleOutcome) {
        self.entities_typed += o.entities_typed;
        self.facts_quarantined += o.facts_quarantined;
        self.facts_reinstated += o.facts_reinstated;
    }

    pub fn absorb_reconcile(&mut self, o: &ReconcileOutcome) {
        self.entities_reconciled += o.entities_reconciled;
        self.supersessions_recorded += o.supersessions_recorded;
        self.moves_applied += o.moves_applied;
    }

    pub fn absorb_playbook_author(&mut self, o: PlaybookAuthorOutcome) {
        self.playbooks_built += o.playbooks_built;
    }

    pub fn absorb_page_author(&mut self, o: PageAuthorOutcome) {
        self.pages_built += o.pages_built;
    }

    pub fn absorb_cleanup(&mut self, o: CleanupOutcome) {
        self.short_memory_dropped += o.short_memory_dropped;
        self.orphans_gced += o.orphans_gced;
        self.dropped_gced += o.dropped_gced;
        self.entities_gced += o.entities_gced;
        self.entities_rehomed += o.entities_rehomed;
    }

    /// Fold another stage's counts in. The fields partition cleanly by stage - each has
    /// exactly one writer - so this is a plain sum with no double-counting.
    ///
    /// A new counter has to be added here as well as to the struct. That is the one thing
    /// this shape does not catch, and it is worth it: the alternative generates both from
    /// one list and costs you the ability to grep a counter to its declaration.
    pub fn merge(&mut self, other: ConsolidationStats) {
        self.memories_added += other.memories_added;
        self.entities_created += other.entities_created;
        self.entities_reconciled += other.entities_reconciled;
        self.supersessions_recorded += other.supersessions_recorded;
        self.moves_applied += other.moves_applied;
        self.playbooks_built += other.playbooks_built;
        self.pages_built += other.pages_built;
        self.short_memory_dropped += other.short_memory_dropped;
        self.orphans_gced += other.orphans_gced;
        self.dropped_gced += other.dropped_gced;
        self.entities_gced += other.entities_gced;
        self.entities_rehomed += other.entities_rehomed;
        self.entities_typed += other.entities_typed;
        self.entities_merged += other.entities_merged;
        self.resolve_sweeps += other.resolve_sweeps;
        self.resolve_candidate_evaluations += other.resolve_candidate_evaluations;
        self.resolve_candidate_evaluations_after_first_sweep +=
            other.resolve_candidate_evaluations_after_first_sweep;
        self.resolve_decision_attempts += other.resolve_decision_attempts;
        self.resolve_fingerprint_skips += other.resolve_fingerprint_skips;
        self.resolve_conversations += other.resolve_conversations;
        self.resolve_reconsiderations += other.resolve_reconsiderations;
        self.resolve_reconsideration_conversations += other.resolve_reconsideration_conversations;
        self.resolve_merges_after_first_sweep += other.resolve_merges_after_first_sweep;
        self.resolve_identity_state_changes += other.resolve_identity_state_changes;
        self.resolve_identity_pair_changes += other.resolve_identity_pair_changes;
        self.resolve_identity_pair_weakenings += other.resolve_identity_pair_weakenings;
        self.resolve_merges_with_evidence += other.resolve_merges_with_evidence;
        self.resolve_distinct_with_evidence += other.resolve_distinct_with_evidence;
        self.resolve_evidence_corrections += other.resolve_evidence_corrections;
        self.resolve_unresolved_pairs += other.resolve_unresolved_pairs;
        self.entities_minted += other.entities_minted;
        self.facts_quarantined += other.facts_quarantined;
        self.facts_reinstated += other.facts_reinstated;
        self.grounding_corrections += other.grounding_corrections;
        self.grounding_items_dropped += other.grounding_items_dropped;
        self.recall_result_lookups += other.recall_result_lookups;
        self.agent_evidence_no_tool_drops += other.agent_evidence_no_tool_drops;
        self.agent_evidence_strong_matches += other.agent_evidence_strong_matches;
        self.agent_evidence_fallback_reviews += other.agent_evidence_fallback_reviews;
        self.agent_evidence_fallback_retains += other.agent_evidence_fallback_retains;
        self.agent_evidence_invalid_submissions += other.agent_evidence_invalid_submissions;
        self.agent_evidence_lookup_calls += other.agent_evidence_lookup_calls;
        self.agent_evidence_terminal_drops += other.agent_evidence_terminal_drops;
        self.research_coverage.add(&other.research_coverage);
    }
}

impl From<frona_pkm::ResearchCoverageStats> for ResearchCoverageStats {
    fn from(value: frona_pkm::ResearchCoverageStats) -> Self {
        Self {
            messages: value.messages,
            extracted: value.extracted,
            no_durable_claim: value.no_durable_claim,
            duplicate: value.duplicate,
            unsupported: value.unsupported,
            memories_added_by_repair: value.memories_added_by_repair,
            citation_repairs: value.citation_repairs,
            mixed_claim_splits: value.mixed_claim_splits,
            claims: value.claims,
            claims_extracted: value.claims_extracted,
            claims_no_durable_claim: value.claims_no_durable_claim,
            claims_duplicate: value.claims_duplicate,
            claims_unsupported: value.claims_unsupported,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coverage(value: usize) -> ResearchCoverageStats {
        ResearchCoverageStats {
            messages: value,
            extracted: value,
            no_durable_claim: value,
            duplicate: value,
            unsupported: value,
            memories_added_by_repair: value,
            citation_repairs: value,
            mixed_claim_splits: value,
            claims: value,
            claims_extracted: value,
            claims_no_durable_claim: value,
            claims_duplicate: value,
            claims_unsupported: value,
        }
    }

    #[test]
    fn ingest_metrics_sum_across_committed_windows() {
        let mut combined = IngestBatch {
            grounding_corrections: 1,
            grounding_items_dropped: 2,
            recall_result_lookups: 3,
            agent_evidence_no_tool_drops: 4,
            agent_evidence_strong_matches: 5,
            agent_evidence_fallback_reviews: 6,
            agent_evidence_fallback_retains: 7,
            agent_evidence_invalid_submissions: 8,
            agent_evidence_lookup_calls: 9,
            agent_evidence_terminal_drops: 10,
            research_coverage: coverage(11),
            ..Default::default()
        };
        let mut second = IngestBatch {
            grounding_corrections: 10,
            grounding_items_dropped: 20,
            recall_result_lookups: 30,
            agent_evidence_no_tool_drops: 40,
            agent_evidence_strong_matches: 50,
            agent_evidence_fallback_reviews: 60,
            agent_evidence_fallback_retains: 70,
            agent_evidence_invalid_submissions: 80,
            agent_evidence_lookup_calls: 90,
            agent_evidence_terminal_drops: 100,
            research_coverage: coverage(110),
            ..Default::default()
        };
        combined.merge_from(&mut second);

        let mut stats = ConsolidationStats::default();
        stats.absorb_ingest_batch(&combined);
        stats.absorb_ingest_counts(&IngestCounts {
            entities_created: 12,
            memories_added: 13,
        });

        assert_eq!(stats.grounding_corrections, 11);
        assert_eq!(stats.grounding_items_dropped, 22);
        assert_eq!(stats.recall_result_lookups, 33);
        assert_eq!(stats.agent_evidence_no_tool_drops, 44);
        assert_eq!(stats.agent_evidence_strong_matches, 55);
        assert_eq!(stats.agent_evidence_fallback_reviews, 66);
        assert_eq!(stats.agent_evidence_fallback_retains, 77);
        assert_eq!(stats.agent_evidence_invalid_submissions, 88);
        assert_eq!(stats.agent_evidence_lookup_calls, 99);
        assert_eq!(stats.agent_evidence_terminal_drops, 110);
        assert_eq!(stats.research_coverage.messages, 121);
        assert_eq!(stats.research_coverage.extracted, 121);
        assert_eq!(stats.research_coverage.no_durable_claim, 121);
        assert_eq!(stats.research_coverage.duplicate, 121);
        assert_eq!(stats.research_coverage.unsupported, 121);
        assert_eq!(stats.research_coverage.memories_added_by_repair, 121);
        assert_eq!(stats.research_coverage.citation_repairs, 121);
        assert_eq!(stats.research_coverage.mixed_claim_splits, 121);
        assert_eq!(stats.research_coverage.claims, 121);
        assert_eq!(stats.research_coverage.claims_extracted, 121);
        assert_eq!(stats.research_coverage.claims_no_durable_claim, 121);
        assert_eq!(stats.research_coverage.claims_duplicate, 121);
        assert_eq!(stats.research_coverage.claims_unsupported, 121);
        assert_eq!(stats.entities_created, 12);
        assert_eq!(stats.memories_added, 13);
    }

    #[test]
    fn ingest_metrics_keep_the_flat_checkpoint_shape() {
        let stats = ConsolidationStats {
            memories_added: 1,
            grounding_corrections: 2,
            research_coverage: coverage(3),
            ..Default::default()
        };

        let value = serde_json::to_value(stats).unwrap();
        assert_eq!(value["memories_added"], 1);
        assert_eq!(value["grounding_corrections"], 2);
        assert_eq!(value["research_coverage"]["messages"], 3);
        assert!(value.get("extraction_metrics").is_none());
    }
}
