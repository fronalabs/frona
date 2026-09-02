//! Full-stack e2e for the PKM consolidation sweep: seed a real idle Chat +
//! Messages (+ a `remember`ed short memory), run `run_consolidation_sweep` with the
//! real `ChatService`/`ContactService`/`UserService`/`AgentService` (from
//! `AppState`) and a mock-LLM `PkmService`, and assert the sweep keys on the
//! **message clock**: a settled chat is consolidated into pages, the watermark
//! advances, the short memory is marked validated, a second sweep is a no-op - and
//! an in-flight (`Executing`) message is deferred until it completes.

mod helpers;

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::chat::message::models::{Message, MessageRole, MessageStatus};
use frona::chat::models::Chat;
use frona::core::config::{Config, DatabaseConfig, MemoryConfig, StorageConfig};
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::repo::chats::SurrealChatRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::db::repo::pkm::{PkmConsolidationStore, PkmRepo};
use frona::db::repo::tool_calls::SurrealToolCallRepo;
use frona::inference::config::ModelRegistryConfig;
use frona::inference::tool_call::ToolCall;
use frona::memory::pkm::model::{
    ClassificationProgress, EntityCategory, KnowledgeConsolidationEntity,
};
use frona::memory::pkm::{
    ConsolidationStageState, ConsolidationWorkState, KnowledgeConsolidationRecord, PkmService,
};
use frona::storage::StorageService;

use helpers::{
    MockModelProvider, MockResponse, init_metrics, test_harness, test_model_group,
    test_registry_with_group,
};

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
    state: AppState,
    pkm: PkmService,
    harness: Arc<frona::agent::harness::Harness>,
}

async fn working_entity(
    repo: &PkmRepo,
    record: &KnowledgeConsolidationRecord,
    path: &str,
) -> Option<KnowledgeConsolidationEntity> {
    PkmConsolidationStore::new(Arc::new(repo.clone()))
        .scoped(&record.consolidation_id, &record.user_id)
        .working_entity(path)
        .await
        .unwrap()
}

/// Build AppState + a mock-LLM PkmService + harness, and seed user `u1`.
async fn setup(mock: Arc<MockModelProvider>) -> Ctx {
    setup_with_memory_config(mock, MemoryConfig::default()).await
}

async fn setup_with_memory_config(
    mock: Arc<MockModelProvider>,
    memory_config: MemoryConfig,
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
    let state = AppState::new(
        db.clone(),
        &config,
        Some(ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
    );

    state
        .user_service
        .create(&frona::auth::User {
            id: "u1".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
    );
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    let ontology_base = frona::memory::pkm::ontology::Roots {
        release: fixture.join("standard"),
        user: tmp.path().join("user-ontologies"),
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
    let harness = test_harness(&db, &config, mock.clone());
    seed_agent(&db).await;
    Ctx {
        _tmp: tmp,
        db,
        state,
        pkm,
        harness,
    }
}

/// Seed the `a1` agent every seeded chat references.
///
/// Called from [`setup`], so it applies to every test in this file. It used to be absent:
/// `seed_chat` set `agent_id: "a1"` with no matching row, so every stage that opens a
/// tool-bearing conversation - the whole Classify stage, and Playbook Author's
/// evidence tools - failed instantly on agent resolution and consumed nothing. The
/// tests still passed, because a dangling `agent_id` fails *quietly*.
///
/// That is a landmine, not a shortcut: it means agent-dependent behaviour can be added to
/// any stage and this suite will still go green while production breaks. The fixture is
/// now faithful, and the mock queues below budget for the stages that consequently run.
async fn seed_agent(db: &Surreal<Db>) {
    let _ = SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
        .create(&frona::agent::models::Agent {
            id: "a1".into(),
            user_id: "u1".into(),
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
        user_id: "u1".into(),
        space_id: None,
        task_id: None,
        agent_id: "a1".into(),
        title: Some("Test".into()),
        archived_at: None,
        channel_id: None,
        channel_external_id: None,
        metadata: Default::default(),
        created_at: Utc::now() - Duration::hours(3),
        updated_at: Utc::now() - Duration::hours(3),
    };
    SurrealChatRepo::new(db.clone())
        .create(&chat)
        .await
        .unwrap();
    chat
}

/// Add a message with an explicit `created_at` (so tests control the message
/// clock) and status; `build()` would otherwise stamp `created_at = now`.
async fn add_message(
    db: &Surreal<Db>,
    chat_id: &str,
    role: MessageRole,
    content: &str,
    created: DateTime<Utc>,
    status: Option<MessageStatus>,
) -> String {
    let mut m = Message::builder(chat_id, role, content.into()).build();
    m.created_at = created;
    m.status = status;
    let id = m.id.clone();
    SurrealMessageRepo::new(db.clone())
        .create(&m)
        .await
        .unwrap();
    id
}

async fn run_sweep(ctx: &Ctx) {
    ctx.pkm
        .run_consolidation_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();
}

/// One full sweep pass for a single new `services/<name>` page:
/// extract → classify (classify) → reconcile → author.
///
/// `classify_entitys` is how many concept pages the Classify stage will type this pass - one
/// model call each. On the first pass that is two (`people/me`, seeded by
/// `ensure_self_entity`, plus `services/<name>`); on a later pass over an already-consolidated
/// vault it is one. Both classifications answer with the same **standard** class, which
/// leaves no undeclared terms and so skips the adjudicate call entirely.
fn consolidate_page_responses(
    name: &str,
    fact: &str,
    classify_entitys: usize,
    source_message: &str,
) -> Vec<MockResponse> {
    let quote = fact.split_whitespace().last().unwrap_or(fact);
    let extract: Value = json!({
        "new_entities": [{"id":"fixture-page-1",
            "path": format!("services/{name}"), "kind": "service", "name": name,
            "description": "svc",
            "sources": [{"message": source_message, "quote": name, "strength": "explicit"}]
        }],
        "memories": [{"kind": "fact", "content": fact, "entities": [format!("services/{name}")],
            "sources": [{"message": source_message, "quote": quote, "strength": "explicit"}]}]
    });
    let mut out = vec![MockResponse::ToolCalls(vec![(
        "e".into(),
        "submit".into(),
        extract,
    )])];
    for i in 0..classify_entitys {
        let (entity_name, description) = if i + 1 == classify_entitys {
            (name, "svc")
        } else {
            ("Casey Owner", "Account owner")
        };
        let classify = json!({"entity":{"name":entity_name,"description":description,"aliases":[]},
            "classes": [{"class": "schema:SoftwareApplication"}], "relations": []});
        out.push(MockResponse::ToolCalls(vec![(
            format!("k{i}"),
            "submit".into(),
            classify,
        )]));
    }
    out.push(MockResponse::ToolCalls(vec![(
        "r".into(),
        "submit".into(),
        json!({
            "name": name,
            "description": "svc",
            "relations": [],
            "entity_relations": [],
            "outdated": [],
            "attributes": {},
            "attribute_sources": [],
            "moves": []
        }),
    )]));
    out.push(MockResponse::Text(format!("{name} details.")));
    out
}

#[tokio::test]
async fn sweep_consolidates_an_idle_chat_once_and_persists_the_pass() {
    let mock = Arc::new(MockModelProvider::new(consolidate_page_responses(
        "postgres",
        "Dev Postgres port is 5433",
        2, // people/me + services/postgres
        "m1",
    )));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;

    // Messages sit an hour in the past → the chat is idle on the message clock.
    let old = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "my postgres dev port is 5433",
        old,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "got it",
        old + Duration::seconds(1),
        None,
    )
    .await;

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    repo.remember("u1", &chat.id, "Postgres dev port is 5433")
        .await
        .unwrap();

    run_sweep(&ctx).await;

    let page = repo
        .entity_by_path("u1", "services/postgres")
        .await
        .unwrap();
    let paths = repo.list_all_entity_paths("u1").await.unwrap();
    let page = page.unwrap_or_else(|| {
        panic!(
            "chat consolidated into a page; paths={paths:?}; calls={}; history={:?}",
            mock.calls(),
            mock.last_history()
        )
    });
    // The Classify stage genuinely ran: the page carries the CURIE class it was typed
    // with, not the extractor's raw `"service"` label (the extractor is type-blind and
    // writes an empty kind). This is the assertion that would have caught the fixture's
    // dangling `agent_id` - with no agent row, classify failed silently and left the
    // page untyped while every other assertion in this test still passed.
    assert_eq!(
        page.kinds,
        ["https://schema.org/SoftwareApplication"],
        "the Classify stage typed the page during the sweep"
    );
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some(),
        "watermark advanced after consolidation"
    );

    // The pass persisted the effective ontology it reasoned under.
    //
    // Asserting the *outcome*, not the absence of an error, is the whole point: saving
    // it is `warn!`-and-continue, so a failure there leaves every other assertion in
    // this test passing while the stored ontology silently stays empty. That is exactly
    // how a broken seed-set query reached production.
    let stored = repo
        .ontology_get("u1")
        .await
        .unwrap()
        .expect("an ontology row exists");
    assert!(
        !stored.effective_ontology.is_empty(),
        "the effective ontology was saved, not skipped over a swallowed error"
    );
    assert!(
        stored
            .seeds
            .contains(&"https://schema.org/SoftwareApplication".to_string()),
        "and it was cut from the classes the vault actually uses: {:?}",
        stored.seeds
    );
    assert!(
        stored
            .effective_ontology
            .contains("schema.org/SoftwareApplication"),
        "the class the page was typed with is in the stored ontology"
    );
    assert!(
        repo.unconsolidated_short_memories(&chat.id)
            .await
            .unwrap()
            .is_empty(),
        "short memory marked validated (fed into the wiki)"
    );
    // The pass log counts what the pass did, once. Mining's counts are banked by the
    // sweep before the consolidator opens the record; it used to fold the same numbers in a
    // second time, so every mining pass reported double.
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .expect("the pass left a record");
    let mined = repo
        .current_memories_for_entity("u1", "services/postgres")
        .await
        .unwrap();
    assert_eq!(
        record.stats.memories_added,
        mined.len(),
        "extract's count is banked once, not twice: {:?}",
        record.stats
    );
    assert_eq!(
        record.stats.entities_created, 1,
        "and so is the page it minted"
    );
    assert_eq!(
        record.stats.entities_reconciled, 1,
        "the clean pass must reconcile its checkpoint-staged page before final commit"
    );

    let calls_after_first = mock.calls();
    assert!(
        calls_after_first >= 3,
        "extract + reconcile + author ran: {calls_after_first}"
    );

    // Second sweep: nothing new past the watermark → no-op, no new LLM calls.
    run_sweep(&ctx).await;
    assert_eq!(
        mock.calls(),
        calls_after_first,
        "second sweep is idempotent — no re-consolidation"
    );
}

#[tokio::test]
async fn in_flight_message_is_deferred_then_consolidated_after_it_completes() {
    let mut responses =
        consolidate_page_responses("postgres", "Dev Postgres port is 5433", 2, "m1");
    responses.extend(consolidate_page_responses(
        "redis",
        "Redis is on 6380",
        1,
        "m1",
    ));
    let mock = Arc::new(MockModelProvider::new(responses));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;

    // A terminal message (t1), then a still-streaming Executing message (t2 > t1).
    let t1 = Utc::now() - Duration::hours(2);
    let t2 = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "my postgres dev port is 5433",
        t1,
        Some(MessageStatus::Completed),
    )
    .await;
    let executing = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "and redis is on 6380",
        t2,
        Some(MessageStatus::Executing),
    )
    .await;
    let repo = PkmRepo::new(ctx.db.clone(), 8);

    run_sweep(&ctx).await;

    let wm = repo
        .consolidation_watermark(&chat.id)
        .await
        .unwrap()
        .expect("watermark set from the terminal prefix");
    assert!(
        wm < t2,
        "watermark holds strictly below the in-flight message ({wm} !< {t2})"
    );
    assert!(
        repo.entity_by_path("u1", "services/postgres")
            .await
            .unwrap()
            .is_some(),
        "terminal prefix consolidated"
    );
    assert!(
        repo.entity_by_path("u1", "services/redis")
            .await
            .unwrap()
            .is_none(),
        "in-flight message's content is NOT consolidated yet"
    );
    let calls_after_first = mock.calls();

    ctx.db
        .query("UPDATE type::record('message', $id) SET status = $s")
        .bind(("id", executing))
        .bind(("s", MessageStatus::Completed))
        .await
        .unwrap();

    run_sweep(&ctx).await;

    let redis = repo.entity_by_path("u1", "services/redis").await.unwrap();
    let latest = repo.latest_consolidation_record("u1").await.unwrap();
    let paths = repo.list_all_entity_paths("u1").await.unwrap();
    assert!(
        redis.is_some(),
        "the completed message is consolidated on the next sweep (deferred, not lost); \
         first_calls={calls_after_first}, calls={}, paths={paths:?}, checkpoint={latest:?}, \
         history={:?}",
        mock.calls(),
        mock.last_history()
    );
    let wm2 = repo
        .consolidation_watermark(&chat.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        wm2 > wm,
        "watermark advanced past the now-completed message"
    );
    assert!(
        mock.calls() > calls_after_first,
        "the deferred message triggered a second consolidation"
    );
}

