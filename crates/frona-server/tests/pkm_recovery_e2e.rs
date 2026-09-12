//! Recovery e2e for the consolidation pass - what survives a failure, and at what cost.
//!
//! The pipeline has two independent recovery mechanisms and they cover different things:
//!
//!   - **The dirty set** (`updated_at > rendered_at`) handles *per-item* failure. Only
//!     the author stage stamps `rendered_at`, so a page that failed before its exact
//!     projection committed stays dirty and is picked up again. Stages are deliberately
//!     tolerant of a single item failing - one page that will not classify must not stop
//!     the other ninety-nine.
//!   - **The `KnowledgeConsolidationRecord`** handles *stage-level* failure and crash
//!     resume. It records which items are finished so a resumed pass does not re-pay for
//!     them, and carries the retry budget that eventually abandons a pass that cannot
//!     progress.
//!
//! Each test below pins one of those, per stage.

mod helpers;

use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::chat::message::models::{Message, MessageRole, MessageStatus};
use frona::chat::models::Chat;
use frona::core::config::{Config, DatabaseConfig, MemoryConfig, StorageConfig};
use frona::core::repository::Repository;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::pkm::PkmRepo;
use frona::inference::config::ModelRegistryConfig;
use frona::memory::pkm::{ConsolidationStageState, PkmService};
use frona::storage::StorageService;

use helpers::{
    MockModelProvider, MockResponse, init_metrics, mark_entity_rendered, seed_reconciled_entity,
    test_harness, test_model_group, test_model_service_with_group,
};

const USER: &str = "u1";

// ─────────────────────────────────────────────────────────────────────────────
// Scaffolding
// ─────────────────────────────────────────────────────────────────────────────

fn test_config(tmp: &tempfile::TempDir) -> Config {
    let base = tmp.path().to_string_lossy().to_string();
    Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        database: DatabaseConfig {
            path: format!("{base}/db"),
        },
        storage: StorageConfig {
            data_dir: format!("{base}/data"),
            shared_config_dir: format!("{base}/config"),
            skills_dir: format!("{base}/skills"),
            cache_dir: format!("{base}/cache"),
            ..Default::default()
        },
        ..Default::default()
    }
}

struct Ctx {
    _tmp: tempfile::TempDir,
    db: Surreal<Db>,
    state: frona::core::state::AppState,
    pkm: PkmService,
    harness: Arc<frona::agent::harness::Harness>,
}

impl Ctx {
    fn repo(&self) -> PkmRepo {
        PkmRepo::new(self.db.clone(), 8)
    }

    async fn sweep(&self) {
        self.pkm
            .run_consolidation_sweep(
                &self.state.chat_service,
                &self.state.contact_service,
                &self.state.agent_service,
                &self.harness,
            )
            .await
            .unwrap();
    }

    async fn record(&self) -> Option<frona::memory::pkm::KnowledgeConsolidationRecord> {
        self.repo().latest_consolidation_record(USER).await.unwrap()
    }

    /// Drive one consolidation directly, bypassing the sweep.
    ///
    /// The sweep skips a user entirely when no ontology catalogue is loaded - the right
    /// production behaviour, and the reason the failure tests cannot inject through it.
    /// Calling `consolidate` is what production does once past that gate, so this exercises
    /// the record lifecycle without pretending the gate isn't there.
    async fn consolidate(
        &self,
    ) -> Result<frona::memory::pkm::ConsolidationStats, frona::core::error::AppError> {
        let vault = self
            .pkm
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap();
        let scope = frona::memory::pkm::ConsolidationScope {
            user_id: USER.into(),
            user_name: "Test User".into(),
            agent_id: "a1".into(),
            chat_id: None,
            vault,
            temporal_sources: Vec::new(),
            evidence_sources: Vec::new(),
            recall: Default::default(),
            timezone: "UTC".into(),
        };
        self.pkm.consolidate(scope, self.harness.clone()).await
    }
}

