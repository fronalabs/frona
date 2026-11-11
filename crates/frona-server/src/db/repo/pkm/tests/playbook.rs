#[tokio::test]
async fn playbook_projection_and_checkpoint_transition_commit_together_and_replay() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "services/postgres",
        EntityCategory::Concept,
        &[],
        "Postgres",
        "database",
        &[],
    )
    .await
    .unwrap();
    r.upsert_entity_skeleton(
        "u",
        "playbooks/restart-db",
        EntityCategory::Playbook,
        &[],
        "Restart Database",
        "old scope",
        &["Database Restart".into()],
    )
    .await
    .unwrap();
    let memory = r
        .create_sourced_memory(
            "u",
            MemoryKind::Procedural,
            "Restart Postgres safely",
            &["services/postgres".to_string()],
            human_evidence("Restart Postgres safely", "services/postgres"),
        )
        .await
        .unwrap();
    let mut checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: new_id(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::PlaybookAuthor,
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let write = PlaybookResolutionWrite {
        candidate_ids: vec![new_id()],
        candidate_paths: vec!["playbooks/restart-db".into()],
        existing_path: Some("playbooks/restart-db".into()),
        merge_from: Vec::new(),
        path: "playbooks/restart-postgres".into(),
        name: "Restart Postgres".into(),
        description: "Safely restart a Postgres service.".into(),
        memory_ids: vec![memory.clone()],
    };

    r.commit_playbook_resolutions("u", std::slice::from_ref(&write), &checkpoint)
        .await
        .unwrap();
    let entity = r.entity_by_path("u", &write.path).await.unwrap().unwrap();
    assert_eq!(entity.category, EntityCategory::Playbook);
    assert_eq!(entity.kinds, [PLAYBOOK_KIND_IRI]);
    assert!(entity.aliases.contains("Restart Database"));
    assert!(entity.aliases.contains("Database Restart"));
    assert!(
        r.entity_by_path("u", "playbooks/restart-db")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        r.memories_for_entity("u", &write.path).await.unwrap().len(),
        1
    );
    let links = r.links_from_entity("u", "services/postgres").await.unwrap();
    assert!(
        links
            .iter()
            .any(|link| link.to_entity_path == write.path && link.relation == "playbook")
    );
    assert!(matches!(
        r.latest_consolidation_record("u")
            .await
            .unwrap()
            .unwrap()
            .state,
        crate::memory::pkm::ConsolidationStageState::PlaybookAuthor
    ));
    let effective =
        crate::db::repo::pkm::PkmConsolidationStore::new(std::sync::Arc::new(r.clone()))
            .scoped(&checkpoint.consolidation_id, "u");
    let working = effective
        .working_entity(&write.path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(working.lifecycle, ConsolidationEntityLifecycle::Active);
    assert_eq!(working.category, EntityCategory::Playbook);

    checkpoint.updated_at = Utc::now();
    r.commit_playbook_resolutions("u", std::slice::from_ref(&write), &checkpoint)
        .await
        .unwrap();
    assert_eq!(
        r.memories_for_entity("u", &write.path).await.unwrap().len(),
        1
    );
    assert_eq!(
        r.links_from_entity("u", "services/postgres")
            .await
            .unwrap()
            .into_iter()
            .filter(|link| link.to_entity_path == write.path && link.relation == "playbook")
            .count(),
        1,
        "replaying the atomic commit is idempotent",
    );
}

#[tokio::test]
async fn playbook_resolution_relocates_memory_from_its_provisional_candidate_path() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "tools/yfinance",
        EntityCategory::Concept,
        &[],
        "yfinance",
        "library",
        &[],
    )
    .await
    .unwrap();
    r.upsert_entity_skeleton(
        "u",
        "tools/yfinance/fetch-stock-close-price",
        EntityCategory::Playbook,
        &[],
        "Fetch Stock Close Price",
        "Fetch a close price.",
        &[],
    )
    .await
    .unwrap();
    let memory = r
        .create_sourced_memory(
            "u",
            MemoryKind::Procedural,
            "Diagnose secure Yahoo connectivity",
            &[
                "tools/yfinance".into(),
                "tools/yfinance/fetch-stock-close-price".into(),
            ],
            human_evidence("Diagnose secure Yahoo connectivity", "tools/yfinance"),
        )
        .await
        .unwrap();
    let checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: new_id(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::PlaybookAuthor,
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let candidate_id = new_id();
    let mut pending_candidate = KnowledgeConsolidationEntity::pending(
        &checkpoint.consolidation_id,
        "u",
        "tools/yfinance/fetch-stock-close-price",
        EntityCategory::Playbook,
        vec![crate::memory::pkm::PendingEntityContribution {
            name: "Fetch stock close price securely".into(),
            description: "Provisional diagnostic scope.".into(),
            aliases: Default::default(),
            attributes: serde_json::json!({}),
            attribute_evidence: Default::default(),
            source_memory_ids: [memory.clone()].into_iter().collect(),
            existing_only: false,
            occurrence_count: 1,
        }],
        [memory.clone()].into_iter().collect(),
    );
    pending_candidate.consolidation_entity_id = candidate_id.clone();
    r.upsert_consolidation_entity(&pending_candidate)
        .await
        .unwrap();
    let write = PlaybookResolutionWrite {
        candidate_ids: vec![candidate_id],
        candidate_paths: vec!["tools/yfinance/fetch-stock-close-price".into()],
        existing_path: None,
        merge_from: Vec::new(),
        path: "tools/yfinance/diagnose-secure-yahoo-connectivity".into(),
        name: "Diagnose Secure Yahoo Connectivity".into(),
        description: "Diagnose certificate and rate-limit failures.".into(),
        memory_ids: vec![memory.clone()],
    };

    r.commit_playbook_resolutions("u", std::slice::from_ref(&write), &checkpoint)
        .await
        .unwrap();

    assert!(
        r.memories_for_entity("u", "tools/yfinance/fetch-stock-close-price")
            .await
            .unwrap()
            .iter()
            .all(|held| held.id != memory)
    );
    assert!(
        r.memories_for_entity("u", &write.path)
            .await
            .unwrap()
            .iter()
            .any(|held| held.id == memory)
    );
    let library_links = r.links_from_entity("u", "tools/yfinance").await.unwrap();
    assert!(
        library_links
            .iter()
            .any(|link| link.to_entity_path == write.path)
    );
    let stale_links = r
        .links_from_entity("u", "tools/yfinance/fetch-stock-close-price")
        .await
        .unwrap();
    assert!(
        !stale_links
            .iter()
            .any(|link| link.to_entity_path == write.path)
    );
    let effective =
        crate::db::repo::pkm::PkmConsolidationStore::new(std::sync::Arc::new(r.clone()))
            .scoped(&checkpoint.consolidation_id, "u");
    let fetch = effective
        .entity_by_path("tools/yfinance/fetch-stock-close-price")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetch.path, "tools/yfinance/fetch-stock-close-price",
        "an existing Playbook that shares the provisional path must not redirect"
    );
}