/// Playbook Author reconstructs invocation evidence from the procedural memory's source
/// message and the durable tool-call repository.
///
/// This drives the real sweep. It fails if extraction stops preserving the source-message
/// provenance or if Playbook Author can no longer find the call attached to that message.
#[tokio::test]
async fn playbook_author_reconstructs_invocation_from_procedural_evidence() {
    const COMMAND: &str = "bash /data/agents/ops/restart-postgres.sh --force";
    const TOOL_TURN_TEXT: &str =
        "The researched restart procedure uses the recorded force command.";

    let extract = json!({
        "new_entities": [{"id":"fixture-page-2",
            "path":"services/postgres", "kind":"service", "name":"Postgres", "description":"svc",
            "sources":[{"message":"m1","quote":"postgres","strength":"explicit"}]
        }],
        "playbooks": [{
            "id":"restart", "path":"restart-postgres", "name":"Restart Postgres",
            "description":"Restart the Postgres service using the recorded command."
        }],
        "research_dispositions": [{
            "message":"m2", "result":"extracted", "reason":"The command completed.",
            "claims":[{
                "claim":"Restart Postgres with the recorded command.", "result":"extracted",
                "contribution_ids":["restart-memory"]
            }]
        }],
        "memories": [
            {"id":"restart-memory","kind":"procedural","content":"Restart postgres by running the restart script",
             "entities":["services/postgres"],
             "sources":[{"message":"m2","quote":"done","strength":"explicit"}],
             "tool_evidence":[{
                 "message":"m2","evidence_id":"m2:tool1","quote":COMMAND
             }],
             "playbook":"restart"}
        ]
    });
    let resolve = json!({"playbooks": [{
        "path":"restart-postgres", "name":"Restart Postgres",
        "description":"restart the postgres dev service when it refuses connections",
        "memory_ids":["m1"]
    }]});
    // The author receives the reconstructed invocation reference and copies the exact
    // command into the finished procedure.
    let maintain = json!({
        "name":"Restart Postgres",
        "description":"restart the postgres dev service when it refuses connections",
        "body":format!("## Steps\n1. Run `{COMMAND}`\n"),
        "related_playbooks":[]
    });
    // Unlike the other sweep tests, this one seeds a real agent (the playbook stage's
    // tool loop needs one), which makes the Classify stage run for real - so its
    // classify calls occupy queue slots too: one per dirty concept page (`people/me` and
    // `services/postgres`). Both are answered with the same standard class, which keeps
    // `undeclared_terms` empty and so skips adjudicate entirely.
    let classify_self = json!({"entity":{"name":"Casey Owner","description":"Account owner","aliases":[]},
        "classes": [{"class": "schema:SoftwareApplication"}], "relations": []});
    let classify_postgres = json!({"entity":{"name":"Postgres","description":"svc","aliases":[]},
        "classes": [{"class": "schema:SoftwareApplication"}], "relations": []});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"restart postgres force"
            }),
        )]),
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("k1".into(), "submit".into(), classify_self)]),
        MockResponse::ToolCalls(vec![("k2".into(), "submit".into(), classify_postgres)]),
        MockResponse::ToolCalls(vec![(
            "r".into(),
            "submit".into(),
            json!({
                "name":"Postgres", "description":"svc", "relations":[],
                "entity_relations":[], "outdated":[], "attributes":{},
                "attribute_sources":[], "moves":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![("pr".into(), "submit".into(), resolve)]),
        // Named `submit` deliberately: the playbook stage uses a real tool loop that
        // dispatches on the tool name. A wrong name reproduces the "no playbook page"
        // failure.
        MockResponse::ToolCalls(vec![("m".into(), "submit".into(), maintain)]),
        MockResponse::Text("Postgres details.".into()),
        MockResponse::Text("Casey Owner details.".into()),
    ]));

    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let old = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "restart postgres by running the restart script",
        old,
        None,
    )
    .await;
    let agent_at = old + Duration::seconds(1);
    let msg_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "done",
        agent_at,
        None,
    )
    .await;

    // A successful, path-bearing `shell` call attached to the procedural memory's source
    // agent message. Playbook Author reconstructs this evidence from the repositories.
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "tc1".into(),
            chat_id: chat.id.clone(),
            message_id: msg_id,
            turn: 0,
            provider_call_id: String::new(),
            name: "shell".into(),
            arguments: json!({ "command": COMMAND }),
            result: "ok".into(),
            success: true,
            duration_ms: 12,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: Some(TOOL_TURN_TEXT.into()),
            turn_reasoning: None,
            created_at: agent_at,
        })
        .await
        .unwrap();

    run_sweep(&ctx).await;

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let all = repo.list_all_entity_paths("u1").await.unwrap();
    let playbook = repo
        .entity_by_path("u1", "restart-postgres")
        .await
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "the sweep's consolidation built no playbook page — pages present: {all:?} \
                 (llm calls: {})",
                mock.calls(),
            )
        });
    assert!(
        playbook.body.contains(COMMAND),
        "the recorded command must reach the playbook body verbatim through source-message \
         reconstruction.\nbody:\n{}",
        playbook.body
    );
    assert!(
        !playbook.body.contains("[tc1]"),
        "the `[id]` citation token is substituted, not left in the prose:\n{}",
        playbook.body
    );
    let author_request = mock
        .histories()
        .into_iter()
        .map(|history| format!("{history:#?}"))
        .find(|history| history.contains("SOURCE TRANSCRIPT WINDOWS"))
        .expect("Playbook Author request");
    assert!(author_request.contains(TOOL_TURN_TEXT), "{author_request}");

    // A playbook page is a procedure, not an ontology individual: no classes, and no
    // attributes ever. That makes this the pass that exercises the seed set against a
    // page with nothing on it - the shape that broke in production, where reading
    // attribute keys blew up on the first page that had none and took the whole
    // effective ontology down with it.
    assert!(
        playbook.attributes.as_object().is_none_or(|m| m.is_empty()),
        "no attributes"
    );
    let stored = repo
        .ontology_get("u1")
        .await
        .unwrap()
        .expect("an ontology row exists");
    assert!(
        !stored.effective_ontology.is_empty(),
        "the effective ontology survived a page with no attributes — saving it is \
         warn!-and-continue, so a failure here is silent everywhere else"
    );
}

/// Diagnostic extract-only sweeps commit mined rows and watermarks but never enter the
/// user-scoped Classify, Reconcile, and Page Author pipeline.
#[tokio::test]
async fn ingest_only_sweep_stops_before_classify() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-3",
            "path":"services/postgres", "name":"Postgres", "description":"database",
            "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}]
        }],
        "existing_entity_updates": [],
        "memories": [{
            "kind":"fact",
            "content":"Postgres is a database",
            "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres is a database","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("extract".into(), "submit".into(), extract),
    ])]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 1, "only Extract called the model");
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert!(
        repo.entity_by_path("u1", "services/postgres")
            .await
            .unwrap()
            .is_none(),
        "Extract must not materialize a page"
    );
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("extract-only checkpoint advanced past ingest")
    };
    assert!(
        working_entity(&repo, &record, "services/postgres")
            .await
            .is_some()
    );
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );
}

