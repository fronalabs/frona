//! The Obsidian **sync round-trip**, driven through the `PkmSyncService`
//! engine (built the same way the sync API route builds it from `AppState`)
//! against an in-memory SurrealDB with the LLM stubbed at the
//! provider-registry seam. Covers human write-back on an existing page, a
//! human-authored new page, delete, and CAS conflict - the
//! write side that the in-crate `sync` unit tests can't reach without a harness.

mod helpers;

use std::sync::Arc;

use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::core::config::{Config, StorageConfig};
use frona::db::repo::pkm::{PkmConsolidationStore, PkmRepo};
use frona::inference::error::InferenceError;
use frona::memory::pkm::model::{
    Disposition, EntityCategory, EntityOrigin, EvidenceSource, EvidenceStrength, MemoryEvidence,
    MemoryKind, classify_memories,
};
use frona::memory::pkm::sync::{EditGate, EditOp, EditResult, PkmSyncService};
use frona::memory::pkm::{PkmStorage, sha256_hex};
use frona::storage::StorageService;

use helpers::{
    MockModelProvider, MockResponse, seed_asserted_entity_link, test_harness, test_model_group,
    test_model_service_with_group,
};

const U: &str = "test-user";

fn test_user_service(db: &Surreal<Db>) -> frona::auth::user_service::UserService {
    frona::auth::user_service::UserService::new(
        frona::db::repo::generic::SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    )
}

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    db
}

fn resources_prompts() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("resources/prompts")
}

/// Build a PKM service + harness sharing an in-memory DB, with the mock queued.
/// Also yields a `PkmStorage` over the same data dir so tests can author pages to
/// disk (the read side - `manifest`/`get_pages` - serves on-disk bytes).
async fn setup(
    mock: Arc<MockModelProvider>,
) -> (
    Surreal<Db>,
    Arc<frona::agent::harness::Harness>,
    PkmRepo,
    PkmStorage,
    PkmSyncService,
) {
    let db = test_db().await;
    let tmp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let pkm_storage = PkmStorage::new(StorageService::new(&config));
    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(
        test_model_service_with_group(
            "mock",
            mock.clone(),
            &memory_config.model_group,
            test_model_group(),
        )
        .await,
    );
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let harness = test_harness(&db, &config, mock).await;
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    // The sync engine the API builds from `AppState` - same repo + storage + deps.
    let sync = PkmSyncService::new(
        Arc::new(PkmRepo::new(db.clone(), memory_config.pkm_search_top_k)),
        PkmStorage::new(StorageService::new(&config)),
        memory_config,
        test_user_service(&db),
        prompts,
        registry,
    );
    (db, harness, repo, pkm_storage, sync)
}

/// Author a Memory page to disk + DB (skeleton, file bytes, rev) - what the author
/// stage does. Returns the file's rev.
async fn author(repo: &PkmRepo, storage: &PkmStorage, path: &str, body: &str) -> String {
    repo.upsert_entity_skeleton(
        U,
        path,
        EntityCategory::Concept,
        &["https://schema.org/Person".to_string()],
        "Bob",
        "",
        &[],
    )
    .await
    .unwrap();
    storage.write_page(&vault(storage), path, body).unwrap();
    let rev = sha256_hex(body);
    repo.set_page_rev(U, path, &rev).await.unwrap();
    rev
}

/// Seed an authored Memory page with one memory and a known rev.
async fn seed_page(repo: &PkmRepo, path: &str, content: &str, rev: &str) -> String {
    repo.upsert_entity_skeleton(
        U,
        path,
        EntityCategory::Concept,
        &["https://schema.org/Person".to_string()],
        "Bob",
        "",
        &[],
    )
    .await
    .unwrap();
    let mem = repo
        .create_memory_with_entities(U, "a", "c", MemoryKind::Fact, content, &[path.to_string()])
        .await
        .unwrap();
    repo.set_page_rev(U, path, rev).await.unwrap();
    mem
}

/// The vault scope every test in this file operates in.
fn vault(storage: &PkmStorage) -> frona::memory::pkm::VaultScope {
    storage.vault_scope(handle(), "Memory").unwrap()
}

fn handle() -> frona::core::Handle {
    frona::handle!("testuser")
}

#[tokio::test]
async fn writeback_on_existing_page_supersedes_and_recomputes_rev() {
    // The human edited the page; the LLM write-back supersedes the old fact.
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let (.., harness, repo, _, sync) = setup(mock.clone()).await;
    seed_page(&repo, "people/bob", "Bob works at Acme", "rev1").await;
    let ops = json!({ "ops": [
        {"op":"supersede","kind":"fact","content":"Bob works at Globex","memory_id":"m1","note":"moved"}
    ]});
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c1".into(),
        "submit".into(),
        ops,
    )]));

    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some("rev1".into()),
                content: "# Bob\n\nWorks at Globex.".into(),
            },
        )
        .await
        .unwrap();
    let new_rev = match r {
        EditResult::Accepted { rev } => rev,
        other => panic!("expected Accepted, got {other:?}"),
    };
    assert_ne!(new_rev, "rev1", "rev recomputed from the adopted body");

    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    let (cur, hist) = classify_memories(&mems);
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].content, "Bob works at Globex");
    assert!(
        matches!(cur[0].evidence[0].source, EvidenceSource::HumanEdit { .. }),
        "write-back mints Human evidence"
    );
    assert_eq!(hist.len(), 1, "old value demoted to history");
    assert_eq!(hist[0].content, "Bob works at Acme");
}

