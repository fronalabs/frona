//! Explicit boundary between SurrealDB records and portable PKM snapshots.
use super::*;
use frona_pkm::model as portable;
use surrealdb::types::ToSql;

impl From<ConsolidationEntityLifecycle> for portable::ConsolidationEntityLifecycle {
    fn from(value: ConsolidationEntityLifecycle) -> Self {
        match value {
            ConsolidationEntityLifecycle::Pending => Self::Pending,
            ConsolidationEntityLifecycle::Active => Self::Active,
            ConsolidationEntityLifecycle::Coalesced => Self::Coalesced,
            ConsolidationEntityLifecycle::Discarded => Self::Discarded,
        }
    }
}

impl From<portable::ConsolidationEntityLifecycle> for ConsolidationEntityLifecycle {
    fn from(value: portable::ConsolidationEntityLifecycle) -> Self {
        match value {
            portable::ConsolidationEntityLifecycle::Pending => Self::Pending,
            portable::ConsolidationEntityLifecycle::Active => Self::Active,
            portable::ConsolidationEntityLifecycle::Coalesced => Self::Coalesced,
            portable::ConsolidationEntityLifecycle::Discarded => Self::Discarded,
        }
    }
}

impl From<PendingEntityContribution> for portable::PendingEntityContribution {
    fn from(value: PendingEntityContribution) -> Self {
        Self {
            name: value.name,
            description: value.description,
            aliases: value.aliases,
            attributes: value.attributes,
            attribute_evidence: value
                .attribute_evidence
                .into_iter()
                .map(|(key, value)| (key, value.into_iter().map(|value| value.into()).collect()))
                .collect(),
            source_memory_ids: value.source_memory_ids,
            existing_only: value.existing_only,
            occurrence_count: value.occurrence_count,
        }
    }
}

impl From<portable::PendingEntityContribution> for PendingEntityContribution {
    fn from(value: portable::PendingEntityContribution) -> Self {
        Self {
            name: value.name,
            description: value.description,
            aliases: value.aliases,
            attributes: value.attributes,
            attribute_evidence: value
                .attribute_evidence
                .into_iter()
                .map(|(key, value)| (key, value.into_iter().map(|value| value.into()).collect()))
                .collect(),
            source_memory_ids: value.source_memory_ids,
            existing_only: value.existing_only,
            occurrence_count: value.occurrence_count,
        }
    }
}

impl From<ConsolidationEntityLink> for portable::ConsolidationEntityLink {
    fn from(value: ConsolidationEntityLink) -> Self {
        Self {
            relation: value.relation,
            target_path: value.target_path,
            origin: value.origin.into(),
            source_memory_ids: value.source_memory_ids,
        }
    }
}

impl From<portable::ConsolidationEntityLink> for ConsolidationEntityLink {
    fn from(value: portable::ConsolidationEntityLink) -> Self {
        Self {
            relation: value.relation,
            target_path: value.target_path,
            origin: value.origin.into(),
            source_memory_ids: value.source_memory_ids,
        }
    }
}

impl From<ConsolidationEntityProgress> for portable::ConsolidationEntityProgress {
    fn from(value: ConsolidationEntityProgress) -> Self {
        Self {
            classification: value.classification.into(),
            classification_diagnostic: value.classification_diagnostic,
            identity: value.identity.into(),
            reconciliation: value.reconciliation.into(),
            playbook_resolution: value.playbook_resolution.into(),
        }
    }
}

impl From<portable::ConsolidationEntityProgress> for ConsolidationEntityProgress {
    fn from(value: portable::ConsolidationEntityProgress) -> Self {
        Self {
            classification: value.classification.into(),
            classification_diagnostic: value.classification_diagnostic,
            identity: value.identity.into(),
            reconciliation: value.reconciliation.into(),
            playbook_resolution: value.playbook_resolution.into(),
        }
    }
}

