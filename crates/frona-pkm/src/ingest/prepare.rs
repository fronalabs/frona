pub(crate) fn prepare_ingest_batch(batch: &mut super::IngestBatch) {
    let attributes: Vec<_> = batch
        .entities
        .iter()
        .map(|entity| {
            (
                entity.path.clone(),
                entity.attributes.clone(),
                entity.attribute_evidence.clone(),
            )
        })
        .chain(batch.entity_updates.iter().map(|entity| {
            (
                entity.path.clone(),
                entity.attributes.clone(),
                entity.attribute_evidence.clone(),
            )
        }))
        .collect();
    for (path, attributes, evidence_by_key) in attributes {
        for (key, value) in attributes.as_object().into_iter().flatten() {
            let Some(evidence) = evidence_by_key.get(key) else {
                continue;
            };
            let rendered = match value {
                serde_json::Value::String(value) => value.clone(),
                value => value.to_string(),
            };
            batch.memories.push(super::PendingMemory {
                id: uuid::Uuid::now_v7().to_string(),
                kind: crate::model::MemoryKind::Fact,
                evidence: evidence.clone(),
                episode: None,
                content: format!("{key}: {rendered}"),
                paths: vec![path.clone()],
            });
        }
    }
}