/// `check_edit` and `apply_edit` must reach the same CAS verdict, including on the
/// subtle edge: a page that exists but was never rendered has an **empty** head rev, and
/// must conflict rather than accept - otherwise a client passing `base_rev: None` (or any
/// value) would silently overwrite an un-projected page.
///
/// The two used to be independent expressions of that rule, spelled oppositely
/// (`!head_rev.is_empty()` in the gate's Apply arm vs `head_rev.is_empty() ||` in
/// apply's Conflict arm), with only the gate under test. `apply_edit` now delegates, so
/// this pins the shared behaviour against anyone re-inlining it.
#[tokio::test]
async fn check_edit_and_apply_edit_agree_on_an_unrendered_page() {
    let mock = Arc::new(MockModelProvider::new(vec![])); // no LLM - conflict short-circuits
    let (.., harness, repo, _, sync) = setup(mock).await;
    // A page with no rev at all: skeleton created, never authored/projected.
    repo.upsert_entity_skeleton(
        U,
        "people/carol",
        EntityCategory::Concept,
        &["https://schema.org/Person".to_string()],
        "Carol",
        "",
        &[],
    )
    .await
    .unwrap();

    // The gate says Conflict with an empty head…
    match sync
        .check_edit(U, &handle(), "Memory/people/carol", None)
        .await
        .unwrap()
    {
        EditGate::Conflict { head_rev, .. } => assert!(head_rev.is_empty()),
        other => panic!("gate: expected Conflict on an unrendered page, got {other:?}"),
    }
    // …and the write path agrees, rather than treating it as a fresh page or an apply.
    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/carol".into(),
                base_rev: None,
                content: "# Carol\n\nedited".into(),
            },
        )
        .await
        .unwrap();
    match r {
        EditResult::Conflict { head_rev, .. } => assert!(head_rev.is_empty()),
        other => panic!("apply: expected Conflict on an unrendered page, got {other:?}"),
    }
    assert!(
        repo.memories_for_entity(U, "people/carol")
            .await
            .unwrap()
            .is_empty(),
        "a conflicting edit writes nothing back"
    );
}