impl From<ClassificationProgress> for portable::ClassificationProgress {
    fn from(value: ClassificationProgress) -> Self {
        match value {
            ClassificationProgress::Pending => Self::Pending,
            ClassificationProgress::Accepted { decision } => Self::Accepted { decision },
            ClassificationProgress::Discarded { reason, at } => Self::Discarded { reason, at },
        }
    }
}

impl From<portable::ClassificationProgress> for ClassificationProgress {
    fn from(value: portable::ClassificationProgress) -> Self {
        match value {
            portable::ClassificationProgress::Pending => Self::Pending,
            portable::ClassificationProgress::Accepted { decision } => Self::Accepted { decision },
            portable::ClassificationProgress::Discarded { reason, at } => {
                Self::Discarded { reason, at }
            }
        }
    }
}

impl From<IdentityProgress> for portable::IdentityProgress {
    fn from(value: IdentityProgress) -> Self {
        match value {
            IdentityProgress::Pending => Self::Pending,
            IdentityProgress::Distinct {
                fingerprint,
                evidence,
            } => Self::Distinct {
                fingerprint,
                evidence,
            },
            IdentityProgress::Coalesced {
                canonical_path,
                evidence,
            } => Self::Coalesced {
                canonical_path,
                evidence,
            },
            IdentityProgress::Unresolved {
                fingerprint,
                diagnostic,
            } => Self::Unresolved {
                fingerprint,
                diagnostic,
            },
        }
    }
}

impl From<portable::IdentityProgress> for IdentityProgress {
    fn from(value: portable::IdentityProgress) -> Self {
        match value {
            portable::IdentityProgress::Pending => Self::Pending,
            portable::IdentityProgress::Distinct {
                fingerprint,
                evidence,
            } => Self::Distinct {
                fingerprint,
                evidence,
            },
            portable::IdentityProgress::Coalesced {
                canonical_path,
                evidence,
            } => Self::Coalesced {
                canonical_path,
                evidence,
            },
            portable::IdentityProgress::Unresolved {
                fingerprint,
                diagnostic,
            } => Self::Unresolved {
                fingerprint,
                diagnostic,
            },
        }
    }
}

impl From<ReconciliationProgress> for portable::ReconciliationProgress {
    fn from(value: ReconciliationProgress) -> Self {
        match value {
            ReconciliationProgress::Pending => Self::Pending,
            ReconciliationProgress::Accepted {
                promotions,
                retractions,
            } => Self::Accepted {
                promotions,
                retractions,
            },
        }
    }
}

impl From<portable::ReconciliationProgress> for ReconciliationProgress {
    fn from(value: portable::ReconciliationProgress) -> Self {
        match value {
            portable::ReconciliationProgress::Pending => Self::Pending,
            portable::ReconciliationProgress::Accepted {
                promotions,
                retractions,
            } => Self::Accepted {
                promotions,
                retractions,
            },
        }
    }
}

impl From<PlaybookResolutionProgress> for portable::PlaybookResolutionProgress {
    fn from(value: PlaybookResolutionProgress) -> Self {
        match value {
            PlaybookResolutionProgress::Pending => Self::Pending,
            PlaybookResolutionProgress::Accepted { proposal } => Self::Accepted { proposal },
            PlaybookResolutionProgress::Failed { error } => Self::Failed { error },
        }
    }
}

impl From<portable::PlaybookResolutionProgress> for PlaybookResolutionProgress {
    fn from(value: portable::PlaybookResolutionProgress) -> Self {
        match value {
            portable::PlaybookResolutionProgress::Pending => Self::Pending,
            portable::PlaybookResolutionProgress::Accepted { proposal } => {
                Self::Accepted { proposal }
            }
            portable::PlaybookResolutionProgress::Failed { error } => Self::Failed { error },
        }
    }
}