/// An Agent answer copied from a successful foreground memory lookup is recall context,
/// not a new assertion. Extract corrects the proposal to empty, commits the watermark,
/// and leaves no pending pages for Classify.
#[tokio::test]
async fn ingest_omits_agent_answer_grounded_in_prior_recall() {
    let recalled = json!({
        "new_entities": [{"id":"fixture-page-4",
            "path":"people/casey-owner", "name":"Casey Owner", "description":"Casey Owner has phone number 555-0100",
            "sources":[{"message":"m1","quote":"Casey Owner's phone number is 555-0100","strength":"explicit"}],
            "candidate_attributes":[{
                "key":"phone number", "value":"555-0100",
                "sources":[{"message":"m1","quote":"phone number is 555-0100","strength":"explicit"}]
            }]
        }], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Casey Owner's phone number is 555-0100.",
            "entities":["people/casey-owner"],
            "sources":[{"message":"m1","quote":"phone number is 555-0100","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), recalled)]),
        MockResponse::ToolCalls(vec![(
            "drop".into(),
            "submit".into(),
            json!({
                "new_entities":[], "existing_entity_updates":[], "playbooks":[], "memories":[]
            }),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Casey Owner's phone number is 555-0100.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "recall-search".into(),
            chat_id: chat.id.clone(),
            message_id: message_id.clone(),
            turn: 1,
            provider_call_id: "provider-recall".into(),
            name: "memory_search".into(),
            arguments: json!({"query":"Casey Owner phone number"}),
            result: "Casey Owner — phone number is 555-0100".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();
    let page_path = ctx
        .pkm
        .storage()
        .vault_scope(frona::handle!("testuser"), "Memory")
        .unwrap()
        .abs_page_file("people/casey-owner");
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "recall-read".into(),
            chat_id: chat.id.clone(),
            message_id: message_id.clone(),
            turn: 2,
            provider_call_id: "provider-read".into(),
            name: "read".into(),
            arguments: json!({"path":page_path}),
            result: "# Casey Owner\nCasey Owner's phone number is 555-0100.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert!(repo.list_all_memories("u1").await.unwrap().is_empty());
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("extract-only checkpoint advanced past ingest")
    };
    assert!(
        working_entity(&repo, &record, "people/casey-owner")
            .await
            .is_none()
    );
    assert_eq!(record.stats.grounding_corrections, 1);
    assert!(record.stats.agent_evidence_no_tool_drops >= 1);
    assert_eq!(
        mock.calls(),
        2,
        "recall-only Agent memories receive one correction turn"
    );

    let histories = mock.histories();
    let request = format!("{:?}", histories.first().expect("initial Extract request"));
    assert!(request.contains("Recall calls for m1"));
    assert!(request.contains("[T1] keyword search"));
    assert!(request.contains("Casey Owner phone number"));
    assert!(request.contains("[T2] page path"));
    assert!(request.contains("people/casey-owner.md"));
    assert!(!request.contains("result preview"));
    assert!(!request.contains("555-0100\n  result"));
    let tool_names = mock
        .tool_histories()
        .into_iter()
        .flatten()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(tool_names.iter().any(|name| name == "read_recall_result"));
}

#[tokio::test]
async fn ingest_persists_agent_memory_with_durable_web_evidence() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-5",
            "path":"products/acme-4-2", "name":"Acme 4.2", "description":"An Acme release",
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Acme released version 4.2.",
            "entities":["products/acme-4-2"],
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Acme released version 4.2"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Research complete.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "web-release".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-web".into(),
            name: "web_search".into(),
            arguments: json!({"query":"Acme 4.2 release"}),
            result: "Acme released version 4.2. https://acme.example/releases/4.2".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: Some("Acme released version 4.2.".into()),
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let initial_request = format!("{:#?}", mock.histories().first().expect("Extract request"));
    assert!(
        initial_request.contains("Acme released version 4.2"),
        "{initial_request}"
    );
    assert!(
        initial_request.contains("Research complete"),
        "{initial_request}"
    );

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert!(
        memories[0].evidence.iter().any(|item| matches!(
            &item.source,
            frona::memory::pkm::model::EvidenceSource::WebSearch { tool_call_id, query, url, .. }
                if tool_call_id == "web-release"
                    && query.as_deref() == Some("Acme 4.2 release")
                    && url.is_none()
        )),
        "persisted evidence: {:?}",
        memories[0].evidence
    );
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.agent_evidence_strong_matches, 1);
}

#[tokio::test]
async fn ingest_retains_accepted_tool_evidence_when_a_later_correction_omits_that_memory() {
    let initial = json!({
        "new_entities": [{"id":"fixture-page-6",
            "path":"products/acme-4-2", "name":"Acme 4.2", "description":"An Acme release",
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [{
            "message":"m1", "result":"extracted", "reason":"Release fact retained.",
            "claims":[{
                "claim":"Acme released version 4.2.", "result":"extracted",
                "contribution_ids":["release"]
            }]
        }],
        "memories": [
            {
                "id":"release", "kind":"fact", "content":"Acme released version 4.2.",
                "entities":["products/acme-4-2"],
                "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}],
                "tool_evidence":[{"message":"m1","evidence_id":"e1:chunk1","quote":"Acme released version 4.2"}]
            },
            {
                "id":"budget", "kind":"fact", "content":"My budget is $20,000",
                "entities":["people/me"],
                "sources":[{"message":"m2","quote":"My budget is 20k","strength":"explicit"}]
            }
        ]
    });
    let correction = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "id":"budget", "kind":"fact", "content":"My budget is 20k",
            "entities":["people/me"],
            "sources":[{"message":"m2","quote":"My budget is 20k","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Acme released version 4.2"
            }),
        )]),
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), initial)]),
        MockResponse::ToolCalls(vec![("corrected".into(), "submit".into(), correction)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let agent_message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Acme released version 4.2.",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "My budget is 20k",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "web-release-stable".into(),
            chat_id: chat.id.clone(),
            message_id: agent_message_id,
            turn: 1,
            provider_call_id: "provider-web-stable".into(),
            name: "web_search".into(),
            arguments: json!({"query":"Acme 4.2 release"}),
            result: "Acme released version 4.2. https://acme.example/releases/4.2".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let correction_request = format!("{:#?}", mock.last_history());
    assert_eq!(mock.calls(), 3, "history: {correction_request}");
    assert!(
        correction_request.contains("`release` -> e1:chunk1, m1"),
        "accepted memory feedback must keep its stable evidence references: {correction_request}"
    );
    assert!(
        correction_request.contains("`budget` -> m2"),
        "repair feedback must show the current evidence reference: {correction_request}"
    );
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 2);
    let release = memories
        .iter()
        .find(|memory| memory.content.contains("version 4.2"))
        .expect("accepted release memory");
    assert!(release.evidence.iter().any(|item| matches!(
        &item.source,
        frona::memory::pkm::model::EvidenceSource::WebSearch { tool_call_id, .. }
            if tool_call_id == "web-release-stable"
    )));
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn ingest_repairs_unaccounted_research_by_appending_a_grounded_memory() {
    let initial = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [],
        "memories": [{
            "id":"comparison", "kind":"fact", "content":"Casey Owner is comparing AI accelerators.",
            "entities":["people/me"],
            "sources":[{"message":"m1","quote":"I am comparing AI accelerators","strength":"explicit"}]
        }]
    });
    let repaired = json!({
        "new_entities": [{"id":"fixture-page-7",
            "path":"hardware/compute-box", "name":"Compute Box",
            "description":"An AI computer", "aliases":[],
            "sources":[{"message":"m2","quote":"Compute Box","strength":"explicit"}],
            "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [{
            "message":"m2", "result":"extracted", "reason":"A durable hardware specification was found.",
            "claims":[{
                "claim":"Compute Box has 64 GB of unified memory.", "result":"extracted",
                "contribution_ids":["compute-box-memory"]
            }]
        }],
        "memories": [
            {
                "id":"comparison", "kind":"fact", "content":"Casey Owner is comparing AI accelerators.",
                "entities":["people/me"],
                "sources":[{"message":"m1","quote":"I am comparing AI accelerators","strength":"explicit"}]
            },
            {
                "id":"compute-box-memory", "kind":"fact", "content":"Compute Box has 64 GB of unified memory.",
                "entities":["hardware/compute-box"],
                "sources":[{"message":"m2","quote":"Compute Box has 64 GB of unified memory","strength":"explicit"}],
                "tool_evidence":[{"message":"m2","evidence_id":"e1:chunk1","quote":"Compute Box has 64 GB of unified memory"}]
            }
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), initial)]),
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"Compute Box 64 GB unified memory"
            }),
        )]),
        MockResponse::ToolCalls(vec![("repair".into(), "submit".into(), repaired)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "I am comparing AI accelerators.",
        at,
        None,
    )
    .await;
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Compute Box has 64 GB of unified memory.",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "compute-box-spec".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-compute-box".into(),
            name: "web_fetch".into(),
            arguments: json!({"url":"https://example.test/compute-box"}),
            result: "Compute Box has 64 GB of unified memory.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at + Duration::seconds(1),
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        3,
        "unaccounted research must receive a repair turn"
    );
    let initial_request = format!("{:#?}", mock.histories().first().unwrap());
    assert!(
        initial_request.contains("Research messages with successful non-recall tool executions")
    );
    assert!(
        initial_request.contains("`m2`"),
        "research ledger missing from initial request: {initial_request}"
    );
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        2,
        "repair appends research without losing accepted memory"
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content.contains("64 GB"))
    );
    let stats = PkmRepo::new(ctx.db.clone(), 8)
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap()
        .stats
        .research_coverage;
    assert_eq!(stats.messages, 1);
    assert_eq!(stats.extracted, 1);
    assert_eq!(stats.memories_added_by_repair, 1);
    let feedback = format!("{:#?}", mock.last_history());
    assert!(
        feedback.contains("research_message_unaccounted"),
        "history={feedback}"
    );
}