#[tokio::test]
async fn cas_conflict_on_stale_base_rev_does_not_write() {
    let mock = Arc::new(MockModelProvider::new(vec![])); // no LLM - conflict short-circuits
    let (.., harness, repo, _, sync) = setup(mock).await;
    seed_page(&repo, "people/bob", "Bob works at Acme", "rev1").await;

    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some("STALE".into()),
                content: "# Bob\n\nedited".into(),
            },
        )
        .await
        .unwrap();
    match r {
        EditResult::Conflict { head_rev, .. } => assert_eq!(head_rev, "rev1"),
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Untouched: still one Agent memory, no Human write-back.
    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert_eq!(mems.len(), 1);
    assert!(matches!(
        mems[0].evidence[0].source,
        EvidenceSource::AgentMessage { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_with_one_base_revision_accept_only_one() {
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let response = || {
        MockResponse::Barrier(
            start.clone(),
            Box::new(MockResponse::ToolCalls(vec![(
                "c1".into(),
                "submit".into(),
                json!({ "ops": [
                    {"op":"add","kind":"fact","content":"A concurrent human edit was accepted"}
                ] }),
            )])),
        )
    };
    let mock = Arc::new(MockModelProvider::new(vec![response(), response()]));
    let (db, harness, repo, storage, first_sync) = setup(mock.clone()).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob\n\nOriginal.").await;
    let memory_config = frona::core::config::MemoryConfig::default();
    let second_sync = PkmSyncService::new(
        Arc::new(PkmRepo::new(db.clone(), memory_config.pkm_search_top_k)),
        storage.clone(),
        memory_config.clone(),
        test_user_service(&db),
        frona::agent::prompt::PromptLoader::new(resources_prompts()),
        Arc::new(
            test_model_service_with_group(
                "mock",
                mock,
                &memory_config.model_group,
                test_model_group(),
            )
            .await,
        ),
    );

    let edit = |sync: PkmSyncService, content: &'static str| {
        let harness = harness.clone();
        let rev = rev.clone();
        tokio::spawn(async move {
            sync.apply_edit(
                &harness,
                U,
                &handle(),
                EditOp::Upsert {
                    path: "Memory/people/bob".into(),
                    base_rev: Some(rev),
                    content: content.into(),
                },
            )
            .await
            .unwrap()
        })
    };
    let first = edit(first_sync, "# Bob\n\nFirst edit.");
    let second = edit(second_sync, "# Bob\n\nSecond edit.");
    let results = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        [first.await.unwrap(), second.await.unwrap()]
    })
    .await
    .expect("both edits must reach inference and finish");
    let accepted = results
        .iter()
        .filter(|result| matches!(result, EditResult::Accepted { .. }))
        .count();
    let conflicts = results
        .iter()
        .filter(|result| matches!(result, EditResult::Conflict { .. }))
        .count();

    assert_eq!(accepted, 1, "only one edit can consume a base revision");
    assert_eq!(
        conflicts, 1,
        "the other concurrent edit must see a conflict"
    );
    let accepted_rev = results
        .iter()
        .find_map(|result| match result {
            EditResult::Accepted { rev } => Some(rev),
            _ => None,
        })
        .unwrap();
    let (head_rev, head_content) = results
        .iter()
        .find_map(|result| match result {
            EditResult::Conflict {
                head_rev,
                head_content,
            } => Some((head_rev, head_content)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        head_rev, accepted_rev,
        "the conflict must identify the accepted head"
    );
    assert_eq!(
        sha256_hex(head_content),
        *head_rev,
        "the conflict content must match its revision"
    );
    assert_eq!(
        repo.memories_for_entity(U, "people/bob")
            .await
            .unwrap()
            .len(),
        1,
        "the rejected edit must not leave memory mutations",
    );
}

#[tokio::test]
async fn accepted_edit_remains_pullable_when_file_projection_is_missing() {
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "submit".into(), json!({ "ops": [] })),
    ])]));
    let (.., harness, repo, storage, sync) = setup(mock).await;
    let base_rev = author(&repo, &storage, "people/bob", "# Bob\n\nOriginal.").await;
    let accepted_content = "# Bob\n\nAccepted human edit.";

    let result = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some(base_rev),
                content: accepted_content.into(),
            },
        )
        .await
        .unwrap();
    let accepted_rev = match result {
        EditResult::Accepted { rev } => rev,
        other => panic!("expected Accepted, got {other:?}"),
    };
    storage.delete_page(&vault(&storage), "people/bob").unwrap();

    let pages = sync
        .get_pages(U, &handle(), &["Memory/people/bob".into()])
        .await
        .unwrap();
    assert_eq!(
        pages.len(),
        1,
        "the database keeps the accepted sync head durable"
    );
    assert_eq!(pages[0].rev, accepted_rev);
    assert_eq!(pages[0].content, accepted_content);
}

#[tokio::test]
async fn new_page_create_can_retry_after_writeback_failure() {
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("invalid request".into())),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), json!({ "ops": [] }))]),
    ]));
    let (.., harness, repo, _, sync) = setup(mock).await;
    let edit = || EditOp::Upsert {
        path: "Memory/people/carol".into(),
        base_rev: None,
        content: "# Carol\n\nLeads the platform team.".into(),
    };

    assert!(
        sync.apply_edit(&harness, U, &handle(), edit())
            .await
            .is_err(),
        "the first write-back must expose the provider failure",
    );
    let retry = sync
        .apply_edit(&harness, U, &handle(), edit())
        .await
        .unwrap();

    assert!(
        matches!(retry, EditResult::Created { .. }),
        "a failed create must not leave a skeleton that conflicts with its retry: {retry:?}",
    );
    assert!(
        repo.entity_by_path(U, "people/carol")
            .await
            .unwrap()
            .and_then(|page| page.rev)
            .is_some(),
        "the retry must finish the page projection",
    );
}

#[tokio::test]
async fn delete_retires_memories_and_drops_from_manifest() {
    let mock = Arc::new(MockModelProvider::new(vec![])); // delete needs no LLM
    let (.., harness, repo, _, sync) = setup(mock).await;
    seed_page(&repo, "people/bob", "Bob works at Acme", "rev1").await;
    assert_eq!(sync.manifest(U).await.unwrap().len(), 1);

    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Delete {
                path: "Memory/people/bob".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(r, EditResult::Removed);

    // Page gone from the manifest; its memories retired erroneous (kept for suppression).
    assert!(
        sync.manifest(U).await.unwrap().is_empty(),
        "no longer pulled"
    );
    assert!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .is_none()
    );
    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert!(!mems.is_empty() && mems.iter().all(|m| m.disposition == Disposition::Erroneous));
}

#[tokio::test]
async fn new_page_from_human_extracts_source_human_memories() {
    let ops =
        json!({ "ops": [ {"op":"add","kind":"fact","content":"Carol leads the platform team"} ]});
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "submit".into(), ops),
    ])]));
    let (.., harness, repo, _, sync) = setup(mock).await;

    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/carol".into(),
                base_rev: None,
                content: "# Carol\n\nLeads the platform team.".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(r, EditResult::Created { .. }), "new page created");

    let mems = repo.memories_for_entity(U, "people/carol").await.unwrap();
    assert!(!mems.is_empty());
    assert!(
        mems.iter()
            .all(|m| matches!(m.evidence[0].source, EvidenceSource::HumanEdit { .. }))
    );
}