impl From<KnowledgeConsolidationEntity> for portable::KnowledgeConsolidationEntity {
    fn from(value: KnowledgeConsolidationEntity) -> Self {
        Self {
            consolidation_entity_id: value.consolidation_entity_id,
            entity_id: value.entity_id,
            consolidation_id: value.consolidation_id,
            user_id: value.user_id,
            path: value.path,
            category: value.category.into(),
            lifecycle: value.lifecycle.into(),
            searchable: value.searchable,
            canonical_path: value.canonical_path,
            contributions: value
                .contributions
                .into_iter()
                .map(|value| value.into())
                .collect(),
            origin: value.origin.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            identity_evidence: value
                .identity_evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
            aliases: value.aliases,
            attributes: value.attributes,
            attribute_sources: value
                .attribute_sources
                .into_iter()
                .map(|value| value.into())
                .collect(),
            body: value.body,
            sync_content: value.sync_content,
            mirrored_rev: value.mirrored_rev,
            extracted_rev: value.extracted_rev,
            related_playbooks: value.related_playbooks,
            use_count: value.use_count,
            rev: value.rev,
            rendered_at: value.rendered_at,
            outgoing_links: value
                .outgoing_links
                .into_iter()
                .map(|value| value.into())
                .collect(),
            source_memory_ids: value.source_memory_ids,
            search_text: value.search_text,
            search_names: value.search_names,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            progress: value.progress.into(),
            checkpoint_revision: value.checkpoint_revision,
            updated_at: value.updated_at,
        }
    }
}

impl From<portable::KnowledgeConsolidationEntity> for KnowledgeConsolidationEntity {
    fn from(value: portable::KnowledgeConsolidationEntity) -> Self {
        Self {
            consolidation_entity_id: value.consolidation_entity_id,
            entity_id: value.entity_id,
            consolidation_id: value.consolidation_id,
            user_id: value.user_id,
            path: value.path,
            category: value.category.into(),
            lifecycle: value.lifecycle.into(),
            searchable: value.searchable,
            canonical_path: value.canonical_path,
            contributions: value
                .contributions
                .into_iter()
                .map(|value| value.into())
                .collect(),
            origin: value.origin.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            identity_evidence: value
                .identity_evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
            aliases: value.aliases,
            attributes: value.attributes,
            attribute_sources: value
                .attribute_sources
                .into_iter()
                .map(|value| value.into())
                .collect(),
            body: value.body,
            sync_content: value.sync_content,
            mirrored_rev: value.mirrored_rev,
            extracted_rev: value.extracted_rev,
            related_playbooks: value.related_playbooks,
            use_count: value.use_count,
            rev: value.rev,
            rendered_at: value.rendered_at,
            outgoing_links: value
                .outgoing_links
                .into_iter()
                .map(|value| value.into())
                .collect(),
            source_memory_ids: value.source_memory_ids,
            search_text: value.search_text,
            search_names: value.search_names,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            progress: value.progress.into(),
            checkpoint_revision: value.checkpoint_revision,
            updated_at: value.updated_at,
        }
    }
}

impl From<KnowledgeEntitySource> for portable::KnowledgeEntitySource {
    fn from(value: KnowledgeEntitySource) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            memory_id: value.memory_id,
            entity_path: value.entity_path,
            created_at: value.created_at,
        }
    }
}

impl From<portable::KnowledgeEntitySource> for KnowledgeEntitySource {
    fn from(value: portable::KnowledgeEntitySource) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            memory_id: value.memory_id,
            entity_path: value.entity_path,
            created_at: value.created_at,
        }
    }
}

impl From<AttributeSource> for portable::AttributeSource {
    fn from(value: AttributeSource) -> Self {
        Self {
            property: value.property,
            value: value.value,
            source_memory_ids: value.source_memory_ids,
        }
    }
}

impl From<portable::AttributeSource> for AttributeSource {
    fn from(value: portable::AttributeSource) -> Self {
        Self {
            property: value.property,
            value: value.value,
            source_memory_ids: value.source_memory_ids,
        }
    }
}