#[tokio::test]
async fn ingest_rebinds_a_unique_agent_quote_to_its_actual_message() {
    let extract = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [
            {
                "message":"m1", "result":"extracted", "reason":"Compute Box specification",
                "claims":[{
                    "claim":"Compute Box has 64 GB of unified memory.", "result":"extracted",
                    "contribution_ids":["compute-box-memory"]
                }]
            },
            {
                "message":"m2", "result":"no_durable_claim",
                "reason":"The comparison-complete statement adds no durable fact.",
                "claims":[{
                    "claim":"The comparison is complete.", "result":"no_durable_claim",
                    "reason":"This is only task progress."
                }]
            }
        ],
        "memories": [{
            "id":"compute-box-memory", "kind":"fact", "content":"Compute Box has 64 GB of unified memory.",
            "entities":["hardware/compute-box"],
            "sources":[{"message":"m2","quote":"Compute Box has 64 GB of unified memory","strength":"explicit"}],
            "tool_evidence":[{"message":"m2","evidence_id":"e1:chunk1","quote":"Compute Box has 64 GB of unified memory"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"Compute Box 64 GB unified memory"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let research_message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Compute Box has 64 GB of unified memory.",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "The comparison is complete.",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "compute-box-source".into(),
            chat_id: chat.id.clone(),
            message_id: research_message_id.clone(),
            turn: 1,
            provider_call_id: "provider-compute-box-source".into(),
            name: "web_fetch".into(),
            arguments: json!({"url":"https://example.test/compute-box"}),
            result: "Compute Box has 64 GB of unified memory.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        2,
        "a unique source-handle error is repaired without another model turn; history={:#?}",
        mock.last_history()
    );
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|evidence| matches!(
        &evidence.source,
        frona::memory::pkm::model::EvidenceSource::AgentMessage { message_id, .. }
            if message_id == &research_message_id
    )));
    let stats = PkmRepo::new(ctx.db.clone(), 8)
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap()
        .stats
        .research_coverage;
    assert_eq!(stats.citation_repairs, 1);
}

#[tokio::test]
async fn ingest_can_split_a_mixed_research_claim_without_losing_supported_facts() {
    let initial = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [],
        "memories": [{
            "kind":"fact",
            "content":"Compute Box has 64 GB, Accelerator A has 32 GB, and Accelerator B has 48 GB.",
            "entities":["hardware/compute-box","hardware/accelerator-a","hardware/accelerator-b"],
            "sources":[{"message":"m1","quote":"Compute Box has 64 GB, Accelerator A has 32 GB, and Accelerator B has 48 GB","strength":"explicit"}],
            "tool_evidence":[
                {"message":"m1","evidence_id":"e1:chunk1","quote":"Compute Box has 64 GB"},
                {"message":"m1","evidence_id":"e2:chunk1","quote":"Accelerator A has 32 GB"}
            ]
        }]
    });
    let repaired = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "research_dispositions": [],
        "memories": [
            {
                "kind":"fact", "content":"Compute Box has 64 GB.",
                "entities":["hardware/compute-box"],
                "sources":[{"message":"m1","quote":"Compute Box has 64 GB","strength":"explicit"}],
                "tool_evidence":[{"message":"m1","evidence_id":"e1:chunk1","quote":"Compute Box has 64 GB"}]
            },
            {
                "kind":"fact", "content":"Accelerator A has 32 GB.",
                "entities":["hardware/accelerator-a"],
                "sources":[{"message":"m1","quote":"Accelerator A has 32 GB","strength":"explicit"}],
                "tool_evidence":[{"message":"m1","evidence_id":"e2:chunk1","quote":"Accelerator A has 32 GB"}]
            }
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Compute Box 64 GB Accelerator A 32 GB Accelerator B 48 GB"
            }),
        )]),
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), initial)]),
        MockResponse::ToolCalls(vec![("split".into(), "submit".into(), repaired)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Compute Box has 64 GB, Accelerator A has 32 GB, and Accelerator B has 48 GB.",
        at,
        None,
    )
    .await;
    for (id, turn, result) in [
        ("a-compute-box", 1, "Compute Box has 64 GB."),
        ("b-accelerator-a", 2, "Accelerator A has 32 GB."),
    ] {
        SurrealToolCallRepo::new(ctx.db.clone())
            .create(&ToolCall {
                id: id.into(),
                chat_id: chat.id.clone(),
                message_id: message_id.clone(),
                turn,
                provider_call_id: format!("provider-{id}"),
                name: "web_fetch".into(),
                arguments: json!({"url":format!("https://example.test/{id}")}),
                result: result.into(),
                success: true,
                duration_ms: 2,
                hitl: None,
                task_event: None,
                system_prompt: None,
                description: None,
                turn_text: None,
                turn_reasoning: None,
                created_at: at + Duration::milliseconds(i64::from(turn)),
            })
            .await
            .unwrap();
    }

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        2,
        "one rejected mixed claim can become two supported memories"
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content == "Compute Box has 64 GB.")
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content == "Accelerator A has 32 GB.")
    );
    assert!(
        !memories
            .iter()
            .any(|memory| memory.content.contains("48 GB"))
    );
    let stats = PkmRepo::new(ctx.db.clone(), 8)
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap()
        .stats
        .research_coverage;
    assert_eq!(stats.mixed_claim_splits, 1);
}

#[tokio::test]
async fn ingest_returns_all_missing_critical_values_in_one_grounding_feedback() {
    let submission = |content: &str| {
        json!({
            "new_entities": [{"id":"fixture-page-8",
                "path":"routes/sfo-sea", "name":"SFO to SEA", "description":"A flight route",
                "sources":[{"message":"m1","quote":"Flights","strength":"explicit"}],
                "aliases":[], "candidate_attributes":[]
            }],
            "existing_entity_updates": [], "playbooks": [],
            "research_dispositions": [{
                "message":"m1", "result":"extracted", "reason":"Flight schedule retained.",
                "claims":[{
                    "claim":content, "result":"extracted", "contribution_ids":["flights"]
                }]
            }],
            "memories": [{
                "id":"flights", "kind":"fact", "content":content, "entities":["routes/sfo-sea"],
                "sources":[{
                    "message":"m1", "quote":"Flights EX101 and EX202 depart at 9:00 AM",
                    "strength":"explicit"
                }],
                "tool_evidence":[{
                    "message":"m1", "evidence_id":"m1:tool1",
                    "quote":"EXA101 and EXA202 depart at 09:00AM"
                }]
            }]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"SFO SEA flights"
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "invalid".into(),
            "submit".into(),
            submission("Flights EX101 and EX202 depart at 9:00 AM."),
        )]),
        MockResponse::ToolCalls(vec![(
            "corrected".into(),
            "submit".into(),
            submission("Flights EXA101 and EXA202 depart at 09:00AM."),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Flights EX101 and EX202 depart at 9:00 AM.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "flight-search".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-flight-search".into(),
            name: "web_search".into(),
            arguments: json!({"query":"SFO SEA flights"}),
            result: "EXA101 and EXA202 depart at 09:00AM.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 3);
    let histories = mock.histories();
    let correction = format!("{:?}", histories.get(2).expect("corrected Extract request"));
    assert!(
        correction.contains("missing critical values"),
        "{correction}"
    );
    assert!(correction.contains("EX101"), "{correction}");
    assert!(correction.contains("EX202"), "{correction}");
    assert!(
        correction.contains("comparison ignores case, spaces, and punctuation"),
        "{correction}"
    );
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(
        memories[0].content,
        "Flights EXA101 and EXA202 depart at 09:00AM."
    );
}

#[tokio::test]
async fn ingest_persists_agent_memory_with_successful_curl_web_page_evidence() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-9",
            "path":"websites/example-domain", "name":"Example Domain",
            "description":"A domain used in documentation examples",
            "sources":[{"message":"m1","quote":"Example Domain","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Example Domain is for use in documentation examples.",
            "entities":["websites/example-domain"],
            "sources":[{
                "message":"m1", "quote":"Example Domain is for use in documentation examples",
                "strength":"explicit"
            }]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Example Domain documentation examples"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Example Domain is for use in documentation examples.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "curl-example".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-shell".into(),
            name: "shell".into(),
            arguments: json!({"command":"curl https://example.com"}),
            result: "Example Domain is for use in documentation examples.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert!(
        memories[0].evidence.iter().any(|item| matches!(
            &item.source,
            frona::memory::pkm::model::EvidenceSource::WebPage { tool_call_id, url, .. }
                if tool_call_id == "curl-example"
                    && url.as_deref() == Some("https://example.com")
        )),
        "persisted evidence: {:?}",
        memories[0].evidence
    );
    assert_eq!(
        repo.latest_consolidation_record("u1")
            .await
            .unwrap()
            .unwrap()
            .stats
            .agent_evidence_strong_matches,
        1
    );
}

#[tokio::test]
async fn ingest_keeps_a_procedure_with_two_citations_from_one_agent_message() {
    let extract = json!({
        "new_entities": [], "existing_entity_updates": [],
        "playbooks": [{
            "id":"restart", "path":"procedures/restart-postgres",
            "name":"Restart Postgres", "description":"Restart Postgres safely"
        }],
        "memories": [{
            "kind":"procedural", "content":"Stop Postgres, then start Postgres.",
            "entities":["services/postgres"], "playbook":"restart",
            "sources":[
                {"message":"m1","quote":"Stop Postgres","strength":"explicit"},
                {"message":"m1","quote":"start Postgres","strength":"explicit"}
            ]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Stop Postgres then start Postgres"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
    ]));
    let ctx = setup(mock).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Stop Postgres, then start Postgres.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "restart-result".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-restart".into(),
            name: "shell".into(),
            arguments: json!({"command":"service postgres restart"}),
            result: "Stop Postgres, then start Postgres.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(
        memories.len(),
        1,
        "two citations from one Agent message must remain one valid assertion source"
    );
    assert_eq!(
        memories[0].kind,
        frona::memory::pkm::model::MemoryKind::Procedural
    );
}

#[tokio::test]
async fn ingest_drops_agent_memory_when_the_only_execution_failed() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-10",
            "path":"securities/acme", "name":"Acme stock", "description":"Acme shares",
            "sources":[{"message":"m1","quote":"Acme closed at $42","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Acme closed at $42.", "entities":["securities/acme"],
            "sources":[{"message":"m1","quote":"Acme closed at $42","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Acme closed at 42"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "drop".into(),
            "submit".into(),
            json!({
                "new_entities":[], "existing_entity_updates":[], "playbooks":[], "memories":[]
            }),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Acme closed at $42.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "failed-price".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-failed".into(),
            name: "python".into(),
            arguments: json!({"code":"fetch_price('ACME')"}),
            result: "network timeout".into(),
            success: false,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert!(repo.list_all_memories("u1").await.unwrap().is_empty());
    assert!(
        repo.latest_consolidation_record("u1")
            .await
            .unwrap()
            .unwrap()
            .stats
            .agent_evidence_no_tool_drops
            >= 1
    );
    assert_eq!(
        mock.calls(),
        3,
        "the failed execution search returns no admissible evidence and Extract corrects the memory"
    );
    assert!(format!("{:#?}", mock.last_history()).contains("agent_claim_without_tool_evidence"));
}

#[tokio::test]
async fn ingest_requires_a_scheduled_task_handle_to_copy_its_event_time() {
    let missing_date = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"episodic", "content":"A hydration reminder for 08:00 was scheduled.",
            "entities":["people/me"],
            "episode":{"status":"planned","anchor":{"message":"m2","quote":""}},
            "sources":[{"message":"m2","quote":"","strength":"explicit"}]
        }]
    });
    let corrected_date = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"episodic", "content":"A hydration reminder for 08:00 was scheduled.",
            "entities":["people/me"],
            "episode":{
                "status":"planned",
                "anchor":{"message":"m2","quote":""},
                "absolute":{"year":2026,"month":7,"day":18,"hour":20,"minute":48}
            },
            "sources":[{"message":"m2","quote":"","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("extract-1".into(), "submit".into(), missing_date)]),
        MockResponse::ToolCalls(vec![("extract-2".into(), "submit".into(), corrected_date)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = DateTime::parse_from_rfc3339("2026-07-18T20:48:27Z")
        .unwrap()
        .with_timezone(&Utc);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "The reminder is scheduled.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "schedule-reminder".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-schedule".into(),
            name: "create_recurring_task".into(),
            arguments: json!({"title":"Drink water at 08:00"}),
            result: json!({"task_id":"task-hydration"}).to_string(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at + Duration::milliseconds(1),
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        2,
        "the missing task date should require one correction"
    );
    assert!(format!("{:#?}", mock.histories()).contains("task_episode_missing_absolute_time"));
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        &item.source,
        frona::memory::pkm::model::EvidenceSource::TaskLifecycle { task_id, .. }
            if task_id == "task-hydration"
    )));
    assert_eq!(
        memories[0]
            .episode
            .as_ref()
            .and_then(|episode| episode.absolute.as_ref())
            .and_then(|absolute| absolute.minute),
        Some(48)
    );
}

#[tokio::test]
async fn ingest_resume_commits_tool_evidence_checkpoint_and_watermark_once() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-11",
            "path":"products/acme-4-2", "name":"Acme 4.2", "description":"An Acme release",
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Acme released version 4.2.",
            "entities":["products/acme-4-2"],
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::Pending]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Acme released version 4.2.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "web-release-resume".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-web-resume".into(),
            name: "web_search".into(),
            arguments: json!({"query":"Acme 4.2 release"}),
            result: "Acme released version 4.2. https://acme.example/releases/4.2".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            ctx.pkm.run_extraction_sweep(
                &ctx.state.chat_service,
                &ctx.state.contact_service,
                &ctx.state.agent_service,
                &ctx.harness,
            ),
        )
        .await
        .is_err()
    );

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert!(repo.list_all_memories("u1").await.unwrap().is_empty());
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_none()
    );

    mock.enqueue(MockResponse::ToolCalls(vec![(
        "search-resume".into(),
        "search_tool_evidence".into(),
        json!({
            "message_id":"m1", "query":"Acme released version 4.2"
        }),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "extract-resume".into(),
        "submit".into(),
        extract,
    )]));
    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        &item.source,
        frona::memory::pkm::model::EvidenceSource::WebSearch { tool_call_id, .. }
            if tool_call_id == "web-release-resume"
    )));
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );

    let calls = mock.calls();
    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();
    assert_eq!(mock.calls(), calls);
    assert_eq!(
        repo.list_all_memories("u1").await.unwrap().len(),
        1,
        "a committed evidence-bearing window is not duplicated on resume"
    );
}

