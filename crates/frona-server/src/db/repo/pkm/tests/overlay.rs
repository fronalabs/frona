#[tokio::test]
async fn consolidation_overlay_shadows_live_reads_and_searches_both_sources() {
    use crate::db::repo::pkm::{KnowledgeConsolidationEntity, PkmConsolidationStore};
    use crate::memory::pkm::PendingEntityContribution;

    let committed = std::sync::Arc::new(repo().await);
    committed
        .upsert_entity_skeleton(
            "u",
            "services/postgres",
            EntityCategory::Concept,
            &[],
            "Postgres",
            "production database",
            &[],
        )
        .await
        .unwrap();
    committed
        .upsert_entity_skeleton(
            "u",
            "services/redis",
            EntityCategory::Concept,
            &[],
            "Redis",
            "production database cache",
            &[],
        )
        .await
        .unwrap();
    committed
        .upsert_entity_skeleton(
            "u",
            "web/example-documentation",
            EntityCategory::Concept,
            &[],
            "Example Documentation",
            "example.com reference",
            &[],
        )
        .await
        .unwrap();
    let effective = PkmConsolidationStore::new(committed.clone()).scoped("run-1", "u");
    let contribution = |name: &str, description: &str| PendingEntityContribution {
        name: name.into(),
        description: description.into(),
        aliases: Default::default(),
        attributes: serde_json::json!({}),
        attribute_evidence: Default::default(),
        source_memory_ids: Default::default(),
        existing_only: false,
        occurrence_count: 1,
    };

    effective
        .upsert_entity(KnowledgeConsolidationEntity::pending(
            "run-1",
            "u",
            "services/postgres",
            EntityCategory::Concept,
            vec![contribution("Postgres", "pending relational service")],
            Default::default(),
        ))
        .await
        .unwrap();
    effective
        .upsert_entity(KnowledgeConsolidationEntity::pending(
            "run-1",
            "u",
            "services/sqlite",
            EntityCategory::Concept,
            vec![contribution("SQLite", "pending relational database")],
            Default::default(),
        ))
        .await
        .unwrap();
    effective
        .upsert_entity(KnowledgeConsolidationEntity::pending(
            "run-1",
            "u",
            "web/example-com",
            EntityCategory::Concept,
            vec![contribution("Example.com", "example website")],
            Default::default(),
        ))
        .await
        .unwrap();

    let working_postgres = effective
        .working_entity("services/postgres")
        .await
        .unwrap()
        .unwrap();
    let committed_postgres = committed
        .entity_by_path("u", "services/postgres")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        committed_postgres.description, "production database",
        "saving working entity state must not mutate the committed baseline"
    );
    assert_ne!(
        working_postgres.consolidation_entity_id,
        committed_postgres.id
    );
    assert_eq!(
        working_postgres.entity_id.as_deref(),
        Some(committed_postgres.id.as_str())
    );
    let working_sqlite = effective
        .working_entity("services/sqlite")
        .await
        .unwrap()
        .unwrap();
    assert!(working_sqlite.entity_id.is_none());

    let postgres = effective
        .entity_by_path("services/postgres")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(postgres.description, "pending relational service");
    let hits = effective
        .search_entities("database relational")
        .await
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.path == "services/sqlite"),
        "pending hit: {hits:?}"
    );
    assert!(
        hits.iter().any(|hit| hit.path == "services/redis"),
        "live hit: {hits:?}"
    );
    assert_eq!(
        hits.iter()
            .filter(|hit| hit.path == "services/postgres")
            .count(),
        1
    );
    let exact = effective.search_entities("Example.com").await.unwrap();
    assert_eq!(
        exact.first().map(|hit| hit.path.as_str()),
        Some("web/example-com")
    );
    let listed = effective.list_entities().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|entity| entity.path == "services/postgres")
            .count(),
        1
    );
    assert!(listed.iter().any(|entity| entity.path == "services/redis"));
    assert!(listed.iter().any(|entity| entity.path == "services/sqlite"));

    let mut tombstone = effective
        .working_entity("services/postgres")
        .await
        .unwrap()
        .unwrap();
    tombstone.mark_discarded("test tombstone");
    effective.upsert_entity(tombstone).await.unwrap();
    assert!(
        effective
            .entity_by_path("services/postgres")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !effective
            .search_entities("postgres")
            .await
            .unwrap()
            .iter()
            .any(|hit| hit.path == "services/postgres")
    );
    assert!(
        !effective
            .list_entities()
            .await
            .unwrap()
            .iter()
            .any(|entity| entity.path == "services/postgres")
    );
}