impl From<KnowledgeEntity> for portable::KnowledgeEntity {
    fn from(value: KnowledgeEntity) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            path: value.path,
            origin: value.origin.into(),
            category: value.category.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            identity_evidence: value
                .identity_evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
            attribute_sources: value
                .attribute_sources
                .into_iter()
                .map(|value| value.into())
                .collect(),
            source_memory_ids: value.source_memory_ids,
            body: value.body,
            sync_content: value.sync_content,
            mirrored_rev: value.mirrored_rev,
            extracted_rev: value.extracted_rev,
            related_playbooks: value.related_playbooks,
            search_text: value.search_text,
            search_names: value.search_names,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            attributes: value.attributes,
            use_count: value.use_count,
            aliases: value.aliases,
            rev: value.rev,
            updated_at: value.updated_at,
            rendered_at: value.rendered_at,
        }
    }
}

impl From<portable::KnowledgeEntity> for KnowledgeEntity {
    fn from(value: portable::KnowledgeEntity) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            path: value.path,
            origin: value.origin.into(),
            category: value.category.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            identity_evidence: value
                .identity_evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
            attribute_sources: value
                .attribute_sources
                .into_iter()
                .map(|value| value.into())
                .collect(),
            source_memory_ids: value.source_memory_ids,
            body: value.body,
            sync_content: value.sync_content,
            mirrored_rev: value.mirrored_rev,
            extracted_rev: value.extracted_rev,
            related_playbooks: value.related_playbooks,
            search_text: value.search_text,
            search_names: value.search_names,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            attributes: value.attributes,
            use_count: value.use_count,
            aliases: value.aliases,
            rev: value.rev,
            updated_at: value.updated_at,
            rendered_at: value.rendered_at,
        }
    }
}

impl From<LinkOrigin> for portable::LinkOrigin {
    fn from(value: LinkOrigin) -> Self {
        match value {
            LinkOrigin::Asserted => Self::Asserted,
            LinkOrigin::Inferred => Self::Inferred,
        }
    }
}

impl From<portable::LinkOrigin> for LinkOrigin {
    fn from(value: portable::LinkOrigin) -> Self {
        match value {
            portable::LinkOrigin::Asserted => Self::Asserted,
            portable::LinkOrigin::Inferred => Self::Inferred,
        }
    }
}

impl From<KnowledgeEntityLink> for portable::KnowledgeEntityLink {
    fn from(value: KnowledgeEntityLink) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            from_entity_path: value.from_entity_path,
            to_entity_path: value.to_entity_path,
            relation: value.relation,
            source_memory_ids: value.source_memory_ids,
            origin: value.origin.into(),
            created_at: value.created_at,
        }
    }
}

impl From<portable::KnowledgeEntityLink> for KnowledgeEntityLink {
    fn from(value: portable::KnowledgeEntityLink) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            from_entity_path: value.from_entity_path,
            to_entity_path: value.to_entity_path,
            relation: value.relation,
            source_memory_ids: value.source_memory_ids,
            origin: value.origin.into(),
            created_at: value.created_at,
        }
    }
}

impl From<EntityHit> for portable::EntityHit {
    fn from(value: EntityHit) -> Self {
        Self {
            path: value.path,
            origin: value.origin.into(),
            category: value.category.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            aliases: value.aliases,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            body: value.body,
        }
    }
}

impl From<portable::EntityHit> for EntityHit {
    fn from(value: portable::EntityHit) -> Self {
        Self {
            path: value.path,
            origin: value.origin.into(),
            category: value.category.into(),
            kinds: value.kinds,
            name: value.name,
            description: value.description,
            aliases: value.aliases,
            search_name_tokens: value.search_name_tokens,
            search_assertions: value.search_assertions,
            body: value.body,
        }
    }
}

impl From<RankedEntityHit> for portable::RankedEntityHit {
    fn from(value: RankedEntityHit) -> Self {
        Self {
            entity: value.entity.into(),
            score: value.score,
            use_count: value.use_count,
        }
    }
}

impl From<portable::RankedEntityHit> for RankedEntityHit {
    fn from(value: portable::RankedEntityHit) -> Self {
        Self {
            entity: value.entity.into(),
            score: value.score,
            use_count: value.use_count,
        }
    }
}