#[tokio::test]
async fn ingest_accepts_multilingual_user_confirmation_without_tool_evidence() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-12",
            "path":"places/exampletown", "name":"Exampletown", "description":"The user's home",
            "sources":[{"message":"m1","quote":"live in Exampletown","strength":"explicit"}],
            "aliases":[], "candidate_attributes":[]
        }],
        "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"identity", "content":"The user lives in Exampletown.", "entities":["places/exampletown"],
            "sources":[
                {"message":"m1","quote":"live in Exampletown","strength":"explicit"},
                {"message":"m2","quote":"Sí, eso es correcto","strength":"explicit","confirmation":true}
            ]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("extract".into(), "submit".into(), extract),
    ])]));
    let ctx = setup(mock).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "You live in Exampletown.",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Sí, eso es correcto.",
        at + Duration::seconds(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        item.source,
        frona::memory::pkm::model::EvidenceSource::UserConfirmation { .. }
    )));
}

#[tokio::test]
async fn ingest_treats_resolved_hitl_text_as_user_confirmation() {
    let extract = json!({
        "new_entities":[{"id":"fixture-page-13",
            "path":"services/acme","name":"Acme","description":"A service",
            "sources":[{"message":"m1","quote":"Acme is the production service","strength":"explicit"}],
            "aliases":[],"candidate_attributes":[]
        }],"existing_entity_updates":[],"playbooks":[],
        "memories":[{
            "kind":"fact","content":"Acme is the production service.","entities":["services/acme"],
            "sources":[
                {"message":"m1","quote":"Acme is the production service","strength":"explicit"},
                {"message":"m2","quote":"Sí, correcto","strength":"explicit","confirmation":true}
            ]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("extract".into(), "submit".into(), extract),
    ])]));
    let ctx = setup(mock).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    let message_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Acme is the production service.",
        at,
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "hitl-confirm".into(),
            chat_id: chat.id.clone(),
            message_id,
            turn: 1,
            provider_call_id: "provider-hitl".into(),
            name: "ask_user_question".into(),
            arguments: json!({}),
            result: "Sí, correcto".into(),
            success: true,
            duration_ms: 2,
            hitl: Some(frona::inference::hitl::Hitl {
                prompt: "Is Acme the production service?".into(),
                url: String::new(),
                request: frona::inference::hitl::HitlRequest::Question { options: vec![] },
                status: frona::inference::tool_call::ToolStatus::Resolved,
                response: Some(frona::inference::hitl::HitlResponse::Choice(
                    "Sí, correcto".into(),
                )),
                delivery: None,
            }),
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        item.source,
        frona::memory::pkm::model::EvidenceSource::UserConfirmation { .. }
    )));
}

#[tokio::test]
async fn ingest_uses_tool_evidence_from_a_previous_parallel_window() {
    let empty = json!({
        "new_entities":[],"existing_entity_updates":[],"playbooks":[],"memories":[],
        "research_dispositions":[{
            "message":"m1", "result":"no_durable_claim",
            "reason":"The Agent only states that it checked the feed; the result appears later.",
            "claims":[{
                "claim":"The release feed was checked.", "result":"no_durable_claim",
                "reason":"A check without a result is not durable knowledge."
            }]
        }]
    });
    let grounded = json!({
        "new_entities":[{"id":"fixture-page-14",
            "path":"products/acme-4-2","name":"Acme 4.2","description":"An Acme release",
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}],
            "aliases":[],"candidate_attributes":[]
        }],
        "existing_entity_updates":[],"playbooks":[],
        "research_dispositions":[{
            "message":"m1", "result":"extracted", "reason":"Release retained.",
            "claims":[{
                "claim":"Acme released version 4.2.", "result":"extracted",
                "contribution_ids":["release-memory"]
            }]
        }],
        "memories":[{
            "id":"release-memory","kind":"fact","content":"Acme released version 4.2.","entities":["products/acme-4-2"],
            "sources":[{"message":"m1","quote":"Acme released version 4.2","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("first".into(), "submit".into(), empty)]),
        MockResponse::ToolCalls(vec![(
            "search-second".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m1", "query":"Acme released version 4.2"
            }),
        )]),
        MockResponse::ToolCalls(vec![("second".into(), "submit".into(), grounded)]),
    ]));
    let ctx = setup_with_memory_config(
        mock,
        MemoryConfig {
            pkm_extract_max_messages: 1,
            pkm_consolidation_concurrency: 1,
            ..MemoryConfig::default()
        },
    )
    .await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(2);
    let first_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "I checked the Acme release feed.",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Acme released version 4.2.",
        at + Duration::minutes(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "prior-web".into(),
            chat_id: chat.id.clone(),
            message_id: first_id,
            turn: 1,
            provider_call_id: "provider-prior".into(),
            name: "web_fetch".into(),
            arguments: json!({"url":"https://acme.example/releases"}),
            result: "Acme released version 4.2.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        &item.source,
        frona::memory::pkm::model::EvidenceSource::WebPage { tool_call_id, .. }
            if tool_call_id == "prior-web"
    )));
}