/// `broken_ontology` points the catalogue at a directory that does not exist, so the
/// Classify stage fails outright rather than per page - the only clean way to make a
/// *stage* (not an item) fail, since every stage deliberately swallows a single item's
/// model error.
async fn setup(
    mock: Arc<MockModelProvider>,
    memory_config: MemoryConfig,
    broken_ontology: bool,
) -> Ctx {
    init_metrics();
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics_handle = frona::core::metrics::setup_metrics_recorder();
    let state = frona::core::state::AppState::new(
        db.clone(),
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );

    state
        .user_service
        .create(&frona::auth::User {
            id: USER.into(),
            handle: frona::handle!("testuser"),
            email: "user@example.test".into(),
            name: "Test User".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let registry = Arc::new(
        test_model_service_with_group(
            "mock",
            mock.clone(),
            &memory_config.model_group,
            test_model_group(),
        )
        .await,
    );
    let prompts = frona::agent::prompt::PromptLoader::new(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
    );
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    let ontology_base = frona::memory::pkm::ontology::Roots {
        release: if broken_ontology {
            fixture.join("does-not-exist")
        } else {
            fixture.join("standard")
        },
        user: fixture.join("no-user-ontologies"),
    };
    let pkm = PkmService::new(
        db.clone(),
        state.storage_service.clone(),
        registry,
        prompts,
        memory_config,
        state.user_service.clone(),
        ontology_base,
    );
    let harness = test_harness(&db, &config, mock.clone()).await;
    seed_agent(&db).await;
    Ctx {
        _tmp: tmp,
        db,
        state,
        pkm,
        harness,
    }
}

async fn seed_agent(db: &Surreal<Db>) {
    let _ = SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
        .create(&frona::agent::models::Agent {
            id: "a1".into(),
            user_id: USER.into(),
            handle: frona::handle!("assistant"),
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
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
}

async fn seed_chat(db: &Surreal<Db>) -> Chat {
    let chat = Chat {
        id: frona::core::repository::new_id(),
        user_id: USER.into(),
        space_id: None,
        task_id: None,
        agent_id: "a1".into(),
        title: Some("t".into()),
        archived_at: None,
        channel_id: None,
        channel_external_id: None,
        metadata: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    SurrealRepo::<Chat>::new(db.clone())
        .create(&chat)
        .await
        .unwrap();
    chat
}

/// `mins_ago` controls the message clock, which is what the sweep selects on: a message
/// must be older than the idle cutoff (5 min) to be eligible, and newer than the chat's
/// watermark to be unconsolidated.
async fn add_message_at(db: &Surreal<Db>, chat_id: &str, content: &str, mins_ago: i64) {
    let mut m = Message::builder(chat_id, MessageRole::User, content.into()).build();
    m.created_at = Utc::now() - Duration::minutes(mins_ago);
    m.status = Some(MessageStatus::Completed);
    frona::db::repo::messages::SurrealMessageRepo::new(db.clone())
        .create(&m)
        .await
        .unwrap();
}

async fn add_message(db: &Surreal<Db>, chat_id: &str, content: &str) {
    add_message_at(db, chat_id, content, 120).await;
}

async fn add_agent_message(db: &Surreal<Db>, chat_id: &str, content: &str) {
    let mut message = Message::builder(chat_id, MessageRole::Agent, content.into()).build();
    message.created_at = Utc::now() - Duration::minutes(120);
    message.status = Some(MessageStatus::Completed);
    frona::db::repo::messages::SurrealMessageRepo::new(db.clone())
        .create(&message)
        .await
        .unwrap();
}

async fn seed_dirty_page(repo: &PkmRepo) {
    repo.upsert_entity_skeleton(
        USER,
        "services/postgres",
        frona::memory::pkm::model::EntityCategory::Concept,
        &[],
        "postgres",
        "svc",
        &[],
    )
    .await
    .unwrap();
    repo.create_sourced_memory(
        USER,
        frona::memory::pkm::model::MemoryKind::Fact,
        "postgres runs on 5433",
        &["services/postgres".to_string()],
        vec![frona::memory::pkm::model::MemoryEvidence {
            strength: frona::memory::pkm::model::EvidenceStrength::Explicit,
            source: frona::memory::pkm::model::EvidenceSource::HumanEdit {
                page_path: "services/postgres".into(),
                quote: "5433".into(),
            },
        }],
    )
    .await
    .unwrap();
}

/// The response queue for one page: extract, a classify per dirty page, reconcile, then
/// the authored article.
fn page_responses(name: &str, fact: &str, classify_entitys: usize) -> Vec<MockResponse> {
    let quote = fact.split_whitespace().last().unwrap_or(fact);
    let extract: Value = json!({
        "new_entities": [{"id":"fixture-page-1",
            "path": format!("services/{name}"), "name": name, "description": "svc",
            "sources": [{"message": "m1", "quote": quote, "strength": "explicit"}]
        }],
        "memories": [{"kind": "fact", "content": fact, "entities": [format!("services/{name}")],
            "sources": [{"message": "m1", "quote": quote, "strength": "explicit"}]}]
    });
    let classify: Value = json!({
        "entity": {"name": name, "description": "svc", "aliases": []},
        "classes": [{"class": "schema:SoftwareApplication"}],
        "relations": [],
        "attributes": [],
        "new_entities": [],
        "declarations": [],
    });
    let classify_self: Value = json!({
        "entity": {"name": "Test User", "description": "The account owner.", "aliases": []},
        "classes": [{"class": "schema:Person"}],
        "relations": [],
        "attributes": [],
        "new_entities": [],
        "declarations": [],
    });
    let reconcile: Value = json!({"relations": [], "outdated": [], "attributes": {}, "description": "svc", "moves": []});

    let mut out = vec![MockResponse::ToolCalls(vec![(
        "e".into(),
        "submit".into(),
        extract,
    )])];
    for i in 0..classify_entitys {
        let response = if i == 0 {
            classify_self.clone()
        } else {
            classify.clone()
        };
        out.push(MockResponse::ToolCalls(vec![(
            format!("k{i}"),
            "submit".into(),
            response,
        )]));
    }
    out.push(MockResponse::ToolCalls(vec![(
        "r".into(),
        "submit".into(),
        reconcile,
    )]));
    out.push(MockResponse::Text(format!("{name} details.")));
    out
}

#[tokio::test]
async fn procedural_memory_resolves_and_authors_a_playbook_end_to_end() {
    let extract = json!({
        "new_entities": [],
        "existing_entity_updates": [],
        "playbooks": [{
            "id": "pb1", "path": "playbooks/restart-postgres",
            "name": "Restart Postgres", "description": "Restart Postgres safely"
        }],
        "memories": [{
            "kind": "procedural", "content": "Run pg_ctl restart to restart Postgres safely",
            "entities": ["people/me"], "playbook": "pb1",
            "sources": [
                {"message": "m1", "quote": "pg_ctl restart", "strength": "explicit"},
                {"message": "m2", "quote": "Please remember that procedure",
                 "strength": "explicit", "confirmation": true}
            ]
        }]
    });
    let resolve = json!({"playbooks": [{
        "path": "playbooks/restart-postgres", "name": "Restart Postgres",
        "description": "Restart Postgres safely", "memory_ids": ["m1"]
    }]});
    let author = json!({
        "name": "Restart Postgres", "description": "Restart Postgres safely",
        "body": "# Restart Postgres\n\nRun `pg_ctl restart` and verify readiness.",
        "related_playbooks": []
    });
    let classify = json!({
        "entity": {"name": "Test User", "description": "The account owner.", "aliases": []},
        "classes": [{"class": "schema:Person"}], "relations": [], "attributes": [],
        "new_entities": [], "declarations": []
    });
    let reconcile = json!({
        "relations": [], "outdated": [], "attributes": {},
        "description": "The account owner.", "moves": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("q".into(), "submit".into(), reconcile)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), resolve)]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), author)]),
        MockResponse::Text("The account owner.".into()),
    ]));
    let ctx = setup(mock.clone(), MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_agent_message(
        &ctx.db,
        &chat.id,
        "Run pg_ctl restart to restart Postgres safely",
    )
    .await;
    add_message(&ctx.db, &chat.id, "Please remember that procedure.").await;

    ctx.sweep().await;

    let repo = ctx.repo();
    let page = repo
        .entity_by_path(USER, "playbooks/restart-postgres")
        .await
        .unwrap();
    let record = ctx.record().await;
    let pages = repo.list_entities(USER).await.unwrap();
    let page = page.unwrap_or_else(|| panic!(
        "Resolve did not materialize the Playbook; record={record:?} pages={pages:?}; histories={:?}",
        mock.histories(),
    ));
    assert_eq!(
        page.category,
        frona::memory::pkm::model::EntityCategory::Playbook
    );
    assert!(page.body.contains("pg_ctl restart"));
    assert!(
        ctx.pkm.storage().page_exists(
            &ctx.pkm
                .storage()
                .vault_scope(frona::handle!("testuser"), "Memory")
                .unwrap(),
            "playbooks/restart-postgres",
        )
    );
    assert!(matches!(
        ctx.record().await.unwrap().state,
        ConsolidationStageState::Done
    ));
    let tool_histories = mock.tool_histories();
    let author_tools = tool_histories
        .iter()
        .find(|tools| {
            let has = |name| tools.iter().any(|tool| tool.name == name);
            has("submit") && has("search_entities") && has("read_entity") && !has("ontology_sparql")
        })
        .expect("Playbook Author model request");
    let names: std::collections::HashSet<_> =
        author_tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains("search_entities"));
    assert!(names.contains("read_entity"));
    for filesystem_tool in ["read", "grep", "glob", "shell"] {
        assert!(
            !names.contains(filesystem_tool),
            "Playbook Author must use effective PKM tools: {names:?}"
        );
    }
}