impl From<KnowledgeOntology> for portable::KnowledgeOntology {
    fn from(value: KnowledgeOntology) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            owl: value.owl,
            format: value.format,
            version: value.version,
            effective_ontology: value.effective_ontology,
            seeds: value.seeds,
            sources: value.sources,
            catalog_fingerprint: value.catalog_fingerprint,
            updated_at: value.updated_at,
        }
    }
}

impl From<portable::KnowledgeOntology> for KnowledgeOntology {
    fn from(value: portable::KnowledgeOntology) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            owl: value.owl,
            format: value.format,
            version: value.version,
            effective_ontology: value.effective_ontology,
            seeds: value.seeds,
            sources: value.sources,
            catalog_fingerprint: value.catalog_fingerprint,
            updated_at: value.updated_at,
        }
    }
}

impl From<MemoryKind> for portable::MemoryKind {
    fn from(value: MemoryKind) -> Self {
        match value {
            MemoryKind::Identity => Self::Identity,
            MemoryKind::Preference => Self::Preference,
            MemoryKind::Fact => Self::Fact,
            MemoryKind::Reference => Self::Reference,
            MemoryKind::Episodic => Self::Episodic,
            MemoryKind::Procedural => Self::Procedural,
        }
    }
}

impl From<portable::MemoryKind> for MemoryKind {
    fn from(value: portable::MemoryKind) -> Self {
        match value {
            portable::MemoryKind::Identity => Self::Identity,
            portable::MemoryKind::Preference => Self::Preference,
            portable::MemoryKind::Fact => Self::Fact,
            portable::MemoryKind::Reference => Self::Reference,
            portable::MemoryKind::Episodic => Self::Episodic,
            portable::MemoryKind::Procedural => Self::Procedural,
        }
    }
}

impl From<EvidenceStrength> for portable::EvidenceStrength {
    fn from(value: EvidenceStrength) -> Self {
        match value {
            EvidenceStrength::Explicit => Self::Explicit,
            EvidenceStrength::Derived => Self::Derived,
            EvidenceStrength::Inferred => Self::Inferred,
        }
    }
}

impl From<portable::EvidenceStrength> for EvidenceStrength {
    fn from(value: portable::EvidenceStrength) -> Self {
        match value {
            portable::EvidenceStrength::Explicit => Self::Explicit,
            portable::EvidenceStrength::Derived => Self::Derived,
            portable::EvidenceStrength::Inferred => Self::Inferred,
        }
    }
}

impl From<EpisodeStatus> for portable::EpisodeStatus {
    fn from(value: EpisodeStatus) -> Self {
        match value {
            EpisodeStatus::Planned => Self::Planned,
            EpisodeStatus::Occurred => Self::Occurred,
            EpisodeStatus::Cancelled => Self::Cancelled,
            EpisodeStatus::Unconfirmed => Self::Unconfirmed,
        }
    }
}

impl From<portable::EpisodeStatus> for EpisodeStatus {
    fn from(value: portable::EpisodeStatus) -> Self {
        match value {
            portable::EpisodeStatus::Planned => Self::Planned,
            portable::EpisodeStatus::Occurred => Self::Occurred,
            portable::EpisodeStatus::Cancelled => Self::Cancelled,
            portable::EpisodeStatus::Unconfirmed => Self::Unconfirmed,
        }
    }
}

impl From<TemporalDirection> for portable::TemporalDirection {
    fn from(value: TemporalDirection) -> Self {
        match value {
            TemporalDirection::Past => Self::Past,
            TemporalDirection::Present => Self::Present,
            TemporalDirection::Future => Self::Future,
        }
    }
}

impl From<portable::TemporalDirection> for TemporalDirection {
    fn from(value: portable::TemporalDirection) -> Self {
        match value {
            portable::TemporalDirection::Past => Self::Past,
            portable::TemporalDirection::Present => Self::Present,
            portable::TemporalDirection::Future => Self::Future,
        }
    }
}

