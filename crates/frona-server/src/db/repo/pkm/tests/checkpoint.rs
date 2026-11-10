/// The record has to survive the round trip through SurrealDB, including the enum
/// that says where the pass got to. A pass that cannot be read back is a pass that
/// silently never resumes.
#[tokio::test]
async fn consolidation_record_round_trips_through_every_stage() {
    use crate::memory::pkm::{
        ConsolidationStageState, ConsolidationWorkState, PlaybookResolveState,
    };

    let r = repo().await;
    let mut rec = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: new_id(),
        user_id: "u".into(),
        state: ConsolidationStageState::Ingest(Default::default()),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };

    // Every variant, including the unit ones - those are the shapes most likely to
    // round-trip differently from the tuple variants around them.
    let mut classify = ConsolidationWorkState::default();
    classify.revision = 2;
    let playbook = PlaybookResolveState::default();
    for state in [
        ConsolidationStageState::Ingest(Default::default()),
        ConsolidationStageState::Classify(classify),
        ConsolidationStageState::PlaybookResolve(playbook),
        ConsolidationStageState::PlaybookAuthor,
        ConsolidationStageState::PageAuthor,
        ConsolidationStageState::Cleanup,
        ConsolidationStageState::Done,
    ] {
        let label = state.label();
        rec.state = state;
        r.save_consolidation_record(&rec).await.unwrap();
        let back = r
            .latest_consolidation_record("u")
            .await
            .unwrap_or_else(|e| panic!("reading back `{label}` failed: {e}"))
            .unwrap_or_else(|| panic!("no record after saving `{label}`"));
        assert_eq!(
            back.state.label(),
            label,
            "the stage survives the round trip"
        );
        assert_eq!(back.id, rec.id, "same row, upserted by id");
    }

    // `Done` is what tells the sweep a pass is history rather than work in flight.
    let back = r.latest_consolidation_record("u").await.unwrap().unwrap();
    assert!(back.state.is_done());
    assert!(
        r.users_with_open_consolidation().await.unwrap().is_empty(),
        "a finished pass is not open work"
    );

    rec.state = ConsolidationStageState::Cleanup;
    r.save_consolidation_record(&rec).await.unwrap();
    assert_eq!(
        r.users_with_open_consolidation().await.unwrap(),
        ["u"],
        "an unfinished one is"
    );

    // Entity payload is held on consolidation rows. The checkpoint keeps only global
    // stage data and accepted decision data that has not moved to a row yet.
    let mut classify = ConsolidationWorkState::default();
    for index in 0..150 {
        let path = format!("generic/entity-{index}");
        classify
            .resolution_pair_fingerprints
            .insert(path, format!("representative-pair-fingerprint-{index}"));
    }
    rec.state = ConsolidationStageState::Classify(classify);
    r.save_consolidation_record(&rec).await.unwrap();
    let back = r.latest_consolidation_record("u").await.unwrap().unwrap();
    let ConsolidationStageState::Classify(back) = back.state else {
        panic!("large classify checkpoint changed stage while round-tripping");
    };
    assert_eq!(back.resolution_pair_fingerprints.len(), 150);
}

