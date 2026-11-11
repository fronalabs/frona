use crate::model::{
    Disposition, EntityCategory, KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeMemory,
    MemoryKind, PLAYBOOK_KIND_IRI,
};
pub(super) use crate::test_support::tool_call as call;
use crate::test_support::{Requests, output, tool_call};
use frona_harness::Harness;
use rig_core::completion::AssistantContent;
use serde_json::{Value, json};

pub(super) fn harness(outputs: Vec<Vec<AssistantContent>>) -> (Harness<'static>, Requests) {
    crate::test_support::harness(outputs.into_iter().map(|content| Ok(output(content))))
}
pub(super) fn submit(value: Value) -> Vec<AssistantContent> {
    vec![tool_call("submit", value)]
}
pub(super) fn entity(path: &str) -> KnowledgeEntity {
    let mut entity = KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        path,
        EntityCategory::Playbook,
        vec![],
        Default::default(),
    );
    entity.name = "Target".into();
    entity.description = "The reusable procedure".into();
    entity.kinds = vec![PLAYBOOK_KIND_IRI.into()];
    entity.as_knowledge_entity()
}
pub(super) fn memory(id: &str) -> KnowledgeMemory {
    KnowledgeMemory {
        id: id.into(),
        user_id: "owner".into(),
        created_at: chrono::Utc::now(),
        kind: MemoryKind::Procedural,
        episode: None,
        content: "Restart the service and verify its status.".into(),
        relations: vec![],
        disposition: Disposition::default(),
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![],
    }
}
pub(super) fn target() -> Value {
    json!({"existing_path":" /playbooks/target.md ","path":" /playbooks/target.md ","name":"Target","description":"The reusable procedure","memory_ids":["m1"]})
}