impl From<TemporalUnit> for portable::TemporalUnit {
    fn from(value: TemporalUnit) -> Self {
        match value {
            TemporalUnit::Minute => Self::Minute,
            TemporalUnit::Hour => Self::Hour,
            TemporalUnit::Day => Self::Day,
            TemporalUnit::Week => Self::Week,
            TemporalUnit::Month => Self::Month,
            TemporalUnit::Year => Self::Year,
        }
    }
}

impl From<portable::TemporalUnit> for TemporalUnit {
    fn from(value: portable::TemporalUnit) -> Self {
        match value {
            portable::TemporalUnit::Minute => Self::Minute,
            portable::TemporalUnit::Hour => Self::Hour,
            portable::TemporalUnit::Day => Self::Day,
            portable::TemporalUnit::Week => Self::Week,
            portable::TemporalUnit::Month => Self::Month,
            portable::TemporalUnit::Year => Self::Year,
        }
    }
}

impl From<TemporalSemantics> for portable::TemporalSemantics {
    fn from(value: TemporalSemantics) -> Self {
        match value {
            TemporalSemantics::Elapsed => Self::Elapsed,
            TemporalSemantics::Calendar => Self::Calendar,
        }
    }
}

impl From<portable::TemporalSemantics> for TemporalSemantics {
    fn from(value: portable::TemporalSemantics) -> Self {
        match value {
            portable::TemporalSemantics::Elapsed => Self::Elapsed,
            portable::TemporalSemantics::Calendar => Self::Calendar,
        }
    }
}

impl From<RelativeDuration> for portable::RelativeDuration {
    fn from(value: RelativeDuration) -> Self {
        Self {
            direction: value.direction.into(),
            amount: value.amount,
            unit: value.unit.into(),
            semantics: value.semantics.into(),
        }
    }
}

impl From<portable::RelativeDuration> for RelativeDuration {
    fn from(value: portable::RelativeDuration) -> Self {
        Self {
            direction: value.direction.into(),
            amount: value.amount,
            unit: value.unit.into(),
            semantics: value.semantics.into(),
        }
    }
}

impl From<AbsoluteTime> for portable::AbsoluteTime {
    fn from(value: AbsoluteTime) -> Self {
        Self {
            year: value.year,
            month: value.month,
            day: value.day,
            hour: value.hour,
            minute: value.minute,
        }
    }
}

impl From<portable::AbsoluteTime> for AbsoluteTime {
    fn from(value: portable::AbsoluteTime) -> Self {
        Self {
            year: value.year,
            month: value.month,
            day: value.day,
            hour: value.hour,
            minute: value.minute,
        }
    }
}

impl From<Episode> for portable::Episode {
    fn from(value: Episode) -> Self {
        Self {
            status: value.status.into(),
            anchor: value.anchor.into(),
            duration: value.duration.map(|value| value.into()),
            absolute: value.absolute.map(|value| value.into()),
            resolved_start: value.resolved_start,
            resolved_end: value.resolved_end,
        }
    }
}

impl From<portable::Episode> for Episode {
    fn from(value: portable::Episode) -> Self {
        Self {
            status: value.status.into(),
            anchor: value.anchor.into(),
            duration: value.duration.map(|value| value.into()),
            absolute: value.absolute.map(|value| value.into()),
            resolved_start: value.resolved_start,
            resolved_end: value.resolved_end,
        }
    }
}

impl From<TemporalAnchor> for portable::TemporalAnchor {
    fn from(value: TemporalAnchor) -> Self {
        Self {
            message: value.message,
            quote: value.quote,
        }
    }
}

impl From<portable::TemporalAnchor> for TemporalAnchor {
    fn from(value: portable::TemporalAnchor) -> Self {
        Self {
            message: value.message,
            quote: value.quote,
        }
    }
}

impl From<EntityCategory> for portable::EntityCategory {
    fn from(value: EntityCategory) -> Self {
        match value {
            EntityCategory::Concept => Self::Concept,
            EntityCategory::Playbook => Self::Playbook,
        }
    }
}

impl From<portable::EntityCategory> for EntityCategory {
    fn from(value: portable::EntityCategory) -> Self {
        match value {
            portable::EntityCategory::Concept => Self::Concept,
            portable::EntityCategory::Playbook => Self::Playbook,
        }
    }
}

