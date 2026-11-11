use std::collections::HashSet;

use crate::ingest::evidence::{
    batch_without_failed_contributions, episode_anchor_is_grounded, resolve_citation,
    resolve_evidence, value_in_declared_sources,
};
use crate::ingest::submission::{Batch, CandidateAttribute};
use crate::ingest::validation::{distinct_assertion_message_count, validate_batch_with_recall};
use crate::model::MemoryKind;
use crate::path::normalize_path;
use crate::{RecallProjection, TranscriptEvidenceSource};

pub(super) fn terminal_cleanup(batch: &mut Batch, sources: &[TranscriptEvidenceSource]) -> usize {
    let mut dropped = 0usize;
    let proposed: HashSet<String> = batch
        .new_entities
        .iter()
        .filter_map(|p| normalize_path(&p.path))
        .collect();
    for entity in &mut batch.new_entities {
        let before = entity.sources.len();
        let cited_handles: HashSet<String> = entity
            .sources
            .iter()
            .map(|citation| citation.message.clone())
            .collect();
        entity.sources.retain(|citation| {
            let handles = cited_handles.iter().map(String::as_str).collect();
            resolve_citation(citation, sources, &handles).is_ok()
        });
        dropped += before - entity.sources.len();
        let before = entity.aliases.len();
        entity
            .aliases
            .retain(|alias| value_in_declared_sources(alias, &entity.sources, sources));
        dropped += before - entity.aliases.len();
        cleanup_attributes(&mut entity.candidate_attributes, sources, &mut dropped);
    }
    let before = batch.new_entities.len();
    batch
        .new_entities
        .retain(|entity| !entity.sources.is_empty());
    dropped += before - batch.new_entities.len();
    for entity in &mut batch.existing_entity_updates {
        cleanup_attributes(&mut entity.candidate_attributes, sources, &mut dropped);
    }
    batch
        .existing_entity_updates
        .retain(|entity| !entity.candidate_attributes.is_empty());
    let kept: HashSet<String> = batch
        .new_entities
        .iter()
        .filter_map(|p| normalize_path(&p.path))
        .collect();
    let removed: HashSet<String> = proposed.difference(&kept).cloned().collect();
    let playbooks: HashSet<String> = batch
        .playbooks
        .iter()
        .map(|p| p.id.trim().to_string())
        .collect();
    let before_memories = batch.memories.len();
    batch.memories.retain(|memory| {
        let kind = MemoryKind::parse(&memory.kind);
        let evidence_ok = resolve_evidence(&memory.sources, sources).is_some();
        let episode_ok = (kind == Some(MemoryKind::Episodic)) == memory.episode.is_some()
            && memory
                .episode
                .as_ref()
                .is_none_or(|episode| episode.duration.is_none() || episode.absolute.is_none())
            && memory.episode.as_ref().is_none_or(|episode| {
                memory
                    .sources
                    .iter()
                    .any(|s| s.message == episode.anchor.message)
                    && episode_anchor_is_grounded(&episode.anchor, sources)
            });
        let procedural = kind == Some(MemoryKind::Procedural);
        let procedural_ok = if procedural {
            evidence_ok
                && resolve_evidence(&memory.sources, sources)
                    .is_some_and(|items| distinct_assertion_message_count(&items) == 1)
                && memory
                    .playbook
                    .as_ref()
                    .is_some_and(|id| playbooks.contains(id.trim()))
        } else {
            memory.playbook.is_none()
        };
        let path_ok = !memory.entities.is_empty()
            && !memory
                .entities
                .iter()
                .all(|p| normalize_path(p).is_some_and(|p| removed.contains(&p)));
        evidence_ok && episode_ok && procedural_ok && path_ok
    });
    dropped += before_memories - batch.memories.len();
    let referenced: HashSet<String> = batch
        .memories
        .iter()
        .filter_map(|m| m.playbook.clone())
        .collect();
    let before = batch.playbooks.len();
    batch.playbooks.retain(|p| referenced.contains(p.id.trim()));
    dropped += before - batch.playbooks.len();
    dropped
}