#[tokio::test]
async fn consolidation_redirect_cycle_fails_safely() {
    use crate::db::repo::pkm::{KnowledgeConsolidationEntity, PkmConsolidationStore};

    let committed = std::sync::Arc::new(repo().await);
    let effective = PkmConsolidationStore::new(committed).scoped("run-cycle", "u");
    let mut first = KnowledgeConsolidationEntity::pending(
        "run-cycle",
        "u",
        "people/first",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    first.mark_coalesced("people/second");
    effective.upsert_entity(first).await.unwrap();
    let mut second = KnowledgeConsolidationEntity::pending(
        "run-cycle",
        "u",
        "people/second",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    second.mark_coalesced("people/first");
    effective.upsert_entity(second).await.unwrap();

    assert!(effective.entity_by_path("people/first").await.is_err());
}

#[tokio::test]
async fn identity_search_can_retrieve_beyond_the_normal_entity_search_limit() {
    use crate::db::repo::pkm::PkmConsolidationStore;

    let committed = std::sync::Arc::new(repo().await);
    for index in 0..12 {
        committed
            .upsert_entity_skeleton(
                "u",
                &format!("events/shared-{index}"),
                EntityCategory::Concept,
                &[],
                &format!("Shared Match {index}"),
                "shared identity candidate",
                &[],
            )
            .await
            .unwrap();
    }
    let effective = PkmConsolidationStore::new(committed.clone()).scoped("run-1", "u");

    assert_eq!(
        effective
            .search_entities("shared match")
            .await
            .unwrap()
            .len(),
        10
    );
    assert_eq!(
        effective
            .resolution_candidates(
                &["shared match".into()],
                &["shared".into(), "match".into()],
                &[],
                &[],
                "shared match",
                32,
            )
            .await
            .unwrap()
            .len(),
        12
    );

    committed
        .upsert_entity_skeleton(
            "u",
            "organizations/former-corp",
            EntityCategory::Concept,
            &[],
            "Former Corp",
            "online retailer",
            &[],
        )
        .await
        .unwrap();
    assert!(
        effective
            .resolution_candidates(
                &["former corp inc".into()],
                &["former".into(), "corp".into(), "inc".into()],
                &[],
                &[],
                "Former Corp Inc",
                32,
            )
            .await
            .unwrap()
            .iter()
            .any(|hit| hit.path == "organizations/former-corp")
    );
}

#[tokio::test]
async fn resolution_candidates_recall_same_kind_name_and_generic_assertions() {
    use crate::db::repo::pkm::PkmConsolidationStore;
    use frona_pkm::model::derive_resolution_search;

    let committed = std::sync::Arc::new(repo().await);
    for index in 0..70 {
        committed
            .upsert_entity_skeleton(
                "u",
                &format!("artifacts/taylor-{index}"),
                EntityCategory::Concept,
                &["urn:example:Artifact".into()],
                &format!("Taylor artifact {index}"),
                "A named artifact.",
                &[],
            )
            .await
            .unwrap();
    }
    committed
        .upsert_entity_skeleton(
            "u",
            "people/taylor",
            EntityCategory::Concept,
            &["urn:example:Person".into()],
            "Taylor",
            "A child.",
            &[],
        )
        .await
        .unwrap();
    let effective = PkmConsolidationStore::new(committed.clone()).scoped("run-1", "u");
    let hits = effective
        .resolution_candidates(
            &["taylor example".into()],
            &["example".into(), "taylor".into()],
            &[],
            &["urn:example:Person".into()],
            "Taylor Example",
            64,
        )
        .await
        .unwrap();
    assert!(hits.iter().any(|hit| hit.path == "people/taylor"));

    let attributes = serde_json::json!({
        "urn:example:lastDigits": "0000",
        "urn:example:issuer": "Example Bank",
    });
    let (_, _, assertions) = derive_resolution_search(
        "Example Card ending 0000",
        &Default::default(),
        &attributes,
        std::iter::empty(),
    );
    let mut card = KnowledgeConsolidationEntity::pending(
        "run-1",
        "u",
        "payments/example-card-0000",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    card.name = "Example Card ending 0000".into();
    card.attributes = attributes;
    card.rederive_search();
    effective.upsert_entity(card).await.unwrap();
    let hits = effective
        .resolution_candidates(
            &["premium rewards".into()],
            &["premium".into(), "rewards".into()],
            &assertions,
            &[],
            "Premium Rewards",
            64,
        )
        .await
        .unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.path == "payments/example-card-0000")
    );
}

#[tokio::test]
async fn resolution_candidates_normalize_hyphenated_names_the_same_way_as_queries() {
    use crate::db::repo::pkm::PkmConsolidationStore;

    let committed = std::sync::Arc::new(repo().await);
    committed
        .upsert_entity_skeleton(
            "u",
            "veterinary/example-veterinary-clinic",
            EntityCategory::Concept,
            &["https://schema.org/Organization".into()],
            "Example-Veterinary Clinic",
            "Buddy's veterinary clinic.",
            &[],
        )
        .await
        .unwrap();
    let effective = PkmConsolidationStore::new(committed).scoped("run-1", "u");

    let hits = effective
        .resolution_candidates(
            &["example veterinary clinic".into()],
            &["example".into(), "veterinary".into(), "clinic".into()],
            &[],
            &["https://schema.org/Organization".into()],
            "Example Veterinary",
            64,
        )
        .await
        .unwrap();

    assert!(
        hits.iter()
            .any(|hit| hit.path == "veterinary/example-veterinary-clinic"),
        "hyphenated indexed name was invisible to the normalized query: {hits:?}"
    );
}
use super::*;