/// Seed the user + their `system` builtin agent (handle `system`) - the identity a
/// detached sync ingest resolves via `AgentService::system_agent`.
async fn seed_user_and_system_agent(db: &Surreal<Db>) {
    use frona::core::repository::Repository;
    use frona::db::repo::generic::SurrealRepo;

    let now = chrono::Utc::now();
    SurrealRepo::<frona::auth::User>::new(db.clone())
        .create(&frona::auth::User {
            id: U.into(),
            handle: frona::handle!("testuser"),
            email: "t@t.com".into(),
            name: "Test".into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
        .create(&frona::agent::models::Agent {
            id: "sys-agent".into(),
            user_id: U.into(),
            handle: frona::handle!("system"),
            name: "Assistant".into(),
            description: String::new(),
            model_group: "test".into(),
            enabled: true,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            skills: None,
            avatar: None,
            identity: Default::default(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
}

/// The payoff: an **External** push (path outside the Memory directory) has no chat,
/// so `extract_external` runs detached as the user's `system` agent. If that resolution
/// works, the note's entities are mined into `External` memories. Before optional-chat,
/// this path fabricated sentinel ids and the investigator silently no-op'd.
#[tokio::test]
async fn external_ingest_runs_as_system_agent_and_mines_memories() {
    // One structured `extract` response - a fresh entity, so no adjudication call.
    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-1","path":"people/alex","kind":"person","name":"Alexandria Quill","description":"a colleague",
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}],
             "aliases":[],"candidate_attributes":[]}
        ],
        "memories": [
            {"kind":"fact","content":"Alexandria Quill works at Globex","entities":["people/alex"],
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("c1".into(), "submit".into(), extract),
    ])]));
    let (db, harness, repo, _, sync) = setup(mock.clone()).await;
    seed_user_and_system_agent(&db).await;

    // External = path NOT under the Memory directory → read-only ingest.
    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Work Notes/standup".into(),
                base_rev: None,
                content: "Alexandria Quill works at Globex.".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(r, EditResult::Indexed | EditResult::Unchanged),
        "external note indexed: {r:?}"
    );

    // The extraction ran and stored the memory before publication. This only happens if
    // `system_agent(user_id)` resolved; otherwise extract_external errors and is swallowed.
    let mems = repo.list_all_memories(U).await.unwrap();
    assert!(
        !mems.is_empty(),
        "the note was mined into people/alex (system agent resolved): {:#?}",
        mock.histories()
    );
    assert!(
        mems.iter().all(|m| matches!(&m.evidence[0].source, EvidenceSource::ExternalNote { note, .. } if note == "Work Notes/standup")),
        "mined memories are External, tagged with the note path"
    );
    let record = repo.latest_consolidation_record(U).await.unwrap().unwrap();
    let pending = PkmConsolidationStore::new(Arc::new(repo.clone()))
        .scoped(&record.consolidation_id, U)
        .working_entity("people/alex")
        .await
        .unwrap()
        .unwrap();
    assert!(
        pending.source_memory_ids.contains(&mems[0].id),
        "the pending entity retains the extracted memory link until publication"
    );
}

#[tokio::test]
async fn external_note_retries_extraction_after_failure() {
    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-1","path":"people/alex","kind":"person","name":"Alexandria Quill","description":"a colleague",
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}],
             "aliases":[],"candidate_attributes":[]}
        ],
        "memories": [
            {"kind":"fact","content":"Alexandria Quill works at Globex","entities":["people/alex"],
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("invalid request".into())),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), extract)]),
    ]));
    let (db, harness, repo, _, sync) = setup(mock.clone()).await;
    seed_user_and_system_agent(&db).await;
    let edit = || EditOp::Upsert {
        path: "Work Notes/standup".into(),
        base_rev: None,
        content: "Alexandria Quill works at Globex.".into(),
    };

    assert_eq!(
        sync.apply_edit(&harness, U, &handle(), edit())
            .await
            .unwrap(),
        EditResult::Indexed,
    );
    let failed_page = repo
        .entity_by_path(U, "Work Notes/standup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        failed_page.rev, failed_page.mirrored_rev,
        "accepted note was mirrored"
    );
    assert_eq!(
        failed_page.extracted_rev, None,
        "failed extraction was not marked complete"
    );
    let retry = sync
        .apply_edit(&harness, U, &handle(), edit())
        .await
        .unwrap();

    assert_eq!(
        retry,
        EditResult::Indexed,
        "the unchanged note must retry failed extraction"
    );
    assert_eq!(mock.calls(), 2, "the retry must reach extraction again");
    assert!(
        !repo.list_all_memories(U).await.unwrap().is_empty(),
        "the successful retry must store the derived memory",
    );
    let extracted_page = repo
        .entity_by_path(U, "Work Notes/standup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(extracted_page.rev, extracted_page.extracted_rev);
}

