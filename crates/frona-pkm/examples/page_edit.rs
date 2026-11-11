//! Author a page and interpret a human edit. Requires OPENAI_API_KEY.
use frona_harness::{Harness, Model};
use frona_pkm::{
    model::{EntityCategory, KnowledgeConsolidationEntity, KnowledgeMemory, MemoryKind},
    page::{PageAuthor, PageAuthorInput},
    writeback::{Writeback, WritebackInput},
};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let model = Model::from_rig(client.responses("gpt-5-mini"))?;
    // Normally loaded from the application's accepted graph and memory snapshot.
    let memory = KnowledgeMemory {
        id: "memory-1".into(),
        user_id: "owner".into(),
        created_at: chrono::Utc::now(),
        kind: MemoryKind::Fact,
        episode: None,
        content: "The service runs on port 8080.".into(),
        relations: vec![],
        disposition: Default::default(),
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![],
    };
    let mut entity = KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        "systems/service",
        EntityCategory::Concept,
        vec![],
        [memory.id.clone()].into(),
    );
    entity.name = "Service".into();
    entity.description = "The application service.".into();
    let page = PageAuthor::new(Harness::new(model.clone()))
        .run(PageAuthorInput::new(
            entity.as_knowledge_entity(),
            vec![memory.clone()],
        ))
        .await?;
    println!("{}", page.content);
    // The host commits content/rev before publishing its file mirror. Later, it
    // checks the edited file against that revision and supplies both versions.
    let incoming = format!("{}\nThe service now runs on port 9090.\n", page.content);
    let operations = Writeback::new(Harness::new(model))
        .run(WritebackInput {
            path: page.path,
            head: page.content,
            incoming,
            memories: vec![memory],
        })
        .await?;
    // The host applies these operations atomically with the adopted page revision.
    println!("{}", serde_json::to_string_pretty(&operations)?);
    Ok(())
}