impl From<Disposition> for portable::Disposition {
    fn from(value: Disposition) -> Self {
        match value {
            Disposition::None => Self::None,
            Disposition::Outdated => Self::Outdated,
            Disposition::Erroneous => Self::Erroneous,
            Disposition::Suspect => Self::Suspect,
        }
    }
}

impl From<portable::Disposition> for Disposition {
    fn from(value: portable::Disposition) -> Self {
        match value {
            portable::Disposition::None => Self::None,
            portable::Disposition::Outdated => Self::Outdated,
            portable::Disposition::Erroneous => Self::Erroneous,
            portable::Disposition::Suspect => Self::Suspect,
        }
    }
}

impl From<RelationType> for portable::RelationType {
    fn from(value: RelationType) -> Self {
        match value {
            RelationType::Replace => Self::Replace,
            RelationType::Duplicate => Self::Duplicate,
            RelationType::Absorbed => Self::Absorbed,
        }
    }
}

impl From<portable::RelationType> for RelationType {
    fn from(value: portable::RelationType) -> Self {
        match value {
            portable::RelationType::Replace => Self::Replace,
            portable::RelationType::Duplicate => Self::Duplicate,
            portable::RelationType::Absorbed => Self::Absorbed,
        }
    }
}

impl From<MemoryRelation> for portable::MemoryRelation {
    fn from(value: MemoryRelation) -> Self {
        Self {
            relation: value.relation.into(),
            to: match value.to.key {
                surrealdb::types::RecordIdKey::String(id) => id,
                key => key.to_sql(),
            },
            note: value.note,
        }
    }
}

impl From<portable::MemoryRelation> for MemoryRelation {
    fn from(value: portable::MemoryRelation) -> Self {
        Self {
            relation: value.relation.into(),
            to: RecordId::new("knowledge_memory", value.to),
            note: value.note,
        }
    }
}

impl From<EvidenceSource> for portable::EvidenceSource {
    fn from(value: EvidenceSource) -> Self {
        match value {
            EvidenceSource::UserMessage {
                message_id,
                chat_id,
                quote,
            } => Self::UserMessage {
                message_id,
                chat_id,
                quote,
            },
            EvidenceSource::UserConfirmation {
                message_id,
                chat_id,
                quote,
            } => Self::UserConfirmation {
                message_id,
                chat_id,
                quote,
            },
            EvidenceSource::AgentMessage {
                message_id,
                agent_id,
                chat_id,
                quote,
            } => Self::AgentMessage {
                message_id,
                agent_id,
                chat_id,
                quote,
            },
            EvidenceSource::WebSearch {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                query,
                url,
            } => Self::WebSearch {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                query,
                url,
            },
            EvidenceSource::WebPage {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                url,
            } => Self::WebPage {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                url,
            },
            EvidenceSource::ToolResult {
                message_id,
                chat_id,
                tool_call_id,
                quote,
            } => Self::ToolResult {
                message_id,
                chat_id,
                tool_call_id,
                quote,
            },
            EvidenceSource::TaskLifecycle {
                message_id,
                chat_id,
                task_id,
            } => Self::TaskLifecycle {
                message_id,
                chat_id,
                task_id,
            },
            EvidenceSource::HumanEdit { page_path, quote } => Self::HumanEdit { page_path, quote },
            EvidenceSource::ExternalNote { note, quote } => Self::ExternalNote { note, quote },
        }
    }
}