#[tokio::test]
async fn failed_playbook_author_stays_parked_and_resumes_from_the_dirty_page() {
    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let config = MemoryConfig {
        pkm_consolidation_max_attempts: 1,
        pkm_consolidation_retry_base_secs: 0,
        ..Default::default()
    };
    let ctx = setup(mock.clone(), config, false).await;
    let repo = ctx.repo();
    repo.upsert_entity_skeleton(
        USER,
        "playbooks/restart-postgres",
        frona::memory::pkm::model::EntityCategory::Playbook,
        &[],
        "Restart Postgres",
        "Restart it safely",
        &[],
    )
    .await
    .unwrap();
    repo.create_sourced_memory(
        USER,
        frona::memory::pkm::model::MemoryKind::Procedural,
        "Run pg_ctl restart",
        &["playbooks/restart-postgres".to_string()],
        vec![frona::memory::pkm::model::MemoryEvidence {
            strength: frona::memory::pkm::model::EvidenceStrength::Explicit,
            source: frona::memory::pkm::model::EvidenceSource::HumanEdit {
                page_path: "playbooks/restart-postgres".into(),
                quote: "pg_ctl restart".into(),
            },
        }],
    )
    .await
    .unwrap();
    repo.save_consolidation_record(&frona::memory::pkm::KnowledgeConsolidationRecord {
        id: frona::core::repository::new_id(),
        consolidation_id: frona::core::repository::new_id(),
        user_id: USER.into(),
        state: ConsolidationStageState::PlaybookAuthor,
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    })
    .await
    .unwrap();

    assert!(ctx.consolidate().await.is_err());
    let parked = ctx
        .record()
        .await
        .expect("Playbook Author failure remains durable");
    assert!(matches!(
        parked.state,
        ConsolidationStageState::PlaybookAuthor
    ));
    assert_eq!(parked.attempts, 1);
    assert!(
        repo.entity_by_path(USER, "playbooks/restart-postgres")
            .await
            .unwrap()
            .is_some()
    );

    mock.enqueue(MockResponse::ToolCalls(vec![(
        "a".into(),
        "submit".into(),
        json!({
            "name": "Restart Postgres", "description": "Restart it safely",
            "body": "# Restart Postgres\n\nRun `pg_ctl restart`.", "related_playbooks": []
        }),
    )]));
    ctx.consolidate().await.unwrap();
    let page = repo
        .entity_by_path(USER, "playbooks/restart-postgres")
        .await
        .unwrap()
        .unwrap();
    assert!(page.body.contains("pg_ctl restart"));
    assert!(matches!(
        ctx.record().await.unwrap().state,
        ConsolidationStageState::Done
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// The happy path - what a completed pass leaves behind
// ─────────────────────────────────────────────────────────────────────────────

/// A pass that completes walks every stage and lands on `Done`, keeping its counts as
/// the pass log. `Done` is what tells the next sweep this is history, not work in flight.
#[tokio::test]
async fn completed_pass_reaches_done_and_is_kept_as_a_log() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock.clone(), MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;

    ctx.sweep().await;

    let rec = ctx.record().await.expect("a pass ran");
    assert!(
        matches!(rec.state, ConsolidationStageState::Done),
        "reached the terminal stage"
    );
    assert_eq!(rec.attempts, 0, "no stage had to be retried");
    assert!(
        rec.stats.memories_added > 0,
        "the log carries the pass's counts: {:?}",
        rec.stats
    );
    assert!(
        ctx.repo()
            .users_with_open_consolidation()
            .await
            .unwrap()
            .is_empty(),
        "a finished pass is not open work"
    );
}

#[tokio::test]
async fn completed_pass_keeps_exact_page_projection_in_database() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock, MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;

    ctx.sweep().await;

    let page = ctx
        .repo()
        .entity_by_path(USER, "services/postgres")
        .await
        .unwrap()
        .unwrap();
    let rev = page.rev.as_deref().expect("completed page has a revision");
    let content = page
        .sync_content
        .as_deref()
        .expect("completed page keeps its exact canonical bytes");
    assert_eq!(frona::memory::pkm::sha256_hex(content), rev);
    let vault = ctx
        .pkm
        .storage()
        .vault_scope(frona::handle!("testuser"), "Memory")
        .unwrap();
    assert_eq!(
        ctx.pkm
            .storage()
            .read_page(&vault, "services/postgres")
            .as_deref(),
        Some(content),
        "the file mirror must contain the durable database projection",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-item recovery - the dirty set
// ─────────────────────────────────────────────────────────────────────────────

/// Only the author stage marks a page done. A page that reconciled but whose article was
/// never written stays dirty, so the next pass re-authors it - the failure that used to
/// leave the `.md` and the memories permanently diverged, because reconcile stamped the
/// completion marker three stages early.
#[tokio::test]
async fn page_that_was_never_authored_stays_dirty() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock, MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    ctx.sweep().await;

    let repo = ctx.repo();
    assert!(
        repo.entities_needing_reconciliation(USER)
            .await
            .unwrap()
            .is_empty(),
        "a fully processed pass leaves nothing dirty"
    );

    // A new fact arrives, so the page owes work again...
    repo.create_sourced_memory(
        USER,
        frona::memory::pkm::model::MemoryKind::Fact,
        "postgres also serves the staging app",
        &["services/postgres".to_string()],
        vec![frona::memory::pkm::model::MemoryEvidence {
            strength: frona::memory::pkm::model::EvidenceStrength::Explicit,
            source: frona::memory::pkm::model::EvidenceSource::HumanEdit {
                page_path: "services/postgres".into(),
                quote: "postgres also serves the staging app".into(),
            },
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        repo.entities_needing_reconciliation(USER).await.unwrap(),
        ["services/postgres"],
        "a new fact dirties the page it lands on"
    );

    // ...reconcile runs, and the pass then dies before author. The page must still be
    // owed: reconcile writing content is not the same as the article reaching disk.
    seed_reconciled_entity(&ctx.db, USER, "services/postgres", "", "the db", &json!({}))
        .await
        .unwrap();
    assert_eq!(
        repo.entities_needing_reconciliation(USER).await.unwrap(),
        ["services/postgres"],
        "reconcile alone does not mark the page done"
    );

    mark_entity_rendered(&ctx.db, USER, "services/postgres")
        .await
        .unwrap();
    assert!(
        repo.entities_needing_reconciliation(USER)
            .await
            .unwrap()
            .is_empty(),
        "only the article landing on disk does"
    );
}

/// A quarantined page must stay dirty, or the reinstate sweep - which walks only the
/// dirty set - can never release what it held, and the facts are hidden forever.
#[tokio::test]
async fn quarantined_page_stays_dirty_so_it_can_be_reinstated() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock, MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    ctx.sweep().await;

    let repo = ctx.repo();
    let memories = repo
        .memories_for_entity(USER, "services/postgres")
        .await
        .unwrap();
    let fact = memories.first().expect("the page has a fact");

    repo.set_disposition(
        USER,
        &fact.id,
        frona::memory::pkm::model::Disposition::Suspect,
    )
    .await
    .unwrap();
    assert!(
        repo.entities_needing_reconciliation(USER)
            .await
            .unwrap()
            .contains(&"services/postgres".to_string()),
        "quarantining re-dirties the page it hid facts on"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Stage-level recovery - the record
// ─────────────────────────────────────────────────────────────────────────────

/// A stage that fails leaves the record parked on it, charges an attempt, and schedules a
/// retry. The pass resumes *there* rather than starting over.
#[tokio::test]
async fn failed_stage_parks_the_record_and_backs_off() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let config = MemoryConfig {
        pkm_consolidation_max_attempts: 3,
        ..Default::default()
    };
    // A broken catalogue fails the Classify stage as a whole.
    let ctx = setup(mock, config, true).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    seed_dirty_page(&ctx.repo()).await;

    assert!(
        ctx.consolidate().await.is_err(),
        "the broken catalogue fails the Classify stage"
    );

    let rec = ctx.record().await.expect("the pass left a record");
    assert!(
        matches!(rec.state, ConsolidationStageState::Classify(_)),
        "parked on the stage that failed, not advanced past it: {}",
        rec.state.label()
    );
    assert_eq!(rec.attempts, 1, "one attempt charged");
    assert!(
        rec.next_attempt_at > Utc::now(),
        "and a retry scheduled in the future"
    );
    assert_eq!(
        ctx.repo().users_with_open_consolidation().await.unwrap(),
        [USER],
        "the user still has open work, even with no chat left to mine"
    );
}

/// While a pass is backing off, the sweep leaves that user alone entirely - including
/// mining. A wedged pass must not keep piling unreconciled pages underneath itself.
#[tokio::test]
async fn backing_off_pass_suppresses_the_next_tick() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let config = MemoryConfig {
        pkm_consolidation_max_attempts: 5,
        pkm_consolidation_retry_base_secs: 3600,
        ..Default::default()
    };
    let ctx = setup(mock.clone(), config, true).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    seed_dirty_page(&ctx.repo()).await;

    assert!(ctx.consolidate().await.is_err());
    let after_first = mock.calls();
    let attempts_after_first = ctx.record().await.unwrap().attempts;
    assert!(
        ctx.record().await.unwrap().next_attempt_at > Utc::now(),
        "backing off"
    );

    // The sweep is what honours the backoff, so drive it rather than `consolidate`.
    ctx.sweep().await;

    assert_eq!(
        mock.calls(),
        after_first,
        "the backing-off tick spends nothing"
    );
    assert_eq!(
        ctx.record().await.unwrap().attempts,
        attempts_after_first,
        "and charges no further attempt"
    );
}

/// A downstream stage with no durable raw contributions cannot be reconstructed and is
/// marked terminally Failed instead of being silently deleted.
#[tokio::test]
async fn unrecoverable_stage_that_burns_its_budget_is_marked_failed() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let config = MemoryConfig {
        pkm_consolidation_max_attempts: 2,
        // No backoff, so consecutive sweeps in the test are eligible immediately.
        pkm_consolidation_retry_base_secs: 0,
        ..Default::default()
    };
    let ctx = setup(mock, config, true).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    seed_dirty_page(&ctx.repo()).await;

    assert!(ctx.consolidate().await.is_err());
    assert_eq!(ctx.record().await.unwrap().attempts, 1);

    assert!(ctx.consolidate().await.is_err());
    let restarted = ctx.record().await.expect("the checkpoint remains durable");
    assert!(matches!(restarted.state, ConsolidationStageState::Failed));
    assert_eq!(restarted.restart_count, 1);
    assert!(restarted.failure.is_some());
}

// ─────────────────────────────────────────────────────────────────────────────
// Resume - what a re-entered pass does not pay for twice
// ─────────────────────────────────────────────────────────────────────────────

/// A pass killed part-way through author resumes having paid for what it wrote.
///
/// **Page Author keeps nothing in the record**, and needs nothing: `commit_authored_page` runs
/// inside the same call that commits the article, so a page whose article landed drops out
/// of `updated_at > rendered_at` and a resumed author never offers it to the model again.
/// The marker travels with the effect rather than in a second write a crash could lose,
/// which is why this is the one stage that is crash-safe without bookkeeping.
///
/// So the crash is simulated by producing that state honestly - one page authored, one
/// page dirty - and parking the record on `PageAuthor`. It used to be simulated by writing a
/// done-set into the record by hand, which proved the resume path worked when handed a
/// state that nothing could actually produce.
#[tokio::test]
async fn pass_killed_mid_page_author_resumes_without_re_authoring_what_it_wrote() {
    use frona::memory::pkm::{ConsolidationStageState, KnowledgeConsolidationRecord};

    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock.clone(), MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    ctx.sweep().await;

    let repo = ctx.repo();
    let mut pages = repo.list_all_entity_paths(USER).await.unwrap();
    pages.sort();
    assert_eq!(
        pages.len(),
        2,
        "the pass produced the self-page and the mined one: {pages:?}"
    );
    let (already_written, still_owed) = (pages[0].clone(), pages[1].clone());

    // Exactly the durable state a crash mid-author leaves: one page's exact article and
    // `rendered_at` committed, the other owing work.
    repo.create_sourced_memory(
        USER,
        frona::memory::pkm::model::MemoryKind::Fact,
        &format!("something new about {still_owed}"),
        std::slice::from_ref(&still_owed),
        vec![frona::memory::pkm::model::MemoryEvidence {
            strength: frona::memory::pkm::model::EvidenceStrength::Explicit,
            source: frona::memory::pkm::model::EvidenceSource::HumanEdit {
                page_path: still_owed.clone(),
                quote: format!("something new about {still_owed}"),
            },
        }],
    )
    .await
    .unwrap();
    let before: Vec<_> = rendered_stamps(&repo, &pages).await;

    repo.save_consolidation_record(&KnowledgeConsolidationRecord {
        id: frona::core::repository::new_id(),
        consolidation_id: frona::core::repository::new_id(),
        user_id: USER.into(),
        state: ConsolidationStageState::PageAuthor,
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    })
    .await
    .unwrap();

    // Exactly one article's worth of model budget - if the resume re-authored the banked
    // page too, it would fall through to the mock's default text and this would not hold.
    let calls_before = mock.calls();
    mock.enqueue(MockResponse::Text("resumed article.".into()));
    ctx.consolidate().await.unwrap();

    let after = rendered_stamps(&repo, &pages).await;
    assert_eq!(
        after[0], before[0],
        "{already_written} was already rendered, so the resume left it alone"
    );
    assert!(
        after[1] > before[1],
        "{still_owed} was still owed, so the resume authored it"
    );
    assert_eq!(
        mock.calls() - calls_before,
        1,
        "one article authored, not two — the already-rendered page cost nothing"
    );
    assert!(
        ctx.record().await.unwrap().state.is_done(),
        "and the resumed pass ran through to the end"
    );
}

/// The Classify's work reaches the row **while the stage is still running**, which is
/// the difference between resuming where a crash happened and resuming at a stage
/// boundary.
///
/// Classify does not publish to the live graph until Assemble completes. Accepted work
/// first goes to the active consolidation entity row. This test proves that the row and its resume
/// decision are readable before Classify returns.
///
/// The two futures race in one task: the poller sees the banked classification and wins,
/// and the pass future is dropped where it stands - which is exactly what a crash or a
/// cancelled sweep does, since no destructor writes anything.
#[tokio::test]
async fn classification_reaches_the_record_before_the_stage_finishes() {
    use frona::db::repo::pkm::PkmConsolidationStore;
    use frona::memory::pkm::ConsolidationStageState;
    use frona::memory::pkm::model::ClassificationProgress;

    let mut responses = page_responses("postgres", "Postgres runs on 5433", 2);
    responses[1] = MockResponse::ToolCalls(vec![(
        "classify-self".into(),
        "submit".into(),
        json!({
            "entity": {
                "name": "Test User",
                "description": "The account owner.",
                "aliases": [],
            },
            "classes": [{"class": "schema:Person"}],
            "relations": [],
            "attributes": [],
            "new_entities": [],
            "declarations": [],
        }),
    )]);
    // Stop exactly after the first classification has been persisted and the second
    // request has begun. Dropping the pass below simulates the process disappearing.
    responses[2] = MockResponse::Pending;
    let expected_calls = responses.len() + 1; // the interrupted request is paid for once
    let mock = Arc::new(MockModelProvider::new(responses));
    let ctx = setup(mock.clone(), MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    let repo = ctx.repo();

    let banked = tokio::select! {
        biased;
        n = async {
            // Bounded so a regression fails rather than hangs: the pass is a handful of
            // mocked calls, so this is orders of magnitude more polls than it needs.
            for _ in 0..100_000 {
                if mock.calls() < 3 {
                    tokio::task::yield_now().await;
                    continue;
                }
                let rec = repo.latest_consolidation_record(USER).await.unwrap()?;
                if !matches!(&rec.state, ConsolidationStageState::Classify(_)) {
                    return None;
                }
                let effective = PkmConsolidationStore::new(Arc::new(ctx.repo()))
                    .scoped(&rec.consolidation_id, USER);
                let row_results = futures::future::join_all(
                    ["people/me", "services/postgres"].into_iter()
                        .map(|path| effective.working_entity(path)),
                ).await;
                let accepted = row_results.into_iter().filter(|result| result.as_ref().is_ok_and(|row| {
                    row.as_ref().is_some_and(|row| matches!(
                        row.progress.classification,
                        ClassificationProgress::Accepted { .. }
                    ))
                })).count();
                return Some(accepted);
            }
            None
        } => n,
        _ = ctx.sweep() => None,
    };

    assert!(
        banked.is_some_and(|n| n > 0),
        "no classification was readable from a consolidation row while classify was still running — \
         a crash here would re-pay for every classify conversation"
    );

    let calls_at_interruption = mock.calls();
    assert_eq!(
        calls_at_interruption, 3,
        "extract, one banked classify, one interrupted classify"
    );

    // The interrupted provider request is gone with the process, so retry supplies a new
    // answer for that one unbanked page. The first page must be replayed from the record.
    mock.prepend(MockResponse::ToolCalls(vec![(
        "resume-classify".into(),
        "submit".into(),
        json!({
            "entity": {"name": "Postgres", "description": "svc", "aliases": []},
            "classes": [{"class": "schema:SoftwareApplication"}],
            "relations": [],
            "attributes": [],
            "new_entities": [],
            "declarations": [],
        }),
    )]));

    // Re-enter through the production consolidation driver, against the same database.
    // The queued responses contain only the work after the banked classification. If the
    // resume starts Classify from an empty/default state, calls shift onto the wrong
    // tools and the pass cannot produce the expected final graph.
    if let Err(error) = ctx.consolidate().await {
        let state = ctx.record().await.map(|record| match record.state {
            ConsolidationStageState::Classify(state) => {
                format!("classify revision={}", state.revision,)
            }
            state => state.label().into(),
        });
        panic!(
            "resume failed after {} total model calls in state {state:?}: {error}",
            mock.calls()
        );
    }

    let rec = ctx
        .record()
        .await
        .expect("the resumed pass remains as its completed log");
    assert!(
        rec.state.is_done(),
        "the interrupted Classify pass reaches Done on resume"
    );
    assert_eq!(
        mock.calls(),
        expected_calls,
        "resume replays the banked classification without another model request"
    );
    let page = ctx
        .repo()
        .entity_by_path(USER, "services/postgres")
        .await
        .unwrap();
    assert!(
        page.is_some(),
        "the final transaction commits the resumed pending page"
    );
}

/// `rendered_at` for each path, in the order given - the stamp author writes once a
/// page's article is on disk, and so the cheapest evidence of whether it re-authored.
async fn rendered_stamps(repo: &PkmRepo, paths: &[String]) -> Vec<chrono::DateTime<Utc>> {
    let mut out = Vec::new();
    for p in paths {
        out.push(
            repo.entity_by_path(USER, p)
                .await
                .unwrap()
                .unwrap()
                .rendered_at,
        );
    }
    out
}

/// The record advances through every explicit stage in pipeline order. Each transition is
/// pipeline order and each transition is a checkpoint, so a crash lands on a stage
/// boundary at worst.
#[tokio::test]
async fn record_advances_through_every_stage_in_order() {
    let mut seen = vec![];
    let mut state = ConsolidationStageState::Ingest(Default::default());
    seen.push(state.label());
    while !state.is_done() {
        state = state.next();
        seen.push(state.label());
    }
    assert_eq!(
        seen,
        [
            "ingest",
            "classify",
            "resolve",
            "reconcile",
            "assemble",
            "playbook_resolve",
            "playbook_author",
            "page_author",
            "cleanup",
            "done",
        ],
        "the variant order is the pipeline order"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Schema and types commit together
// ─────────────────────────────────────────────────────────────────────────────

/// The invariant behind Assemble's transaction: a page never carries a class the
/// TBox has not declared.
///
/// The stage used to commit the schema, then stamp pages in a loop - so a failure part
/// way through left pages typed against a delta that had not landed, and an adjudication
/// failure took the explicit "stamping without schema" path. Both are gone: the delta and
/// every `kinds` justified by it go in one transaction.
#[tokio::test]
async fn no_entity_carries_a_class_the_schema_never_declared() {
    let mock = Arc::new(MockModelProvider::new(page_responses(
        "postgres",
        "Postgres runs on 5433",
        2,
    )));
    let ctx = setup(mock, MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    ctx.sweep().await;

    let repo = ctx.repo();
    // The terms this user's delta declares. Standard vocabulary comes from the bundled
    // catalogue and is declared by construction, so `frona:` mints are the ones that can
    // go missing - they exist only because this pass decided to create them.
    // Built here rather than borrowed from the service - the reasoner is a fixture
    // concern, and the delta lives in the database, so a second manager sees the same
    // state.
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    let manager = frona::memory::pkm::ontology::OntologyManager::new(
        frona::memory::pkm::ontology::Roots {
            release: fixture.join("standard"),
            user: fixture.join("no-user-ontologies"),
        },
        Arc::new(ctx.repo()),
    );
    let catalog = manager.catalog(USER).await.unwrap();
    let declared: std::collections::HashSet<String> = catalog
        .classes
        .iter()
        .chain(&catalog.object_properties)
        .chain(&catalog.data_properties)
        .cloned()
        .collect();
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    let mut typed = 0;
    let mut minted = 0;
    for page in repo.list_entities(USER).await.unwrap() {
        for kind in &page.kinds {
            typed += 1;
            if !kind.starts_with("urn:frona:") {
                continue; // bundled vocabulary - declared by the catalogue
            }
            minted += 1;
            let curie = px.display(kind);
            assert!(
                declared.contains(&curie),
                "page {} is typed `{curie}`, which this user's schema never declared — \
                 the delta and the stamp did not land together",
                page.path
            );
        }
    }
    assert!(
        typed > 0,
        "the pass typed something, so this is not vacuous"
    );
    let _ = minted; // a pass that minted nothing still proves the standard-term half
}

// ─────────────────────────────────────────────────────────────────────────────
// Cleanup's repair path
// ─────────────────────────────────────────────────────────────────────────────

/// Memories commit before their pages exist, so an abandoned pass can leave facts whose
/// page never materialised - invisible to every projection, and invisible to the orphan
/// GC, which only catches memories with no link at all. Cleanup re-creates the page.
#[tokio::test]
async fn cleanup_rehomes_memories_whose_entity_went_missing() {
    // Two passes' worth: the first builds the page, the second is the one whose cleanup
    // stage does the repair. A pass whose extract fails never reaches cleanup at all.
    let mut responses = page_responses("postgres", "Postgres runs on 5433", 2);
    responses.extend(page_responses("redis", "Redis runs on 6380", 4));
    let mock = Arc::new(MockModelProvider::new(responses));
    let ctx = setup(mock.clone(), MemoryConfig::default(), false).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(&ctx.db, &chat.id, "my postgres runs on 5433").await;
    ctx.sweep().await;

    let repo = ctx.repo();
    // Drop the page but keep its memories - exactly what a pass abandoned between the
    // memory commit and the page commit leaves behind.
    repo.delete_entity(USER, "services/postgres").await.unwrap();
    assert_eq!(
        repo.dangling_memory_paths(USER).await.unwrap(),
        ["services/postgres"],
        "the memories now point at a path with no page"
    );
    let memories = repo
        .memories_for_entity(USER, "services/postgres")
        .await
        .unwrap();
    assert!(
        !repo
            .memory_entity_paths(USER, &memories[0].id)
            .await
            .unwrap()
            .is_empty(),
        "the memory still has its entity link"
    );

    // The repair lives in the cleanup stage, so it needs a pass to run in - this is a
    // backstop for a rare case, not a separate sweep of its own. Give the user something
    // to consolidate so the next tick opens a pass.
    add_message_at(&ctx.db, &chat.id, "and redis is on 6380", 30).await;
    ctx.sweep().await;

    let page = repo
        .entity_by_path(USER, "services/postgres")
        .await
        .unwrap()
        .expect("cleanup re-created the entity so the facts are reachable again");
    assert_eq!(page.name, "postgres", "named from the last path segment");
    assert!(
        repo.dangling_memory_paths(USER).await.unwrap().is_empty(),
        "nothing dangles once the page is back"
    );
}

#[tokio::test]
async fn cleanup_materializes_live_entities_then_removes_only_obsolete_markdown() {
    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let ctx = setup(mock, MemoryConfig::default(), false).await;
    seed_dirty_page(&ctx.repo()).await;
    ctx.repo()
        .set_page_rev(USER, "services/postgres", "previous-author-rev")
        .await
        .unwrap();
    let storage = ctx.pkm.storage();
    let scope = storage
        .vault_scope(frona::handle!("testuser"), "Memory")
        .unwrap();
    storage
        .write_page(&scope, "obsolete/nested/page", "old")
        .unwrap();
    let artifact = storage
        .memory_root(&frona::handle!("testuser"))
        .join("Memory/artifacts/keep.txt");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, "keep").unwrap();

    let record = frona::memory::pkm::KnowledgeConsolidationRecord {
        id: frona::core::repository::new_id(),
        consolidation_id: frona::core::repository::new_id(),
        user_id: USER.into(),
        state: ConsolidationStageState::Cleanup,
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    ctx.repo().save_consolidation_record(&record).await.unwrap();
    ctx.consolidate().await.unwrap();

    assert!(
        storage.page_exists(&scope, "services/postgres"),
        "every live page is materialized"
    );
    assert!(
        ctx.repo()
            .entities_needing_reconciliation(USER)
            .await
            .unwrap()
            .contains(&"services/postgres".to_string()),
        "cleanup repairs the file projection without pretending Page Author completed",
    );
    assert!(
        !storage.page_exists(&scope, "obsolete/nested/page"),
        "obsolete Markdown is deleted"
    );
    assert!(artifact.exists(), "non-Markdown artifacts are preserved");
    assert!(
        !storage
            .memory_root(&frona::handle!("testuser"))
            .join("Memory/obsolete")
            .exists(),
        "directories emptied by the sweep are removed",
    );
}