#[tokio::test]
async fn invalid_entity_row_does_not_advance_the_checkpoint() {
    let committed = std::sync::Arc::new(repo().await);
    let mut saved_state = crate::memory::pkm::ConsolidationWorkState::default();
    saved_state.revision = 0;
    let mut checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: "run-invalid-row".into(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(saved_state),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    committed
        .save_consolidation_record(&checkpoint)
        .await
        .unwrap();
    let effective = PkmConsolidationStore::new(committed.clone()).scoped("run-invalid-row", "u");
    let mut row = KnowledgeConsolidationEntity::pending(
        "run-invalid-row",
        "u",
        "people/casey-owner",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    row.lifecycle = ConsolidationEntityLifecycle::Coalesced;
    row.searchable = false;
    row.checkpoint_revision = 1;
    let mut next_state = crate::memory::pkm::ConsolidationWorkState::default();
    next_state.revision = 1;
    checkpoint.state = crate::memory::pkm::ConsolidationStageState::Classify(next_state);

    assert!(
        effective
            .commit_transition(&[row], &checkpoint)
            .await
            .is_err()
    );
    assert!(
        effective
            .working_entity("people/casey-owner")
            .await
            .unwrap()
            .is_none()
    );
    let stored = committed
        .latest_consolidation_record("u")
        .await
        .unwrap()
        .unwrap();
    let crate::memory::pkm::ConsolidationStageState::Classify(state) = stored.state else {
        panic!("checkpoint stage changed after an invalid entity transition");
    };
    assert_eq!(state.revision, 0);
}

#[tokio::test]
async fn invalid_checkpoint_scope_does_not_write_entity_rows() {
    let committed = std::sync::Arc::new(repo().await);
    let saved = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: "run-invalid-checkpoint".into(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(Default::default()),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    committed.save_consolidation_record(&saved).await.unwrap();
    let effective =
        PkmConsolidationStore::new(committed.clone()).scoped("run-invalid-checkpoint", "u");
    let mut wrong_checkpoint = saved.clone();
    wrong_checkpoint.user_id = "other-user".into();
    let mut next_state = crate::memory::pkm::ConsolidationWorkState::default();
    next_state.revision = 1;
    wrong_checkpoint.state = crate::memory::pkm::ConsolidationStageState::Classify(next_state);
    let mut row = KnowledgeConsolidationEntity::pending(
        "run-invalid-checkpoint",
        "u",
        "people/casey-owner",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    row.checkpoint_revision = 1;

    assert!(
        effective
            .commit_transition(&[row], &wrong_checkpoint)
            .await
            .is_err()
    );
    assert!(
        effective
            .working_entity("people/casey-owner")
            .await
            .unwrap()
            .is_none()
    );
    let stored = committed
        .latest_consolidation_record("u")
        .await
        .unwrap()
        .unwrap();
    let crate::memory::pkm::ConsolidationStageState::Classify(state) = stored.state else {
        panic!("checkpoint stage changed after a wrong-scope transition");
    };
    assert_eq!(state.revision, 0);
}

#[tokio::test]
async fn contribution_growth_changes_the_entity_row_not_the_checkpoint() {
    let committed = std::sync::Arc::new(repo().await);
    let checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: "run-large-contribution".into(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(Default::default()),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    committed
        .save_consolidation_record(&checkpoint)
        .await
        .unwrap();
    let checkpoint_size = serde_json::to_vec(&checkpoint).unwrap().len();
    let contribution = PendingEntityContribution {
        name: "Large entity".into(),
        description: "x".repeat(100_000),
        aliases: Default::default(),
        attributes: serde_json::json!({}),
        attribute_evidence: Default::default(),
        source_memory_ids: Default::default(),
        existing_only: false,
        occurrence_count: 1,
    };
    let row = KnowledgeConsolidationEntity::pending(
        "run-large-contribution",
        "u",
        "things/large",
        EntityCategory::Concept,
        vec![contribution],
        Default::default(),
    );
    let row_size = serde_json::to_vec(&row).unwrap().len();
    PkmConsolidationStore::new(committed.clone())
        .scoped("run-large-contribution", "u")
        .upsert_entity(row)
        .await
        .unwrap();

    let stored = committed
        .latest_consolidation_record("u")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_vec(&stored).unwrap().len(), checkpoint_size);
    assert!(row_size > checkpoint_size + 100_000);
}

#[tokio::test]
async fn invalid_reconciliation_leaves_memory_projection_and_checkpoint_unchanged() {
    let committed = repo().await;
    committed
        .upsert_entity_skeleton(
            "u",
            "services/postgres",
            EntityCategory::Concept,
            &[],
            "Postgres",
            "",
            &[],
        )
        .await
        .unwrap();
    let memory = committed
        .create_sourced_memory(
            "u",
            MemoryKind::Fact,
            "Postgres uses port 5432",
            &["services/postgres".to_string()],
            human_evidence("Postgres uses port 5432", "services/postgres"),
        )
        .await
        .unwrap();
    let mut checkpoint = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: "run-invalid-reconcile".into(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(Default::default()),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    committed
        .save_consolidation_record(&checkpoint)
        .await
        .unwrap();
    let mut next_state = crate::memory::pkm::ConsolidationWorkState::default();
    next_state.revision = 1;
    checkpoint.state = crate::memory::pkm::ConsolidationStageState::Classify(next_state);
    let row = KnowledgeConsolidationEntity::pending(
        "run-invalid-reconcile",
        "u",
        "services/postgres",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    let write = ReconcileCommit {
        outdated_memories: vec![ReconcileOutdatedWrite {
            memory_id: memory.clone(),
            reason: "stale".into(),
        }],
        entity: Some(row),
        ..Default::default()
    };

    assert!(
        committed
            .commit_reconciliation(&write, &checkpoint)
            .await
            .is_err()
    );
    let memories = committed
        .memories_for_entity("u", "services/postgres")
        .await
        .unwrap();
    assert_eq!(memories[0].disposition, Disposition::None);
    let effective = PkmConsolidationStore::new(std::sync::Arc::new(committed.clone()))
        .scoped("run-invalid-reconcile", "u");
    assert!(
        effective
            .working_entity("services/postgres")
            .await
            .unwrap()
            .is_none()
    );
    let stored = committed
        .latest_consolidation_record("u")
        .await
        .unwrap()
        .unwrap();
    let crate::memory::pkm::ConsolidationStageState::Classify(state) = stored.state else {
        panic!("invalid reconciliation changed the checkpoint stage");
    };
    assert_eq!(state.revision, 0);
}

#[tokio::test]
async fn schema_entity_and_checkpoint_advance_commit_together() {
    use crate::memory::pkm::{ConsolidationStageState, PlaybookResolveState};

    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "people/casey-owner",
        EntityCategory::Concept,
        &[],
        "Casey Owner",
        "",
        &[],
    )
    .await
    .unwrap();
    let mut record = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: new_id(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(
            crate::memory::pkm::ConsolidationWorkState::default(),
        ),
        stats: Default::default(),
        attempts: 2,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    r.save_consolidation_record(&record).await.unwrap();
    record.state = ConsolidationStageState::PlaybookResolve(PlaybookResolveState::default());
    record.attempts = 0;

    assert!(
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[("people/casey-owner".into(), vec!["schema:Person".into()])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            Some(&record),
        )
        .await
        .unwrap()
    );

    let entity = r
        .entity_by_path("u", "people/casey-owner")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.kinds, ["schema:Person"]);
    let saved = r.latest_consolidation_record("u").await.unwrap().unwrap();
    assert!(matches!(
        saved.state,
        ConsolidationStageState::PlaybookResolve(_)
    ));
    assert_eq!(saved.attempts, 0);
    let effective =
        crate::db::repo::pkm::PkmConsolidationStore::new(std::sync::Arc::new(r.clone()))
            .scoped(&record.consolidation_id, "u");
    let working = effective
        .working_entity("people/casey-owner")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(working.lifecycle, ConsolidationEntityLifecycle::Active);
    assert_eq!(working.kinds, ["schema:Person"]);
}

#[tokio::test]
async fn schema_commit_moves_chained_merge_sources_to_only_the_canonical_path() {
    let r = repo().await;
    for path in [
        "domains/example-com",
        "services/example-com",
        "websites/example-com",
    ] {
        r.upsert_entity_skeleton(
            "u",
            path,
            EntityCategory::Concept,
            &[],
            "example.com",
            "",
            &[],
        )
        .await
        .unwrap();
    }
    let old = KnowledgeEntitySource {
        id: new_id(),
        user_id: "u".into(),
        memory_id: "m1".into(),
        entity_path: "services/example-com".into(),
        created_at: Utc::now(),
    };
    r.db.create::<Option<surrealdb::types::Value>>(("knowledge_entity_source", old.id.clone()))
        .content(old)
        .await
        .unwrap();
    let second = KnowledgeEntitySource {
        id: new_id(),
        user_id: "u".into(),
        memory_id: "m2".into(),
        entity_path: "websites/example-com".into(),
        created_at: Utc::now(),
    };
    r.db.create::<Option<surrealdb::types::Value>>(("knowledge_entity_source", second.id.clone()))
        .content(second)
        .await
        .unwrap();

    assert!(
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[
                (
                    "services/example-com".into(),
                    "domains/example-com".into(),
                    "m1".into()
                ),
                (
                    "websites/example-com".into(),
                    "domains/example-com".into(),
                    "m2".into()
                ),
            ],
            &[(
                "domains/example-com".into(),
                vec!["Example Domain service".into()]
            )],
            &[],
            None,
        )
        .await
        .unwrap()
    );

    let mut response =
        r.db.query(
            "SELECT VALUE entity_path FROM knowledge_entity_source
             WHERE user_id = 'u' AND memory_id IN ['m1', 'm2'] ORDER BY memory_id",
        )
        .await
        .unwrap();
    let paths: Vec<String> = response.take(0).unwrap();
    assert_eq!(paths, ["domains/example-com", "domains/example-com"]);
}

#[tokio::test]
async fn schema_commit_unions_identity_evidence_and_retains_the_redirect_until_cleanup() {
    let r = repo().await;
    for path in ["organizations/acme", "companies/acme-inc"] {
        r.upsert_entity_skeleton("u", path, EntityCategory::Concept, &[], "Acme", "", &[])
            .await
            .unwrap();
    }
    let mut canonical = r
        .entity_by_path("u", "organizations/acme")
        .await
        .unwrap()
        .unwrap();
    canonical.identity_evidence.push(MemoryEvidence {
        strength: EvidenceStrength::Explicit,
        source: EvidenceSource::HumanEdit {
            page_path: "notes/old".into(),
            quote: "Acme Inc.".into(),
        },
    });
    r.db.query("UPDATE type::record('knowledge_entity', $id) CONTENT $entity")
        .bind(("id", canonical.id.clone()))
        .bind(("entity", canonical.clone()))
        .await
        .unwrap();
    let mut losing = r
        .entity_by_path("u", "companies/acme-inc")
        .await
        .unwrap()
        .unwrap();
    losing.identity_evidence.push(MemoryEvidence {
        strength: EvidenceStrength::Explicit,
        source: EvidenceSource::HumanEdit {
            page_path: "notes/new".into(),
            quote: "Acme".into(),
        },
    });
    let record = KnowledgeConsolidationRecord {
        id: new_id(),
        consolidation_id: new_id(),
        user_id: "u".into(),
        state: crate::memory::pkm::ConsolidationStageState::Classify(
            crate::memory::pkm::ConsolidationWorkState::default(),
        ),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    r.save_consolidation_record(&record).await.unwrap();
    r.upsert_consolidation_entity(&KnowledgeConsolidationEntity::from_committed(
        &record.consolidation_id,
        canonical,
    ))
    .await
    .unwrap();
    r.upsert_consolidation_entity(&KnowledgeConsolidationEntity::from_committed(
        &record.consolidation_id,
        losing,
    ))
    .await
    .unwrap();

    assert!(
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[(
                "companies/acme-inc".into(),
                Some("organizations/acme".into())
            )],
            Some(&record),
        )
        .await
        .unwrap()
    );

    let entity = r
        .entity_by_path("u", "organizations/acme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.identity_evidence.len(), 2);
    let effective =
        crate::db::repo::pkm::PkmConsolidationStore::new(std::sync::Arc::new(r.clone()))
            .scoped(&record.consolidation_id, "u");
    let redirect = effective
        .working_entity("companies/acme-inc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(redirect.lifecycle, ConsolidationEntityLifecycle::Coalesced);
    assert_eq!(
        redirect.canonical_path.as_deref(),
        Some("organizations/acme")
    );
}

#[tokio::test]
async fn schema_commit_reports_an_ontology_statement_error() {
    let r = repo().await;
    r.db.query("DEFINE FIELD owl ON knowledge_ontology TYPE int")
        .await
        .unwrap()
        .check()
        .unwrap();

    let result = r
        .commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "an invalid ontology statement must fail publication"
    );
    assert!(
        r.ontology_get("u").await.unwrap().is_none(),
        "a failed ontology statement must leave no published row",
    );
}

#[tokio::test]
async fn ontology_cas_writes_and_rejects_stale() {
    let r = repo().await;
    // absent row → expected_version 0 mints v1.
    assert_eq!(
        r.ontology_upsert_cas("u", "Ontology()", "ofn", 0)
            .await
            .unwrap(),
        Some(1)
    );
    let got = r.ontology_get("u").await.unwrap().unwrap();
    assert_eq!((got.version, got.owl.as_str()), (1, "Ontology()"));

    // matching version writes v2.
    assert_eq!(
        r.ontology_upsert_cas("u", "Ontology(A)", "ofn", 1)
            .await
            .unwrap(),
        Some(2)
    );

    // stale expected_version → rejected, no clobber.
    assert_eq!(
        r.ontology_upsert_cas("u", "SHOULD_NOT_WRITE", "ofn", 1)
            .await
            .unwrap(),
        None
    );
    let got = r.ontology_get("u").await.unwrap().unwrap();
    assert_eq!((got.version, got.owl.as_str()), (2, "Ontology(A)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ontology_cas_allows_only_one_concurrent_writer() {
    let r = std::sync::Arc::new(repo().await);
    let writers = 32;
    for round in 0..4 {
        let user_id = format!("u-{round}");
        assert_eq!(
            r.ontology_upsert_cas(&user_id, "Ontology()", "ofn", 0)
                .await
                .unwrap(),
            Some(1),
        );
        let start = std::sync::Arc::new(tokio::sync::Barrier::new(writers));
        let mut tasks = tokio::task::JoinSet::new();
        for writer in 0..writers {
            let r = r.clone();
            let start = start.clone();
            let user_id = user_id.clone();
            tasks.spawn(async move {
                start.wait().await;
                let owl = format!("Ontology(Declaration(Class(:Writer{writer})))");
                let version = r
                    .ontology_upsert_cas(&user_id, &owl, "ofn", 1)
                    .await
                    .unwrap();
                version.map(|version| (version, owl))
            });
        }

        let mut successful_writers = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Some(winner) = result.unwrap() {
                successful_writers.push(winner);
            }
        }
        assert_eq!(
            successful_writers.len(),
            1,
            "a version can be consumed by only one concurrent writer (round {round})",
        );
        let (version, owl) = successful_writers.pop().unwrap();
        assert_eq!(version, 2);
        let stored = r.ontology_get(&user_id).await.unwrap().unwrap();
        assert_eq!((stored.version, stored.owl), (version, owl));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ontology_cas_allows_only_one_concurrent_initial_writer() {
    let r = std::sync::Arc::new(repo().await);
    let writers = 32;
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(writers));
    let mut tasks = tokio::task::JoinSet::new();
    for writer in 0..writers {
        let r = r.clone();
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            let owl = format!("Ontology(Declaration(Class(:Writer{writer})))");
            let version = r
                .ontology_upsert_cas("new-user", &owl, "ofn", 0)
                .await
                .unwrap();
            version.map(|version| (version, owl))
        });
    }

    let mut successful_writers = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Some(winner) = result.unwrap() {
            successful_writers.push(winner);
        }
    }
    assert_eq!(
        successful_writers.len(),
        1,
        "an absent version can be consumed only once"
    );
    let (version, owl) = successful_writers.pop().unwrap();
    assert_eq!(version, 1);
    let stored = r.ontology_get("new-user").await.unwrap().unwrap();
    assert_eq!((stored.version, stored.owl), (version, owl));
}

#[tokio::test]
async fn inferred_links_wiped_asserted_survive_idempotently() {
    let r = repo().await;
    seed_asserted_entity_link(&r, "u", "a", "b", "worksFor")
        .await
        .unwrap(); // asserted
    r.insert_inferred_links("u", &[("b".into(), "a".into(), "employs".into())])
        .await
        .unwrap();
    assert_eq!(r.links_from_entity("u", "a").await.unwrap().len(), 1);
    assert_eq!(r.links_from_entity("u", "b").await.unwrap().len(), 1);

    // wipe inferred → asserted survives, inferred gone.
    r.wipe_inferred_links("u").await.unwrap();
    assert_eq!(
        r.links_from_entity("u", "a").await.unwrap().len(),
        1,
        "asserted survives"
    );
    assert_eq!(
        r.links_from_entity("u", "b").await.unwrap().len(),
        0,
        "inferred wiped"
    );

    // wipe + re-insert twice → still exactly one inferred edge (idempotent).
    r.insert_inferred_links("u", &[("b".into(), "a".into(), "employs".into())])
        .await
        .unwrap();
    r.wipe_inferred_links("u").await.unwrap();
    r.insert_inferred_links("u", &[("b".into(), "a".into(), "employs".into())])
        .await
        .unwrap();
    assert_eq!(
        r.links_from_entity("u", "b").await.unwrap().len(),
        1,
        "idempotent"
    );
}
use super::*;