pub(super) fn terminal_cleanup_with_recall(
    batch: &mut Batch,
    sources: &[TranscriptEvidenceSource],
    recall: &RecallProjection,
) -> usize {
    let mut dropped = terminal_cleanup(batch, sources);
    let failures = validate_batch_with_recall(batch, sources, recall);
    let cleaned = batch_without_failed_contributions(batch, &failures);
    dropped += batch.memories.len().saturating_sub(cleaned.memories.len());
    dropped += batch
        .new_entities
        .len()
        .saturating_sub(cleaned.new_entities.len());
    dropped += batch
        .new_entities
        .iter()
        .map(|entity| entity.candidate_attributes.len())
        .sum::<usize>()
        .saturating_sub(
            cleaned
                .new_entities
                .iter()
                .map(|entity| entity.candidate_attributes.len())
                .sum(),
        );
    dropped += batch
        .existing_entity_updates
        .iter()
        .map(|entity| entity.candidate_attributes.len())
        .sum::<usize>()
        .saturating_sub(
            cleaned
                .existing_entity_updates
                .iter()
                .map(|entity| entity.candidate_attributes.len())
                .sum(),
        );
    *batch = cleaned;
    let supported_paths = batch
        .memories
        .iter()
        .flat_map(|memory| memory.entities.iter())
        .filter_map(|path| normalize_path(path))
        .collect::<HashSet<_>>();
    let before = batch.new_entities.len();
    batch.new_entities.retain(|entity| {
        normalize_path(&entity.path).is_some_and(|path| supported_paths.contains(&path))
    });
    dropped += before - batch.new_entities.len();
    let referenced: HashSet<String> = batch
        .memories
        .iter()
        .filter_map(|memory| memory.playbook.clone())
        .collect();
    let before = batch.playbooks.len();
    batch
        .playbooks
        .retain(|playbook| referenced.contains(playbook.id.trim()));
    dropped += before - batch.playbooks.len();
    dropped
}

pub(super) fn cleanup_attributes(
    attributes: &mut Vec<CandidateAttribute>,
    sources: &[TranscriptEvidenceSource],
    dropped: &mut usize,
) {
    for attribute in attributes.iter_mut() {
        let before = attribute.sources.len();
        let cited_handles: HashSet<String> = attribute
            .sources
            .iter()
            .map(|citation| citation.message.clone())
            .collect();
        attribute.sources.retain(|citation| {
            let handles = cited_handles.iter().map(String::as_str).collect();
            resolve_citation(citation, sources, &handles).is_ok()
        });
        *dropped += before - attribute.sources.len();
    }
    let before = attributes.len();
    attributes.retain(|attribute| {
        !attribute.sources.is_empty()
            && value_in_declared_sources(&attribute.value, &attribute.sources, sources)
    });
    *dropped += before - attributes.len();
}

pub(super) fn candidate_attribute_map(
    candidate_attributes: &[CandidateAttribute],
) -> serde_json::Map<String, serde_json::Value> {
    let mut attributes = serde_json::Map::new();
    for attr in candidate_attributes {
        let (key, value) = (attr.key.trim(), attr.value.trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        attributes.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    attributes
}

pub(super) fn remove_multi_entity_candidate_attributes(batch: &mut Batch) {
    let memories = &batch.memories;
    let conflicts = |path: &str, attribute: &CandidateAttribute| {
        let Some(path) = normalize_path(path) else {
            return false;
        };
        memories.iter().any(|memory| {
            let mut entities = memory
                .entities
                .iter()
                .filter_map(|entity| normalize_path(entity))
                .collect::<Vec<_>>();
            entities.sort();
            entities.dedup();
            entities.len() > 1
                && entities.contains(&path)
                && attribute.sources.iter().any(|attribute_source| {
                    memory.sources.iter().any(|memory_source| {
                        attribute_source.message.trim() == memory_source.message.trim()
                            && attribute_source.quote.trim() == memory_source.quote.trim()
                    })
                })
        })
    };

    for entity in &mut batch.new_entities {
        entity
            .candidate_attributes
            .retain(|attribute| !conflicts(&entity.path, attribute));
    }
    for entity in &mut batch.existing_entity_updates {
        entity
            .candidate_attributes
            .retain(|attribute| !conflicts(&entity.path, attribute));
    }
    batch
        .existing_entity_updates
        .retain(|entity| !entity.candidate_attributes.is_empty());
}