#[tokio::test]
async fn external_note_preserves_previous_memories_when_reextraction_fails() {
    let initial_extract = json!({
        "new_entities": [
            {"id":"fixture-page-1","path":"people/alex","kind":"person","name":"Alexandria Quill","description":"a colleague",
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}],
             "aliases":[],"candidate_attributes":[]}
        ],
        "memories": [
            {"kind":"fact","content":"Alexandria Quill works at Globex","entities":["people/alex"],
             "sources":[{"message":"m1","quote":"Alexandria Quill works at Globex","strength":"explicit"}]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), initial_extract)]),
        MockResponse::Error(InferenceError::InferenceFailed("invalid request".into())),
    ]));
    let (db, harness, repo, _, sync) = setup(mock).await;
    seed_user_and_system_agent(&db).await;
    sync.apply_edit(
        &harness,
        U,
        &handle(),
        EditOp::Upsert {
            path: "Work Notes/standup".into(),
            base_rev: None,
            content: "Alexandria Quill works at Globex.".into(),
        },
    )
    .await
    .unwrap();
    let previous_memory = repo.list_all_memories(U).await.unwrap()[0].id.clone();

    sync.apply_edit(
        &harness,
        U,
        &handle(),
        EditOp::Upsert {
            path: "Work Notes/standup".into(),
            base_rev: None,
            content: "Alexandria Quill now works at Initech.".into(),
        },
    )
    .await
    .unwrap();
    let old_rev = sha256_hex("Alexandria Quill works at Globex.");
    let new_rev = sha256_hex("Alexandria Quill now works at Initech.");
    let failed_page = repo
        .entity_by_path(U, "Work Notes/standup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_page.rev.as_deref(), Some(new_rev.as_str()));
    assert_eq!(failed_page.mirrored_rev.as_deref(), Some(new_rev.as_str()));
    assert_eq!(failed_page.extracted_rev.as_deref(), Some(old_rev.as_str()));

    assert!(
        repo.list_all_memories(U)
            .await
            .unwrap()
            .iter()
            .any(|memory| memory.id == previous_memory),
        "old derived memories must remain until replacement extraction commits",
    );
}

#[tokio::test]
async fn external_note_replaces_memories_and_completes_all_revisions_atomically() {
    let extraction = |company: &str| {
        json!({
            "new_entities": [
                {"id":"fixture-page-1","path":"people/alex","kind":"person","name":"Alexandria Quill","description":"a colleague",
                 "sources":[{"message":"m1","quote":format!("Alexandria Quill works at {company}"),"strength":"explicit"}],
                 "aliases":[],"candidate_attributes":[]}
            ],
            "memories": [
                {"kind":"fact","content":format!("Alexandria Quill works at {company}"),"entities":["people/alex"],
                 "sources":[{"message":"m1","quote":format!("Alexandria Quill works at {company}"),"strength":"explicit"}]}
            ]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), extraction("Globex"))]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), extraction("Initech"))]),
    ]));
    let (db, harness, repo, _, sync) = setup(mock.clone()).await;
    seed_user_and_system_agent(&db).await;
    let push = |content: &str| EditOp::Upsert {
        path: "Work Notes/standup".into(),
        base_rev: None,
        content: content.into(),
    };

    sync.apply_edit(
        &harness,
        U,
        &handle(),
        push("Alexandria Quill works at Globex."),
    )
    .await
    .unwrap();
    let old_memory = repo.list_all_memories(U).await.unwrap()[0].id.clone();
    let current = "Alexandria Quill works at Initech.";
    sync.apply_edit(&harness, U, &handle(), push(current))
        .await
        .unwrap();

    let memories = repo.list_all_memories(U).await.unwrap();
    assert_eq!(
        memories.len(),
        1,
        "replacement commits one current derived memory"
    );
    assert_ne!(memories[0].id, old_memory, "old derived memory was removed");
    assert_eq!(memories[0].content, "Alexandria Quill works at Initech");
    let page = repo
        .entity_by_path(U, "Work Notes/standup")
        .await
        .unwrap()
        .unwrap();
    let current_rev = sha256_hex(current);
    assert_eq!(page.rev.as_deref(), Some(current_rev.as_str()));
    assert_eq!(page.mirrored_rev, page.rev);
    assert_eq!(page.extracted_rev, page.rev);
    assert_eq!(
        sync.apply_edit(&harness, U, &handle(), push(current))
            .await
            .unwrap(),
        EditResult::Unchanged,
    );
    assert_eq!(
        mock.calls(),
        2,
        "a complete revision does not run extraction again"
    );
}