#[tokio::test]
async fn ingest_revises_a_full_batch_to_cite_structured_tool_evidence() {
    let submission = |with_support: bool| {
        let agent_sources = vec![json!({
            "message":"m2","quote":"production deployment succeeded","strength":"derived"
        })];
        let tool_evidence = if with_support {
            vec![json!({
                "message":"m2",
                "evidence_id":"m2:tool1",
                "quote":"environment=prod status=green failed_checks=0",
            })]
        } else {
            Vec::new()
        };
        json!({
            "new_entities":[{"id":"fixture-page-15",
                "path":"services/postgres","name":"Postgres","description":"A database service",
                "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
                "aliases":[],"candidate_attributes":[]
            }],
            "existing_entity_updates":[],"playbooks":[],
            "research_dispositions":[{
                "message":"m2", "result":"extracted", "reason":"Deployment result retained.",
                "claims":[{
                    "claim":"The production deployment succeeded.", "result":"extracted",
                    "contribution_ids":["deployment"]
                }]
            }],
            "memories":[
                {"id":"postgres-user","kind":"fact","content":"The user uses Postgres.","entities":["services/postgres"],
                 "sources":[{"message":"m1","quote":"I use Postgres","strength":"explicit"}]},
                {"id":"deployment","kind":"episodic","content":"The production deployment succeeded.","entities":["services/postgres"],
                 "episode":{"status":"occurred","anchor":{"message":"m2","quote":"deployment succeeded"}},
                 "sources":agent_sources,"tool_evidence":tool_evidence}
            ]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), submission(false))]),
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"prod status green"
            }),
        )]),
        MockResponse::ToolCalls(vec![("grounded".into(), "submit".into(), submission(true))]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "I use Postgres.",
        at,
        None,
    )
    .await;
    let agent_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "The production deployment succeeded.",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "deploy-result".into(),
            chat_id: chat.id.clone(),
            message_id: agent_id,
            turn: 1,
            provider_call_id: "provider-deploy".into(),
            name: "shell".into(),
            arguments: json!({"command":"deploy prod"}),
            result: "environment=prod status=green failed_checks=0".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        3,
        "Agent evidence is searched before the corrected submission; history={:#?}",
        mock.last_history()
    );
    let tools = mock
        .tool_histories()
        .into_iter()
        .flatten()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(tools.iter().any(|name| name == "search_tool_evidence"));
    assert!(!tools.iter().any(|name| name == "read_tool_execution"));
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        2,
        "the accepted User memory survives the Agent correction"
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.evidence.iter().any(|item| matches!(
                &item.source,
                frona::memory::pkm::model::EvidenceSource::ToolResult { tool_call_id, .. }
                    if tool_call_id == "deploy-result"
            )))
    );
    let record = PkmRepo::new(ctx.db.clone(), 8)
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.agent_evidence_lookup_calls, 1);
    assert_eq!(record.stats.grounding_corrections, 1);
    assert!(record.stats.agent_evidence_fallback_retains >= 1);
}

#[tokio::test]
async fn ingest_corrects_tool_evidence_without_a_matching_agent_source() {
    let submission = |include_tool_evidence: bool| {
        let tool_evidence = if include_tool_evidence {
            vec![json!({
                "message":"m2", "evidence_id":"m2:tool1",
                "quote":"Model Alpha V1 has a mixture-of-experts architecture"
            })]
        } else {
            Vec::new()
        };
        json!({
            "new_entities":[{"id":"fixture-page-16",
                "path":"models/qwen3-235b", "name":"Model Alpha V1",
                "description":"A mixture-of-experts language model",
                "sources":[{"message":"m1","quote":"Model Alpha V1","strength":"explicit"}],
                "aliases":[], "candidate_attributes":[]
            }],
            "existing_entity_updates":[], "playbooks":[],
            "research_dispositions":[{
                "message":"m2", "result":"no_durable_claim",
                "reason":"The User already stated the durable fact.",
                "claims":[{
                    "claim":"Model Alpha V1 has a mixture-of-experts architecture.",
                    "result":"duplicate", "reason":"The User already supplied this fact."
                }]
            }],
            "memories":[{
                "id":"model-alpha-memory", "kind":"fact", "content":"Model Alpha V1 is a mixture-of-experts model.",
                "entities":["models/qwen3-235b"],
                "sources":[{
                    "message":"m1", "quote":"Model Alpha V1 is a mixture-of-experts model",
                    "strength":"explicit"
                }],
                "tool_evidence":tool_evidence
            }]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"Model Alpha V1 mixture of experts"
            }),
        )]),
        MockResponse::ToolCalls(vec![("invalid".into(), "submit".into(), submission(true))]),
        MockResponse::ToolCalls(vec![(
            "corrected".into(),
            "submit".into(),
            submission(false),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Model Alpha V1 is a mixture-of-experts model.",
        at,
        None,
    )
    .await;
    let agent_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Model Alpha V1 has a mixture-of-experts architecture.",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "model-alpha-search".into(),
            chat_id: chat.id.clone(),
            message_id: agent_id,
            turn: 1,
            provider_call_id: "provider-model-alpha".into(),
            name: "web_search".into(),
            arguments: json!({"query":"Model Alpha V1 architecture"}),
            result: "Model Alpha V1 has a mixture-of-experts architecture".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 3, "history={:#?}", mock.last_history());
    let history = format!("{:#?}", mock.last_history());
    assert!(
        history.contains("tool_evidence_without_agent_source"),
        "history={history}"
    );
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert_eq!(repo.list_all_memories("u1").await.unwrap().len(), 1);
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.grounding_corrections, 1);
}

#[tokio::test]
async fn ingest_validates_supplied_tool_evidence_when_user_evidence_is_also_present() {
    let submission = |tool_quote: &str| {
        json!({
            "new_entities":[{"id":"fixture-page-17",
                "path":"products/example-cable","name":"Example cable",
                "description":"A display cable",
                "sources":[{"message":"m1","quote":"Example cable","strength":"explicit"}],
                "aliases":[],"candidate_attributes":[]
            }],
            "existing_entity_updates":[],"playbooks":[],
            "research_dispositions":[{
                "message":"m2", "result":"extracted", "reason":"Availability retained.",
                "claims":[{
                    "claim":"Example cable is available.", "result":"extracted",
                    "contribution_ids":["example-cable-memory"]
                }]
            }],
            "memories":[{
                "id":"example-cable-memory","kind":"fact","content":"Example cable is available.",
                "entities":["products/example-cable"],
                "sources":[
                    {"message":"m1","quote":"Example cable","strength":"explicit"},
                    {"message":"m2","quote":"Example cable is available","strength":"derived"}
                ],
                "tool_evidence":[{
                    "message":"m2","evidence_id":"m2:tool1","quote":tool_quote
                }]
            }]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"Example cable available"
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "invalid".into(),
            "submit".into(),
            submission("unsupported evidence span"),
        )]),
        MockResponse::ToolCalls(vec![(
            "corrected".into(),
            "submit".into(),
            submission("Example cable is available"),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Find a Example cable.",
        at,
        None,
    )
    .await;
    let agent_id = add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "Example cable is available.",
        at + Duration::seconds(1),
        None,
    )
    .await;
    SurrealToolCallRepo::new(ctx.db.clone())
        .create(&ToolCall {
            id: "example-cable-search".into(),
            chat_id: chat.id.clone(),
            message_id: agent_id,
            turn: 1,
            provider_call_id: "provider-example-cable".into(),
            name: "web_search".into(),
            arguments: json!({"query":"Example cable"}),
            result: "Example cable is available.".into(),
            success: true,
            duration_ms: 2,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            turn_reasoning: None,
            created_at: at,
        })
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 3);
    let history = format!("{:#?}", mock.last_history());
    assert!(
        history.contains("tool_evidence_quote_not_found"),
        "history={history}"
    );
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].evidence.iter().any(|item| matches!(
        &item.source,
        frona::memory::pkm::model::EvidenceSource::WebSearch { tool_call_id, .. }
            | frona::memory::pkm::model::EvidenceSource::WebPage { tool_call_id, .. }
            | frona::memory::pkm::model::EvidenceSource::ToolResult { tool_call_id, .. }
            if tool_call_id == "example-cable-search"
    )));
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.grounding_corrections, 1);
}

#[tokio::test]
async fn ingest_returns_unsupported_agent_contributions_for_correction() {
    let extract = json!({
        "new_entities":[{"id":"fixture-page-18",
            "path":"devices/acme-router","name":"Acme Router","description":"The user's router",
            "sources":[{"message":"m1","quote":"Acme Router","strength":"explicit"}],"aliases":[],
            "candidate_attributes":[]
        }],
        "existing_entity_updates":[],"playbooks":[],
        "memories":[
            {"kind":"fact","content":"The user uses the Acme Router.","entities":["devices/acme-router"],
             "sources":[{"message":"m1","quote":"I use the Acme Router","strength":"explicit"}]},
            {"kind":"claim","content":"The user uses the Acme Router.","entities":["devices/acme-router"],
             "sources":[{"message":"m1","quote":"I use the Acme Router","strength":"explicit"}]},
            {"kind":"fact","content":"The Acme Router has a 100 Gbps uplink.","entities":["devices/acme-router"],
             "sources":[{"message":"m2","quote":"100 Gbps uplink","strength":"explicit"}]}
        ]
    });
    let corrected = json!({
        "new_entities":[{"id":"fixture-page-19",
            "path":"devices/acme-router","name":"Acme Router","description":"The user's router",
            "sources":[{"message":"m1","quote":"Acme Router","strength":"explicit"}],"aliases":[],
            "candidate_attributes":[]
        }],
        "existing_entity_updates":[],"playbooks":[],
        "memories":[
            {"kind":"fact","content":"The user uses the Acme Router.","entities":["devices/acme-router"],
             "sources":[{"message":"m1","quote":"I use the Acme Router","strength":"explicit"}]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "search".into(),
            "search_tool_evidence".into(),
            json!({
                "message_id":"m2", "query":"100 Gbps uplink"
            }),
        )]),
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("corrected".into(), "submit".into(), corrected)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "I use the Acme Router.",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::Agent,
        "It has a 100 Gbps uplink.",
        at + Duration::seconds(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        3,
        "unsupported memories must receive a correction turn; history={:#?}",
        mock.last_history()
    );
    let feedback = format!("{:#?}", mock.last_history());
    assert!(
        feedback.contains("agent_claim_without_tool_evidence"),
        "the correction must state why the memory cannot be accepted: {feedback}"
    );
    assert!(
        feedback.contains("invalid_memory_kind"),
        "the same correction must include independent structural memory errors: {feedback}"
    );
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "The user uses the Acme Router.");
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("expected ingest")
    };
    let entity = working_entity(&repo, &record, "devices/acme-router")
        .await
        .expect("User memory keeps entity");
    assert!(
        entity.contributions.iter().all(|contribution| {
            contribution
                .attributes
                .as_object()
                .is_some_and(|attributes| attributes.is_empty())
        }),
        "unsupported Agent attribute is removed"
    );
}