#[tokio::test]
async fn entity_identity_merge_and_checkpoint_transition_commit_together() {
    use crate::db::repo::pkm::{KnowledgeConsolidationEntity, PkmConsolidationStore};
    use crate::memory::pkm::{ConsolidationStageState, PendingEntityContribution};

    let committed = std::sync::Arc::new(repo().await);
    let effective = PkmConsolidationStore::new(committed.clone()).scoped("identity-run", "u");
    let contribution = |name: &str, memory_id: &str| PendingEntityContribution {
        name: name.into(),
        description: "The same person.".into(),
        aliases: Default::default(),
        attributes: serde_json::json!({}),
        attribute_evidence: Default::default(),
        source_memory_ids: [memory_id.to_string()].into_iter().collect(),
        existing_only: false,
        occurrence_count: 1,
    };
    let mut canonical = KnowledgeConsolidationEntity::pending(
        "identity-run",
        "u",
        "people/full-name",
        EntityCategory::Concept,
        vec![contribution("Full Name", "memory-full")],
        ["memory-full".to_string()].into_iter().collect(),
    );
    let mut losing = KnowledgeConsolidationEntity::pending(
        "identity-run",
        "u",
        "people/short-name",
        EntityCategory::Concept,
        vec![contribution("Short Name", "memory-short")],
        ["memory-short".to_string()].into_iter().collect(),
    );
    effective.upsert_entity(canonical.clone()).await.unwrap();
    effective.upsert_entity(losing.clone()).await.unwrap();

    canonical
        .aliases
        .extend(["Short Name".to_string(), "Shared Alias".to_string()]);
    canonical.source_memory_ids.push("memory-short".into());
    canonical.checkpoint_revision = 1;
    canonical.rederive_search();
    losing.mark_coalesced_with_evidence(
        "people/full-name",
        Some(serde_json::json!({"reason": "same person"})),
    );
    losing.checkpoint_revision = 1;
    let state = crate::memory::pkm::ConsolidationWorkState {
        revision: 1,
        ..Default::default()
    };
    let checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: "identity-run".into(),
        user_id: "u".into(),
        state: ConsolidationStageState::Classify(state),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };

    committed
        .commit_entity_identity_merge(&canonical, &losing, &checkpoint)
        .await
        .unwrap();

    let stored = effective
        .working_entity("people/full-name")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.aliases.contains("Short Name"));
    assert!(stored.aliases.contains("Shared Alias"));
    assert!(
        stored
            .source_memory_ids
            .contains(&"memory-short".to_string())
    );
    assert_eq!(stored.checkpoint_revision, 1);
    let redirect = effective
        .working_entity("people/short-name")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(redirect.lifecycle, ConsolidationEntityLifecycle::Coalesced);
    assert!(!redirect.searchable);
    assert_eq!(redirect.canonical_path.as_deref(), Some("people/full-name"));
    assert!(matches!(
        redirect.progress.identity,
        crate::memory::pkm::model::IdentityProgress::Coalesced {
            evidence: Some(_),
            ..
        }
    ));
    let next_iteration_candidates = effective.search_entities("Shared Alias").await.unwrap();
    assert!(
        next_iteration_candidates
            .iter()
            .any(|entity| entity.path == "people/full-name")
    );
    assert!(
        !next_iteration_candidates
            .iter()
            .any(|entity| entity.path == "people/short-name")
    );

    let saved = committed
        .latest_consolidation_record("u")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Classify(saved) = saved.state else {
        panic!("identity transition must remain in Classify");
    };
    assert_eq!(saved.revision, 1);
}
use super::*;