/// Create a plain Agent-sourced memory on `people/bob` (a write-back target).
async fn add_mem(repo: &PkmRepo, content: &str) -> String {
    repo.create_memory_with_entities(
        U,
        "a",
        "c",
        MemoryKind::Fact,
        content,
        &["people/bob".into()],
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn manifest_lists_rendered_pages_with_vault_paths_and_revs() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob\n\nbody").await;
    // A skeleton with no rev yet must NOT appear in the manifest.
    repo.upsert_entity_skeleton(
        U,
        "people/unrendered",
        EntityCategory::Concept,
        &["https://schema.org/Person".to_string()],
        "X",
        "",
        &[],
    )
    .await
    .unwrap();

    let m = sync.manifest(U).await.unwrap();
    assert_eq!(m.len(), 1, "only the rendered page");
    assert_eq!(
        m[0].path, "Memory/people/bob",
        "vault path (directory-prefixed)"
    );
    assert_eq!(m[0].rev, rev);
}

#[tokio::test]
async fn get_pages_returns_content_for_requested_paths_and_skips_unknown() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob\n\nfile bytes").await;

    let got = sync
        .get_pages(
            U,
            &handle(),
            &["Memory/people/bob".into(), "Memory/people/nope".into()],
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "unknown path skipped");
    assert_eq!(got[0].path, "Memory/people/bob");
    assert_eq!(got[0].rev, rev);
    assert_eq!(
        got[0].content, "# Bob\n\nfile bytes",
        "on-disk bytes returned"
    );
}

#[tokio::test]
async fn bootstrap_via_manifest_then_all_entities() {
    // A fresh client: manifest → fetch every path == full bootstrap.
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    author(&repo, &storage, "people/bob", "bob").await;
    author(&repo, &storage, "projects/phoenix", "phoenix").await;

    let paths: Vec<String> = sync
        .manifest(U)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    let got = sync.get_pages(U, &handle(), &paths).await.unwrap();
    assert_eq!(got.len(), 2, "every page fetched on bootstrap");
}

