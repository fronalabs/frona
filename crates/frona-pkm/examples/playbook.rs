//! Resolve and author a playbook without server storage. Requires OPENAI_API_KEY.
use frona_harness::{Harness, Model};
use frona_pkm::{
    ingest::PendingPlaybookCandidate,
    model::{
        Disposition, EntityCategory, KnowledgeConsolidationEntity, KnowledgeMemory, MemoryKind,
    },
    ontology::PrefixMap,
    page::PageLayout,
    playbook::{
        KnowledgeSnapshot,
        author::{PlaybookAuthor, PlaybookAuthorInput},
        resolve::{PlaybookResolve, PlaybookResolveInput},
    },
};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let model = Model::from_rig(client.responses("gpt-5-mini"))?;
    // Normally loaded from accepted Ingest results and the application's store.
    let memory = KnowledgeMemory {
        id: "memory-1".into(),
        user_id: "owner".into(),
        created_at: chrono::Utc::now(),
        kind: MemoryKind::Procedural,
        episode: None,
        content: "Restart the service, then check that its status is active.".into(),
        relations: vec![],
        disposition: Disposition::default(),
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![],
    };
    let targets = PlaybookResolve::new(Harness::new(model.clone()))
        .run(PlaybookResolveInput {
            candidate: PendingPlaybookCandidate {
                id: "candidate-1".into(),
                path: "operations/restart-service".into(),
                name: "Restart a service".into(),
                description: "Restart and verify the running service.".into(),
                source_memory_ids: [memory.id.clone()].into(),
            },
            memories: vec![memory.clone()],
            knowledge: KnowledgeSnapshot::default(),
            staged_paths: Default::default(),
        })
        .await?;
    let author = PlaybookAuthor::new(Harness::new(model));
    for target in targets {
        let mut page = KnowledgeConsolidationEntity::pending(
            "run",
            "owner",
            &target.path,
            EntityCategory::Playbook,
            vec![],
            target.memory_ids.iter().cloned().collect(),
        );
        page.name = target.name;
        page.description = target.description;
        let output = author
            .run(PlaybookAuthorInput {
                path: target.path,
                page: page.as_knowledge_entity(),
                memories: vec![memory.clone()],
                knowledge: KnowledgeSnapshot::default(),
                links: vec![],
                redirects: Default::default(),
                layout: PageLayout::default(),
                prefixes: PrefixMap::standard(),
            })
            .await?;
        // The host commits content/rev before updating its file mirror or checkpoint.
        println!("{}", output.content);
    }
    Ok(())
}