#[tokio::test]
async fn ingest_groups_all_memory_admission_errors_in_one_correction() {
    let initial = json!({
        "new_entities":[], "existing_entity_updates":[], "playbooks":[],
        "memories":[
            {"kind":"claim","content":"Postgres runs on 5433.","entities":["services/postgres"],
             "sources":[{"message":"m1","quote":"Postgres runs on 5433","strength":"explicit"}]},
            {"kind":"fact","content":"   ","entities":["services/redis"],
             "sources":[{"message":"m1","quote":"Redis runs on 6380","strength":"explicit"}]},
            {"kind":"fact","content":"Nginx runs on 8080.","entities":[],
             "sources":[{"message":"m1","quote":"Nginx runs on 8080","strength":"explicit"}]}
        ]
    });
    let corrected = json!({
        "new_entities":[], "existing_entity_updates":[], "playbooks":[],
        "memories":[
            {"kind":"fact","content":"Postgres runs on 5433.","entities":["services/postgres"],
             "sources":[{"message":"m1","quote":"Postgres runs on 5433","strength":"explicit"}]},
            {"kind":"fact","content":"Redis runs on 6380.","entities":["services/redis"],
             "sources":[{"message":"m1","quote":"Redis runs on 6380","strength":"explicit"}]},
            {"kind":"fact","content":"Nginx runs on 8080.","entities":["services/nginx"],
             "sources":[{"message":"m1","quote":"Nginx runs on 8080","strength":"explicit"}]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("initial".into(), "submit".into(), initial)]),
        MockResponse::ToolCalls(vec![("corrected".into(), "submit".into(), corrected)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres runs on 5433. Redis runs on 6380. Nginx runs on 8080.",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        2,
        "all memory admission errors must be returned in one correction turn"
    );
    let feedback = format!("{:#?}", mock.last_history());
    for reason in [
        "invalid_memory_kind",
        "empty_memory_content",
        "memory_has_no_usable_entity",
    ] {
        assert!(
            feedback.contains(reason),
            "grouped feedback is missing {reason}: {feedback}"
        );
    }
    let memories = PkmRepo::new(ctx.db.clone(), 8)
        .list_all_memories("u1")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        3,
        "the corrected valid memories must all survive extraction"
    );
}

#[tokio::test]
async fn ingest_returns_previously_rejected_memories_for_correction() {
    let repeated = json!({
        "new_entities":[], "existing_entity_updates":[], "playbooks":[],
        "memories":[{
            "kind":"fact", "content":"Postgres runs on 5432.", "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres runs on 5432","strength":"explicit"}]
        }]
    });
    let empty = json!({
        "new_entities":[], "existing_entity_updates":[], "playbooks":[], "memories":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("repeated".into(), "submit".into(), repeated)]),
        MockResponse::ToolCalls(vec![("drop".into(), "submit".into(), empty)]),
    ]));
    let ctx = setup(mock.clone()).await;
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let rejected_id = repo
        .create_memory_with_entities(
            "u1",
            "a1",
            "old-chat",
            frona::memory::pkm::model::MemoryKind::Fact,
            "Postgres runs on 5432.",
            &["services/postgres".into()],
        )
        .await
        .unwrap();
    repo.set_disposition(
        "u1",
        &rejected_id,
        frona::memory::pkm::model::Disposition::Erroneous,
    )
    .await
    .unwrap();
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres runs on 5432.",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        2,
        "re-learn suppression must use a model correction turn"
    );
    assert!(format!("{:#?}", mock.last_history()).contains("memory_was_previously_rejected"));
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(
        memories.len(),
        1,
        "the rejected memory remains only as its original tombstone"
    );
}

/// An alias is optional identity metadata. If the model suggests one that the cited
/// message never used, extraction keeps the grounded page and memory without spending a
/// second model call, and omits only that alias from the durable checkpoint.
#[tokio::test]
async fn ingest_discards_an_unsupported_optional_alias_without_resubmission() {
    let extract = json!({
        "new_entities": [{"id":"fixture-page-20",
            "path":"services/postgres", "name":"Postgres", "description":"database",
            "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
            "aliases":["PG"], "candidate_attributes":[]
        }],
        "existing_entity_updates": [],
        "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Postgres is a database",
            "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres is a database","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("extract".into(), "submit".into(), extract),
    ])]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        1,
        "an optional unsupported alias needs no correction turn"
    );
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("extract-only checkpoint advanced past ingest")
    };
    let entity = working_entity(&repo, &record, "services/postgres")
        .await
        .expect("the grounded entity survives optional alias cleanup");
    assert!(
        entity.aliases.is_empty(),
        "the unsupported alias is not stored"
    );
    assert_eq!(record.stats.grounding_corrections, 0);
    assert_eq!(
        record.stats.grounding_items_dropped, 1,
        "the atomically committed checkpoint counts deterministic alias cleanup"
    );
}

/// Grounding correction metrics are part of the extraction window commit. A crash after
/// advancing the watermark must therefore neither lose nor replay the correction count.
#[tokio::test]
async fn ingest_commits_grounding_corrections_with_the_window_checkpoint() {
    let submission = |quote: &str| {
        json!({
            "new_entities": [{"id":"fixture-page-21",
                "path":"services/postgres", "name":"Postgres", "description":"database",
                "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
                "aliases":[], "candidate_attributes":[]
            }],
            "existing_entity_updates": [],
            "playbooks": [],
            "memories": [{
                "kind":"fact", "content":"Postgres is a database",
                "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":quote,"strength":"explicit"}]
            }]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "extract-invalid".into(),
            "submit".into(),
            submission("Postgres is a relational database"),
        )]),
        MockResponse::ToolCalls(vec![(
            "extract-corrected".into(),
            "submit".into(),
            submission("Postgres is a database"),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 2);
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.grounding_corrections, 1);
    assert_eq!(record.stats.grounding_items_dropped, 0);

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();
    let resumed = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.stats.grounding_corrections, 1,
        "a consumed extraction window must not count its correction twice on resume"
    );
}

#[tokio::test]
async fn ingest_validates_the_complete_batch_with_stable_evidence_on_every_correction() {
    let submission = |budget: &str, database_quote: &str| {
        json!({
            "new_entities": [{"id":"fixture-page-22",
                "path":"services/postgres", "name":"Postgres", "description":"database",
                "sources":[{"message":"m2","quote":"Postgres","strength":"explicit"}],
                "aliases":[], "candidate_attributes":[]
            }],
            "existing_entity_updates": [], "playbooks": [],
            "memories": [
                {
                    "id":"budget", "kind":"fact", "content":budget, "entities":["people/me"],
                    "sources":[{"message":"m1","quote":"My budget is 20k","strength":"explicit"}]
                },
                {
                    "id":"database", "kind":"fact", "content":"Postgres is a database",
                    "entities":["services/postgres"],
                    "sources":[{"message":"m2","quote":database_quote,"strength":"explicit"}]
                }
            ]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "initial".into(),
            "submit".into(),
            submission("My budget is $20,000", "Postgres is a relational database"),
        )]),
        MockResponse::ToolCalls(vec![(
            "corrected".into(),
            "submit".into(),
            submission("My budget is 20k", "Postgres is a database"),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    let at = Utc::now() - Duration::hours(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "My budget is 20k",
        at,
        None,
    )
    .await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        at + Duration::seconds(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(
        mock.calls(),
        2,
        "all current errors must be returned in one correction"
    );
    let correction = format!("{:#?}", mock.last_history());
    assert!(
        correction.contains("$20,000"),
        "feedback must report the value mismatch: {correction}"
    );
    assert!(
        correction.contains("Postgres is a relational database"),
        "feedback must report the invalid quote: {correction}"
    );
    assert!(
        correction.contains("budget"),
        "feedback must identify the memory under repair: {correction}"
    );
    assert!(
        correction.contains("database"),
        "feedback must identify the memory under repair: {correction}"
    );

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 2);
    assert!(
        memories
            .iter()
            .any(|memory| memory.content == "My budget is 20k")
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content == "Postgres is a database")
    );
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_some()
    );
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.grounding_corrections, 1);
    assert_eq!(record.stats.grounding_items_dropped, 0);
}

/// The correction conversation resubmits a complete batch, but only rejected fields are
/// applied. Drift elsewhere is ignored without spending another model submission.
#[tokio::test]
async fn ingest_applies_only_allowed_grounding_corrections() {
    let submission = |content: &str, quote: &str| {
        json!({
            "new_entities": [{"id":"fixture-page-23",
                "path":"services/postgres", "name":"Postgres", "description":"database",
                "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
                "aliases":[], "candidate_attributes":[]
            }],
            "existing_entity_updates": [], "playbooks": [],
            "memories": [{
                "kind":"fact", "content":content, "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":quote,"strength":"explicit"}]
            }]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "initial".into(),
            "submit".into(),
            submission(
                "Postgres is a database",
                "Postgres is a relational database",
            ),
        )]),
        MockResponse::ToolCalls(vec![(
            "drift".into(),
            "submit".into(),
            submission("Postgres is very reliable", "Postgres is a database"),
        )]),
        MockResponse::ToolCalls(vec![(
            "fixed".into(),
            "submit".into(),
            submission("Postgres is a database", "Postgres is a database"),
        )]),
    ]));
    let ctx = setup(mock.clone()).await;
    let chat = seed_chat(&ctx.db).await;
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        Utc::now() - Duration::hours(1),
        None,
    )
    .await;

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 2);
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let memories = repo.list_all_memories("u1").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "Postgres is a database");
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stats.grounding_corrections, 1);
}

/// Extraction windows from one large chat may execute concurrently, while the public
/// sweep commits them in order through the ordinary checkpoint transactions.
#[tokio::test]
async fn one_chat_runs_multiple_ingest_windows_concurrently() {
    let extracted = |quote: &str| {
        json!({
            "new_entities": [{"id":"fixture-page-24",
                "path":"services/postgres", "name":"Postgres", "description":"database",
                "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
                "aliases":["PG"], "candidate_attributes":[]
            }],
            "existing_entity_updates": [],
            "playbooks": [],
            "memories": [{
                "kind":"fact", "content":"Postgres is a database",
                "entities":["services/postgres"],
                "sources":[{"message":"m1","quote":quote,"strength":"explicit"}]
            }]
        })
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::Barrier(
            barrier.clone(),
            Box::new(MockResponse::ToolCalls(vec![(
                "extract-1".into(),
                "submit".into(),
                extracted("Postgres is a relational database"),
            )])),
        ),
        MockResponse::Barrier(
            barrier,
            Box::new(MockResponse::ToolCalls(vec![(
                "extract-2".into(),
                "submit".into(),
                extracted("Postgres is a relational database"),
            )])),
        ),
        MockResponse::ToolCalls(vec![(
            "extract-1-fixed".into(),
            "submit".into(),
            extracted("Postgres is a database"),
        )]),
        MockResponse::ToolCalls(vec![(
            "extract-2-fixed".into(),
            "submit".into(),
            extracted("Postgres is a database"),
        )]),
    ]));
    let memory_config = MemoryConfig {
        pkm_extract_max_messages: 1,
        pkm_consolidation_concurrency: 2,
        ..MemoryConfig::default()
    };
    let ctx = setup_with_memory_config(mock.clone(), memory_config).await;
    let chat = seed_chat(&ctx.db).await;
    let first = Utc::now() - Duration::hours(2);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        first,
        Some(MessageStatus::Completed),
    )
    .await;
    let second = first + Duration::minutes(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        "Postgres is a database",
        second,
        Some(MessageStatus::Completed),
    )
    .await;

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        ctx.pkm.run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        ),
    )
    .await
    .expect("both windows must be in flight together")
    .unwrap();

    assert_eq!(mock.calls(), 4);
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let watermark = repo
        .consolidation_watermark(&chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(watermark, second, "ordered commits reach the final window");
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("extract-only checkpoint advanced past ingest")
    };
    assert!(
        working_entity(&repo, &record, "services/postgres")
            .await
            .is_some()
    );
    assert_eq!(record.stats.grounding_corrections, 2);
    assert_eq!(record.stats.grounding_items_dropped, 2);
}