#[tokio::test]
async fn config_returns_directory_default() {
    let (.., sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    assert_eq!(sync.memory_directory(U).await.unwrap(), "Memory");
}

#[tokio::test]
async fn set_memory_directory_upserts_and_validates() {
    let (.., sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    assert_eq!(sync.memory_directory(U).await.unwrap(), "Memory");
    sync.set_memory_directory(U, "Brain").await.unwrap();
    assert_eq!(sync.memory_directory(U).await.unwrap(), "Brain", "created");
    sync.set_memory_directory(U, "Knowledge").await.unwrap();
    assert_eq!(
        sync.memory_directory(U).await.unwrap(),
        "Knowledge",
        "updated in place"
    );
    // A non-segment name is rejected.
    assert!(sync.set_memory_directory(U, "a/b").await.is_err());
    assert!(sync.set_memory_directory(U, "..").await.is_err());
    assert!(sync.set_memory_directory(U, "  ").await.is_err());
}

#[tokio::test]
async fn check_edit_gates_on_rev() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob\n\nv1").await;

    // Matching base_rev → Apply (with head bytes for the diff).
    match sync
        .check_edit(U, &handle(), "Memory/people/bob", Some(&rev))
        .await
        .unwrap()
    {
        EditGate::Apply {
            clean_path,
            head_content,
        } => {
            assert_eq!(clean_path, "people/bob");
            assert_eq!(head_content, "# Bob\n\nv1");
        }
        other => panic!("expected Apply, got {other:?}"),
    }
    // Stale base_rev → Conflict carrying the current head.
    match sync
        .check_edit(U, &handle(), "Memory/people/bob", Some("stale"))
        .await
        .unwrap()
    {
        EditGate::Conflict {
            head_rev,
            head_content,
        } => {
            assert_eq!(head_rev, rev);
            assert_eq!(head_content, "# Bob\n\nv1");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Unknown path → New.
    assert_eq!(
        sync.check_edit(U, &handle(), "Memory/people/carol", None)
            .await
            .unwrap(),
        EditGate::New {
            clean_path: "people/carol".into()
        }
    );
    // A path outside the Memory directory → validation error.
    assert!(
        sync.check_edit(U, &handle(), "Work Notes/x", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rename_moves_a_single_page() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    author(&repo, &storage, "people/bob", "# Bob").await;

    // A `.md` source is a single-page rename → Accepted with the moved file's rev.
    let r = sync
        .rename(
            U,
            &handle(),
            "Memory/people/bob.md",
            "Memory/people/robert.md",
        )
        .await
        .unwrap();
    assert!(
        matches!(r, EditResult::Accepted { .. }),
        "single-page rename → Accepted, got {r:?}"
    );
    assert!(
        repo.entity_by_path(U, "people/robert")
            .await
            .unwrap()
            .is_some(),
        "record moved"
    );
    assert!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .is_none(),
        "old record gone"
    );
    assert!(
        storage
            .read_page(&vault(&storage), "people/robert")
            .is_some(),
        "file moved"
    );
    assert!(
        storage.read_page(&vault(&storage), "people/bob").is_none(),
        "old file gone"
    );
}

#[tokio::test]
async fn rename_dir_moves_every_page_under_the_prefix() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    author(&repo, &storage, "people/bob", "# Bob").await;
    author(&repo, &storage, "people/alice", "# Alice").await;
    author(&repo, &storage, "places/paris", "# Paris").await; // outside the moved dir

    // No `.md` on the source → directory rename of everything under `people/`.
    let r = sync
        .rename(U, &handle(), "Memory/people", "Memory/humans")
        .await
        .unwrap();
    assert!(
        matches!(r, EditResult::Renamed { count: 2 }),
        "dir rename → Renamed{{count:2}}, got {r:?}"
    );
    assert!(
        repo.entity_by_path(U, "humans/bob")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path(U, "humans/alice")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .is_none(),
        "old paths gone"
    );
    assert!(
        storage.read_page(&vault(&storage), "humans/bob").is_some(),
        "file moved"
    );
    // A page outside the renamed directory is untouched.
    assert!(
        repo.entity_by_path(U, "places/paris")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn rename_dir_rejects_when_a_target_is_occupied() {
    let (.., repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    author(&repo, &storage, "people/bob", "# Bob").await;
    author(&repo, &storage, "humans/bob", "# Someone else").await; // occupies the target

    let err = sync
        .rename(U, &handle(), "Memory/people", "Memory/humans")
        .await
        .unwrap_err();
    assert!(
        matches!(err, frona::core::error::AppError::Conflict(_)),
        "occupied target → Conflict, got {err:?}"
    );
    // Rejected up front - nothing moved.
    assert!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .is_some(),
        "source untouched on conflict"
    );
}

#[tokio::test]
async fn rename_rewrites_wikilinks_in_the_server_mirror() {
    let (db, _, repo, storage, sync) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    author(&repo, &storage, "people/bob", "# Bob").await;
    // A linker page whose body references bob, plus the DB edge that records the link.
    author(&repo, &storage, "services/db", "See [[Memory/people/bob]].").await;
    seed_asserted_entity_link(&db, U, "services/db", "people/bob", "related")
        .await
        .unwrap();

    sync.rename(
        U,
        &handle(),
        "Memory/people/bob.md",
        "Memory/people/robert.md",
    )
    .await
    .unwrap();

    // The server fixed its *own* mirror, not just the DB edge - the client rewrites
    // its vault copy separately.
    let linker = storage.read_page(&vault(&storage), "services/db").unwrap();
    assert!(
        linker.contains("[[Memory/people/robert]]"),
        "link repointed to new path: {linker}"
    );
    assert!(
        !linker.contains("[[Memory/people/bob]]"),
        "old link gone: {linker}"
    );
}

#[tokio::test]
async fn external_mirror_index_and_delete() {
    // The External ingest's mirror + index + delete (repo/storage only - the
    // extraction LLM step is covered by `external_ingest_runs_as_system_agent`).
    let (.., repo, storage, _) = setup(Arc::new(MockModelProvider::new(vec![]))).await;
    let note = "# Standup\n\nDeploy Postgres at 3pm.";
    let path = "Work Notes/standup";
    let rev = sha256_hex(note);

    let accepted = repo
        .upsert_external_page(U, path, note, &rev)
        .await
        .unwrap();
    assert_eq!(accepted.rev, rev);
    assert!(
        accepted.mirror_pending,
        "new note needs its first mirror write"
    );
    assert!(
        accepted.extraction_pending,
        "new note needs its first extraction"
    );
    storage.write_user_note(&handle(), path, note).unwrap();

    // Seed a memory derived from this note (source = External { note = path }) so the
    // delete step exercises the real `drop_derived_memories` predicate.
    repo.create_sourced_memory(
        U,
        MemoryKind::Fact,
        "Deploy Postgres at 3pm",
        &["people/alex".to_string()],
        vec![MemoryEvidence {
            strength: EvidenceStrength::Explicit,
            source: EvidenceSource::ExternalNote {
                note: path.to_string(),
                quote: "Alex works at Globex".into(),
            },
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        repo.memories_for_entity(U, "people/alex")
            .await
            .unwrap()
            .len(),
        1,
        "derived memory seeded"
    );

    let page = repo.entity_by_path(U, path).await.unwrap().unwrap();
    assert_eq!(page.origin, EntityOrigin::External);
    assert!(
        storage
            .memory_root(&handle())
            .join(format!("{path}.md"))
            .is_file(),
        "mirrored under root, not Memory/"
    );
    let hits = repo.search_entities(U, "postgres deploy").await.unwrap();
    assert!(
        hits.iter()
            .any(|hh| hh.path == path && hh.origin == EntityOrigin::External),
        "external note is searchable and tagged External"
    );

    // Re-upsert identical content keeps the same pending state. Accepting the
    // content revision alone must not claim that mirror or extraction succeeded.
    let same = repo
        .upsert_external_page(U, path, note, &rev)
        .await
        .unwrap();
    assert_eq!(same, accepted, "same rev preserves its durable progress");

    // Delete → page + mirror gone, and the note's derived memories dropped.
    repo.delete_external_page(U, path).await.unwrap();
    storage.delete_user_note(&handle(), path).unwrap();
    assert!(repo.entity_by_path(U, path).await.unwrap().is_none());
    assert!(
        !storage
            .memory_root(&handle())
            .join(format!("{path}.md"))
            .exists()
    );
    assert!(
        repo.memories_for_entity(U, "people/alex")
            .await
            .unwrap()
            .is_empty(),
        "delete_external_page dropped the note's derived memories"
    );
}

#[tokio::test]
async fn writeback_add_mints_a_human_memory() {
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let (.., harness, repo, storage, sync) = setup(mock.clone()).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob").await;
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c1".into(),
        "submit".into(),
        json!({ "ops": [{"op":"add","kind":"fact","content":"Bob's backups run nightly"}] }),
    )]));
    let r = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some(rev),
                content: "# Bob\n\nBob's backups run nightly.".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(r, EditResult::Accepted { .. }),
        "write-back applied: {r:?}"
    );
    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert_eq!(mems.len(), 1);
    assert!(
        matches!(mems[0].evidence[0].source, EvidenceSource::HumanEdit { .. }),
        "minted as a human fact"
    );
    assert_eq!(mems[0].content, "Bob's backups run nightly");
}

#[tokio::test]
async fn writeback_does_not_adopt_file_when_a_memory_mutation_fails() {
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        (
            "c1".into(),
            "submit".into(),
            json!({ "ops": [{"op":"add","kind":"fact","content":"Bob's backups run nightly"}] }),
        ),
    ])]));
    let (db, harness, repo, storage, sync) = setup(mock).await;
    let original = "# Bob\n\nOriginal.";
    let rev = author(&repo, &storage, "people/bob", original).await;
    db.query("DEFINE FIELD user_id ON knowledge_memory TYPE int")
        .await
        .unwrap()
        .check()
        .unwrap();

    let result = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some(rev.clone()),
                content: "# Bob\n\nBob's backups run nightly.".into(),
            },
        )
        .await;

    assert!(
        result.is_err(),
        "a failed canonical memory mutation must fail the edit"
    );
    assert_eq!(
        storage.read_page(&vault(&storage), "people/bob").as_deref(),
        Some(original),
        "the file must keep its old bytes after the memory mutation fails",
    );
    assert_eq!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .unwrap()
            .rev
            .as_deref(),
        Some(rev.as_str()),
        "the stored revision must keep its old value",
    );
}

