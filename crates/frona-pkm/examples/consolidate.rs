//! Run ontology-backed phases without a Frona server or database. Requires
//! OPENAI_API_KEY and PKM_ONTOLOGY_DIR (a directory containing ontology files).
use chrono::Utc;
use frona_harness::{Harness, Model};
use frona_pkm::{
    GraphSnapshot,
    assemble::{Assemble, AssembleInput},
    classify::{Classify, ClassifyInput, ProposalSet},
    model::{
        Disposition, EntityCategory, KnowledgeConsolidationEntity, KnowledgeMemory,
        KnowledgeOntology, MemoryKind,
    },
    ontology::{Ontology, OntologyCatalogue, catalogue::roots::Root},
    reconcile::{Reconcile, ReconcileInput},
    resolve::{IdentityResolution, Resolve, ResolveInput},
};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let model = Model::from_rig(client.responses("gpt-5-mini"))?;
    let root = std::path::PathBuf::from(std::env::var("PKM_ONTOLOGY_DIR")?);
    let catalogue = OntologyCatalogue::load(&[(Root::User, root.as_path())])?;

    // Normally the host builds this snapshot from accepted Ingest output and its store.
    let memory = KnowledgeMemory {
        id: "memory-1".into(),
        user_id: "owner".into(),
        created_at: Utc::now(),
        kind: MemoryKind::Fact,
        episode: None,
        content: "Casey is a software engineer.".into(),
        relations: vec![],
        disposition: Disposition::None,
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![],
    };
    let mut entity = KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        "people/casey",
        EntityCategory::Concept,
        vec![],
        [memory.id.clone()].into(),
    );
    entity.name = "Casey".into();
    entity.description = "A software engineer.".into();
    entity.rederive_search();
    let graph = GraphSnapshot {
        scope_id: "owner".into(),
        memory_entities: [(memory.id.clone(), vec![entity.path.clone()])].into(),
        entities: vec![entity],
        links: vec![],
        memories: vec![memory],
    };
    let ontology = Ontology::new(
        catalogue,
        KnowledgeOntology {
            id: "ontology".into(),
            user_id: graph.scope_id.clone(),
            owl: "Ontology()".into(),
            format: "ofn".into(),
            version: 0,
            effective_ontology: String::new(),
            seeds: vec![],
            sources: vec![],
            catalog_fingerprint: String::new(),
            updated_at: Utc::now(),
        },
        graph.clone(),
    );
    let classified = Classify::new(Harness::new(model.clone()))
        .run(ClassifyInput {
            path: "people/casey".into(),
            graph: graph.clone(),
            ontology: ontology.clone(),
            proposals: ProposalSet::default(),
            initial_rejection: None,
        })
        .await?;
    if classified.classification.is_none() {
        println!("Classification rejected: {}", classified.diagnostic);
        return Ok(());
    }
    // Carry accepted proposals between phases; nothing has been committed yet.
    let resolved = Resolve::new(Harness::new(model.clone()))
        .run(ResolveInput {
            path: "people/casey".into(),
            graph: graph.clone(),
            ontology: ontology.clone(),
            proposals: classified.proposals,
            eligible_targets: ["people/casey".into()].into(),
            reconsider: false,
            prior_identity_fingerprint: None,
            prior_pair_fingerprints: Default::default(),
        })
        .await?;
    match &resolved.decision {
        Some(IdentityResolution::Distinct { .. }) => {}
        // A real host applies merges, refreshes its snapshot, and requeues affected
        // identities. Unresolved investigations remain pending for a later retry.
        other => {
            println!("Host must handle identity decision: {other:?}");
            return Ok(());
        }
    }
    let reconciled = Reconcile::new(Harness::new(model.clone()))
        .run(ReconcileInput {
            path: "people/casey".into(),
            graph: graph.clone(),
            ontology: ontology.clone(),
            proposals: resolved.proposals,
            initial_rejection: None,
        })
        .await?;
    println!("Accepted entity update: {:?}", reconciled.entity_update);
    println!(
        "Entities requiring another pass: {:?}",
        reconciled.reconcile_dirty
    );
    if !reconciled.reconcile_dirty.is_empty() || reconciled.move_entity.is_some() {
        // Apply the requested lifecycle/move changes, refresh the snapshot, and
        // run the queued work before assembling the complete graph.
        return Ok(());
    }
    let plan = Assemble::new(Harness::new(model))
        .run(AssembleInput {
            graph,
            ontology,
            proposals: reconciled.proposals,
            banked_adjudication: None,
            resolved_into: Default::default(),
            discarded_paths: Default::default(),
        })
        .await?;
    println!(
        "Atomic plan for ontology version {}: {} entity types, {} schema edits",
        plan.expected_ontology_version,
        plan.types.len(),
        plan.edits.len()
    );
    // The host persists the returned schema/type/attribute/coalescing plan and
    // accepted reconciliation changes atomically, then advances its checkpoint.
    // On a version conflict, rebuild snapshots and rerun Assemble.
    Ok(())
}