/// A later speculative result must wait behind the earlier window. Cancelling while the
/// first request is still unresolved therefore leaves both the watermark and checkpoint
/// untouched, so a later sweep can replay the complete suffix safely.
#[tokio::test]
async fn later_ingest_window_cannot_commit_ahead_of_an_unfinished_window() {
    const FIRST_WINDOW: &str = "Postgres is a database in the first window";
    const SECOND_WINDOW: &str = "Postgres is a database in the second window";
    let extracted = json!({
        "new_entities": [{"id":"fixture-page-25",
            "path":"services/postgres", "name":"Postgres", "description":"database",
            "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}],
            "aliases":["PG"], "candidate_attributes":[]
        }],
        "existing_entity_updates": [],
        "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Postgres is a database",
            "entities":["services/postgres"],
            "sources":[{"message":"m1","quote":"Postgres is a database","strength":"explicit"}]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ForUserText {
            text: FIRST_WINDOW.into(),
            response: Box::new(MockResponse::Pending),
        },
        MockResponse::ForUserText {
            text: SECOND_WINDOW.into(),
            response: Box::new(MockResponse::ToolCalls(vec![(
                "extract-2".into(),
                "submit".into(),
                extracted.clone(),
            )])),
        },
    ]));
    let memory_config = MemoryConfig {
        pkm_extract_max_messages: 1,
        pkm_consolidation_concurrency: 2,
        ..MemoryConfig::default()
    };
    let ctx = setup_with_memory_config(mock.clone(), memory_config).await;
    let chat = seed_chat(&ctx.db).await;
    let first = Utc::now() - Duration::hours(2);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        FIRST_WINDOW,
        first,
        Some(MessageStatus::Completed),
    )
    .await;
    let second = first + Duration::minutes(1);
    add_message(
        &ctx.db,
        &chat.id,
        MessageRole::User,
        SECOND_WINDOW,
        second,
        Some(MessageStatus::Completed),
    )
    .await;

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            ctx.pkm.run_extraction_sweep(
                &ctx.state.chat_service,
                &ctx.state.contact_service,
                &ctx.state.agent_service,
                &ctx.harness,
            ),
        )
        .await
        .is_err(),
        "the first model request deliberately remains in flight"
    );

    assert_eq!(mock.calls(), 2, "the later window was mined speculatively");
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    assert!(
        repo.consolidation_watermark(&chat.id)
            .await
            .unwrap()
            .is_none()
    );
    if let Some(record) = repo.latest_consolidation_record("u1").await.unwrap() {
        let ConsolidationStageState::Ingest(_) = &record.state else {
            panic!("cancelled extraction stays at ingest")
        };
        assert!(
            working_entity(&repo, &record, "services/postgres")
                .await
                .is_none(),
            "later output was not committed out of order"
        );
        assert_eq!(
            record.stats.grounding_items_dropped, 0,
            "a completed but uncommitted later window must not add its metrics"
        );
    }

    mock.enqueue(MockResponse::ToolCalls(vec![(
        "extract-retry-1".into(),
        "submit".into(),
        extracted.clone(),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "extract-retry-2".into(),
        "submit".into(),
        extracted,
    )]));
    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();
    assert_eq!(mock.calls(), 4, "resume replays both uncommitted windows");
    assert_eq!(
        repo.consolidation_watermark(&chat.id).await.unwrap(),
        Some(second)
    );
    let record = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Ingest(_) = &record.state else {
        panic!("extract-only resume remains at ingest")
    };
    assert!(
        working_entity(&repo, &record, "services/postgres")
            .await
            .is_some()
    );
    assert_eq!(
        record.stats.grounding_items_dropped, 2,
        "the two replayed and committed windows each add their metrics once"
    );
}

#[tokio::test]
async fn sweep_preserves_a_parked_classify_checkpoint_when_no_chat_needs_mining() {
    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let ctx = setup(mock.clone()).await;
    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let mut classify = ConsolidationWorkState::default();
    classify.revision = 1;
    let consolidation_id = frona::core::repository::new_id();
    let record = KnowledgeConsolidationRecord {
        id: frona::core::repository::new_id(),
        consolidation_id: consolidation_id.clone(),
        user_id: "u1".into(),
        state: ConsolidationStageState::Classify(classify),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: Utc::now(),
        updated_at: Utc::now(),
    };
    repo.save_consolidation_record(&record).await.unwrap();
    let mut row = KnowledgeConsolidationEntity::pending(
        &consolidation_id,
        "u1",
        "people/me",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    row.progress.classification = ClassificationProgress::Accepted {
        decision: json!({"entity":{"name":"Casey Owner","description":"Account owner","aliases":[]},
            "classes":[{"class":"schema:Person"}]}),
    };
    PkmConsolidationStore::new(Arc::new(repo.clone()))
        .scoped(&consolidation_id, "u1")
        .upsert_entity(row)
        .await
        .unwrap();

    ctx.pkm
        .run_extraction_sweep(
            &ctx.state.chat_service,
            &ctx.state.contact_service,
            &ctx.state.agent_service,
            &ctx.harness,
        )
        .await
        .unwrap();

    assert_eq!(mock.calls(), 0, "a pure resume must not call Extract");
    let parked = repo
        .latest_consolidation_record("u1")
        .await
        .unwrap()
        .unwrap();
    let ConsolidationStageState::Classify(_) = &parked.state else {
        panic!("the sweep moved a parked Classify checkpoint back to Ingest")
    };
    let row = working_entity(&repo, &parked, "people/me").await.unwrap();
    assert!(matches!(
        row.progress.classification,
        ClassificationProgress::Accepted { .. }
    ));
}

/// Two chats mentioning the *same* entity mine separately and land on one page.
///
/// This does **not** prove the split's call-saving on its own, and is not written as
/// if it does: `MockModelProvider` serves an untagged positional queue, so settling
/// per chat does not simply consume more responses - it shifts every later stage onto
/// the wrong payload, which then fails silently. The totals coincide. Asserting a call
/// count here would look like proof and be worth nothing.
///
/// What it does pin is that the split did not break the multi-chat path: both
/// transcripts are mined, both watermarks advance, and the shared page exists once.
#[tokio::test]
async fn two_chats_touching_one_entity_land_on_a_single_entity() {
    let extract = |fact: &str| -> Value {
        let quote = fact.split_whitespace().last().unwrap_or(fact);
        json!({
            "new_entities": [{"id":"fixture-page-26",
                "path":"services/postgres", "kind":"service", "name":"Postgres", "description":"svc",
                "sources":[{"message":"m1","quote":"postgres","strength":"explicit"}]
            }],
            "memories": [{"kind":"fact","content":fact,"entities":["services/postgres"],
                "sources":[{"message":"m1","quote":quote,"strength":"explicit"}]}]
        })
    };
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e1".into(), "submit".into(), extract("postgres"))]),
        MockResponse::ToolCalls(vec![("e2".into(), "submit".into(), extract("postgres"))]),
        MockResponse::ToolCalls(vec![(
            "k1".into(),
            "submit".into(),
            json!({"entity":{"name":"Casey Owner","description":"Account owner","aliases":[]},
                "classes":[{"class":"schema:SoftwareApplication"}],"relations":[]}),
        )]),
        MockResponse::ToolCalls(vec![(
            "k2".into(),
            "submit".into(),
            json!({"entity":{"name":"Postgres","description":"svc","aliases":[]},
                "classes":[{"class":"schema:SoftwareApplication"}],"relations":[]}),
        )]),
        MockResponse::ToolCalls(vec![(
            "r".into(),
            "submit".into(),
            json!({
                "name":"Postgres", "description":"svc", "relations":[],
                "entity_relations":[], "outdated":[], "attributes":{},
                "attribute_sources":[], "moves":[]
            }),
        )]),
        MockResponse::Text("Postgres details.".into()),
    ]));

    let ctx = setup(mock.clone()).await;
    let old = Utc::now() - Duration::hours(1);
    let mut chat_ids = Vec::new();
    for (text, reply) in [
        ("my postgres runs on port 5433", "got it"),
        ("restart postgres with brew services restart", "noted"),
    ] {
        let chat = seed_chat(&ctx.db).await;
        add_message(&ctx.db, &chat.id, MessageRole::User, text, old, None).await;
        add_message(
            &ctx.db,
            &chat.id,
            MessageRole::Agent,
            reply,
            old + Duration::seconds(1),
            None,
        )
        .await;
        chat_ids.push(chat.id);
    }

    run_sweep(&ctx).await;

    let repo = PkmRepo::new(ctx.db.clone(), 8);
    let page = repo
        .entity_by_path("u1", "services/postgres")
        .await
        .unwrap();
    assert!(
        page.is_some(),
        "both chats mined into the one page; paths={:?}; calls={}; history={:?}",
        repo.list_all_entity_paths("u1").await.unwrap(),
        mock.calls(),
        mock.last_history()
    );
    for id in &chat_ids {
        assert!(
            repo.consolidation_watermark(id).await.unwrap().is_some(),
            "every ingested chat advanced its watermark, not just the last"
        );
    }
}