#[tokio::test]
async fn writeback_outdated_and_wrong_set_disposition() {
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let (.., harness, repo, storage, sync) = setup(mock.clone()).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob").await;
    let m1 = add_mem(&repo, "was true").await;
    let m2 = add_mem(&repo, "never true").await;
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c1".into(),
        "submit".into(),
        json!({ "ops": [
            {"op":"outdated","memory_id":"m2"},
            {"op":"wrong","memory_id":"m1"}
        ]}),
    )]));
    sync.apply_edit(
        &harness,
        U,
        &handle(),
        EditOp::Upsert {
            path: "Memory/people/bob".into(),
            base_rev: Some(rev),
            content: "# Bob\n\nedited".into(),
        },
    )
    .await
    .unwrap();
    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    let disp = |id: &str| mems.iter().find(|m| m.id == id).unwrap().disposition;
    assert_eq!(disp(&m1), Disposition::Outdated);
    assert_eq!(disp(&m2), Disposition::Erroneous);
}

#[tokio::test]
async fn writeback_rejects_ops_referencing_unknown_model_local_ids() {
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let (.., harness, repo, storage, sync) = setup(mock.clone()).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob").await;
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c1".into(),
        "submit".into(),
        json!({ "ops": [
            {"op":"outdated","memory_id":"ghost"},
            {"op":"supersede","kind":"fact","content":"x","memory_id":"ghost"}
        ]}),
    )]));
    let error = sync
        .apply_edit(
            &harness,
            U,
            &handle(),
            EditOp::Upsert {
                path: "Memory/people/bob".into(),
                base_rev: Some(rev),
                content: "# Bob\n\nedited".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown model-local m id `ghost`")
    );
    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert!(
        mems.is_empty(),
        "a rejected write-back must not mutate memories"
    );
}

#[tokio::test]
async fn writeback_drops_add_ops_with_unknown_kinds() {
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let (.., harness, repo, storage, sync) = setup(mock.clone()).await;
    let rev = author(&repo, &storage, "people/bob", "# Bob").await;
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c1".into(),
        "submit".into(),
        json!({ "ops": [
            {"op":"add","kind":"not-a-kind","content":"ignored"}
        ]}),
    )]));
    sync.apply_edit(
        &harness,
        U,
        &handle(),
        EditOp::Upsert {
            path: "Memory/people/bob".into(),
            base_rev: Some(rev),
            content: "# Bob\n\nedited".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        repo.memories_for_entity(U, "people/bob")
            .await
            .unwrap()
            .is_empty()
    );
}