impl From<portable::EvidenceSource> for EvidenceSource {
    fn from(value: portable::EvidenceSource) -> Self {
        match value {
            portable::EvidenceSource::UserMessage {
                message_id,
                chat_id,
                quote,
            } => Self::UserMessage {
                message_id,
                chat_id,
                quote,
            },
            portable::EvidenceSource::UserConfirmation {
                message_id,
                chat_id,
                quote,
            } => Self::UserConfirmation {
                message_id,
                chat_id,
                quote,
            },
            portable::EvidenceSource::AgentMessage {
                message_id,
                agent_id,
                chat_id,
                quote,
            } => Self::AgentMessage {
                message_id,
                agent_id,
                chat_id,
                quote,
            },
            portable::EvidenceSource::WebSearch {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                query,
                url,
            } => Self::WebSearch {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                query,
                url,
            },
            portable::EvidenceSource::WebPage {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                url,
            } => Self::WebPage {
                message_id,
                chat_id,
                tool_call_id,
                quote,
                url,
            },
            portable::EvidenceSource::ToolResult {
                message_id,
                chat_id,
                tool_call_id,
                quote,
            } => Self::ToolResult {
                message_id,
                chat_id,
                tool_call_id,
                quote,
            },
            portable::EvidenceSource::TaskLifecycle {
                message_id,
                chat_id,
                task_id,
            } => Self::TaskLifecycle {
                message_id,
                chat_id,
                task_id,
            },
            portable::EvidenceSource::HumanEdit { page_path, quote } => {
                Self::HumanEdit { page_path, quote }
            }
            portable::EvidenceSource::ExternalNote { note, quote } => {
                Self::ExternalNote { note, quote }
            }
        }
    }
}

impl From<MemoryEvidence> for portable::MemoryEvidence {
    fn from(value: MemoryEvidence) -> Self {
        Self {
            strength: value.strength.into(),
            source: value.source.into(),
        }
    }
}

impl From<portable::MemoryEvidence> for MemoryEvidence {
    fn from(value: portable::MemoryEvidence) -> Self {
        Self {
            strength: value.strength.into(),
            source: value.source.into(),
        }
    }
}

impl From<EntityOrigin> for portable::EntityOrigin {
    fn from(value: EntityOrigin) -> Self {
        match value {
            EntityOrigin::Internal => Self::Internal,
            EntityOrigin::External => Self::External,
        }
    }
}

impl From<portable::EntityOrigin> for EntityOrigin {
    fn from(value: portable::EntityOrigin) -> Self {
        match value {
            portable::EntityOrigin::Internal => Self::Internal,
            portable::EntityOrigin::External => Self::External,
        }
    }
}

impl From<KnowledgeShortMemory> for portable::KnowledgeShortMemory {
    fn from(value: KnowledgeShortMemory) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            content: value.content,
            created_at: value.created_at,
            last_accessed_at: value.last_accessed_at,
            source_chat_id: value.source_chat_id,
            validated: value.validated,
        }
    }
}

impl From<portable::KnowledgeShortMemory> for KnowledgeShortMemory {
    fn from(value: portable::KnowledgeShortMemory) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            content: value.content,
            created_at: value.created_at,
            last_accessed_at: value.last_accessed_at,
            source_chat_id: value.source_chat_id,
            validated: value.validated,
        }
    }
}

impl From<KnowledgeMemory> for portable::KnowledgeMemory {
    fn from(value: KnowledgeMemory) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            created_at: value.created_at,
            kind: value.kind.into(),
            episode: value.episode.map(|value| value.into()),
            content: value.content,
            relations: value
                .relations
                .into_iter()
                .map(|value| value.into())
                .collect(),
            disposition: value.disposition.into(),
            ended_at: value.ended_at,
            comment: value.comment,
            erroneous_at: value.erroneous_at,
            evidence: value
                .evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
        }
    }
}

impl From<portable::KnowledgeMemory> for KnowledgeMemory {
    fn from(value: portable::KnowledgeMemory) -> Self {
        Self {
            id: value.id,
            user_id: value.user_id,
            created_at: value.created_at,
            kind: value.kind.into(),
            episode: value.episode.map(|value| value.into()),
            content: value.content,
            relations: value
                .relations
                .into_iter()
                .map(|value| value.into())
                .collect(),
            disposition: value.disposition.into(),
            ended_at: value.ended_at,
            comment: value.comment,
            erroneous_at: value.erroneous_at,
            evidence: value
                .evidence
                .into_iter()
                .map(|value| value.into())
                .collect(),
        }
    }
}
