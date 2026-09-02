//! End-to-end test of the PKM memory feature surface, driven through the public
//! `PkmService` + its tools against an in-memory SurrealDB with the LLM stubbed
//! at the provider-registry seam. The module is dormant (not harness-wired), but
//! directly constructible, so this exercises the same paths a wired agent would:
//! remember → consolidate → search → read-the-file → cite, plus a focused
//! supersession-history check.

mod helpers;

use std::sync::Arc;

use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::core::config::{Config, StorageConfig};
use frona::db::repo::pkm::PkmRepo;
use frona::memory::pkm::model::{
    Disposition, EntityCategory, EvidenceSource, EvidenceStrength, LinkOrigin, MemoryEvidence,
    MemoryKind, classify_memories,
};
use frona::memory::pkm::ontology::SchemaEdit;
use frona::memory::pkm::{ConsolidationScope, PkmService};
use frona::memory::service::MemoryService;
use frona::storage::StorageService;

use helpers::{
    MockModelProvider, MockResponse, commit_checkpointed_extract_patch, mark_entity_rendered,
    mock_context, seed_asserted_entity_link, seed_entity_kinds, seed_reconciled_entity,
    test_harness, test_model_group, test_registry_with_group,
};

fn empty_reconcile() -> serde_json::Value {
    json!({
        "relations": [],
        "entity_relations": [],
        "outdated": [],
        "attributes": {},
        "description": "",
        "moves": []
    })
}

fn classification(name: &str, description: &str, class: &str) -> serde_json::Value {
    json!({
        "entity":{"name":name,"description":description,"aliases":[]},
        "classes":[{"class":class}],
        "relations":[],"attributes":[],"new_entities":[],"declarations":[],
        "has_keys":[],"inverse_functional_properties":[]
    })
}

fn adjudication_declarations(target: serde_json::Value) -> Vec<serde_json::Value> {
    std::iter::once(target)
        .chain((1..10).map(|index| {
            json!({
                "kind":"class",
                "term":format!("frona:AdjudicationFixture{index}"),
                "description":format!("A test-only adjudication fixture class {index}."),
                "parents":["schema:Thing"]
            })
        }))
        .collect()
}

fn adjudication_decisions(target: serde_json::Value) -> Vec<serde_json::Value> {
    std::iter::once(target)
        .chain((1..10).map(|index| {
            json!({
                "term":format!("frona:AdjudicationFixture{index}"),
                "decision":"accept_proposal"
            })
        }))
        .collect()
}

fn adjudication_classes(target: serde_json::Value) -> Vec<serde_json::Value> {
    std::iter::once(target)
        .chain((1..10).map(|index| {
            json!({
                "class":format!("frona:AdjudicationFixture{index}")
            })
        }))
        .collect()
}

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    db
}

fn test_user_service(db: &Surreal<Db>) -> frona::auth::user_service::UserService {
    frona::auth::user_service::UserService::new(
        frona::db::repo::generic::SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    )
}

/// Seed the `test-user` / `test-agent` / `test-chat` rows the consolidation scope
/// references. The investigator path (`complete_structured_with_tools`) now requires
/// a real user/agent/chat to exist; a bare (policy-unauthorized) agent yields an empty
/// tool registry, so it falls back to the plain structured call the mocks drive.
async fn seed_identity(db: &Surreal<Db>) {
    test_user_service(db)
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    seed_agent_and_chat(db).await;
}

/// The agent + chat rows `test-agent` / `test-chat` refer to.
///
/// Split out from [`seed_identity`] because reconcile now drives a tool conversation like
/// classify and resolve do, and `structured_conversation` resolves the agent (and the
/// chat, when the scope names one) before it will run. Tests that create their own `User`
/// need these two without a second user row.
async fn seed_agent_and_chat(db: &Surreal<Db>) {
    use frona::core::repository::Repository;
    use frona::db::repo::generic::SurrealRepo;

    SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
        .create(&frona::agent::models::Agent {
            id: "test-agent".into(),
            user_id: "test-user".into(),
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    SurrealRepo::<frona::chat::models::Chat>::new(db.clone())
        .create(&frona::chat::models::Chat {
            id: "test-chat".into(),
            user_id: "test-user".into(),
            space_id: None,
            task_id: None,
            agent_id: "test-agent".into(),
            title: Some("Test".into()),
            archived_at: None,
            channel_id: None,
            channel_external_id: None,
            metadata: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
}

/// The mining, batch commit, and consolidation operations used by the sweep, for a
/// transcript with no chat window behind it.
///
/// A helper *here* rather than a method on `PkmService`: nothing in production
/// consolidates a bare transcript. This composes the retained production operations and
/// owns no pipeline, so the test cannot introduce a second production entry point.
///
/// It deliberately does *not* reproduce the sweep's record bookkeeping around mining
/// (banking `mined`, charging an ingest failure). That is the sweep's, and
/// `pkm_recovery_e2e` / `pkm_consolidation_sweep_e2e` cover it.
async fn full_pass(
    service: &PkmService,
    mut scope: ConsolidationScope,
    transcript: &str,
    harness: Arc<frona::agent::harness::Harness>,
) -> Result<frona::memory::pkm::ConsolidationStats, frona::core::error::AppError> {
    if !transcript.trim().is_empty() && scope.evidence_sources.is_empty() {
        let chat_id = scope.chat_id.clone().unwrap_or_else(|| "test-chat".into());
        for (index, line) in transcript
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
        {
            let handle = format!("m{}", index + 1);
            let (text, kind) = if let Some(text) = line.strip_prefix("Agent:") {
                (
                    text.trim(),
                    frona::memory::pkm::TranscriptEvidenceKind::AgentMessage {
                        message_id: format!("test-message-{}", index + 1),
                        agent_id: scope.agent_id.clone(),
                        chat_id: chat_id.clone(),
                    },
                )
            } else {
                (
                    line.strip_prefix("User:").unwrap_or(line).trim(),
                    frona::memory::pkm::TranscriptEvidenceKind::UserMessage {
                        message_id: format!("test-message-{}", index + 1),
                        chat_id: chat_id.clone(),
                    },
                )
            };
            scope
                .evidence_sources
                .push(frona::memory::pkm::TranscriptEvidenceSource {
                    handle,
                    text: text.to_string(),
                    kind,
                });
        }
    }
    let transcript = transcript
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| format!("[m{}] {line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let batch = service
        .mine_window(scope.clone(), &transcript, harness.clone())
        .await?;
    let repo = service.repo();
    commit_checkpointed_extract_patch(&repo, &scope.user_id, &batch, None, &[]).await;
    service.consolidate(scope, harness).await
}

/// An `OntologyManager` over the same roots and database the service uses.
///
/// Built here rather than borrowed from the service: the reasoner is a fixture concern,
/// and a public accessor existing only for tests is the thing this suite keeps paying
/// for. The delta lives in the database, so a second manager sees the same state.
fn ontology_manager(db: &Surreal<Db>) -> frona::memory::pkm::ontology::OntologyManager {
    frona::memory::pkm::ontology::OntologyManager::new(
        ontology_base(),
        Arc::new(PkmRepo::new(db.clone(), 8)),
    )
}

fn resources_prompts() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("resources")
        .join("prompts")
}

#[tokio::test]
async fn service_pipeline_consolidates_searches_reads_and_cites_entities_and_playbooks() {
    let db = test_db().await;
    seed_identity(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);

    // Mock LLM, queued in pipeline order: Extract, Classify, Reconcile,
    // Playbook Resolve, Playbook Author, then Page Author.
    let extract = json!({
        "new_entities": [
            {"id":"page-postgres","path":"services/postgres","name":"Postgres","description":"the dev database",
             "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}]}
        ],
        "playbooks": [
            {"id":"p1","path":"procedures/restart-postgres","name":"Restart Postgres",
             "description":"Restart the local Postgres development service."}
        ],
        "memories": [
            {"kind":"fact","sources":[{"message":"m1","quote":"Postgres runs on port 5433","strength":"explicit"}],"content":"Postgres runs on port 5433","entities":["services/postgres"]},
            {"kind":"procedural","sources":[
                 {"message":"m1","quote":"restart it with brew services restart postgresql","strength":"explicit"}],
             "content":"Restart Postgres with: brew services restart postgresql",
             "entities":["services/postgres"],"playbook":"p1"}
        ]
    });
    let resolve_playbook = json!({
        "playbooks": [
            {"path":"procedures/restart-postgres","name":"Restart Postgres",
             "description":"Restart the local Postgres development service.",
             "memory_ids":["m1"]}
        ]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "k".into(),
            "submit".into(),
            json!({"entity":{"name":"Casey Owner","description":"","aliases":[]},
                "classes": [{"class": "schema:Thing"}], "relations": []}),
        )]),
        MockResponse::ToolCalls(vec![(
            "k".into(),
            "submit".into(),
            json!({"entity":{"name":"Postgres","description":"the dev database","aliases":[]},
                "classes": [{"class": "schema:Thing"}], "relations": []}),
        )]),
        MockResponse::ToolCalls(vec![("c3".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("c4".into(), "submit".into(), resolve_playbook)]),
        MockResponse::ToolCalls(vec![(
            "c5".into(),
            "submit".into(),
            json!({
                "name": "Restart Postgres",
                "description": "Restart the local Postgres development service.",
                "body": "## Steps\n1. Run `brew services restart postgresql`.\n",
                "related_playbooks": []
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "c6".into(),
            "submit".into(),
            json!({
                "body": "Local Postgres is the dev database. It currently runs on port **5433** — the default 5432 is wrong here, use 5433."
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "c7".into(),
            "submit".into(),
            json!({"body": "Casey Owner's personal knowledge base."}),
        )]),
    ]));

    let memory_config = frona::core::config::MemoryConfig::default();
    // Register the consolidation model group under the configured name so the
    // service's lazy resolution (memory.model_group → "primary") finds it.
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let ctx = mock_context(); // user id "test-user", handle "testuser", chat "test-chat"

    let tools = service.tools();
    for t in &tools {
        assert_eq!(
            t.definitions().len(),
            1,
            "tool {} loads exactly one definition from tools/pkm/",
            t.name()
        );
    }

    let remember = tools
        .iter()
        .find(|t| t.name() == "memory_remember")
        .unwrap();
    remember
        .execute(
            "memory_remember",
            json!({"content": "working on the postgres setup"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        repo.list_short_memory("test-user").await.unwrap().len(),
        1,
        "memory_remember wrote a short memory"
    );

    let scope = ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: Vec::new(),
        evidence_sources: Vec::new(),
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    let stats = full_pass(&service, scope,
        "User: Postgres runs on port 5433. I restart it with brew services restart postgresql.\nAgent: Postgres runs on port 5433. Restart it with brew services restart postgresql.",
        harness.clone())
        .await
        .unwrap();
    assert!(stats.memories_added >= 2, "memories created: {stats:?}");
    assert!(
        stats.pages_built >= 1,
        "a concept page was authored: {stats:?}"
    );
    assert!(
        stats.playbooks_built >= 1,
        "a playbook page was created: {stats:?}"
    );

    let pg = repo
        .entity_by_path("test-user", "services/postgres")
        .await
        .unwrap()
        .expect("postgres concept page exists");
    assert_eq!(pg.category, EntityCategory::Concept);
    let pb = repo
        .entity_by_path("test-user", "procedures/restart-postgres")
        .await
        .unwrap()
        .expect("playbook page at the LLM-chosen path (no playbooks/ prefix)");
    assert_eq!(pb.category, EntityCategory::Playbook);

    let pg_abs = format!("{base}/users/testuser/pkm/Memory/services/postgres.md");
    let pb_abs = format!("{base}/users/testuser/pkm/Memory/procedures/restart-postgres.md");
    let search = tools.iter().find(|t| t.name() == "memory_search").unwrap();
    let out = search
        .execute("memory_search", json!({"query": "postgres restart"}), &ctx)
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(out.text_content()).unwrap();
    let results = result["results"].as_array().unwrap();
    let concept = results
        .iter()
        .find(|item| item["path"] == "services/postgres")
        .expect("search returns the concept");
    let playbook = results
        .iter()
        .find(|item| item["path"] == "procedures/restart-postgres")
        .expect("search returns the playbook");
    assert_eq!(concept["category"], "concept");
    assert_eq!(concept["file"], pg_abs);
    assert_eq!(playbook["category"], "playbook");
    assert_eq!(playbook["file"], pb_abs);
    assert!(
        playbook["matched_by"]
            .as_array()
            .unwrap()
            .iter()
            .any(|matched| matched["kind"] == "body_text" && matched["snippet"].is_string())
    );
    assert!(
        results
            .iter()
            .all(|item| !item["file"].as_str().unwrap().contains("/pages/")),
        "no pages/ subdir in paths: {result:#}"
    );

    let file = std::fs::read_to_string(&pg_abs).expect("concept page file written to disk");
    assert!(
        file.contains("path: services/postgres"),
        "path frontmatter stays clean:\n{file}"
    );
    assert!(
        file.contains("category: concept"),
        "category frontmatter:\n{file}"
    );
    assert!(!file.contains("## Facts"), "no body Facts section:\n{file}");
    assert!(
        !file.contains("## Playbooks"),
        "no body Playbooks section:\n{file}"
    );

    let cite = tools.iter().find(|t| t.name() == "memory_cite").unwrap();
    cite.execute("memory_cite", json!({"path": pg_abs}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        repo.entity_by_path("test-user", "services/postgres")
            .await
            .unwrap()
            .unwrap()
            .use_count,
        1,
        "memory_cite bumped use_count via the absolute .md path"
    );
}

#[tokio::test]
async fn playbook_resolve_reads_pending_candidates_and_merges_duplicate_goals() {
    let db = test_db().await;
    seed_identity(&db).await;
    use frona::core::repository::Repository;
    let message_repo =
        frona::db::repo::generic::SurrealRepo::<frona::chat::message::models::Message>::new(
            db.clone(),
        );
    let mut first_user_message = frona::chat::message::models::Message::builder(
        "test-chat",
        frona::chat::message::models::MessageRole::User,
        "Use yfinance to read the latest daily Close value.".into(),
    )
    .build();
    first_user_message.id = "test-message-1".into();
    first_user_message.created_at = chrono::Utc::now();
    message_repo.create(&first_user_message).await.unwrap();
    let mut second_user_message = frona::chat::message::models::Message::builder(
        "test-chat",
        frona::chat::message::models::MessageRole::User,
        "Run the same yfinance lookup locally when remote access fails.".into(),
    )
    .build();
    second_user_message.id = "test-message-3".into();
    second_user_message.created_at = first_user_message.created_at + chrono::Duration::seconds(1);
    message_repo.create(&second_user_message).await.unwrap();
    let (tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;

    let extract = json!({
        "new_entities": [],
        "existing_entity_updates": [],
        "playbooks": [
            {
                "id":"pb1", "path":"programming/fetch-stock-close-with-yfinance",
                "name":"Fetch a stock close price with yfinance",
                "description":"Fetch the latest daily stock close with yfinance."
            },
            {
                "id":"pb2", "path":"markets/retrieve-stock-close-price",
                "name":"Retrieve stock close price locally",
                "description":"Retrieve a stock close locally when remote access fails."
            }
        ],
        "memories": [
            {
                "kind":"procedural",
                "sources":[{"message":"m1","quote":"Use yfinance to read the latest daily Close value.","strength":"explicit"}],
                "content":"Use yfinance to read the latest daily Close value.",
                "entities":["people/me"], "playbook":"pb1"
            },
            {
                "kind":"procedural",
                "sources":[{"message":"m3","quote":"Run the same yfinance lookup locally when remote access fails.","strength":"explicit"}],
                "content":"Run the same yfinance lookup locally when remote access fails.",
                "entities":["people/me"], "playbook":"pb2"
            }
        ],
        "research_dispositions": []
    });
    let first_resolution = json!({"playbooks":[{
        "path":"programming/fetch-stock-close-with-yfinance",
        "name":"Fetch a stock close price with yfinance",
        "description":"Fetch the latest daily stock close with yfinance.",
        "memory_ids":["m1"]
    }]});
    let merged_resolution = json!({"playbooks":[{
        "existing_path":"programming/fetch-stock-close-with-yfinance",
        "path":"programming/fetch-stock-close-with-yfinance",
        "name":"Fetch a stock close price with yfinance",
        "description":"Fetch the latest daily stock close with yfinance, including local execution when remote access fails.",
        "memory_ids":["m1"]
    }]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("extract".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "classify".into(),
            "submit".into(),
            json!({
                "entity":{"name":"Casey Owner","description":"x","aliases":[]},
                "classes":[{"class":"schema:Person"}],
                "relations":[], "attributes":[], "new_entities":[],
                "declarations":[], "has_keys":[], "inverse_functional_properties":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::ToolCalls(vec![(
            "resolve-first".into(),
            "submit".into(),
            first_resolution,
        )]),
        MockResponse::ToolCalls(vec![(
            "find".into(),
            "find_playbooks".into(),
            json!({"query":"stock close yfinance"}),
        )]),
        MockResponse::ToolCalls(vec![(
            "read".into(),
            "read_playbook".into(),
            json!({"path":"programming/fetch-stock-close-with-yfinance"}),
        )]),
        MockResponse::ToolCalls(vec![(
            "resolve-merge".into(),
            "submit".into(),
            merged_resolution,
        )]),
        MockResponse::ToolCalls(vec![(
            "author-playbook".into(),
            "submit".into(),
            json!({
                "name":"Fetch a stock close price with yfinance",
                "description":"Fetch the latest daily stock close with yfinance, including local execution when remote access fails.",
                "body":"# Fetch a stock close price with yfinance\n\nUse yfinance locally when remote access fails.",
                "related_playbooks":[]
            }),
        )]),
        MockResponse::Text("Casey Owner uses a reusable stock-price retrieval procedure.".into()),
    ]));
    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;

    let result = full_pass(
        &service,
        ontology_scope(&service),
        "User: Use yfinance to read the latest daily Close value.\nAgent: Use yfinance to read the latest daily Close value.\nUser: Run the same yfinance lookup locally when remote access fails.\nAgent: Run the same yfinance lookup locally when remote access fails.",
        harness,
    ).await;
    assert!(result.is_ok(), "{result:?}\n{:?}", mock.histories());

    let canonical = repo
        .entity_by_path("test-user", "programming/fetch-stock-close-with-yfinance")
        .await
        .unwrap()
        .expect("canonical Playbook");
    assert_eq!(canonical.category, EntityCategory::Playbook);
    assert!(
        repo.entity_by_path("test-user", "markets/retrieve-stock-close-price")
            .await
            .unwrap()
            .is_none(),
        "duplicate Playbook survived"
    );
    let memories = repo
        .memories_for_entity("test-user", "programming/fetch-stock-close-with-yfinance")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        2,
        "both procedure memories must reach the winner"
    );
    let histories = format!("{:?}", mock.histories());
    assert!(
        histories.contains("(not authored yet)"),
        "read_playbook did not expose the pending Playbook: {histories}"
    );
    let author_history = mock
        .histories()
        .into_iter()
        .find(|history| {
            history.iter().any(|message| {
                let rendered = format!("{message:?}");
                rendered.contains("PATH: programming/fetch-stock-close-with-yfinance")
                    && rendered.contains("SOURCE TRANSCRIPT WINDOWS:")
            })
        })
        .expect("Playbook Author request");
    let author_history = format!("{author_history:?}");
    assert!(
        author_history.contains("[t1 user]") || author_history.contains("[t2 user]"),
        "Playbook Author did not receive a User-anchored transcript window: {author_history}"
    );
    assert!(
        !author_history.contains("(source transcript unavailable)"),
        "Playbook Author treated User assertion evidence as unavailable: {author_history}"
    );
    assert!(
        tmp.path()
            .join("users/testuser/pkm/Memory/programming/fetch-stock-close-with-yfinance.md",)
            .is_file()
    );
    assert!(
        !tmp.path()
            .join("users/testuser/pkm/Memory/markets/retrieve-stock-close-price.md",)
            .exists()
    );
}

/// Full integrated scenario (one consolidation pass, no ontology layer) - validates
/// the main non-ontology paths working together: self-page routing, entity creation
/// with aliases, `User` write-through, and the `<user_profile>` injection.
/// Reconcile and Page Author responses are identical so the test is robust to
/// `entities_needing_reconciliation` ordering. Entity resolution moved to the
/// Resolve stage, which the `resolve_*` end-to-end tests cover.
#[tokio::test]
async fn consolidation_routes_self_memories_persists_aliases_and_updates_user_profile() {
    let db = test_db().await;
    seed_agent_and_chat(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);

    let users = test_user_service(&db);
    users
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let extract1 = json!({
        "new_entities": [
            {"id":"fixture-page-1","path":"organizations/former-corp","kind":"organization","name":"Former Corp",
             "description":"Casey Owner's employer","aliases":["EXC"],
             "sources":[
                 {"message":"m1","quote":"Former Corp","strength":"explicit"},
                 {"message":"m1","quote":"EXC","strength":"explicit"}
             ]}
        ],
        "memories": [
            {"kind":"identity","sources":[{"message":"m1","quote":"backend engineer at Former Corp","strength":"explicit"}],"content":"Backend engineer at Former Corp","entities":["organizations/former-corp"]},
            {"kind":"identity","sources":[{"message":"m1","quote":"backend engineer","strength":"explicit"}],"content":"Backend engineer","entities":["people/me"]},
            {"kind":"identity","sources":[{"message":"m1","quote":"UTC","strength":"explicit"}],"content":"On UTC","entities":["people/me"]},
            {"kind":"identity","sources":[{"message":"m1","quote":"deploy to EXC","strength":"explicit"}],"content":"Deploys to EXC","entities":["organizations/former-corp"]}
        ]
    });
    let reconcile_self = json!({
        "supersessions": [],
        "attributes": {"timezone":"UTC","role":"Backend engineer"},
        "attribute_sources": [
            {"property":"timezone","value":"UTC","source_memory_ids":["m1","m2"]},
            {"property":"role","value":"Backend engineer","source_memory_ids":["m1","m2"]}
        ],
        "declarations": [{
            "kind":"data_property", "term":"frona:timezone",
            "description":"The IANA time zone used by the person.",
            "datatype":"xsd:string"
        }],
        "description": "reconciled",
        "moves": []
    });

    let mock = Arc::new(MockModelProvider::new(vec![
        // extract, 2× reconcile (identical), 2× author (identical)
        MockResponse::ToolCalls(vec![("e1".into(), "submit".into(), extract1)]),
        MockResponse::ToolCalls(vec![(
            "k1".into(),
            "submit".into(),
            json!({
                "entity":{"name":"Former Corp","description":"Casey Owner's employer","aliases":["EXC"]},
                "classes":[{"class":"schema:Organization"}],
                "relations":[],"attributes":[],"new_entities":[],"declarations":[],
                "has_keys":[],"inverse_functional_properties":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "k2".into(),
            "submit".into(),
            classification("Casey Owner", "The account owner.", "schema:Person"),
        )]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), reconcile_self)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("page body".into()),
        MockResponse::Text("page body".into()),
    ]));

    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    let scope = |chat: &str| ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some(chat.into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: Vec::new(),
        evidence_sources: Vec::new(),
        recall: Default::default(),
        timezone: "UTC".into(),
    };

    full_pass(
        &service,
        scope("test-chat"),
        "I'm a backend engineer at Former Corp, we deploy to EXC, I'm on UTC.",
        harness.clone(),
    )
    .await
    .unwrap();

    let self_entity = repo
        .self_entity("test-user")
        .await
        .unwrap()
        .expect("self-page exists");
    assert_eq!(self_entity.path, "people/me");
    assert!(
        !repo
            .memories_for_entity("test-user", "people/me")
            .await
            .unwrap()
            .is_empty(),
        "owner identity facts routed to the self-page"
    );
    let employer = repo
        .entity_by_path("test-user", "organizations/former-corp")
        .await
        .unwrap()
        .expect("employer page");
    assert!(
        employer.aliases.contains("EXC"),
        "alias stored: {:?}\n{:#?}",
        employer.aliases,
        mock.histories()
    );
    // Write-through: valid IANA timezone reached the User record.
    assert_eq!(
        users
            .find_by_id("test-user")
            .await
            .unwrap()
            .unwrap()
            .timezone
            .as_deref(),
        Some("UTC"),
        "timezone written through to User: {:#?}",
        mock.histories()
    );
    // <user_profile> reflects the learned self-page enrichment.
    let ctx = mock_context();
    let mut sp = String::new();
    let mut hist: Vec<rig_core::completion::Message> = Vec::new();
    let mut mcx = frona::memory::service::MemoryContext::new(&mut sp, &mut hist, &ctx);
    service.retrieve(&mut mcx).await.unwrap();
    assert!(
        sp.contains("<user_profile>") && sp.contains("schema:roleName: Backend engineer"),
        "profile enrichment:\n{sp}"
    );
}

/// Self-page + `<user_profile>` injection: the reserved self-page is seeded and
/// `retrieve()` always injects a profile block - live header from the `User`
/// record (name/handle) plus learned enrichment from the self-page's attributes.
#[tokio::test]
async fn self_entity_injects_user_profile() {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    // Seed the reserved self-page + a learned attribute (as reconcile would).
    repo.ensure_self_entity("test-user", "Casey Owner")
        .await
        .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "the account owner",
        &json!({"role": "Backend engineer"}),
    )
    .await
    .unwrap();
    assert!(
        repo.self_entity("test-user").await.unwrap().is_some(),
        "self-page lives at the reserved path"
    );

    // retrieve() injects <user_profile>: live header + self-page enrichment.
    let ctx = mock_context(); // user id "test-user", handle "testuser", name "Test"
    let mut system_prompt = String::new();
    let mut history: Vec<rig_core::completion::Message> = Vec::new();
    let mut mcx =
        frona::memory::service::MemoryContext::new(&mut system_prompt, &mut history, &ctx);
    service.retrieve(&mut mcx).await.unwrap();

    assert!(
        system_prompt.contains("<user_profile>"),
        "profile block injected:\n{system_prompt}"
    );
    assert!(
        system_prompt.contains("@testuser"),
        "live handle from the User record:\n{system_prompt}"
    );
    assert!(
        system_prompt.contains("role: Backend engineer"),
        "learned enrichment from the self-page:\n{system_prompt}"
    );
}

/// Self-page → `User` write-through: when the self-page reconciles with a valid
/// IANA `timezone` attribute, that value is projected onto the `User` record
/// (the `{name, timezone}` allowlist), while everything else stays page-only.
#[tokio::test]
async fn self_entity_write_through_updates_user_timezone() {
    let db = test_db().await;
    seed_agent_and_chat(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);

    let users = test_user_service(&db);
    users
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "t@t.com".into(),
            name: "Old Name".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // extract routes a self-fact to people/me; reconcile sets a valid IANA tz.
    let extract = json!({
        "new_entities": [],
        "memories": [{"kind":"identity","sources":[{"message":"m1","quote":"UTC","strength":"explicit"}],"content":"I'm on UTC","entities":["people/me"]}]
    });
    let reconcile = json!({
        "supersessions": [],
        "attributes": {"timezone":"UTC"},
        "attribute_sources": [{
            "property":"timezone", "value":"UTC",
            "source_memory_ids":["m1"]
        }],
        "declarations": [{
            "kind":"data_property", "term":"frona:timezone",
            "description":"The IANA time zone used by the person.",
            "datatype":"xsd:string"
        }],
        "description": "the account owner",
        "moves": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "k".into(),
            "submit".into(),
            classification("Old Name", "the account owner", "schema:Person"),
        )]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), reconcile)]),
        MockResponse::Text("The account owner is on UTC.".into()),
    ]));
    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());

    let scope = ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Old Name".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: Vec::new(),
        evidence_sources: Vec::new(),
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    full_pass(&service, scope, "User: I'm on UTC.", harness.clone())
        .await
        .unwrap();

    let updated = users.find_by_id("test-user").await.unwrap().unwrap();
    assert_eq!(
        updated.timezone.as_deref(),
        Some("UTC"),
        "the valid IANA timezone was written through to the User record: {:?}",
        mock.histories(),
    );
    assert_eq!(
        updated.name, "Old Name",
        "name unchanged (not in the reconcile attributes)"
    );
}

/// Consolidation wiring - the new repo layer: short-memory consume/validate,
/// the per-chat watermark, and the real `chats_needing_consolidation` eligibility
/// query (idle + first-time via `?? epoch`).
#[tokio::test]
async fn checkpoint_commit_consumes_short_memory_advances_watermark_and_selects_idle_chats() {
    let db = test_db().await;
    let repo = PkmRepo::new(db.clone(), 8);

    // Short memory: create → unconsolidated → mark validated → gone from the queue.
    repo.remember("u", "c1", "Postgres port is 5433")
        .await
        .unwrap();
    let sm = repo.unconsolidated_short_memories("c1").await.unwrap();
    assert_eq!(sm.len(), 1, "one un-consolidated short memory for the chat");
    // Through the production path: a window's rows, its watermark, and the short
    // memories it consumed are one transaction, so there is no separate "mark validated".
    commit_checkpointed_extract_patch(
        &repo,
        "u",
        &Default::default(),
        None,
        std::slice::from_ref(&sm[0].id),
    )
    .await;
    assert!(
        repo.unconsolidated_short_memories("c1")
            .await
            .unwrap()
            .is_empty(),
        "validated short memory is no longer fed to consolidation"
    );

    assert!(repo.consolidation_watermark("c1").await.unwrap().is_none());
    let t = chrono::Utc::now() - chrono::Duration::hours(1);
    commit_checkpointed_extract_patch(&repo, "u", &Default::default(), Some(("c1", t)), &[]).await;
    assert!(
        repo.consolidation_watermark("c1").await.unwrap().is_some(),
        "watermark persisted (upsert)"
    );

    // Eligibility via the real repo, keyed on the **message clock**: e1 has an idle
    // first-time (terminal) message → eligible; e2's message is recent → still active,
    // not eligible; e3 is archived → excluded.
    let now = chrono::Utc::now();
    db.query(
        "CREATE chat:e1 SET user_id='u', updated_at=$h1, archived_at=NONE, task_id=NONE;
         CREATE chat:e2 SET user_id='u', updated_at=$h1, archived_at=NONE, task_id=NONE;
         CREATE chat:e3 SET user_id='u', updated_at=$h1, archived_at=$h1, task_id=NONE;
         CREATE message SET chat_id='e1', role='user', content='hi', created_at=$h1, status=$done;
         CREATE message SET chat_id='e2', role='user', content='hi', created_at=$recent, status=$done;
         CREATE message SET chat_id='e3', role='user', content='hi', created_at=$h1, status=$done;",
    )
    .bind(("h1", now - chrono::Duration::hours(1)))
    .bind(("recent", now - chrono::Duration::minutes(1)))
    .bind(("done", frona::chat::message::models::MessageStatus::Completed))
    .await
    .unwrap();
    let eligible = repo
        .chats_needing_consolidation(now - chrono::Duration::minutes(15))
        .await
        .unwrap();
    assert!(
        eligible.contains(&"e1".to_string()),
        "idle first-time chat is eligible: {eligible:?}"
    );
    assert!(
        !eligible.contains(&"e2".to_string()),
        "recent chat is not idle"
    );
    assert!(
        !eligible.contains(&"e3".to_string()),
        "archived chat excluded"
    );
}

/// Focused: a `Replace` chain (each older value replaced by the next) - stored as
/// per-memory `links` and read back through `classify_memories`: only the latest
/// value is current; every superseded value lands in History.
#[tokio::test]
async fn replace_chain_classifies_current_and_history() {
    use frona::memory::pkm::model::{RelationType, classify_memories};
    let db = test_db().await;
    let repo = PkmRepo::new(db, 8);
    let pages = ["services/pg".to_string()];

    let id1 = repo
        .create_memory_with_entities("u", "a", "c", MemoryKind::Fact, "port was 5432", &pages)
        .await
        .unwrap();
    let id2 = repo
        .create_memory_with_entities("u", "a", "c", MemoryKind::Fact, "port is now 5433", &pages)
        .await
        .unwrap();
    let id3 = repo
        .create_memory_with_entities("u", "a", "c", MemoryKind::Fact, "port is now 5500", &pages)
        .await
        .unwrap();

    // 5432 replaced by 5433, 5433 replaced by 5500.
    repo.add_relation("test-user", &id1, RelationType::Replace, &id2, "corrected")
        .await
        .unwrap();
    repo.add_relation("test-user", &id2, RelationType::Replace, &id3, "moved")
        .await
        .unwrap();

    let mems = repo.memories_for_entity("u", "services/pg").await.unwrap();
    let (cur, hist) = classify_memories(&mems);
    assert_eq!(cur.len(), 1, "only the latest value is current");
    assert_eq!(cur[0].content, "port is now 5500");
    let mut h: Vec<&str> = hist.iter().map(|m| m.content.as_str()).collect();
    h.sort();
    assert_eq!(
        h,
        vec!["port is now 5433", "port was 5432"],
        "both older values in History"
    );
}

/// Crash recovery: a rename that committed in the DB but never finished on disk is
/// repaired at boot by `reconcile_vault` - the file is relocated (not rebuilt, so its
/// content survives), stale duplicates are dropped, and a fully-missing file is
/// re-rendered deterministically from the persisted `body`. No LLM, no data loss.
#[tokio::test]
async fn recovery_repairs_a_revision_that_does_not_match_the_file() {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let users = test_user_service(&db);
    users
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let memory_config = frona::core::config::MemoryConfig::default();
    let mock = Arc::new(MockModelProvider::new(vec![]));
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock,
        &memory_config.model_group,
        test_model_group(),
    ));
    let service = PkmService::new(
        db.clone(),
        StorageService::new(&config),
        registry,
        frona::agent::prompt::PromptLoader::new(resources_prompts()),
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let repo = PkmRepo::new(db, memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/casey",
        EntityCategory::Concept,
        &[],
        "Casey",
        "",
        &[],
    )
    .await
    .unwrap();
    let vault = service
        .storage()
        .vault_scope(frona::handle!("testuser"), "Memory")
        .unwrap();
    let content = "# Casey\n\nCanonical file bytes.";
    service
        .storage()
        .write_page(&vault, "people/casey", content)
        .unwrap();
    repo.set_page_rev("test-user", "people/casey", "stale-revision")
        .await
        .unwrap();

    service.reconcile_vault().await.unwrap();

    let page = repo
        .entity_by_path("test-user", "people/casey")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.rev.as_deref(),
        Some(frona::memory::pkm::sha256_hex(content).as_str()),
        "recovery must make the stored revision match the canonical file bytes",
    );
    assert_eq!(
        page.sync_content.as_deref(),
        Some(content),
        "legacy file bytes become the durable database projection"
    );

    let durable = "# Casey\n\nDatabase-authoritative bytes.";
    let durable_rev = frona::memory::pkm::sha256_hex(durable);
    repo.set_page_projection("test-user", "people/casey", durable, &durable_rev)
        .await
        .unwrap();
    service
        .storage()
        .write_page(&vault, "people/casey", "# Casey\n\nStale file bytes.")
        .unwrap();

    service.reconcile_vault().await.unwrap();

    let repaired = repo
        .entity_by_path("test-user", "people/casey")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repaired.rev.as_deref(), Some(durable_rev.as_str()));
    assert_eq!(repaired.sync_content.as_deref(), Some(durable));
    assert_eq!(
        service
            .storage()
            .read_page(&vault, "people/casey")
            .as_deref(),
        Some(durable),
        "recovery replaces a stale mirror from the authoritative database bytes",
    );
}

#[tokio::test]
async fn recovery_relocates_deduplicates_and_rerenders() {
    let db = test_db().await;
    seed_agent_and_chat(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);

    // A real User row so recovery can resolve the handle from user_id.
    let users = test_user_service(&db);
    users
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // Same pipeline shape as pkm_pipeline_e2e: extract → reconcile → maintain → author.
    let extract = json!({
        "new_entities": [
            // the name must be anchored in the transcript below - grounding drops entities
            // the conversation never names.
            {"id":"fixture-page-2","path":"services/postgres","kind":"service","name":"Postgres","description":"the dev database",
             "sources":[{"message":"m1","quote":"my postgres runs on 5433","strength":"explicit"}]}
        ],
        "memories": [
            {"kind":"fact","sources":[{"message":"m1","quote":"my postgres runs on 5433","strength":"explicit"}],"content":"Postgres runs on port 5433","entities":["services/postgres"]}
        ]
    });
    let reconcile = json!({"supersessions":[],"attributes":{"port":5433},"description":"the local dev postgres","moves":[]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "k1".into(),
            "submit".into(),
            classification("Casey Owner", "The account owner.", "schema:Person"),
        )]),
        MockResponse::ToolCalls(vec![(
            "k2".into(),
            "submit".into(),
            classification("Postgres", "the dev database", "schema:Thing"),
        )]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), reconcile)]),
        MockResponse::Text("Postgres is the local development database.".into()),
        MockResponse::Text("The account owner.".into()),
    ]));

    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    let scope = ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: Vec::new(),
        evidence_sources: Vec::new(),
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    let stats = full_pass(
        &service,
        scope,
        "User: my postgres runs on 5433.",
        harness.clone(),
    )
    .await
    .unwrap();

    let vault = StorageService::new(&config)
        .user_pkm_path(&frona::handle!("testuser"))
        .join("Memory");
    let pg = vault.join("services/postgres.md");
    assert!(
        pg.exists(),
        "consolidate authored the concept page file: {stats:?}\n{:#?}",
        mock.histories()
    );
    let content = std::fs::read_to_string(&pg).unwrap();
    assert!(
        content.contains("uid:"),
        "file carries its DB uid in frontmatter"
    );

    // Simulate a crash mid-rename: DB says services/pg, file still at postgres.md,
    // plus a stale duplicate copy left behind by an interrupted move.
    std::fs::write(vault.join("services/postgres-dup.md"), &content).unwrap();
    repo.rename_entity("test-user", "services/postgres", "services/pg")
        .await
        .unwrap();

    service.reconcile_vault().await.unwrap();

    let renamed = vault.join("services/pg.md");
    assert!(
        renamed.exists(),
        "file relocated to the DB's canonical path"
    );
    assert_eq!(
        std::fs::read_to_string(&renamed).unwrap(),
        content,
        "relocated content preserved byte-for-byte (moved, not rebuilt)"
    );
    assert!(!pg.exists(), "old path removed");
    assert!(
        !vault.join("services/postgres-dup.md").exists(),
        "stale duplicate (same uid) deleted"
    );

    std::fs::remove_file(&renamed).unwrap();
    service.reconcile_vault().await.unwrap();
    assert!(
        renamed.exists(),
        "missing file re-rendered from persisted body"
    );
    let rebuilt = std::fs::read_to_string(&renamed).unwrap();
    assert!(
        rebuilt.contains("title: Postgres"),
        "deterministic frontmatter"
    );
    assert!(
        rebuilt.contains("Postgres is the local development database."),
        "persisted body restored from the DB, not regenerated:\n{rebuilt}"
    );
}

/// Directory-rename crash recovery: the atomic dir rename committed in the DB, but
/// the files never moved (crash before Phase 2). `reconcile_vault` must relocate
/// *every* page under the renamed prefix to its new path - matched by `uid`, content
/// preserved - so a half-applied directory rename fully heals at boot.
#[tokio::test]
async fn recovery_relocates_a_renamed_directory() {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };

    // A real User row so recovery can resolve the handle from user_id.
    test_user_service(&db)
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "casey@example.com".into(),
            name: "Casey Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        Arc::new(MockModelProvider::new(vec![])),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        StorageService::new(&config),
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let handle = frona::handle!("testuser");
    let vault = StorageService::new(&config)
        .user_pkm_path(&handle)
        .join("Memory");

    // Seed two pages under `people/`, each with a `uid`-stamped file on disk.
    let mut moves = Vec::new();
    for (path, name) in [("people/alice", "Alice"), ("people/bob", "Bob")] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &["https://schema.org/Person".to_string()],
            name,
            "",
            &[],
        )
        .await
        .unwrap();
        let id = repo
            .entity_by_path("test-user", path)
            .await
            .unwrap()
            .unwrap()
            .id;
        let file = vault.join(format!("{path}.md"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            format!("---\nuid: {id}\ntitle: {name}\n---\n# {name}\n\nbody\n"),
        )
        .unwrap();
        moves.push((path.to_string(), path.replace("people/", "humans/")));
    }

    // Crash simulation: the atomic DB dir-rename lands, but Phase 2 (the file moves)
    // never runs - so the files are still at their old paths.
    repo.rename_entities("test-user", &moves).await.unwrap();
    assert!(
        vault.join("people/alice.md").exists() && vault.join("people/bob.md").exists(),
        "files still at the old paths (Phase 2 never ran)"
    );

    service.reconcile_vault().await.unwrap();

    // Every page under the renamed directory is relocated to its new path (content
    // preserved), and the old paths are gone - no split, no data loss.
    for (old, new) in [
        ("people/alice", "humans/alice"),
        ("people/bob", "humans/bob"),
    ] {
        assert!(vault.join(format!("{new}.md")).exists(), "{new} relocated");
        assert!(!vault.join(format!("{old}.md")).exists(), "{old} removed");
        assert!(
            std::fs::read_to_string(vault.join(format!("{new}.md")))
                .unwrap()
                .contains("body"),
            "content preserved for {new}"
        );
    }
}

/// The committed standard-vocabulary fixture. Deliberately *not* the real catalogue:
/// that is fetched into the image from a pinned release and is not in this repo, so a
/// suite that read it would fail on a fresh clone.
fn resources_ontology() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology/standard")
}

/// The catalogue roots every `PkmService` in these tests is built with: the standard
/// fixture as the bundled half, and no user half - these tests add no ontologies of
/// their own.
fn ontology_base() -> frona::memory::pkm::ontology::Roots {
    frona::memory::pkm::ontology::Roots {
        release: resources_ontology(),
        user: resources_ontology().join("no-user-ontologies"),
    }
}

fn ontology_scope(service: &PkmService) -> ConsolidationScope {
    ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: Vec::new(),
        evidence_sources: Vec::new(),
        recall: Default::default(),
        timezone: "UTC".into(),
    }
}

/// The Classify stage types a new concept page with an ontology class CURIE and
/// mints the `frona:` term (declared + versioned in `knowledge_ontology`).
#[tokio::test]
async fn classify_types_entities_and_assemble_mints_schema() {
    let db = test_db().await;
    seed_identity(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);

    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-3","path":"services/postgres","kind":"service","name":"Postgres","description":"the primary datastore",
             "sources":[{"message":"m1","quote":"postgres","strength":"explicit"}]}
        ],
        "memories": [
            {"kind":"fact","sources":[{"message":"m1","quote":"postgres","strength":"explicit"}],"content":"Postgres is our primary datastore","entities":["services/postgres"]}
        ]
    });
    // The Ingest stage also ensures the owner's self-page, so the Classify stage
    // classifies two concept pages this pass. Both responses type as frona:Service
    // (order-independent) - the test only asserts on the postgres page.
    let classify_owner = json!({
        "entity":{"name":"Casey Owner","description":"The account owner.","aliases":[]},
        "classes":[{"class":"schema:Person"}],
        "relations":[],"attributes":[],"new_entities":[],"declarations":[],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let classify_postgres = json!({
        "entity":{"name":"Postgres","description":"the primary datastore","aliases":[]},
        "classes":[{"class":"frona:Service"}],
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":[{
            "kind":"class","term":"frona:Service",
            "description":"A software service.",
            "parents":["schema:SoftwareApplication"]
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });
    // Adjudicate owns every schema commit now: classify only *proposes* frona:Service, and it
    // is not declared until the adjudicator says so.
    let adjudicate = json!({"decisions":[
        {"term":"frona:Service","decision":"declare","parent":"schema:SoftwareApplication"}
    ]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify_owner)]),
        MockResponse::ToolCalls(vec![("c3".into(), "submit".into(), classify_postgres)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("c4".into(), "submit".into(), adjudicate)]),
    ]));

    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let ontology_manager = ontology_manager(&db);
    let harness = test_harness(&db, &config, mock.clone());

    let result = full_pass(
        &service,
        ontology_scope(&service),
        "User: postgres is our primary datastore.",
        harness,
    )
    .await;
    assert!(result.is_ok(), "{result:?}\n{:#?}", mock.histories());

    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let pg = repo
        .entity_by_path("test-user", "services/postgres")
        .await
        .unwrap()
        .expect("postgres page exists");
    assert_eq!(
        pg.kinds,
        [frona::memory::pkm::ontology::PrefixMap::standard().expand("frona:Service")],
        "page re-keyed to the ontology class CURIE"
    );

    // the frona: mint is persisted + versioned in the delta.
    let onto = repo
        .ontology_get("test-user")
        .await
        .unwrap()
        .expect("delta persisted");
    assert!(onto.version >= 1, "delta versioned: {}", onto.version);
    assert!(
        onto.owl.contains("Service"),
        "delta declares frona:Service:\n{}",
        onto.owl
    );
    assert!(
        ontology_manager
            .catalog("test-user")
            .await
            .unwrap()
            .classes
            .contains(&"frona:Service".to_string()),
        "catalog reflects the mint"
    );
}

/// Mirrors the default `pkm_consolidation_max_submissions` value and thus how many
/// violating answers exhaust one page's classify conversation.
const CLASSIFY_MAX_SUBMISSIONS: usize = 8;

/// A classification that never produces a valid projection is discarded. The page keeps
/// its last valid type and its durable facts remain current for a later repair pass.
#[tokio::test]
async fn classify_discards_a_clashing_submission_without_hiding_facts() {
    let db = test_db().await;
    seed_identity(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);
    let repo = PkmRepo::new(db.clone(), 8);
    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    mark_clean(
        &db,
        &repo,
        "organizations/acme",
        "schema:Organization",
        "Acme",
        json!({}),
    )
    .await;
    repo.create_memory_with_entities(
        "test-user",
        "test-agent",
        "test-chat",
        MemoryKind::Fact,
        "Acme is our primary vendor",
        &["organizations/acme".into()],
    )
    .await
    .unwrap();
    touch(&db, "organizations/acme", "schema:Organization").await;

    // The page keeps proposing the contradictory class through every semantic revision.
    let classify = json!({
        "entity":{"name":"Acme","description":"x","aliases":[]},
        "classes":[{"class":"frona:Confused"}],
        "relations":[],"attributes":[],"new_entities":[],"declarations":[],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let mut responses = Vec::new();
    for i in 0..CLASSIFY_MAX_SUBMISSIONS {
        responses.push(MockResponse::ToolCalls(vec![(
            format!("c{i}"),
            "submit".into(),
            classify.clone(),
        )]));
    }
    let mock = Arc::new(MockModelProvider::new(responses));

    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let ontology_manager = ontology_manager(&db);

    // Pre-seed a contradictory class: frona:Confused ⊑ Person AND ⊑ Organization
    // (disjoint in frona.ttl). Any page typed with it clashes.
    ontology_manager
        .commit(
            "test-user",
            &[
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Person".into(),
                },
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Organization".into(),
                },
            ],
        )
        .await
        .unwrap();

    let harness = test_harness(&db, &config, mock.clone());
    let result = full_pass(&service, ontology_scope(&service), "", harness).await;
    assert!(result.is_ok(), "{result:?}\n{:#?}", mock.histories());
    let stats = result.unwrap();

    let mems = repo
        .memories_for_entity("test-user", "organizations/acme")
        .await
        .unwrap();
    assert!(
        mems.iter().all(|m| m.disposition == Disposition::None),
        "the invalid class must not hide valid facts: {:?}; stats={stats:?}",
        mems.iter()
            .map(|m| (&m.content, m.disposition))
            .collect::<Vec<_>>(),
    );
    assert_eq!(stats.facts_quarantined, 0);
    let entity = repo
        .entity_by_path("test-user", "organizations/acme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entity.kinds,
        [frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization"),]
    );
}

/// Build an ontology-enabled service over a temporary vault for tests that start with
/// durable PKM state and run only the user-scoped consolidation stages.
/// The mock queue is consolidate-only: these drive `full_pass` with an **empty
/// transcript**, so extract returns before any model call and only the user-scoped
/// stages run.
async fn consolidate_only_service(
    db: &Surreal<Db>,
    mock: Arc<MockModelProvider>,
) -> (
    PkmService,
    Arc<frona::agent::harness::Harness>,
    Config,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let memory_config = frona::core::config::MemoryConfig::default();
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let service = PkmService::new(
        db.clone(),
        StorageService::new(&config),
        registry,
        frona::agent::prompt::PromptLoader::new(resources_prompts()),
        memory_config,
        test_user_service(db),
        ontology_base(),
    );
    let harness = test_harness(db, &config, mock);
    (service, harness, config, tmp)
}

/// One consolidation over a single dirty page: classify → reconcile → author.
fn consolidate_responses() -> Vec<MockResponse> {
    vec![
        MockResponse::ToolCalls(vec![(
            "k".into(),
            "submit".into(),
            json!({
                "entity":{"name":"Acme","description":"a vendor","aliases":[]},
                "classes":[{"class":"schema:Organization"}],
                "relations":[],"attributes":[],"new_entities":[],"declarations":[],
                "has_keys":[],"inverse_functional_properties":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("Acme details.".into()),
    ]
}

/// A model that **never answered** is not a model that answered wrongly. A classify
/// conversation that errors before producing a first candidate is a transient failure -
/// timeout, provider error, budget - and nothing about the page has been shown to clash,
/// so its facts stay live. Quarantining them there hid true facts from every projection
/// until some later pass happened to classify the page cleanly.
#[tokio::test]
async fn classify_that_never_answers_returns_an_error_without_quarantining_the_entity() {
    let db = test_db().await;
    seed_identity(&db).await;
    // Classify answers with prose and never produces a valid typed submission, so its
    // caller-owned submission budget ends without a candidate.
    let mut responses = vec![MockResponse::Text(
        "I would rather not classify that.".into(),
    )];
    responses.extend(consolidate_responses().into_iter().skip(1));
    let mock = Arc::new(MockModelProvider::new(responses));
    let (service, harness, _config, _tmp) = consolidate_only_service(&db, mock.clone()).await;
    let repo = PkmRepo::new(db.clone(), 8);

    repo.upsert_entity_skeleton(
        "test-user",
        "organizations/acme",
        EntityCategory::Concept,
        &[],
        "Acme",
        "a vendor",
        &[],
    )
    .await
    .unwrap();
    repo.create_memory_with_entities(
        "test-user",
        "a",
        "c",
        MemoryKind::Fact,
        "Acme is our primary vendor",
        &["organizations/acme".into()],
    )
    .await
    .unwrap();

    let error = full_pass(&service, ontology_scope(&service), "", harness)
        .await
        .expect_err("a missing Classify stage submission must remain retryable");
    assert!(error.to_string().contains("submission budget exhausted"));
    assert_eq!(
        mock.calls(),
        CLASSIFY_MAX_SUBMISSIONS,
        "each missing answer consumes one caller-owned submission attempt"
    );

    let mems = repo
        .memories_for_entity("test-user", "organizations/acme")
        .await
        .unwrap();
    assert!(
        mems.iter().all(|m| m.disposition == Disposition::None),
        "the fact is still live: {:?}",
        mems.iter()
            .map(|m| (&m.content, m.disposition))
            .collect::<Vec<_>>()
    );
    let (cur, _) = classify_memories(&mems);
    assert_eq!(cur.len(), 1, "and visible to the projection");
    let page = repo
        .entity_by_path("test-user", "organizations/acme")
        .await
        .unwrap()
        .unwrap();
    assert!(
        page.kinds.iter().all(|k| k.trim().is_empty()),
        "typing was deferred: {:?}",
        page.kinds
    );
}

#[tokio::test]
async fn agent_echo_of_historical_memory_is_retired_before_page_projection() {
    let db = test_db().await;
    seed_identity(&db).await;
    let repo = PkmRepo::new(db.clone(), 8);
    repo.upsert_entity_skeleton(
        "test-user",
        "dogs/buddy",
        EntityCategory::Concept,
        &[],
        "Buddy",
        "Casey Owner's dog",
        &[],
    )
    .await
    .unwrap();

    let historical = repo
        .create_sourced_memory(
            "test-user",
            MemoryKind::Fact,
            "Buddy needs ear medication",
            &["dogs/buddy".into()],
            vec![frona::memory::pkm::model::MemoryEvidence {
                strength: frona::memory::pkm::model::EvidenceStrength::Explicit,
                source: frona::memory::pkm::model::EvidenceSource::UserMessage {
                    message_id: "user-original".into(),
                    chat_id: "chat-old".into(),
                    quote: "Buddy needs ear medication".into(),
                },
            }],
        )
        .await
        .unwrap();
    repo.set_disposition("test-user", &historical, Disposition::Outdated)
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "dogs/buddy")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [], "existing_entity_updates": [], "playbooks": [],
        "memories": [{
            "kind":"fact", "content":"Buddy needs ear medication",
            "entities":["dogs/buddy"],
            "sources":[
                {"message":"m1", "quote":"Buddy needs ear medication", "strength":"explicit"},
                {"message":"m2", "quote":"Yes, that is correct", "strength":"explicit", "confirmation":true}
            ]
        }]
    });

    let first = json!({
        "relations": [{"memory": "m2", "links": [{
            "relation": "duplicate", "to": "m1", "note": "retrieval echo"
        }]}],
        "outdated": [],
        "attributes": {},
        "description": "Buddy currently needs ear medication.",
        "moves": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![(
            "k".into(),
            "submit".into(),
            classification("Buddy", "Casey Owner's dog", "schema:Thing"),
        )]),
        MockResponse::ToolCalls(vec![(
            "km".into(),
            "submit".into(),
            classification("Casey Owner", "The account owner.", "schema:Person"),
        )]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), first)]),
        MockResponse::Text("Buddy is Casey Owner's dog.".into()),
        MockResponse::Text("Casey Owner is the account owner.".into()),
    ]));
    let (service, harness, _config, _tmp) = consolidate_only_service(&db, mock.clone()).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Agent: Buddy needs ear medication\nUser: Yes, that is correct",
        harness,
    )
    .await
    .unwrap();

    let memories = repo
        .memories_for_entity("test-user", "dogs/buddy")
        .await
        .unwrap();
    assert!(
        memories.iter().all(|memory| !matches!(
            memory.evidence.first().map(|item| &item.source),
            Some(frona::memory::pkm::model::EvidenceSource::AgentMessage { .. })
        )),
        "cleanup removed the subordinate Agent echo: {memories:?}\n{:#?}",
        mock.histories()
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.id == historical && memory.disposition == Disposition::Outdated),
        "historical source remains outdated"
    );
    let page = repo
        .entity_by_path("test-user", "dogs/buddy")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.attributes,
        json!({}),
        "the first verdict's echo-derived attribute never projected"
    );
}

/// A shared ontology-enabled service + harness for the Resolve e2es.
async fn ontology_service(
    db: &Surreal<Db>,
    config: &Config,
    mock: Arc<MockModelProvider>,
    memory_config: &frona::core::config::MemoryConfig,
) -> (PkmService, Arc<frona::agent::harness::Harness>) {
    let storage = StorageService::new(config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(db),
        ontology_base(),
    );
    let harness = test_harness(db, config, mock);
    (service, harness)
}

#[tokio::test]
async fn memory_search_merges_and_ranks_identity_semantic_metadata_and_body_matches() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let (service, _harness) = ontology_service(&db, &config, mock, &memory_config).await;
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let prefixes = frona::memory::pkm::ontology::PrefixMap::standard();

    service
        .ontology_manager()
        .commit(
            "test-user",
            &[
                SchemaEdit::DeclareClass {
                    class: "frona:Engineer".into(),
                },
                SchemaEdit::SubClassOf {
                    sub: "frona:Engineer".into(),
                    sup: "schema:Person".into(),
                },
            ],
        )
        .await
        .unwrap();
    for (path, class, name, description) in [
        ("people/sarah", "frona:Engineer", "Sarah", "An engineer"),
        ("people/bob", "schema:Person", "Bob", "A person"),
        (
            "topics/person-handbook",
            "schema:Thing",
            "Person handbook",
            "A lexical metadata match",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[prefixes.expand(class)],
            name,
            description,
            &[],
        )
        .await
        .unwrap();
    }
    repo.upsert_entity_skeleton(
        "test-user",
        "notes/standup",
        EntityCategory::Concept,
        &[],
        "Standup notes",
        "Weekly notes",
        &[],
    )
    .await
    .unwrap();
    db.query(
        "UPDATE knowledge_entity SET body = 'Sarah approved the Postgres migration.'
         WHERE user_id = 'test-user' AND path = 'people/sarah';
         UPDATE knowledge_entity SET body = 'Sarah attended the weekly standup.'
         WHERE user_id = 'test-user' AND path = 'notes/standup';",
    )
    .await
    .unwrap()
    .check()
    .unwrap();

    let tools = service.tools();
    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
    for removed in [
        "memory_graph_find",
        "memory_schema_search",
        "memory_schema_inspect",
        "memory_graph_query",
    ] {
        assert!(
            !names.contains(&removed),
            "removed tool is registered: {removed}"
        );
    }
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "memory_search")
        .expect("memory_search is registered");
    let output = tool
        .execute("memory_search", json!({"query": "Sarah"}), &mock_context())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(output.text_content()).unwrap();

    assert_eq!(result["results"][0]["path"], "people/sarah");
    assert_eq!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["path"] == "people/sarah")
            .count(),
        1,
        "a path returned by several queries is emitted once: {result:#}"
    );
    let sarah_matches = result["results"][0]["matched_by"].as_array().unwrap();
    assert!(
        sarah_matches
            .iter()
            .any(|item| item["kind"] == "exact_name")
            && sarah_matches
                .iter()
                .any(|item| item["kind"] == "metadata_text")
            && sarah_matches.iter().any(|item| {
                item["kind"] == "body_text"
                    && item["snippet"] == "Sarah approved the Postgres migration."
            }),
        "the merged result keeps evidence from every query: {result:#}"
    );
    assert_eq!(result["results"][1]["path"], "notes/standup");

    let output = tool
        .execute("memory_search", json!({"query": "person"}), &mock_context())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(output.text_content()).unwrap();
    let paths: Vec<_> = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        &paths[..2],
        &["people/bob", "people/sarah"],
        "reasoned class members rank before lexical metadata matches: {result:#}"
    );
    assert!(
        result["results"][0]["matched_by"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "asserted_type")
            && result["results"][1]["matched_by"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["kind"] == "inferred_type"),
        "asserted and inferred membership share a tier but retain provenance: {result:#}"
    );
    assert!(
        paths
            .iter()
            .position(|path| *path == "topics/person-handbook")
            .is_some_and(|index| index >= 2),
        "metadata text follows semantic class membership: {result:#}"
    );

    let output = tool
        .execute("memory_search", json!({"query": "people"}), &mock_context())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(output.text_content()).unwrap();
    assert_eq!(
        result["results"].as_array().unwrap().len(),
        2,
        "common plural class queries resolve without fuzzy schema expansion: {result:#}"
    );
}

#[tokio::test]
async fn memory_search_ranks_structural_person_evidence_above_incidental_link_text() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let (service, _harness) = ontology_service(&db, &config, mock, &memory_config).await;
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let prefixes = frona::memory::pkm::ontology::PrefixMap::standard();

    for (path, class, name, description) in [
        (
            "assistants/dark-matter",
            "schema:Thing",
            "Dark Matter",
            "The assistant",
        ),
        ("people/me", "schema:Person", "Mina", "The account owner"),
        ("notes/alpha", "schema:Thing", "Alpha", "Unrelated note"),
        ("notes/beta", "schema:Thing", "Beta", "Unrelated note"),
        ("notes/gamma", "schema:Thing", "Gamma", "Unrelated note"),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[prefixes.expand(class)],
            name,
            description,
            &[],
        )
        .await
        .unwrap();
    }
    db.query(
        "UPDATE knowledge_entity
         SET body = 'Dark Matter is the name [[people/me|Mina]] chose for this assistant.'
         WHERE user_id = 'test-user' AND path = 'assistants/dark-matter';
         UPDATE knowledge_entity
         SET body = 'Mina is the account owner represented by `people/me`. Mina chose the assistant Dark Matter, and research into a potential long-term project remains documented here.'
         WHERE user_id = 'test-user' AND path = 'people/me';
         UPDATE knowledge_entity SET body = 'A quiet alpha document.'
         WHERE user_id = 'test-user' AND path = 'notes/alpha';
         UPDATE knowledge_entity SET body = 'A quiet beta document.'
         WHERE user_id = 'test-user' AND path = 'notes/beta';
         UPDATE knowledge_entity SET body = 'A quiet gamma document.'
         WHERE user_id = 'test-user' AND path = 'notes/gamma';",
    )
    .await
    .unwrap()
    .check()
    .unwrap();

    let tools = service.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "memory_search")
        .expect("memory_search is registered");
    let output = tool
        .execute(
            "memory_search",
            json!({"query": "persons people contacts names"}),
            &mock_context(),
        )
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(output.text_content()).unwrap();

    assert_eq!(
        result["results"][0]["path"], "people/me",
        "path and class evidence must beat incidental raw link text: {result:#}"
    );
    let matched_by = result["results"][0]["matched_by"].as_array().unwrap();
    assert!(
        matched_by
            .iter()
            .any(|item| item["kind"] == "path_token" && item["token"] == "people"),
        "the winning path evidence is explained: {result:#}"
    );
    assert!(
        matched_by.iter().any(|item| {
            item["kind"] == "asserted_type_token"
                && matches!(item["token"].as_str(), Some("people" | "persons"))
                && item["term"] == "schema:Person"
        }),
        "the winning effective-ontology evidence is explained: {result:#}"
    );
}

fn tmp_config() -> (tempfile::TempDir, Config) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        storage: StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    (tmp, config)
}

/// Resolve merges two mentions of the same entity into one canonical page.
#[tokio::test]
async fn resolve_merges_duplicate_mention() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    // Two mentions of "Former Corp" at different proposed paths.
    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-5","path":"orgs/former-corp","name":"Former Corp","description":"the retailer",
             "sources":[{"message":"m1","quote":"Former Corp","strength":"explicit"}]},
            {"id":"fixture-page-6","path":"orgs/former-corp-inc","name":"Former Corp","description":"the retailer",
             "sources":[{"message":"m1","quote":"Former Corp","strength":"explicit"}]}
        ],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Former Corp","strength":"explicit"}],"content":"Former Corp is a retailer","entities":["orgs/former-corp"]}]
    });
    let classify = json!({
        "classes": [{"class": "schema:Organization"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Former Corp Inc", "description": "the retailer", "aliases": []}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![(
            "resolve".into(),
            "submit".into(),
            json!({
                "canonical":"orgs/former-corp", "same_as":[],
                "merge_because":[], "distinct_because":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Former Corp\n\nFormer Corp is a retailer.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Former Corp is a retailer.",
        harness,
    )
    .await
    .unwrap();

    // Exactly one of the two mention pages survives (the other merged in).
    let a = repo
        .entity_by_path("test-user", "orgs/former-corp")
        .await
        .unwrap()
        .is_some();
    let b = repo
        .entity_by_path("test-user", "orgs/former-corp-inc")
        .await
        .unwrap()
        .is_some();
    assert!(
        a ^ b,
        "the duplicate mention was merged into a single canonical page"
    );
    // and the fact rode along to the survivor.
    let survivor = if a {
        "orgs/former-corp"
    } else {
        "orgs/former-corp-inc"
    };
    assert!(
        repo.memories_for_entity("test-user", survivor)
            .await
            .unwrap()
            .iter()
            .any(|m| m.content.contains("retailer")),
        "the merged page carries the fact"
    );
}

/// Type-filtered resolve: two same-named entities of DIFFERENT (disjoint) types are
/// NOT merged - the type prunes the identity candidate ("Mercury" the place is not
/// "Mercury" the person).
#[tokio::test]
async fn resolve_type_filter_blocks_cross_type() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();

    // Pre-seed an established, clean "Mercury" typed as a Person (the singer).
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/mercury",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Mercury",
        "the singer",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/mercury",
        "",
        "the singer",
        &json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/mercury")
        .await
        .unwrap();
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    // A new mention "Mercury" that the Classify stage types as a Place.
    let extract = json!({
        "new_entities": [{"id":"fixture-page-7","path":"places/mercury","name":"Mercury","description":"the planet",
            "sources":[{"message":"m1","quote":"Mercury","strength":"explicit"}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Mercury","strength":"explicit"}],"content":"Mercury is the closest planet to the sun","entities":["places/mercury"]}]
    });
    let classify = json!({
        "classes": [{"class": "schema:Place"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Mercury", "description": "the planet", "aliases": []}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Mercury is the closest planet to the sun.",
        harness,
    )
    .await
    .unwrap();

    assert!(
        stats.resolve_sweeps >= 1,
        "the type-filtered resolve sweep ran: {stats:?}"
    );
    assert_eq!(
        stats.resolve_conversations, 0,
        "disjoint candidates never reach the model"
    );

    // Neither merged into the other - both survive (different types).
    assert!(
        repo.entity_by_path("test-user", "places/mercury")
            .await
            .unwrap()
            .is_some(),
        "the Place mention was NOT merged into the same-named Person"
    );
    assert!(
        repo.entity_by_path("test-user", "people/mercury")
            .await
            .unwrap()
            .is_some(),
        "the established Person page is untouched"
    );
}

/// Resolve adjudication: a variant-named mention ("Former Corp Inc" vs the canonical
/// "Former Corp", same type) has no exact-name fast-path, so the resolver LLM decides -
/// and its verdict merges the mention into the canonical page.
#[tokio::test]
async fn resolve_llm_merges_variant_name() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();

    // Established, clean canonical "Former Corp" (Organization).
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "orgs/former-corp",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization")],
        "Former Corp",
        "the retailer",
        &[],
    )
    .await
    .unwrap();
    repo.create_memory_with_entities(
        "test-user",
        "test-agent",
        "older-chat",
        MemoryKind::Fact,
        "Former Corp is a retailer.",
        &["orgs/former-corp".into()],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "orgs/former-corp",
        "",
        "the retailer",
        &json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "orgs/former-corp")
        .await
        .unwrap();
    for index in 0..70 {
        let path = format!("artifacts/former-corp-{index}");
        repo.upsert_entity_skeleton(
            "test-user",
            &path,
            EntityCategory::Concept,
            &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:CreativeWork")],
            &format!("Former Corp artifact {index}"),
            "an unrelated named artifact",
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(
            &db,
            "test-user",
            &path,
            "",
            "an unrelated named artifact",
            &json!({}),
        )
        .await
        .unwrap();
        mark_entity_rendered(&db, "test-user", &path).await.unwrap();
    }

    let extract = json!({
        "new_entities": [{"id":"fixture-page-8",
            "path":"orgs/former-corp-inc","name":"Former Corp Inc","description":"the retailer",
            "sources":[{"message":"m1","quote":"Former Corp Inc","strength":"explicit"}]
        }],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Former Corp Inc","strength":"explicit"}],"content":"Former Corp Inc reported earnings","entities":["orgs/former-corp-inc"]}]
    });
    let classify = json!({
        "classes": [{"class": "schema:Organization"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Former Corp Inc", "description": "the retailer", "aliases": []
        }
    });
    // resolve adjudication uses the investigator path, which falls back to a plain
    // structured call in tests → the mock returns the last ToolCalls tuple's args.
    let resolve = json!({
        "canonical":"orgs/former-corp",
        "same_as":[],
        "merge_because":[{
            "candidate":"orgs/former-corp",
            "reason":"same_grounded_identity",
            "evidence":[
                {"side":"subject", "field":"name", "quote":"Former Corp Inc"},
                {"side":"candidate", "field":"name", "quote":"Former Corp"}
            ]
        }],
        "distinct_because":[]
    });
    let classify_me = json!({
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Casey Owner", "description": "The account owner.", "aliases": []}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![(
            "read-candidate".into(),
            "read_entity".into(),
            json!({"path":"orgs/former-corp-inc"}),
        )]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), resolve)]), // adjudication
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::ToolCalls(vec![(
            "author-search".into(),
            "search_entities".into(),
            json!({"query":"Former Corp"}),
        )]),
        MockResponse::Text("# Former Corp\n\nFormer Corp reported earnings.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Former Corp Inc reported earnings.",
        harness,
    )
    .await
    .unwrap();
    assert_eq!(
        stats.entities_merged, 1,
        "Resolve accepted one identity merge"
    );
    let histories = mock.histories();
    assert!(
        histories
            .iter()
            .any(|history| history.iter().any(|message| {
                let rendered = format!("{message:?}");
                rendered.contains("read-candidate")
                    && rendered.contains("orgs/former-corp-inc")
                    && rendered.contains("the retailer")
                    && rendered.contains("Former Corp Inc reported earnings")
                    && rendered.contains("not authored yet")
                    && !rendered.contains("file not found")
                    && !rendered.contains("tool error")
            })),
        "Resolve read_entity returned the effective candidate: {histories:?}"
    );
    let resolve_tools = histories
        .iter()
        .zip(mock.tool_histories())
        .find_map(|(history, tools)| {
            history
                .iter()
                .any(|message| {
                    format!("{message:?}").contains("Subject path: orgs/former-corp-inc")
                })
                .then_some(tools)
        })
        .expect("Resolve model request");
    let tool_names: std::collections::HashSet<_> = resolve_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(tool_names.contains("search_entities"));
    assert!(tool_names.contains("read_entity"));
    for filesystem_tool in ["read", "grep", "glob", "shell"] {
        assert!(
            !tool_names.contains(filesystem_tool),
            "Resolve must not inspect the eventual filesystem projection: {tool_names:?}"
        );
    }
    let author_tools = histories
        .iter()
        .zip(mock.tool_histories())
        .find_map(|(history, tools)| {
            history
                .iter()
                .any(|message| format!("{message:?}").contains("Page name: Former Corp"))
                .then_some(tools)
        })
        .expect("Page Author model request");
    let author_tool_names: std::collections::HashSet<_> =
        author_tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(author_tool_names.contains("search_entities"));
    assert!(author_tool_names.contains("read_entity"));
    for filesystem_tool in ["read", "grep", "glob", "shell"] {
        assert!(
            !author_tool_names.contains(filesystem_tool),
            "Page Author must not inspect the eventual filesystem projection: {author_tool_names:?}"
        );
    }
    assert!(
        histories
            .iter()
            .any(|history| history.iter().any(|message| {
                let rendered = format!("{message:?}");
                rendered.contains("author-search")
                    && rendered.contains("orgs/former-corp")
                    && !rendered.contains("tool error")
            })),
        "Page Author searched the effective page state: {histories:?}"
    );

    // The variant merged into the canonical; the variant name is now an alias.
    assert!(
        repo.entity_by_path("test-user", "orgs/former-corp-inc")
            .await
            .unwrap()
            .is_none(),
        "the variant-named mention merged into the canonical page"
    );
    let canon = repo
        .entity_by_path("test-user", "orgs/former-corp")
        .await
        .unwrap()
        .unwrap();
    assert!(
        canon.aliases.contains("Former Corp Inc"),
        "variant folded into aliases: {:?}",
        canon.aliases
    );
    assert!(
        repo.memories_for_entity("test-user", "orgs/former-corp")
            .await
            .unwrap()
            .iter()
            .any(|m| m.content.contains("reported earnings")),
        "the mention's fact rode onto the canonical"
    );
}

/// A strong exact-name candidate may remain separate only when Resolve cites grounded
/// evidence for both roles. This keeps an assistant distinct from its visual avatar.
#[tokio::test]
async fn resolve_accepts_grounded_distinction_evidence() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    for (path, kind, name, description) in [
        ("people/me", "schema:Person", "Casey Owner", "the owner"),
        (
            "assistants/example-avatar",
            "schema:SoftwareApplication",
            "Example Avatar",
            "A personal AI assistant that helps the user.",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[px.expand(kind)],
            name,
            description,
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(&db, "test-user", path, "", description, &json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }

    let extract = json!({
        "new_entities": [{"id":"fixture-page-9",
            "path":"avatars/example-avatar", "name":"Example Avatar",
            "description":"A plush puppy avatar representing the assistant.",
            "sources":[{"message":"m1","quote":"Example Avatar avatar","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Example Avatar avatar","strength":"explicit"}],
            "content":"The Example Avatar avatar is a plush puppy.",
            "entities":["avatars/example-avatar"]
        }]
    });
    let classify = json!({
        "classes": [{"class":"schema:CreativeWork"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name":"Example Avatar", "description":"A plush puppy avatar representing the assistant.",
            "aliases":[]
        }
    });
    let distinct = json!({
        "canonical":"",
        "same_as":[],
        "merge_because":[],
        "distinct_because":[{
            "candidate":"assistants/example-avatar",
            "reason":"representation_or_role",
            "evidence":[
                {"side":"subject", "field":"description", "quote":"plush puppy avatar"},
                {"side":"candidate", "field":"description", "quote":"personal AI assistant"}
            ]
        }]
    });
    let mut ungrounded_distinct = distinct.clone();
    ungrounded_distinct["distinct_because"][0]["evidence"][1]["quote"] =
        json!("a nonexistent robot avatar");
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), ungrounded_distinct)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), distinct)]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text(
            "# Example Avatar\n\nA plush puppy avatar representing the assistant.".into(),
        ),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "The Example Avatar avatar is a plush puppy.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(stats.entities_merged, 0);
    assert_eq!(stats.resolve_distinct_with_evidence, 1);
    assert_eq!(stats.resolve_evidence_corrections, 1);
    assert!(
        repo.entity_by_path("test-user", "assistants/example-avatar")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "avatars/example-avatar")
            .await
            .unwrap()
            .is_some()
    );
}

/// Evidence discovered by reading the effective page is valid grounding even when the
/// candidate-search projection did not include that assertion in Resolve's initial input.
#[tokio::test]
async fn resolve_accepts_grounding_from_effective_entity_state() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    repo.upsert_entity_skeleton(
        "test-user",
        "devices/atlas-existing",
        EntityCategory::Concept,
        &[px.expand("schema:Product")],
        "Atlas",
        "an established Atlas device",
        &[],
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "devices/atlas-existing")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-10",
            "path":"devices/atlas-new", "name":"Atlas", "description":"another Atlas device",
            "sources":[{"message":"m1","quote":"Atlas device with serial A-1","strength":"explicit"}],
            "candidate_attributes":[{
                "key":"serial number", "value":"A-1",
                "sources":[{"message":"m1","quote":"serial A-1","strength":"explicit"}]
            }]
        }],
        "existing_entity_updates": [{
            "path":"devices/atlas-existing",
            "candidate_attributes":[{
                "key":"serial number", "value":"B-2",
                "sources":[{"message":"m1","quote":"Atlas device with serial B-2","strength":"explicit"}]
            }]
        }],
        "memories": [
            {
                "kind":"fact",
                "sources":[{"message":"m1","quote":"Atlas device with serial A-1","strength":"explicit"}],
                "content":"One Atlas device has serial A-1.", "entities":["devices/atlas-new"]
            },
            {
                "kind":"fact",
                "sources":[{"message":"m1","quote":"Atlas device with serial B-2","strength":"explicit"}],
                "content":"The established Atlas device has serial B-2.",
                "entities":["devices/atlas-existing"]
            }
        ]
    });
    let classify = json!({
        "classes":[{"class":"schema:Product"}],
        "relations":[],
        "attributes":[{"from":"serial number", "to":"schema:serialNumber", "targets":[]}],
        "new_entities":[], "declarations":[],
        "entity":{"name":"Atlas", "description":"another Atlas device", "aliases":[]}
    });
    let classify_me = json!({
        "classes":[{"class":"schema:Person"}],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[],
        "entity":{"name":"Casey Owner", "description":"The account owner.", "aliases":[]}
    });
    let distinct = json!({
        "canonical":"", "same_as":[], "merge_because":[],
        "distinct_because":[{
            "candidate":"devices/atlas-new",
            "reason":"conflicting_unique_identifier",
            "evidence":[
                {"side":"subject", "field":"attributes", "property":"schema:serialNumber", "value":"B-2"},
                {"side":"candidate", "field":"attributes", "property":"schema:serialNumber", "value":"A-1"}
            ]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![(
            "read-atlas".into(),
            "read_entity".into(),
            json!({"path":"devices/atlas-new"}),
        )]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), distinct)]),
        MockResponse::ToolCalls(vec![(
            "reconcile-1".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile-2".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Atlas\n\nAn established Atlas device with serial B-2.".into()),
        MockResponse::Text("# Atlas\n\nAn Atlas device with serial A-1.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Atlas device with serial A-1 and Atlas device with serial B-2.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(
        stats.resolve_evidence_corrections, 0,
        "an assertion read from effective state must be accepted without resubmission"
    );
    assert_eq!(
        stats.resolve_unresolved_pairs, 0,
        "tool-grounded attribute evidence must produce a completed Resolve verdict"
    );
    assert_eq!(stats.entities_merged, 0);
    assert!(
        repo.entity_by_path("test-user", "devices/atlas-existing")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "devices/atlas-new")
            .await
            .unwrap()
            .is_some()
    );
    let histories = mock.histories();
    assert!(
        histories.iter().any(|history| {
            let rendered = format!("{history:?}");
            rendered.contains("Subject path: devices/atlas-existing")
                && rendered.contains("read-atlas")
                && rendered.contains("schema:serialNumber")
                && rendered.contains("A-1")
        }),
        "Resolve must read the candidate's effective attribute state"
    );
}

#[tokio::test]
async fn resolve_revises_a_merge_that_invalidates_the_projected_graph() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    for (path, kind, name, description) in [
        ("people/me", "schema:Person", "Casey Owner", "the owner"),
        (
            "records/alex",
            "frona:NeutralRecord",
            "Alex",
            "a corporate registry record",
        ),
        (
            "organizations/registry",
            "schema:Organization",
            "Registry",
            "the registry",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[px.expand(kind)],
            name,
            description,
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(&db, "test-user", path, "", description, &json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }
    repo.create_memory_with_entities(
        "test-user",
        "test-agent",
        "older-chat",
        MemoryKind::Fact,
        "Alex is a registry record.",
        &["records/alex".into()],
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &db,
        "test-user",
        "records/alex",
        "organizations/registry",
        "frona:registeredBy",
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "records/alex")
        .await
        .unwrap();
    ontology_manager(&db)
        .commit(
            "test-user",
            &[
                SchemaEdit::DeclareClass {
                    class: "frona:NeutralRecord".into(),
                },
                SchemaEdit::DeclareObjectProperty {
                    property: "frona:registeredBy".into(),
                },
                SchemaEdit::ObjectPropertyDomain {
                    property: "frona:registeredBy".into(),
                    class: "schema:Organization".into(),
                },
                SchemaEdit::ObjectPropertyRange {
                    property: "frona:registeredBy".into(),
                    class: "schema:Organization".into(),
                },
            ],
        )
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-11",
            "path":"people/alex", "name":"Alex", "description":"a person",
            "sources":[{"message":"m1","quote":"Alex","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Alex","strength":"explicit"}],
            "content":"Alex is a person.",
            "entities":["people/alex"]
        }]
    });
    let classify = json!({
        "classes":[{"class":"schema:Person"}],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[],
        "entity":{"name":"Alex","description":"a person","aliases":[]}
    });
    let invalid_merge = json!({
        "canonical":"records/alex", "same_as":[],
        "merge_because":[{
            "candidate":"records/alex", "reason":"same_grounded_identity",
            "evidence":[
                {"side":"subject","field":"name","quote":"Alex"},
                {"side":"candidate","field":"name","quote":"Alex"}
            ]
        }],
        "distinct_because":[]
    });
    let distinct = json!({
        "canonical":"", "same_as":[], "merge_because":[],
        "distinct_because":[{
            "candidate":"records/alex", "reason":"different_entity_role",
            "evidence":[
                {"side":"subject","field":"description","quote":"a person"},
                {"side":"candidate","field":"description","quote":"corporate registry record"}
            ]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), invalid_merge)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), distinct)]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Alex\n\nAlex is a person.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Alex is a person.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(stats.entities_merged, 0);
    assert!(stats.resolve_evidence_corrections >= 1);
    assert!(
        repo.entity_by_path("test-user", "people/alex")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "records/alex")
            .await
            .unwrap()
            .is_some()
    );
}

/// Resolve can discover that more than one existing candidate is the same person. The
/// explicit canonical page survives while the current mention and every listed duplicate
/// contribute their memories and aliases to it.
#[tokio::test]
async fn resolve_coalesces_multiple_existing_candidates() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let person = frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person");

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[person.clone()],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    for (path, name, fact) in [
        (
            "people/jordan-lee-example",
            "Jordan Lee Example",
            "Jordan Lee Example is Casey Owner's parent.",
        ),
        (
            "people/jordan-example",
            "Jordan Example",
            "Jordan Example traveled on booking BOOKING-001.",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[person.clone()],
            name,
            "a member of Casey Owner's family",
            &["Jordan".into()],
        )
        .await
        .unwrap();
        repo.create_memory_with_entities(
            "test-user",
            "test-agent",
            "older-chat",
            MemoryKind::Fact,
            fact,
            &[path.into()],
        )
        .await
        .unwrap();
        seed_reconciled_entity(
            &db,
            "test-user",
            path,
            "",
            "a member of Casey Owner's family",
            &json!({}),
        )
        .await
        .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }

    let extract = json!({
        "new_entities": [{"id":"fixture-page-12",
            "path":"people/jordan",
            "name":"Jordan",
            "description":"A person tracked in the family babysitter list.",
            "sources":[{"message":"m1","quote":"Jordan","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Jordan","strength":"explicit"}],
            "content":"Jordan is listed in the family babysitter tracker.",
            "entities":["people/jordan"]
        }]
    });
    let classify = json!({
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Jordan",
            "description": "A person tracked in the family babysitter list.",
            "aliases": []
        }
    });
    let resolve = json!({
        "canonical": "people/jordan-lee-example",
        "same_as": ["people/jordan-example"],
        "merge_because": [
            {
                "candidate":"people/jordan-lee-example",
                "reason":"same_grounded_identity",
                "evidence":[
                    {"side":"subject", "field":"name", "quote":"Jordan"},
                    {"side":"candidate", "field":"name", "quote":"Jordan"}
                ]
            },
            {
                "candidate":"people/jordan-example",
                "reason":"same_grounded_identity",
                "evidence":[
                    {"side":"subject", "field":"name", "quote":"Jordan"},
                    {"side":"candidate", "field":"name", "quote":"Jordan"}
                ]
            }
        ],
        "distinct_because": []
    });
    let classify_me = json!({
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Casey Owner", "description": "The account owner.", "aliases": []}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), resolve)]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Jordan Lee Example\n\nJordan is Casey Owner's parent.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Jordan is listed in the family babysitter tracker.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(
        stats.entities_merged, 2,
        "the mention and existing duplicate both merge"
    );
    assert!(
        repo.entity_by_path("test-user", "people/jordan")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.entity_by_path("test-user", "people/jordan-example")
            .await
            .unwrap()
            .is_none()
    );
    let canonical = repo
        .entity_by_path("test-user", "people/jordan-lee-example")
        .await
        .unwrap()
        .expect("canonical Jordan page");
    assert!(canonical.aliases.contains("Jordan"));
    assert!(
        canonical.aliases.contains("Jordan Example"),
        "aliases={:?}",
        canonical.aliases
    );
    let facts: Vec<String> = repo
        .memories_for_entity("test-user", "people/jordan-lee-example")
        .await
        .unwrap()
        .into_iter()
        .map(|memory| memory.content)
        .collect();
    assert_eq!(
        facts.len(),
        3,
        "all three identities contribute their memories: {facts:?}"
    );
}

/// An evidence-exhausted subject remains available to a later Resolve conversation. A
/// subsequent subject can still coalesce that page and another candidate into one
/// canonical identity instead of treating the unresolved verdict as permanent.
#[tokio::test]
async fn resolve_later_subject_can_merge_an_unresolved_candidate() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let reservation =
        frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Reservation");

    repo.upsert_entity_skeleton(
        "test-user",
        "bookings/canonical",
        EntityCategory::Concept,
        &[reservation.clone()],
        "Canonical Reservation",
        "A reservation tracked under its canonical record.",
        &["identity bridge one".into()],
    )
    .await
    .unwrap();
    repo.create_memory_with_entities(
        "test-user",
        "test-agent",
        "older-chat",
        MemoryKind::Fact,
        "The canonical reservation exists.",
        &["bookings/canonical".into()],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "bookings/canonical",
        "",
        "A reservation tracked under its canonical record.",
        &json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "bookings/canonical")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-13",
                "path":"bookings/alias-a",
                "name":"Identity Bridge Two",
                "description":"A later mention of the same reservation.",
                "sources":[{"message":"m2","quote":"Identity Bridge Two","strength":"explicit"}]
            },
            {"id":"fixture-page-14",
                "path":"bookings/alias-b",
                "name":"Identity Bridge One",
                "description":"Another mention of the canonical reservation.",
                "aliases":["Identity Bridge Two"],
                "sources":[
                    {"message":"m1","quote":"Identity Bridge One","strength":"explicit"},
                    {"message":"m2","quote":"Identity Bridge Two","strength":"explicit"}
                ]
            }
        ],
        "memories": [
            {
                "kind":"fact",
                "sources":[{"message":"m1","quote":"Identity Bridge One","strength":"explicit"}],
                "content":"Identity Bridge One refers to the tracked reservation.",
                "entities":["bookings/alias-b"]
            },
            {
                "kind":"fact",
                "sources":[{"message":"m2","quote":"Identity Bridge Two","strength":"explicit"}],
                "content":"Identity Bridge Two refers to the tracked reservation.",
                "entities":["bookings/alias-a"]
            }
        ]
    });
    let classify_a = json!({
        "classes": [{"class": "schema:Reservation"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Identity Bridge Two",
            "description": "A later mention of the same reservation.",
            "aliases": []
        }
    });
    let classify_b = json!({
        "classes": [{"class": "schema:Reservation"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Identity Bridge One",
            "description": "Another mention of the canonical reservation.",
            "aliases": ["Identity Bridge Two"]
        }
    });
    let resolve = json!({
        "canonical":"bookings/canonical", "same_as":["bookings/alias-a"],
        "merge_because":[
            {
                "candidate":"bookings/canonical",
                "reason":"same_grounded_identity",
                "evidence":[
                    {"side":"subject", "field":"name", "quote":"Identity Bridge One"},
                    {"side":"candidate", "field":"aliases", "quote":"identity bridge one"}
                ]
            },
            {
                "candidate":"bookings/alias-a",
                "reason":"same_grounded_identity",
                "evidence":[
                    {"side":"subject", "field":"aliases", "quote":"Identity Bridge Two"},
                    {"side":"candidate", "field":"name", "quote":"Identity Bridge Two"}
                ]
            }
        ],
        "distinct_because":[]
    });
    let distinct = json!({
        "canonical":"", "same_as":[], "merge_because":[],
        "distinct_because":[]
    });
    let classify_me = json!({
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Casey Owner", "description": "The account owner.", "aliases": []}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("ca1".into(), "submit".into(), classify_a.clone())]),
        MockResponse::ToolCalls(vec![("ca2".into(), "submit".into(), classify_a)]),
        MockResponse::ToolCalls(vec![("cb1".into(), "submit".into(), classify_b.clone())]),
        MockResponse::ToolCalls(vec![("cb2".into(), "submit".into(), classify_b)]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![("ra1".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra2".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra3".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra4".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra5".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra6".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra7".into(), "submit".into(), distinct.clone())]),
        MockResponse::ToolCalls(vec![("ra8".into(), "submit".into(), distinct)]),
        MockResponse::ToolCalls(vec![("rb".into(), "submit".into(), resolve.clone())]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text(
            "# Canonical Reservation\n\nThe tracked reservation was previously recorded as \
             [[bookings/alias-a|Identity Bridge Two]]."
                .into(),
        ),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Identity Bridge One refers to a reservation.\nIdentity Bridge Two refers to the same reservation.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(
        stats.entities_merged, 2,
        "both aliases merge into the canonical page: {stats:?}"
    );
    assert_eq!(
        stats.resolve_sweeps, 2,
        "one initial and one incremental Resolve sweep"
    );
    assert_eq!(stats.resolve_candidate_evaluations, 3, "stats={stats:?}");
    assert_eq!(stats.resolve_candidate_evaluations_after_first_sweep, 1);
    assert_eq!(stats.resolve_decision_attempts, 2);
    assert_eq!(stats.resolve_fingerprint_skips, 0);
    assert_eq!(stats.resolve_conversations, 2);
    assert_eq!(stats.resolve_reconsiderations, 0);
    assert_eq!(stats.resolve_reconsideration_conversations, 0);
    assert_eq!(stats.resolve_merges_after_first_sweep, 0);
    assert_eq!(stats.resolve_merges_with_evidence, 2);
    assert_eq!(stats.resolve_evidence_corrections, 8);
    assert_eq!(stats.resolve_unresolved_pairs, 2);
    assert!(
        repo.entity_by_path("test-user", "bookings/alias-a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.entity_by_path("test-user", "bookings/alias-b")
            .await
            .unwrap()
            .is_none()
    );
    let canonical = repo
        .entity_by_path("test-user", "bookings/canonical")
        .await
        .unwrap()
        .expect("canonical reservation page");
    assert!(canonical.aliases.contains("Identity Bridge One"));
    assert!(canonical.aliases.contains("Identity Bridge Two"));
    let facts = repo
        .memories_for_entity("test-user", "bookings/canonical")
        .await
        .unwrap();
    assert_eq!(
        facts.len(),
        3,
        "both alias memories survive on the canonical page"
    );
    let authored = std::fs::read_to_string(
        tmp.path()
            .join("users/testuser/pkm/Memory/bookings/canonical.md"),
    )
    .expect("canonical page authored");
    assert!(
        authored.contains("[[bookings/canonical|Identity Bridge Two]]"),
        "authored prose rewrites a losing identity to its canonical path: {authored}",
    );
    assert!(
        !authored.contains("[[bookings/alias-a"),
        "no authored wikilink may retain a losing identity path: {authored}",
    );
}

/// Reconcile can reveal identity evidence that was unavailable during the initial
/// Resolve sweep. That changed page must be resolved again, and an accepted merge must
/// put the canonical winner back through Reconcile with the combined memories.
#[tokio::test]
async fn reconcile_identity_change_resolves_and_reconciles_the_merge_winner() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let reservation =
        frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Reservation");

    repo.upsert_entity_skeleton(
        "test-user",
        "bookings/canonical",
        EntityCategory::Concept,
        &[reservation],
        "Identity Bridge One",
        "The canonical reservation record.",
        &[],
    )
    .await
    .unwrap();
    repo.create_memory_with_entities(
        "test-user",
        "test-agent",
        "older-chat",
        MemoryKind::Fact,
        "Identity Bridge One is the canonical reservation.",
        &["bookings/canonical".into()],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "bookings/canonical",
        "",
        "The canonical reservation record.",
        &json!({"schema:email": "bridge-1@example.test"}),
    )
    .await
    .unwrap();
    db.query(
        "UPDATE knowledge_entity SET search_assertions = $assertions
         WHERE user_id = $uid AND path = $path",
    )
    .bind((
        "assertions",
        vec![json!(["attribute", "schema:email", "bridge-1@example.test"]).to_string()],
    ))
    .bind(("uid", "test-user".to_string()))
    .bind(("path", "bookings/canonical".to_string()))
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "bookings/canonical")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-15",
            "path": "bookings/identity-bridge-two",
            "name": "Identity Bridge Two",
            "description": "A reservation whose canonical identity is not yet settled.",
            "candidate_attributes": [{
                "key":"booking contact", "value":"pending@example.test",
                "sources":[{"message":"m1","quote":"booking contact pending@example.test","strength":"explicit"}]
            }],
            "sources": [{"message":"m1","quote":"Identity Bridge Two","strength":"explicit"}]
        }],
        "memories": [{
            "kind": "fact",
            "sources": [{"message":"m1","quote":"Identity Bridge Two uses booking contact pending@example.test and is the same reservation as bridge-1@example.test","strength":"explicit"}],
            "content": "Identity Bridge Two uses booking contact pending@example.test and is the same reservation as bridge-1@example.test.",
            "entities": ["bookings/identity-bridge-two"]
        }]
    });
    let classify = json!({
        "classes": [{"class": "schema:Reservation"}],
        "relations": [],
        "attributes": [{
            "from":"booking contact", "to":"schema:email", "targets":[]
        }],
        "new_entities": [], "declarations": [],
        "has_keys": [{
            "class": "schema:Reservation",
            "properties": ["schema:email"]
        }],
        "inverse_functional_properties": [],
        "entity": {
            "name": "Identity Bridge Two",
            "description": "A reservation whose canonical identity is not yet settled.",
            "aliases": []
        }
    });
    let classify_me = json!({
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "has_keys": [], "inverse_functional_properties": [],
        "entity": {"name": "Casey Owner", "description": "The account owner.", "aliases": []}
    });
    let reconcile_identity = json!({
        "relations": [], "entity_relations": [], "relation_retractions": [],
        "entity_relation_replacements": [], "outdated": [],
        "attributes": {"schema:email": "bridge-1@example.test"},
        "attribute_sources": [{
            "property": "schema:email",
            "value": "bridge-1@example.test",
            "source_memory_ids": ["m2"]
        }],
        "attribute_replacements": [],
        "name": "Identity Bridge Two",
        "description": "This is the Identity Bridge One canonical reservation record.",
        "moves": [], "declarations": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![(
            "initial-distinct".into(),
            "submit".into(),
            json!({
                "canonical":"", "same_as":[],
                "merge_because":[],
                "distinct_because":[{
                    "candidate":"bookings/canonical",
                    "reason":"conflicting_unique_identifier",
                    "evidence":[
                        {"side":"subject","field":"attributes","property":"schema:email","value":"pending@example.test"},
                        {"side":"candidate","field":"attributes","property":"schema:email","value":"bridge-1@example.test"}
                    ]
                }]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile-identity".into(),
            "submit".into(),
            reconcile_identity,
        )]),
        MockResponse::ToolCalls(vec![(
            "incremental-merge".into(),
            "submit".into(),
            json!({
                "canonical":"bookings/canonical", "same_as":[],
                "merge_because":[{
                    "candidate":"bookings/canonical",
                    "reason":"same_unique_identifier",
                    "evidence":[
                        {"side":"subject", "field":"assertions", "quote":"bridge-1@example.test"},
                        {"side":"candidate", "field":"assertions", "quote":"bridge-1@example.test"}
                    ]
                }],
                "distinct_because":[]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile-winner".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Identity Bridge One\n\nThe canonical reservation record.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let result = full_pass(
        &service,
        ontology_scope(&service),
        "Identity Bridge Two uses booking contact pending@example.test and is the same reservation as bridge-1@example.test.",
        harness,
    )
    .await;
    assert!(result.is_ok(), "{result:?}\n{:#?}", mock.histories());
    let stats = result.unwrap();

    assert_eq!(
        stats.entities_merged,
        1,
        "Reconcile-triggered Resolve merged the identity; calls={} stats={stats:?}\n{:#?}",
        mock.calls(),
        mock.histories(),
    );
    assert_eq!(
        stats.resolve_sweeps, 2,
        "one initial and one incremental Resolve sweep"
    );
    assert_eq!(stats.resolve_reconsideration_conversations, 1);
    assert_eq!(
        stats.resolve_identity_state_changes, 2,
        "the duplicate changes once, then the combined winner is reconsidered",
    );
    assert_eq!(stats.resolve_identity_pair_changes, 1);
    assert_eq!(stats.resolve_identity_pair_weakenings, 0);
    assert_eq!(
        stats.entities_reconciled, 2,
        "the merged winner was reconciled again"
    );
    assert!(
        repo.entity_by_path("test-user", "bookings/identity-bridge-two")
            .await
            .unwrap()
            .is_none(),
        "the reconciled duplicate must not survive",
    );
    let memories = repo
        .memories_for_entity("test-user", "bookings/canonical")
        .await
        .unwrap();
    assert_eq!(
        memories.len(),
        3,
        "the winner owns its original memory and both extracted contributions",
    );
}

#[tokio::test]
async fn missing_update_only_path_never_reaches_classify_or_page_author() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    let extract = json!({
        "new_entities": [{"id":"fixture-page-16",
            "path":"services/retailer",
            "name":"Retailer",
            "description":"The online shopping service that processed the return.",
            "sources":[{"message":"m1","quote":"Retailer","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Retailer processed the return","strength":"explicit"}],
            "content":"Retailer processed the return.",
            "entities":["services/retailer","organizations/retailer"]
        }]
    });
    let classify = json!({
        "classes": [{"class": "schema:OnlineStore"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Retailer",
            "description": "The online shopping service that processed the return.",
            "aliases": []
        }
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), classify.clone())]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("# Retailer\n\nRetailer processed the return.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Retailer processed the return.",
        harness,
    )
    .await
    .unwrap();

    assert!(
        repo.entity_by_path("test-user", "services/retailer")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "organizations/retailer")
            .await
            .unwrap()
            .is_none()
    );
    let vault = tmp.path().join("users/testuser/pkm/Memory");
    assert!(vault.join("services/retailer.md").exists());
    assert!(!vault.join("organizations/retailer.md").exists());
}

#[tokio::test]
async fn resolve_merges_reversed_sports_event_participants() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let sports_event =
        frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:SportsEvent");

    for (path, kind, name, description) in [
        (
            "people/me",
            frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person"),
            "Casey Owner",
            "the owner",
        ),
        (
            "events/exampleland-vs-samplestan",
            sports_event,
            "Exampleland vs Samplestan",
            "The match Casey Owner planned to attend in Example City.",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[kind],
            name,
            description,
            &[],
        )
        .await
        .unwrap();
    }
    let planned_memory = repo
        .create_memory_with_entities(
            "test-user",
            "test-agent",
            "older-chat",
            MemoryKind::Fact,
            "Casey Owner planned to attend the Exampleland vs Samplestan match in Example City.",
            &["events/exampleland-vs-samplestan".into()],
        )
        .await
        .unwrap();
    for path in ["people/me", "events/exampleland-vs-samplestan"] {
        seed_reconciled_entity(&db, "test-user", path, "", "clean", &json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }

    let extract = json!({
        "new_entities": [{"id":"fixture-page-17",
            "path":"events/samplestan-exampleland-example-city-2030-01-01",
            "name":"Samplestan vs Exampleland — Example City, January 1 2030",
            "description":"The Samplestan versus Exampleland match Casey Owner attended in Example City.",
            "sources":[{
                "message":"m1",
                "quote":"Samplestan vs Exampleland — Example City, January 1 2030",
                "strength":"explicit"
            }]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Samplestan vs Exampleland — Example City, January 1 2030","strength":"explicit"}],
            "content":"Casey Owner attended Samplestan vs Exampleland in Example City.",
            "entities":["events/samplestan-exampleland-example-city-2030-01-01"]
        }]
    });
    let classify = json!({
        "classes": [{"class": "schema:SportsEvent"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {
            "name": "Samplestan vs Exampleland — Example City, January 1 2030",
            "description": "The Samplestan versus Exampleland match Casey Owner attended in Example City.",
            "aliases": []
        }
    });
    let resolve = json!({
        "canonical":"events/exampleland-vs-samplestan", "same_as":[],
        "merge_because":[{
            "candidate":"events/exampleland-vs-samplestan",
            "reason":"same_event_identity",
            "evidence":[
                {"side":"subject", "field":"name", "quote":"Samplestan vs Exampleland"},
                {"side":"candidate", "field":"name", "quote":"Exampleland vs Samplestan"}
            ]
        }],
        "distinct_because":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("i".into(), "submit".into(), resolve)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text(
            "# Exampleland vs Samplestan\n\nCasey Owner attended the match in Example City.".into(),
        ),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Casey Owner attended Samplestan vs Exampleland — Example City, January 1 2030.",
        harness,
    )
    .await
    .unwrap();
    assert_eq!(
        stats.entities_merged, 1,
        "Resolve accepted one identity merge"
    );

    assert!(
        repo.entity_by_path(
            "test-user",
            "events/samplestan-exampleland-example-city-2030-01-01",
        )
        .await
        .unwrap()
        .is_none()
    );
    let canonical = repo
        .entity_by_path("test-user", "events/exampleland-vs-samplestan")
        .await
        .unwrap()
        .expect("canonical event page");
    assert!(
        canonical
            .aliases
            .contains("Samplestan vs Exampleland — Example City, January 1 2030"),
        "the losing event name survives as an alias: {:?}",
        canonical.aliases,
    );
    let memory_ids: std::collections::HashSet<String> = repo
        .memories_for_entity("test-user", "events/exampleland-vs-samplestan")
        .await
        .unwrap()
        .into_iter()
        .map(|memory| memory.id)
        .collect();
    assert!(memory_ids.contains(&planned_memory));
    assert_eq!(
        memory_ids.len(),
        2,
        "both chats contribute to the canonical event"
    );
}

/// The schema-satisfaction loop: the model first submits a contradictory class, the
/// SYSTEM's reasoner check rejects it, the rejection is fed back, and the model's
/// revised submission (a consistent class) is accepted - all in one non-persistent
/// conversation. Proves external validation drives the revision.
#[tokio::test]
async fn classify_loop_revises_after_reasoner_rejection() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();

    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "the owner",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-18",
            "path":"orgs/acme","name":"Acme","description":"a company",
            "sources":[{"message":"m1","quote":"Acme","strength":"explicit"}]
        }],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Acme","strength":"explicit"}],"content":"Acme is a company","entities":["orgs/acme"]}]
    });
    // First a contradictory class (⊑ two disjoint bases → unsatisfiable), then a good one.
    let bad = json!({
        "classes": [{"class": "frona:Confused"}],
        "entity": {"name":"Acme","description":"a company","aliases":[]}
    });
    let good = json!({
        "classes": [{"class": "schema:Organization"}],
        "entity": {"name":"Acme","description":"a company","aliases":[]}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("s1".into(), "submit".into(), bad)]),
        MockResponse::ToolCalls(vec![("s2".into(), "submit".into(), good)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let ontology_manager = ontology_manager(&db);

    // Seed the contradictory class so the FIRST submission is guaranteed to clash.
    ontology_manager
        .commit(
            "test-user",
            &[
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Person".into(),
                },
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Organization".into(),
                },
            ],
        )
        .await
        .unwrap();

    let harness = test_harness(&db, &config, mock.clone());
    full_pass(
        &service,
        ontology_scope(&service),
        "Acme is a company.",
        harness,
    )
    .await
    .unwrap();

    // The REVISED class was accepted (the loop recovered); the fact is NOT quarantined.
    let acme = repo
        .entity_by_path("test-user", "orgs/acme")
        .await
        .unwrap()
        .expect("acme page");
    assert_eq!(
        acme.kinds,
        [frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization")],
        "loop revised to the consistent class"
    );
    let mems = repo
        .memories_for_entity("test-user", "orgs/acme")
        .await
        .unwrap();
    assert!(
        mems.iter().all(|m| m.disposition == Disposition::None),
        "no quarantine — the revised classification validated"
    );
}

#[tokio::test]
async fn classify_validates_minted_entities_in_the_same_submission() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();
    ontology_manager(&db)
        .commit(
            "test-user",
            &[
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Person".into(),
                },
                SchemaEdit::SubClassOf {
                    sub: "frona:Confused".into(),
                    sup: "schema:Organization".into(),
                },
            ],
        )
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-19",
            "path":"organizations/acme", "name":"Acme", "description":"a company",
            "sources":[{"message":"m1","quote":"Acme","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Acme","strength":"explicit"}],
            "content":"Acme is a company.", "entities":["organizations/acme"]
        }]
    });
    let bad = json!({
        "classes":[{"class":"schema:Organization"}],
        "relations":[], "attributes":[], "declarations":[],
        "new_entities":[{
            "path":"people/confused", "name":"Confused", "description":"an invalid mint",
            "class":"frona:Confused", "from_facts":["f1"]
        }],
        "entity":{"name":"Acme","description":"a company","aliases":[]}
    });
    let good = json!({
        "classes":[{"class":"schema:Organization"}],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[],
        "entity":{"name":"Acme","description":"a company","aliases":[]}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c1".into(), "submit".into(), bad)]),
        MockResponse::ToolCalls(vec![("c2".into(), "submit".into(), good)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("# Acme\n\nA company.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Acme is a company.",
        harness,
    )
    .await
    .unwrap();

    assert!(
        repo.entity_by_path("test-user", "organizations/acme")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "people/confused")
            .await
            .unwrap()
            .is_none(),
        "the invalid mint must never be staged or committed",
    );
    assert_eq!(
        mock.calls(),
        5,
        "Classify stage must request a corrected complete submission"
    );
}

/// A CURIE the schema cannot hold is pushed back and the revision is taken.
///
/// This is the one failure the reasoner cannot catch: `expand` never fails, so
/// `frona:Soldering Iron` becomes an IRI without complaint, the delta is written with OFN
/// that no parser will read back, and from then on **every** schema call for that user
/// throws - permanently, for one bad key. So the check is lexical and runs before any
/// reasoning, and what it must prove is that the bad term never reaches the delta.
#[tokio::test]
async fn term_that_cannot_be_written_is_pushed_back_before_it_reaches_the_schema() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "the owner",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-20","path":"devices/device-x","name":"Device X","description":"a soldering iron",
            "sources":[{"message":"m1","quote":"Device X","strength":"explicit"}],
            "candidate_attributes":[{"key":"firmware version","value":"1.0",
                "sources":[{"message":"m1","quote":"firmware version 1.0","strength":"explicit"}]}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"The Device X is a soldering iron with firmware version 1.0","strength":"explicit"}],"content":"The Device X is a soldering iron with firmware version 1.0","entities":["devices/device-x"]}]
    });
    // A space in the local name, and an attribute under a prefix nothing binds. Both are
    // syntactically fine JSON and semantically plausible - only unwritable.
    let bad = json!({
        "entity":{"name":"Device X","description":"a soldering iron","aliases":[]},
        "classes":[{"class":"frona:Soldering Iron"}],
        "relations":[],
        "attributes":[{"from":"firmware version","to":"dc:firmwareVersion"}],
        "new_entities":[],
        "declarations":[{
            "kind":"class", "term":"frona:Soldering Iron",
            "description":"A soldering iron.", "parents":["schema:Product"]
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let good = json!({
        "entity":{"name":"Device X","description":"a soldering iron","aliases":[]},
        "classes":[{"class":"frona:SolderingIron"}],
        "relations":[],
        "attributes":[{"from":"firmware version","to":"frona:firmwareVersion"}],
        "new_entities":[],
        "declarations":[
            {"kind":"class", "term":"frona:SolderingIron",
             "description":"A soldering iron.", "parents":["schema:Product"]},
            {"kind":"data_property", "term":"frona:firmwareVersion",
             "description":"The firmware version installed on the device.",
             "datatype":"xsd:string"}
        ],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("s1".into(), "submit".into(), bad)]),
        MockResponse::ToolCalls(vec![("s2".into(), "submit".into(), good)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![(
            "a".into(),
            "submit".into(),
            json!({"decisions":[
                {"term":"frona:SolderingIron","decision":"declare","parent":"schema:Product"},
                {"term":"frona:firmwareVersion","decision":"accept_proposal"}
            ]}),
        )]),
        MockResponse::Text("The Device X is a soldering iron.".into()),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let ontology_manager = ontology_manager(&db);

    let harness = test_harness(&db, &config, mock.clone());
    full_pass(
        &service,
        ontology_scope(&service),
        "The Device X is a soldering iron with firmware version 1.0.",
        harness,
    )
    .await
    .unwrap();

    // The revision was taken: the page carries the writable class.
    let page = repo
        .entity_by_path("test-user", "devices/device-x")
        .await
        .unwrap()
        .expect("device-x page");
    assert_eq!(
        page.kinds,
        [px.expand("frona:SolderingIron")],
        "the revised class was accepted"
    );

    // And the point of the whole exercise: nothing unwritable was committed, so the delta
    // still parses. `load` is what every later schema call goes through.
    let delta = ontology_manager
        .serialize("test-user")
        .await
        .expect("the delta must still parse");
    assert!(
        !delta.contains("Soldering Iron"),
        "the unwritable term must not be in the delta: {}",
        delta
    );
    assert!(
        !page.kinds.iter().any(|k| k.contains(' ')),
        "no stored kind may contain a space: {:?}",
        page.kinds
    );
}

/// A decision still counts when the adjudicator spells the term differently from the
/// proposal - which is what every real run does.
///
/// This is the bug that made whole passes commit an empty schema. ProposalSet were listed as
/// whatever spelling reached the pass (`support_email`); the model, having just `test_edit`ed
/// `frona:supportEmail` and been told it was consistent, submitted *that*; the match was an
/// exact string compare, so it missed; and a miss was counted as a deliberate deferral. No
/// warning, no rejection, `Verdict::Accept` with zero edits.
///
/// Every other test in this file spells the term identically on both sides, which is exactly
/// why none of them caught it.
#[tokio::test]
async fn adjudicated_term_matches_its_proposal_across_spellings() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "the owner",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-21","path":"orgs/example-tools","name":"Example Tools","description":"a tool maker",
            "sources":[{"message":"m1","quote":"Example Tools","strength":"explicit"}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Example Tools","strength":"explicit"}],"content":"Example Tools makes soldering irons","entities":["orgs/example-tools"]}]
    });
    // Classify mints in snake_case - legal (an underscore is a legal IRI character), so it
    // is not rejected; it is merely not the house spelling, and repair normalises it.
    let classify = json!({
        "entity":{"name":"Example Tools","description":"a tool maker","aliases":[]},
        "classes":adjudication_classes(json!({"class":"frona:tool_maker"})),
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":adjudication_declarations(json!({
            "kind":"class", "term":"frona:tool_maker",
            "description":"An organization that makes tools.",
            "parents":["schema:Organization"]
        })),
        "has_keys":[],"inverse_functional_properties":[]
    });
    // The adjudicator answers in the spelling it would naturally use - the repaired one.
    let adjudicate = json!({"decisions": adjudication_decisions(json!(
        {"term": "frona:ToolMaker", "decision": "declare", "parent": "schema:Organization"}
    ))});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e1".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
        MockResponse::Text("Example Tools makes soldering irons.".into()),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let ontology_manager = ontology_manager(&db);

    let harness = test_harness(&db, &config, mock.clone());
    let result = full_pass(
        &service,
        ontology_scope(&service),
        "Example Tools makes soldering irons.",
        harness,
    )
    .await;
    assert!(result.is_ok(), "{result:?}\n{:#?}", mock.histories());

    // The decision was applied: the term is declared, so the delta is not empty.
    let declared = ontology_manager.catalog("test-user").await.unwrap().classes;
    assert!(
        declared.contains(&"frona:ToolMaker".to_string()),
        "the decision must apply across the spelling difference; declared: {declared:?}"
    );
    let delta = ontology_manager.serialize("test-user").await.unwrap();
    assert!(
        !delta.is_empty(),
        "an adjudicated term means a non-empty delta"
    );

    // And the page is typed with the one repaired spelling, not two near-duplicates.
    let page = repo
        .entity_by_path("test-user", "orgs/example-tools")
        .await
        .unwrap()
        .expect("page");
    assert!(
        page.kinds.contains(&px.expand("frona:ToolMaker")),
        "house spelling: {:?}",
        page.kinds
    );
}

/// Resolve never applies a merge whose claimed evidence remains absent through the
/// correction budget. The unresolved duplicate is safer than a destructive false merge.
#[tokio::test]
async fn resolve_exhausted_ungrounded_merge_remains_unresolved() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    for (path, kind, name) in [
        ("people/me", "schema:Person", "Casey Owner"),
        ("orgs/former-corp", "schema:Organization", "Former Corp"),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[frona::memory::pkm::ontology::PrefixMap::standard().expand(kind)],
            name,
            "x",
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(&db, "test-user", path, "", "x", &serde_json::json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }
    let extract = json!({
        "new_entities": [{"id":"fixture-page-22",
            "path":"orgs/former-corp-inc","name":"Former Corp Inc","description":"the retailer",
            "sources":[{"message":"m1","quote":"Former Corp Inc","strength":"explicit"}]
        }],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Former Corp Inc","strength":"explicit"}],"content":"Former Corp Inc reported earnings","entities":["orgs/former-corp-inc"]}]
    });
    let classify = json!({
        "classes": [{"class": "schema:Organization"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "entity": {"name": "Former Corp Inc", "description": "the retailer", "aliases": []}
    });
    let bogus = json!({
        "canonical":"orgs/former-corp",
        "same_as":[],
        "merge_because":[{
            "candidate":"orgs/former-corp",
            "reason":"same_grounded_identity",
            "evidence":[
                {"side":"subject", "field":"name", "quote":"Former Corp Inc"},
                {"side":"candidate", "field":"name", "quote":"Globex"}
            ]
        }],
        "distinct_because":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r3".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r4".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r5".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r6".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r7".into(), "submit".into(), bogus.clone())]),
        MockResponse::ToolCalls(vec![("r8".into(), "submit".into(), bogus)]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            empty_reconcile(),
        )]),
        MockResponse::Text("# Former Corp Inc\n\nFormer Corp Inc reported earnings.".into()),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());

    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Former Corp Inc reported earnings.",
        harness,
    )
    .await
    .unwrap();

    assert_eq!(stats.resolve_evidence_corrections, 8);
    assert_eq!(stats.resolve_unresolved_pairs, 1);

    // The unsafe merge was never applied; both identities remain available for repair.
    assert!(
        repo.entity_by_path("test-user", "orgs/former-corp-inc")
            .await
            .unwrap()
            .is_some(),
        "the mention must survive when Resolve exhausts its evidence corrections"
    );
    let mention_memories = repo
        .memories_for_entity("test-user", "orgs/former-corp-inc")
        .await
        .unwrap();
    assert!(
        mention_memories
            .iter()
            .any(|m| m.content.contains("reported earnings")),
        "the unresolved mention keeps its own fact: {mention_memories:?}"
    );
}

/// A text-grounded entity proposal without a memory is an identity/link shell. A
/// surviving relation makes it a typed database individual but never a disk article;
/// without a surviving relation it is discarded before the production page commit.
#[tokio::test]
async fn memoryless_entity_is_only_a_resolve_candidate_and_link_target() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "account owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "account owner",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [
            {"id":"fixture-page-23",
                "path":"people/sarah", "name":"Sarah", "description":"an engineer",
                "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}],
                "candidate_attributes":[{
                    "key":"model", "value":"Claude Fable 5",
                    "sources":[{"message":"m1","quote":"mentioned Claude Fable 5","strength":"explicit"}]
                }]
            },
            {"id":"fixture-page-24",
                "path":"ai-models/claude-fable-5", "name":"Claude Fable 5",
                "description":"a model Sarah mentioned",
                "sources":[{"message":"m1","quote":"Claude Fable 5","strength":"explicit"}]
            },
            {"id":"fixture-page-25",
                "path":"ai-models/orphan-model", "name":"Orphan Model",
                "description":"an unreferenced model proposal",
                "sources":[{"message":"m1","quote":"Orphan Model","strength":"explicit"}]
            }
        ],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Sarah is an engineer","strength":"explicit"}],
            "content":"Sarah is an engineer.",
            "entities":["people/sarah"]
        }]
    });
    let classify_sarah = json!({
        "entity": {"name":"Sarah", "description":"an engineer", "aliases":[]},
        "classes": [{"class":"schema:Person"}],
        "attributes": [{
            "from":"model", "to":"frona:mentionsModel",
            "targets":["ai-models/claude-fable-5"]
        }],
        "relations":[],"new_entities":[{
            "path":"ai-models/claude-fable-5", "name":"Claude Fable 5",
            "description":"a model Sarah mentioned",
            "class":"schema:SoftwareApplication", "from_facts":[]
        }],"declarations":[{
            "kind":"object_property", "term":"frona:mentionsModel",
            "description":"Links a person to a model that the person mentions."
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify_sarah)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("Sarah is an engineer.".into()),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage.clone(),
        registry,
        prompts,
        memory_config,
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());

    let result = full_pass(
        &service,
        ontology_scope(&service),
        "Sarah is an engineer and mentioned Claude Fable 5 and Orphan Model.",
        harness,
    )
    .await;
    assert!(
        result.is_ok(),
        "pass failed: {:?}; calls={}; last_history={:?}",
        result.err(),
        mock.calls(),
        mock.last_history(),
    );

    assert!(
        repo.entity_by_path("test-user", "people/sarah")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.entity_by_path("test-user", "ai-models/claude-fable-5")
            .await
            .unwrap()
            .is_some(),
        "a referenced memoryless shell must become a typed database page: links={:?}; histories={:#?}",
        repo.links_from_entity("test-user", "people/sarah")
            .await
            .unwrap(),
        mock.histories()
    );
    assert!(
        !service
            .storage()
            .page_exists(&ontology_scope(&service).vault, "ai-models/claude-fable-5",),
        "a referenced memoryless shell must not be authored to disk"
    );
    assert!(
        repo.entity_by_path("test-user", "ai-models/orphan-model")
            .await
            .unwrap()
            .is_none(),
        "an unreferenced memoryless shell must not become a knowledge page"
    );
    assert!(
        !service
            .storage()
            .page_exists(&ontology_scope(&service).vault, "ai-models/orphan-model"),
        "an unreferenced memoryless shell must not be authored"
    );
}

/// The "Map" half of classify: the Classify stage types a stated free-text relation ("works for")
/// to a `frona:` object property and re-keys the entity's link to the CURIE. It does not
/// invent an inverse without evidence for the reverse assertion.
#[tokio::test]
async fn attribute_naming_an_entity_becomes_an_edge_without_inventing_an_inverse() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    for (path, kind, name) in [
        ("people/me", "schema:Person", "Casey Owner"),
        ("orgs/acme", "schema:Organization", "Acme"),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[frona::memory::pkm::ontology::PrefixMap::standard().expand(kind)],
            name,
            "x",
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(&db, "test-user", path, "", "x", &serde_json::json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }

    // Extract states the fact as a free-text attribute whose value is a bare string. It
    // sees existing page names for identity reuse, but has no ontology vocabulary, so
    // "Acme" is still indistinguishable from a version number as a property value.
    let extract = json!({
        "new_entities": [{"id":"fixture-page-26",
            "path":"people/sarah","name":"Sarah","description":"an engineer",
            "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}],
            "candidate_attributes":[{"key":"employer","value":"Acme","sources":[{"message":"m1","quote":"works for Acme","strength":"explicit"}]}]
        }],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}],"content":"Sarah is an engineer","entities":["people/sarah"]}]
    });
    // The Classify stage types Sarah and reads that value as an entity: `target` makes
    // `employer` an OBJECT property, so the literal becomes an edge to the org.
    let classify = json!({
        "entity":{"name":"Sarah","description":"an engineer","aliases":[]},
        "classes":[{"class":"schema:Person"}],
        "relations":[],
        "attributes":[{"from":"employer","to":"frona:worksFor","targets":["orgs/acme"]}],
        "new_entities":[],
        "declarations":[{
            "kind":"object_property", "term":"frona:worksFor",
            "description":"Links a person to an organization for which the person works."
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("Sarah is an engineer at Acme.".into()),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());

    full_pass(
        &service,
        ontology_scope(&service),
        "Sarah is an engineer who works for Acme.",
        harness,
    )
    .await
    .unwrap();

    // The attribute became an asserted edge under the CURIE object property.
    let sarah_links = repo
        .links_from_entity("test-user", "people/sarah")
        .await
        .unwrap();
    assert!(
        sarah_links
            .iter()
            .any(|l| l.relation == "frona:worksFor" && l.to_entity_path == "orgs/acme"),
        "the attribute was promoted to an edge: {:?}",
        sarah_links
            .iter()
            .map(|l| (&l.relation, &l.to_entity_path))
            .collect::<Vec<_>>()
    );
    // …and stopped being a literal. Holding it both ways is the duplication this whole
    // path exists to remove, so "the edge exists" is only half the assertion.
    let sarah = repo
        .entity_by_path("test-user", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    let attrs = sarah.attributes.as_object().cloned().unwrap_or_default();
    assert!(
        !attrs.contains_key("employer") && !attrs.contains_key("frona:worksFor"),
        "the fact is stored once, as an edge: {attrs:?}"
    );
    // One directional assertion is not evidence for an inverse axiom.
    let acme_links = repo
        .links_from_entity("test-user", "orgs/acme")
        .await
        .unwrap();
    assert!(
        !acme_links.iter().any(|l| l.origin == LinkOrigin::Inferred
            && l.relation == "frona:employs"
            && l.to_entity_path == "people/sarah"),
        "no unsupported inverse edge: {:?}",
        acme_links
            .iter()
            .map(|l| (&l.relation, &l.to_entity_path, l.origin))
            .collect::<Vec<_>>(),
    );

    //
    // The memory `"employer: Acme"` is still live, and reconcile re-derives the attribute
    // map from the page's memories on every pass. So a second pass is where the promotion
    // was previously undone: the literal came back, the next pass promoted it away again,
    // and the graph oscillated for ever without either representation ever winning.
    //
    // The reconcile below is a model doing exactly that - re-emitting the fact as an
    // attribute, under a *different* key from the property the edge carries, which is the
    // shape a re-derivation actually takes.
    // Classify runs before reconcile, so the pass consumes a classification first. It
    // proposes nothing: the attribute is gone from the page, which is the state under test.
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c2".into(),
        "submit".into(),
        classification("Sarah", "an engineer", "schema:Person"),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "r2".into(),
        "submit".into(),
        json!({
            "relations": [], "outdated": [], "moves": [], "name": "", "description": "engineer",
            "attributes": { "employer": "Acme" }
        }),
    )]));
    mock.enqueue(MockResponse::Text("Sarah is an engineer at Acme.".into()));

    // Re-dirty the page the way anything else would - quarantining and releasing a memory
    // bumps `updated_at` on the pages that render it, which is what puts a page back on
    // reconcile's worklist.
    let mem = repo
        .memories_for_entity("test-user", "people/sarah")
        .await
        .unwrap();
    let mid = mem[0].id.clone();
    repo.set_disposition("test-user", &mid, Disposition::Suspect)
        .await
        .unwrap();
    repo.set_disposition("test-user", &mid, Disposition::None)
        .await
        .unwrap();

    service
        .consolidate(
            ontology_scope(&service),
            test_harness(&db, &config, mock.clone()),
        )
        .await
        .unwrap();

    let sarah = repo
        .entity_by_path("test-user", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    let attrs = sarah.attributes.as_object().cloned().unwrap_or_default();
    assert!(
        !attrs.values().any(|v| v.as_str() == Some("Acme")),
        "the fact is held as an edge, so no pass may put the literal back: {attrs:?}"
    );
    assert_eq!(
        repo.links_from_entity("test-user", "people/sarah")
            .await
            .unwrap()
            .iter()
            .filter(|l| l.to_entity_path == "orgs/acme")
            .count(),
        1,
        "and the edge is not duplicated either"
    );
}

/// Seed a published page carrying a free-text attribute and its source fact, then return
/// the memory ID that model-facing fixtures cite. Extraction transaction behavior has its
/// own repository tests; these tests begin at Classify.
async fn seed_mention(
    db: &Surreal<Db>,
    repo: &PkmRepo,
    path: &str,
    name: &str,
    fact: &str,
    attribute: (&str, &str),
) -> String {
    repo.upsert_entity_skeleton(
        "test-user",
        path,
        EntityCategory::Concept,
        &[],
        name,
        "x",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        path,
        "",
        "x",
        &json!({attribute.0: attribute.1}),
    )
    .await
    .unwrap();
    repo.create_sourced_memory(
        "test-user",
        MemoryKind::Fact,
        fact,
        &[path.to_string()],
        vec![MemoryEvidence {
            strength: EvidenceStrength::Explicit,
            source: EvidenceSource::HumanEdit {
                page_path: path.into(),
                quote: fact.into(),
            },
        }],
    )
    .await
    .unwrap()
}

/// **The gap this closes.** An attribute value names a real entity that has no page, so
/// the search finds nothing - and the pipeline used to read "found nothing" as "this is a
/// string", declare `worksFor` a data property, and store the fact as a literal for ever.
/// Nothing would ever create the page that a later pass needed in order to know better:
/// extract only mints pages for entities the transcript is *about*.
///
/// So the Classify stage mints it. The value becomes an edge in the same pass, and the new
/// page shares the memory that stated the fact rather than being a name with nothing on
/// it - `knowledge_entity_source` is many-to-many, so the fact is on both pages, stored once.
#[tokio::test]
async fn attribute_naming_an_unmaterialized_entity_mints_it_and_shares_the_fact() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let px = frona::memory::pkm::ontology::PrefixMap::standard();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    // Note what is *not* seeded: there is no `organizations/example-corp`. "Example Corp" exists only as
    // the value of a free-text key.
    let fact = seed_mention(
        &db,
        &repo,
        "people/sarah",
        "Sarah",
        "Sarah works at Example Corp",
        ("employer", "Example Corp"),
    )
    .await;

    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    // Classify reads the value as an entity, declares the page it needs, and points the
    // attribute at it. `from_facts` is what makes the new page more than a title.
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "c".into(),
        "submit".into(),
        json!({
            "entity":{"name":"Sarah","description":"x","aliases":[]},
            "classes": [{"class": "schema:Person"}],
            "relations":[],
            "attributes": [{"from": "employer", "to": "frona:worksFor",
                            "targets": ["organizations/example-corp"]}],
            "new_entities": [{
                "path": "organizations/example-corp", "name": "Example Corp",
                "description": "Technology company.",
                "class": "schema:Organization", "from_facts": ["f1"]
            }],
            "declarations":[{
                "kind":"object_property", "term":"frona:worksFor",
                "description":"Links a person to an organization for which the person works."
            }],
            "has_keys":[],"inverse_functional_properties":[]
        }),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "r".into(),
        "submit".into(),
        empty_reconcile(),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "r2".into(),
        "submit".into(),
        empty_reconcile(),
    )]));
    mock.enqueue(MockResponse::ToolCalls(vec![(
        "a".into(),
        "submit".into(),
        json!({"decisions": [{"term": "frona:worksFor", "decision": "declare"}]}),
    )]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );

    service
        .consolidate(
            ontology_scope(&service),
            test_harness(&db, &config, mock.clone()),
        )
        .await
        .unwrap();

    let organization = repo
        .entity_by_path("test-user", "organizations/example-corp")
        .await
        .unwrap()
        .expect("the Classify stage minted the entity the value named");
    assert_eq!(organization.name, "Example Corp");
    assert_eq!(organization.category, EntityCategory::Concept);
    // Created with no kinds and typed by the same commit that declares the schema, so it
    // is never stamped with a term the TBox has not seen.
    assert_eq!(
        organization.kinds,
        [px.expand("schema:Organization")],
        "typed in the same pass"
    );

    // It shares the fact rather than starting empty. Same memory, two pages - the fact is
    // stored once and reachable from both.
    let on_organization = repo
        .memories_for_entity("test-user", "organizations/example-corp")
        .await
        .unwrap();
    assert_eq!(
        on_organization.len(),
        1,
        "the cited fact reached the new page: {on_organization:?}"
    );
    assert_eq!(on_organization[0].id, fact, "the same memory, not a copy");
    assert!(
        repo.memories_for_entity("test-user", "people/sarah")
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == fact),
        "and it is still on the page that stated it"
    );

    // The attribute became an edge, and stopped being a literal.
    assert!(
        repo.links_from_entity("test-user", "people/sarah")
            .await
            .unwrap()
            .iter()
            .any(|l| l.relation == "frona:worksFor"
                && l.to_entity_path == "organizations/example-corp"
                && l.origin == LinkOrigin::Asserted),
        "the value names an entity, so it is an edge"
    );
    let sarah = repo
        .entity_by_path("test-user", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    let attrs = sarah.attributes.as_object().cloned().unwrap_or_default();
    assert!(
        !attrs.values().any(|v| v.as_str() == Some("Example Corp")),
        "and not also a string: {attrs:?}"
    );

    // The property is declared for what it is. This is the reading that used to be decided
    // by whether a search happened to hit, and written into the schema permanently.
    let declared = ontology_manager(&db).catalog("test-user").await.unwrap();
    assert!(
        declared
            .object_properties
            .contains(&"frona:worksFor".to_string()),
        "an OBJECT property: {:?}",
        declared.object_properties
    );
    assert!(
        !declared
            .data_properties
            .contains(&"frona:worksFor".to_string()),
        "and not also a data property: {:?}",
        declared.data_properties
    );
}

/// Minting is idempotent, which is what lets the resume path replay a banked
/// classification by simply running it again.
///
/// The Classify stage banks each classification so a crashed pass does not re-pay for the
/// conversation, and the replay rebuilds the proposals from it - including the mints,
/// because they write pages and links that reconstructing only the in-memory half would
/// drop. So the same mint arrives twice by design, and must land once.
#[tokio::test]
async fn re_minting_the_same_entity_creates_no_duplicate() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let px = frona::memory::pkm::ontology::PrefixMap::standard();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let fact = seed_mention(
        &db,
        &repo,
        "people/sarah",
        "Sarah",
        "Sarah works at Example Corp",
        ("employer", "Example Corp"),
    )
    .await;
    let classify = json!({
        "entity":{"name":"Sarah","description":"x","aliases":[]},
        "classes": [{"class": "schema:Person"}],
        "relations":[],
        "attributes": [{"from": "employer", "to": "frona:worksFor",
                        "targets": ["organizations/example-corp"]}],
        "new_entities": [{
            "path": "organizations/example-corp", "name": "Example Corp", "description": "Technology company.",
            "class": "schema:Organization", "from_facts": ["f1"]
        }],
        "declarations":[{
            "kind":"object_property", "term":"frona:worksFor",
            "description":"Links a person to an organization for which the person works."
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });

    let mock = Arc::new(MockModelProvider::new(Vec::new()));
    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );

    for iteration in 0..2 {
        if iteration == 0 {
            mock.enqueue(MockResponse::ToolCalls(vec![(
                "c".into(),
                "submit".into(),
                classify.clone(),
            )]));
        } else {
            mock.enqueue(MockResponse::ToolCalls(vec![(
                "cm".into(),
                "submit".into(),
                classification("Example Corp", "Technology company.", "schema:Organization"),
            )]));
            mock.enqueue(MockResponse::ToolCalls(vec![(
                "cs".into(),
                "submit".into(),
                classify.clone(),
            )]));
            mock.enqueue(MockResponse::ToolCalls(vec![(
                "rm".into(),
                "submit".into(),
                empty_reconcile(),
            )]));
            mock.enqueue(MockResponse::ToolCalls(vec![(
                "rs".into(),
                "submit".into(),
                empty_reconcile(),
            )]));
            mock.enqueue(MockResponse::Text(
                "Example Corp is a technology company.".into(),
            ));
            mock.enqueue(MockResponse::Text("Sarah works at Example Corp.".into()));
        }
        // Re-dirty the source page so the second pass classifies it again with the same
        // answer - the same path the resume replay takes.
        repo.set_disposition("test-user", &fact, Disposition::Suspect)
            .await
            .unwrap();
        repo.set_disposition("test-user", &fact, Disposition::None)
            .await
            .unwrap();
        let result = service
            .consolidate(
                ontology_scope(&service),
                test_harness(&db, &config, mock.clone()),
            )
            .await;
        assert!(
            result.is_ok(),
            "iteration {iteration}: {result:?}\n{:#?}",
            mock.histories()
        );
    }

    assert!(
        repo.entity_by_path("test-user", "organizations/example-corp")
            .await
            .unwrap()
            .is_some(),
        "the minted page survives the second pass"
    );
    let on_meta = repo
        .memories_for_entity("test-user", "organizations/example-corp")
        .await
        .unwrap();
    assert_eq!(
        on_meta.len(),
        1,
        "the fact is attached once, not once per pass: {on_meta:?}"
    );
    assert_eq!(
        repo.links_from_entity("test-user", "people/sarah")
            .await
            .unwrap()
            .iter()
            .filter(|l| l.to_entity_path == "organizations/example-corp")
            .count(),
        1,
        "and the edge is not duplicated either"
    );
}

/// A page is several things at once, and the whole pipeline has to carry that: the
/// Classify stage returns a set, every member is stamped, and every member reaches the ABox.
#[tokio::test]
async fn classify_keeps_every_returned_class_in_the_reasoned_entity() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "x",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "x",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-27",
            "path":"people/sarah", "name":"Sarah", "description":"an engineer",
            "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}]
        }],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}],"content":"Sarah is an engineer","entities":["people/sarah"]}]
    });
    // Two classes at once: a standard one, and a bespoke mint under a different parent.
    let classify = json!({
        "entity":{"name":"Sarah","description":"an engineer","aliases":[]},
        "classes":[{"class":"schema:Person"},{"class":"frona:Engineer"}],
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":[{
            "kind":"class", "term":"frona:Engineer",
            "description":"A person who works as an engineer.",
            "parents":["schema:Person"]
        }],
        "has_keys":[],"inverse_functional_properties":[]
    });
    // Declared, so `frona:Engineer ⊑ schema:Person` reaches the delta before stamping -
    // which is what lets normalisation see that `Person` is implied.
    let adjudicate = json!({"decisions":[
        {"term":"frona:Engineer","decision":"declare","parent":"schema:Person"}
    ]});
    let reconcile = json!({
        "relations": [], "outdated": [], "moves": [], "description": "an engineer",
        "attributes": {}
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), reconcile)]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Sarah is an engineer.",
        harness,
    )
    .await
    .unwrap();

    let declared = ontology_manager(&db)
        .catalog("test-user")
        .await
        .unwrap()
        .classes;
    assert!(
        declared.iter().any(|c| c == "frona:Engineer"),
        "the mint was declared, so the subsumption is live: {declared:?}"
    );

    // The Classify stage returned both, and both were stamped - but `frona:Engineer ⊑
    // schema:Person` makes `Person` implied, so normalisation retires it rather than
    // storing a class the reasoner derives anyway.
    let sarah = repo
        .entity_by_path("test-user", "people/sarah")
        .await
        .unwrap()
        .expect("page");
    assert_eq!(
        sarah.kinds,
        ["urn:frona:Engineer"],
        "normalised to the most specific"
    );

    // Both reach the reasoner, so the page is an instance of each.
    // ...and nothing is lost: the retired class is still entailed.
    let ontology = ontology_manager(&db);
    for want in [
        "urn:frona:Engineer",
        "https://schema.org/Person",
        "https://schema.org/Thing",
    ] {
        assert!(
            ontology
                .entails_type("test-user", "people/sarah", want)
                .await
                .unwrap(),
            "still an instance of {want}"
        );
    }
}

#[tokio::test]
async fn reconcile_rekeys_attributes_to_curies() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Person")],
        "Casey Owner",
        "x",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "people/me",
        "",
        "x",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{"id":"fixture-page-28","path":"services/postgres","name":"Postgres","description":"the db",
            "sources":[{"message":"m1","quote":"Postgres","strength":"explicit"}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Postgres runs on port 5432, admin is admin@pg.local","strength":"explicit"}],"content":"Postgres on port 5432, admin admin@pg.local","entities":["services/postgres"]}]
    });
    let classify = classification("Postgres", "the db", "schema:SoftwareApplication");
    // reconcile returns FREE-TEXT attribute keys; the layer re-keys them to CURIEs.
    let reconcile = json!({
        "relations": [], "outdated": [], "moves": [], "description": "the dev database",
        "attributes": {"port": 5432, "email": "admin@pg.local"},
        "attribute_sources":[
            {"property":"port","value":5432,"source_memory_ids":["m1"]},
            {"property":"email","value":"admin@pg.local","source_memory_ids":["m1"]}
        ],
        "declarations":[{
            "kind":"data_property", "term":"frona:port",
            "description":"The network port used by the service.",
            "datatype":"xsd:integer"
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), reconcile)]),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service), // must contain what the extract fixture claims to find - grounding drops the rest
        "Postgres runs on port 5432, admin is admin@pg.local.",
        harness,
    )
    .await
    .unwrap();

    let pg = repo
        .entity_by_path("test-user", "services/postgres")
        .await
        .unwrap()
        .expect("pg page");
    let attrs = pg.attributes.as_object().expect("attributes object");
    // bespoke → frona: ; standard → schema: ; originals gone.
    assert_eq!(
        attrs.get("frona:port").and_then(|v| v.as_i64()),
        Some(5432),
        "{attrs:?}"
    );
    assert_eq!(
        attrs.get("schema:email").and_then(|v| v.as_str()),
        Some("admin@pg.local"),
        "{attrs:?}"
    );
    assert!(
        !attrs.contains_key("port") && !attrs.contains_key("email"),
        "free-text keys removed: {attrs:?}"
    );
}

#[tokio::test]
async fn reconcile_revises_a_graph_edit_before_it_reaches_the_checkpoint() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    let px = frona::memory::pkm::ontology::PrefixMap::standard();

    repo.upsert_entity_skeleton(
        "test-user",
        "people/me",
        EntityCategory::Concept,
        &[px.expand("schema:Person")],
        "Casey Owner",
        "the owner",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", "people/me", "", "the owner", &json!({}))
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", "people/me")
        .await
        .unwrap();
    ontology_manager(&db)
        .commit(
            "test-user",
            &[
                SchemaEdit::DeclareObjectProperty {
                    property: "frona:chip".into(),
                },
                SchemaEdit::ObjectPropertyDomain {
                    property: "frona:chip".into(),
                    class: "schema:IndividualProduct".into(),
                },
            ],
        )
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{
            "id":"p1", "path":"phones/example-phone", "name":"Example Phone", "description":"a phone model",
            "sources":[{"message":"m1","quote":"Example Phone","strength":"explicit"}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Example Phone uses a 2nm chip","strength":"explicit"}],
            "content":"Example Phone uses a 2nm chip.",
            "entities":["phones/example-phone"]
        }]
    });
    let classify = json!({
        "classes":[{"class":"schema:ProductModel"}],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[],
        "entity":{"name":"Example Phone","description":"a phone model","aliases":[]}
    });
    let invalid_reconcile = json!({
        "relations":[], "entity_relations":[], "relation_retractions":[],
        "entity_relation_replacements":[], "outdated":[],
        "attributes":{"frona:chip":"2nm"},
        "attribute_sources":[{
            "property":"frona:chip", "value":"2nm", "source_memory_ids":["m1"]
        }],
        "attribute_replacements":[], "name":"Example Phone",
        "description":"a phone model", "moves":[], "declarations":[]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), invalid_reconcile)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), empty_reconcile())]),
        MockResponse::Text("# Example Phone\n\nA phone model.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Example Phone uses a 2nm chip.",
        harness,
    )
    .await
    .unwrap();

    let page = repo
        .entity_by_path("test-user", "phones/example-phone")
        .await
        .unwrap()
        .expect("the corrected page");
    assert!(
        !page
            .attributes
            .as_object()
            .is_some_and(|attributes| attributes.contains_key("frona:chip")),
        "the rejected literal must never reach staged or committed state: {:?}",
        page.attributes,
    );
    assert_eq!(
        mock.calls(),
        5,
        "Reconcile must request one corrected submission"
    );
}

#[tokio::test]
async fn reconcile_does_not_apply_a_rejected_fallback_after_its_turn_budget() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;

    let extract = json!({
        "new_entities": [{
            "id":"p1", "path":"people/buddy", "name":"Buddy",
            "description":"Casey Owner's golden retriever",
            "sources":[{"message":"m1","quote":"Buddy","strength":"explicit"}]
        }],
        "existing_entity_updates": [],
        "memories": [
            {
                "kind":"fact",
                "sources":[{
                    "message":"m1",
                    "quote":"Casey Owner has a golden retriever named Buddy.",
                    "strength":"explicit"
                }],
                "content":"Casey Owner has a golden retriever named Buddy.",
                "entities":["people/me","people/buddy"]
            },
            {
                "kind":"fact",
                "sources":[{
                    "message":"m1",
                    "quote":"Casey Owner has a golden retriever named Buddy.",
                    "strength":"explicit"
                }],
                "content":"Buddy is Casey Owner's golden retriever.",
                "entities":["people/me","people/buddy"]
            }
        ]
    });
    let classify = json!({
        "entity": {
            "name":"Buddy", "description":"Casey Owner's golden retriever", "aliases":[]
        },
        "classes":[{
            "class":"frona:GoldenRetriever", "new_class_parent":"schema:Product"
        }],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[{
            "kind":"class", "term":"frona:GoldenRetriever",
            "description":"A golden retriever represented as a product for this graph-conflict fixture.",
            "parents":["schema:Product"]
        }],
        "has_keys":[], "inverse_functional_properties":[]
    });
    let classify_me = json!({
        "entity": {
            "name":"Casey Owner", "description":"The account owner.", "aliases":[]
        },
        "classes":[{"class":"schema:Person"}],
        "relations":[], "attributes":[], "new_entities":[], "declarations":[],
        "has_keys":[], "inverse_functional_properties":[]
    });
    let invalid_reconcile = json!({
        "relations": [{
            "memory":"m2",
            "links":[{"relation":"duplicate", "to":"m1", "note":"Same fact."}]
        }],
        "entity_relations": [{
            "attribute":"hasPet", "value":"Buddy", "property":"frona:hasPet",
            "target":"people/buddy", "source_memory_ids":["m1"]
        }],
        "outdated": [], "attributes": {"unsupportedFlag":true}, "moves": [],
        "attribute_sources": [{
            "property":"unsupportedFlag", "value":true, "source_memory_ids":["m1"]
        }],
        "description":"Casey Owner has a golden retriever named Buddy.",
        "declarations": [{
            "kind":"object_property", "term":"frona:hasPet",
            "description":"A pet cared for by the subject.",
            "domain":["schema:Person"], "range":["schema:Person"]
        }]
    });
    let adjudicate = json!({"decisions":[
        {"term":"frona:GoldenRetriever", "decision":"accept_proposal"}
    ]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("cm".into(), "submit".into(), classify_me)]),
        MockResponse::ToolCalls(vec![(
            "r1".into(),
            "submit".into(),
            invalid_reconcile.clone(),
        )]),
        MockResponse::ToolCalls(vec![(
            "r2".into(),
            "submit".into(),
            invalid_reconcile.clone(),
        )]),
        MockResponse::ToolCalls(vec![(
            "r3".into(),
            "submit".into(),
            invalid_reconcile.clone(),
        )]),
        MockResponse::ToolCalls(vec![(
            "r4".into(),
            "submit".into(),
            invalid_reconcile.clone(),
        )]),
        MockResponse::ToolCalls(vec![(
            "r5".into(),
            "submit".into(),
            invalid_reconcile.clone(),
        )]),
        MockResponse::ToolCalls(vec![("r6".into(), "submit".into(), invalid_reconcile)]),
        MockResponse::ToolCalls(vec![("r7".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let result = full_pass(
        &service,
        ontology_scope(&service),
        "Casey Owner has a golden retriever named Buddy.",
        harness,
    )
    .await;
    let memories = repo
        .list_all_memories("test-user")
        .await
        .unwrap()
        .into_iter()
        .filter(|memory| memory.content.contains("golden retriever"))
        .collect::<Vec<_>>();
    let (current, _) = classify_memories(&memories);
    assert_eq!(
        current.len(),
        2,
        "a rejected fallback suppressed one of its source memories: {memories:?}; run={result:?}",
    );
    assert!(
        result.is_ok(),
        "Reconcile accepted a range that made the pending target both a Person and a Product: \
         {result:?}\n{:?}",
        mock.histories(),
    );

    let histories = format!("{:?}", mock.histories());
    assert!(
        histories.contains("cax-dw")
            && histories.contains("needs one data_property declaration")
            && histories.contains("people/buddy")
            && histories.contains("schema:Person"),
        "Reconcile did not return all validation errors in one response: {histories}",
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content == "Casey Owner has a golden retriever named Buddy."),
        "the failed proposal must not lose its source memory: {memories:?}",
    );
}

#[tokio::test]
async fn reconcile_batches_entity_suggestions_and_commits_an_accepted_relation() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    for (path, kind, name) in [
        ("people/me", "schema:Person", "Casey Owner"),
        (
            "organizations/example-corp",
            "schema:Organization",
            "Example Corp",
        ),
    ] {
        repo.upsert_entity_skeleton(
            "test-user",
            path,
            EntityCategory::Concept,
            &[frona::memory::pkm::ontology::PrefixMap::standard().expand(kind)],
            name,
            "x",
            &[],
        )
        .await
        .unwrap();
        seed_reconciled_entity(&db, "test-user", path, "", "x", &json!({}))
            .await
            .unwrap();
        mark_entity_rendered(&db, "test-user", path).await.unwrap();
    }

    let extract = json!({
        "new_entities": [{
            "id":"p1", "path":"people/sarah", "name":"Sarah", "description":"an engineer",
            "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}]
        }],
        "existing_entity_updates": [],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Sarah currently works at Example Corp.","strength":"explicit"}],
            "content":"Sarah currently works at Example Corp.",
            "entities":["people/sarah"]
        }]
    });
    let classify = json!({
        "entity": {"name":"Sarah", "description":"an engineer", "aliases":[]},
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "has_keys": [], "inverse_functional_properties": []
    });
    let first_reconcile = json!({
        "relations": [], "entity_relations": [], "outdated": [], "moves": [],
        "description": "an engineer at Example Corp",
        "attributes": {"employer": "Example Corp"}
    });
    let revised_reconcile = json!({
        "relations": [],
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp",
            "property": "frona:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["m1"]
        }],
        "outdated": [], "moves": [],
        "description": "an engineer at Example Corp",
        "attributes": {},
        "declarations": [{
            "kind":"object_property", "term":"frona:worksFor",
            "description":"Links a person to an organization for which the person works."
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), first_reconcile)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), revised_reconcile)]),
        MockResponse::ToolCalls(vec![(
            "a".into(),
            "submit".into(),
            json!({"decisions": [
                {"term": "frona:worksFor", "decision": "declare"}
            ]}),
        )]),
        MockResponse::Text("Sarah is an engineer at Example Corp.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Sarah currently works at Example Corp.",
        harness,
    )
    .await
    .unwrap();

    let links = repo
        .links_from_entity("test-user", "people/sarah")
        .await
        .unwrap();
    assert!(
        links.iter().any(|link| {
            link.origin == LinkOrigin::Asserted
                && link.relation == "frona:worksFor"
                && link.to_entity_path == "organizations/example-corp"
        }),
        "accepted advisory relation was not committed: {links:?}"
    );
    let sarah = repo
        .entity_by_path("test-user", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !sarah.attributes.as_object().is_some_and(|attrs| {
            attrs.contains_key("employer") || attrs.contains_key("frona:employer")
        }),
        "the promoted fact must not remain a literal: {:?}",
        sarah.attributes
    );
    let catalogue = ontology_manager(&db).catalog("test-user").await.unwrap();
    assert!(
        catalogue
            .object_properties
            .contains(&"frona:worksFor".to_string()),
        "the accepted relation must be declared in the ontology: {:?}",
        catalogue.object_properties
    );
}

#[tokio::test]
async fn reconcile_returns_a_failed_relation_to_the_model_before_it_can_be_staged() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    repo.upsert_entity_skeleton(
        "test-user",
        "organizations/example-corp",
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization")],
        "Example Corp",
        "x",
        &["Example Corporation".into()],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &db,
        "test-user",
        "organizations/example-corp",
        "",
        "x",
        &json!({}),
    )
    .await
    .unwrap();
    mark_entity_rendered(&db, "test-user", "organizations/example-corp")
        .await
        .unwrap();

    let extract = json!({
        "new_entities": [{
            "id":"p1", "path":"people/sarah",
            "name":"Sarah",
            "description":"an engineer",
            "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}]
        }],
        "existing_entity_updates": [],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Sarah currently works at Example Corp.","strength":"explicit"}],
            "content":"Sarah currently works at Example Corp.",
            "entities":["people/sarah"]
        }]
    });
    let classify = json!({
        "entity": {"name":"Sarah", "description":"an engineer", "aliases":[]},
        "classes": [{"class": "schema:Person"}],
        "relations": [], "attributes": [], "new_entities": [], "declarations": [],
        "has_keys": [], "inverse_functional_properties": []
    });
    let unsupported_relation = json!({
        "relations": [],
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp (Example Corporation)",
            "property": "frona:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["m1"]
        }],
        "outdated": [], "moves": [],
        "description": "an engineer at Example Corp",
        "attributes": {},
        "declarations": [{
            "kind":"object_property", "term":"frona:worksFor",
            "description":"Links a person to an organization for which the person works."
        }]
    });
    let corrected_relation = json!({
        "relations": [],
        "entity_relations": [{
            "attribute": "employer",
            "value": "Example Corp",
            "property": "frona:worksFor",
            "target": "organizations/example-corp",
            "source_memory_ids": ["m1"]
        }],
        "outdated": [], "moves": [],
        "description": "an engineer at Example Corp",
        "attributes": {},
        "declarations": [{
            "kind":"object_property", "term":"frona:worksFor",
            "description":"Links a person to an organization for which the person works."
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r1".into(), "submit".into(), unsupported_relation)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), corrected_relation)]),
        MockResponse::ToolCalls(vec![(
            "a".into(),
            "submit".into(),
            json!({"decisions": [
                {"term": "frona:worksFor", "decision": "declare"}
            ]}),
        )]),
        MockResponse::Text("Sarah is an engineer at Example Corp.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock.clone(), &memory_config).await;
    let result = full_pass(
        &service,
        ontology_scope(&service),
        "Sarah currently works at Example Corp.",
        harness,
    )
    .await;
    assert!(result.is_ok(), "{result:?}\n{:?}", mock.histories());

    let links = repo
        .links_from_entity("test-user", "people/sarah")
        .await
        .unwrap();
    assert!(
        links.iter().any(|link| {
            link.origin == LinkOrigin::Asserted
                && link.relation == "frona:worksFor"
                && link.to_entity_path == "organizations/example-corp"
        }),
        "the corrected relation was not committed: {links:?}\n{:?}",
        mock.histories()
    );
    let histories = format!("{:?}", mock.histories());
    assert!(
        histories.contains("value_supported_by_cited_memory"),
        "the failed rule was not returned to Reconcile"
    );
    assert!(
        histories.contains("Example Corp (Example Corporation)"),
        "feedback omitted the failed value"
    );
    assert!(
        histories.contains("Sarah currently works at Example Corp"),
        "feedback omitted supporting memory text"
    );
}

#[tokio::test]
async fn reconcile_does_not_duplicate_a_pending_classify_promotion() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    for (path, kind, name) in [
        ("people/me", "schema:Person", "Casey Owner"),
        (
            "organizations/example-corp",
            "schema:Organization",
            "Example Corp",
        ),
    ] {
        mark_clean(&db, &repo, path, kind, name, json!({})).await;
    }

    let extract = json!({
        "new_entities": [{
            "id":"p1", "path":"people/sarah",
            "name":"Sarah",
            "description":"an engineer",
            "sources":[{"message":"m1","quote":"Sarah","strength":"explicit"}],
            "candidate_attributes":[{"key":"employer","value":"Example Corp","sources":[{"message":"m1","quote":"works at Example Corp","strength":"explicit"}]}]
        }],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"Sarah currently works at Example Corp.","strength":"explicit"}],
            "content":"Sarah currently works at Example Corp.",
            "entities":["people/sarah"]
        }]
    });
    let classify = json!({
        "entity": {"name":"Sarah", "description":"an engineer", "aliases":[]},
        "classes": [{"class": "schema:Person"}],
        "relations": [],
        "attributes": [{
            "from": "employer",
            "to": "schema:worksFor",
            "targets": ["organizations/example-corp"]
        }],
        "new_entities": [], "declarations": [],
        "has_keys": [], "inverse_functional_properties": []
    });
    // Reconcile independently chooses another predicate for the same source
    // attribute and target. The pending Classify promotion is authoritative.
    let reconcile = json!({
        "relations": [],
        "entity_relations": [{
            "attribute": "frona:employer",
            "value": "Example Corp",
            "property": "frona:usesOrganization",
            "target": "organizations/example-corp",
            "source_memory_ids": ["m1"]
        }],
        "outdated": [],
        "moves": [],
        "description": "an engineer at Example Corp",
        "attributes": {},
        "declarations": [{
            "kind":"object_property", "term":"frona:usesOrganization",
            "description":"Links a person to an organization that the person uses."
        }]
    });
    let corrected_reconcile = json!({
        "relations": [], "entity_relations": [], "outdated": [], "moves": [],
        "description": "an engineer at Example Corp", "attributes": {}, "declarations": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), reconcile)]),
        MockResponse::ToolCalls(vec![("r2".into(), "submit".into(), corrected_reconcile)]),
        MockResponse::Text("Sarah is an engineer at Example Corp.".into()),
    ]));

    let (service, harness) = ontology_service(&db, &config, mock, &memory_config).await;
    full_pass(
        &service,
        ontology_scope(&service),
        "Sarah currently works at Example Corp.",
        harness,
    )
    .await
    .unwrap();

    let links = repo
        .links_from_entity("test-user", "people/sarah")
        .await
        .unwrap();
    let asserted: Vec<_> = links
        .iter()
        .filter(|link| link.origin == frona::memory::pkm::model::LinkOrigin::Asserted)
        .collect();
    assert_eq!(
        asserted.len(),
        1,
        "one source attribute yields one asserted fact: {asserted:?}"
    );
    assert_eq!(asserted[0].relation, "schema:worksFor");
    assert_eq!(asserted[0].to_entity_path, "organizations/example-corp");
}

/// Clean a page so it is not re-classified by the pass under test.
///
/// The author timestamp does the settling: the dirty predicate is
/// `updated_at > rendered_at`, and only the author stage stamps `rendered_at`. Seeding a
/// page without it leaves the page dirty, and the pass under test would spend its mocked
/// responses re-processing the fixture.
async fn mark_clean(
    db: &Surreal<Db>,
    repo: &PkmRepo,
    path: &str,
    kind: &str,
    name: &str,
    attrs: serde_json::Value,
) {
    repo.upsert_entity_skeleton(
        "test-user",
        path,
        EntityCategory::Concept,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand(kind)],
        name,
        "x",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(&db, "test-user", path, "", "x", &attrs)
        .await
        .unwrap();
    mark_entity_rendered(&db, "test-user", path).await.unwrap();
}

/// Mark a clean page dirty again so the next pass picks it up - re-setting the kind
/// bumps `updated_at` past `rendered_at` without disturbing the attributes just seeded.
async fn touch(db: &Surreal<Db>, path: &str, kind: &str) {
    seed_entity_kinds(
        &db,
        "test-user",
        path,
        &[frona::memory::pkm::ontology::PrefixMap::standard().expand(kind)],
    )
    .await
    .unwrap();
}

/// **Align + re-key.** The pass proposes a `frona:` term; the adjudicator recognises it
/// as an existing standard class. Because only accepted terms are committed, the new page
/// is stamped `schema:Organization` *directly* - `frona:Company` is never written to
/// a page - and a page from an earlier pass that was already on the superseded term is
/// moved with it.
#[tokio::test]
async fn assemble_align_stamps_the_standard_term_and_retypes_prior_entities() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    // A previous pass left this page on the (undeclared) frona: term.
    mark_clean(
        &db,
        &repo,
        "orgs/oldco",
        "frona:Company",
        "Oldco",
        json!({}),
    )
    .await;

    let extract = json!({
        "new_entities": [{"id":"fixture-page-29","path":"orgs/newco","name":"Newco","description":"a company",
            "sources":[{"message":"m1","quote":"Newco","strength":"explicit"}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Newco","strength":"explicit"}],"content":"Newco is a company","entities":["orgs/newco"]}]
    });
    let classify = json!({
        "entity":{"name":"Newco","description":"a company","aliases":[]},
        "classes":adjudication_classes(json!({"class":"frona:Company"})),
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":adjudication_declarations(json!({
            "kind":"class", "term":"frona:Company",
            "description":"A commercial organization.",
            "parents":["schema:Organization"]
        })),
        "has_keys":[],"inverse_functional_properties":[]
    });
    let adjudicate = json!({"decisions":adjudication_decisions(json!(
        {"term":"frona:Company","decision":"align","standard":"schema:Organization"}
    ))});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    full_pass(
        &service,
        ontology_scope(&service),
        "Newco is a company.",
        harness,
    )
    .await
    .unwrap();

    let newco = repo
        .entity_by_path("test-user", "orgs/newco")
        .await
        .unwrap()
        .unwrap();
    assert!(
        newco.kinds.contains(
            &frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization"),
        ),
        "stamped with the ADJUDICATED term: {:?}",
        newco.kinds,
    );
    assert!(
        !newco.kinds.contains(
            &frona::memory::pkm::ontology::PrefixMap::standard().expand("frona:Company"),
        ),
        "the proposed term was replaced: {:?}", newco.kinds,
    );
    let oldco = repo
        .entity_by_path("test-user", "orgs/oldco")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        oldco.kinds,
        [frona::memory::pkm::ontology::PrefixMap::standard().expand("schema:Organization")],
        "a prior pass's page moved off the superseded term too"
    );
    let onto = repo
        .ontology_get("test-user")
        .await
        .unwrap()
        .expect("delta persisted");
    assert!(
        onto.owl.contains("Company") && onto.owl.contains("Organization"),
        "the equivalence axiom is recorded:\n{}",
        onto.owl
    );
}

/// The adjudicator bounds a data property used by the pass. Existing values satisfy the
/// facet, so the validated restriction is committed without quarantine.
#[tokio::test]
async fn assemble_restrict_commits_when_existing_values_satisfy_the_facet() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    // An existing service with a valid replica count. `frona:replicas` is NOT
    // in the bundled ontology - that matters: a property the base already bounds (like
    // `frona:port`) would fail its base facet during classify and never reach adjudicate.
    mark_clean(
        &db,
        &repo,
        "services/db",
        "schema:SoftwareApplication",
        "Db",
        json!({"frona:replicas": 8}),
    )
    .await;
    // …and it is dirty this pass, so its `frona:replicas` attribute is scanned and
    // becomes an undeclared data-property proposal.
    touch(&db, "services/db", "schema:SoftwareApplication").await;

    let extract = json!({
        "new_entities": [],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Db serves the app with 8 replicas","strength":"explicit"}],"content":"Db serves the app with 8 replicas","entities":["services/db"]}]
    });
    let classify = json!({
        "entity":{"name":"Db","description":"x","aliases":[]},
        "classes":adjudication_classes(json!({"class":"schema:SoftwareApplication"})),
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":adjudication_declarations(json!({
            "kind":"data_property", "term":"frona:replicas",
            "description":"The configured replica count of the underlying service.",
            "datatype":"xsd:integer"
        })),
        "has_keys":[],"inverse_functional_properties":[]
    });
    let adjudicate = json!({"decisions":adjudication_decisions(json!({
        "term":"frona:replicas","decision":"restrict","datatype":"xsd:integer","min":1,"max":64
    }))});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![(
            "r".into(),
            "submit".into(),
            json!({
                "relations": [], "entity_relations": [], "outdated": [], "moves": [],
                "description": "", "attributes": {"frona:replicas": 8},
                "attribute_sources": [{
                    "property":"frona:replicas", "value":8,
                    "source_memory_ids":["m1"]
                }],
                "declarations": [{
                    "kind":"data_property", "term":"frona:replicas",
                    "description":"The configured replica count of the underlying service.",
                    "datatype":"xsd:integer"
                }]
            }),
        )]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    let stats = full_pass(
        &service,
        ontology_scope(&service),
        "Db serves the app with 8 replicas.",
        harness,
    )
    .await
    .unwrap();

    let onto = repo
        .ontology_get("test-user")
        .await
        .unwrap()
        .expect("delta persisted");
    assert!(
        onto.owl.contains("replicas") && onto.owl.contains("64"),
        "the facet is committed:\n{}\n{:#?}",
        onto.owl,
        mock.histories()
    );
    assert_eq!(
        stats.facts_quarantined, 0,
        "valid existing data needs no quarantine"
    );
}

/// A data property first minted by Reconcile must join the same staged T-box as the
/// page update. The old flow validated classification, then let Reconcile add an
/// undeclared `frona:` attribute which failed only at the final invariant.
#[tokio::test]
async fn reconcile_minted_data_property_is_declared_before_assemble_commit() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    mark_clean(
        &db,
        &repo,
        "services/db",
        "schema:SoftwareApplication",
        "Db",
        json!({}),
    )
    .await;
    touch(&db, "services/db", "schema:SoftwareApplication").await;

    let extract = json!({
        "new_entities": [],
        "memories": [{
            "kind":"fact",
            "sources":[{"message":"m1","quote":"strict mode","strength":"explicit"}],
            "content":"Db uses strict mode.",
            "entities":["services/db"]
        }]
    });
    let classify = classification("Db", "x", "schema:SoftwareApplication");
    let reconcile = json!({
        "relations": [], "entity_relations": [], "outdated": [], "moves": [],
        "description": "", "attributes": {"frona:strictConnectionPolicyForE2e":"strict"},
        "attribute_sources": [{
            "property":"frona:strictConnectionPolicyForE2e", "value":"strict",
            "source_memory_ids":["m1"]
        }],
        "declarations": [{
            "kind":"data_property", "term":"frona:strictConnectionPolicyForE2e",
            "description":"The active security mode of the underlying service.",
            "datatype":"xsd:string"
        }]
    });
    let adjudicate = json!({"decisions":[
        {"term":"frona:strictConnectionPolicyForE2e", "decision":"accept_proposal"}
    ]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), reconcile)]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config,
        test_user_service(&db),
        ontology_base(),
    );
    full_pass(
        &service,
        ontology_scope(&service),
        "Db uses strict mode.",
        test_harness(&db, &config, mock.clone()),
    )
    .await
    .unwrap();

    let page = repo
        .entity_by_path("test-user", "services/db")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.attributes["frona:strictConnectionPolicyForE2e"],
        "strict"
    );
    let ontology = repo
        .ontology_get("test-user")
        .await
        .unwrap()
        .expect("delta persisted");
    assert!(
        ontology.owl.contains("strictConnectionPolicyForE2e"),
        "property declaration missing; calls={}; last_history={:?}:\n{}",
        mock.calls(),
        mock.last_history(),
        ontology.owl,
    );
}

/// Any proposed ontology edit that breaks existing pages is rejected regardless of how
/// many pages are affected, so nothing is committed.
#[tokio::test]
async fn assemble_restrict_that_invalidates_existing_entities_is_rejected() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig {
        pkm_adjudication_max_attempts_per_batch: 1,
        ..Default::default()
    };
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;
    // Distinct names on purpose: near-identical ones would look like duplicates to
    // resolve's matcher, which would then burn the adjudicate response on a merge
    // verdict instead.
    for name in ["alpha", "bravo", "charlie", "delta", "echo"] {
        mark_clean(
            &db,
            &repo,
            &format!("services/{name}"),
            "schema:SoftwareApplication",
            name,
            json!({ "frona:replicas": 99999 }),
        )
        .await;
    }
    // Only one is dirty, but the dry run validates against all five existing offenders.
    touch(&db, "services/alpha", "schema:SoftwareApplication").await;

    let extract = json!({
        "new_entities": [],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Alpha","strength":"explicit"}],"content":"Alpha serves the app","entities":["services/alpha"]}]
    });
    let classify = json!({
        "entity": {"name": "alpha", "description": "A service", "aliases": []},
        "classes": [{"class": "schema:SoftwareApplication"}]
    });
    let adjudicate = json!({"decisions":[
        {"term":"frona:replicas","decision":"restrict","datatype":"xsd:integer","min":1,"max":64}
    ]});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    full_pass(
        &service,
        ontology_scope(&service),
        "Alpha serves the app.",
        harness,
    )
    .await
    .expect("a rejected ontology edit is discarded when no revision remains");

    let onto = repo.ontology_get("test-user").await.unwrap();
    let owl = onto.map(|o| o.owl).unwrap_or_default();
    assert!(
        !owl.contains("replicas"),
        "an edit that invalidates existing pages must not be committed:\n{owl}"
    );
}

/// A defer decision keeps the Classify's validated baseline. This prevents a page from
/// using an undeclared term while postponing any stronger schema decision.
#[tokio::test]
async fn assemble_defer_keeps_the_validated_baseline_declaration() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);

    mark_clean(
        &db,
        &repo,
        "people/me",
        "schema:Person",
        "Casey Owner",
        json!({}),
    )
    .await;

    let extract = json!({
        "new_entities": [{"id":"fixture-page-30","path":"things/widget","name":"Widget","description":"a thing",
            "sources":[{"message":"m1","quote":"Widget","strength":"explicit"}]}],
        "memories": [{"kind":"fact","sources":[{"message":"m1","quote":"Widget","strength":"explicit"}],"content":"Widget is a thing","entities":["things/widget"]}]
    });
    let classify = json!({
        "entity":{"name":"Widget","description":"a thing","aliases":[]},
        "classes":adjudication_classes(json!({"class":"frona:Widget"})),
        "relations":[],"attributes":[],"new_entities":[],
        "declarations":adjudication_declarations(json!({
            "kind":"class", "term":"frona:Widget",
            "description":"A widget.", "parents":["schema:Thing"]
        })),
        "has_keys":[],"inverse_functional_properties":[]
    });
    let adjudicate = json!({"decisions":adjudication_decisions(json!({
        "term":"frona:Widget","decision":"defer"
    }))});
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("e".into(), "submit".into(), extract)]),
        MockResponse::ToolCalls(vec![("c".into(), "submit".into(), classify)]),
        MockResponse::ToolCalls(vec![("r".into(), "submit".into(), empty_reconcile())]),
        MockResponse::ToolCalls(vec![("a".into(), "submit".into(), adjudicate)]),
    ]));

    let storage = StorageService::new(&config);
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let prompts = frona::agent::prompt::PromptLoader::new(resources_prompts());
    let service = PkmService::new(
        db.clone(),
        storage,
        registry,
        prompts,
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let harness = test_harness(&db, &config, mock.clone());
    full_pass(
        &service,
        ontology_scope(&service),
        "Widget is a thing.",
        harness,
    )
    .await
    .unwrap();

    let page = repo
        .entity_by_path("test-user", "things/widget")
        .await
        .unwrap()
        .unwrap();
    assert!(
        page.kinds
            .contains(&frona::memory::pkm::ontology::PrefixMap::standard().expand("frona:Widget"),),
        "the deferred term is still in use on the page: {:?}",
        page.kinds
    );
    let owl = repo
        .ontology_get("test-user")
        .await
        .unwrap()
        .map(|o| o.owl)
        .unwrap_or_default();
    assert!(
        owl.contains("Widget"),
        "the safe baseline remains declared:\n{owl}"
    );
}

/// A temporary reminder is persisted as a grounded episode and must not leak into the
/// page's current attribute bag. This drives the public ingest entry point, structured
/// model boundary, grounding, transactional commit, and DB read-back.
#[tokio::test]
async fn episodic_reminder_is_grounded_without_current_attributes() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let extract = json!({
        "new_entities": [{"id":"fixture-page-31",
            "path": "dogs/buddy",
            "name": "Buddy",
            "description": "Casey Owner's dog",
            "aliases": [],
            "sources": [{
                "message": "m1", "quote": "Buddy", "strength": "explicit",
                "confirmation": false
            }],
            "candidate_attributes": []
        }],
        "existing_entity_updates": [],
        "memories": [{
            "kind": "episodic",
            "sources": [{
                "message": "m1", "quote": "I will give Buddy ear medication next week.",
                "strength": "explicit", "confirmation": false
            }],
            "episode": {
                "status": "planned",
                "anchor": {"message": "m1", "quote": "next week"},
                "duration": {
                    "direction": "future",
                    "amount": 1,
                    "unit": "week",
                    "semantics": "calendar"
                },
                "absolute": null
            },
            "content": "Casey Owner planned to give Buddy ear medication next week",
            "entities": ["dogs/buddy"]
        }]
    });
    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("extract".into(), "submit".into(), extract),
    ])]));
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let service = PkmService::new(
        db.clone(),
        StorageService::new(&config),
        registry,
        frona::agent::prompt::PromptLoader::new(resources_prompts()),
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let at = chrono::DateTime::parse_from_rfc3339("2030-01-05T17:30:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let transcript = "[m1] User: I will give Buddy ear medication next week.";
    let scope = ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service
            .storage()
            .vault_scope(frona::handle!("testuser"), "Memory")
            .unwrap(),
        temporal_sources: vec![frona::memory::pkm::TemporalSource {
            handle: "m1".into(),
            text: "I will give Buddy ear medication next week.".into(),
            created_at: at,
            task_event_at: None,
            task_target_at: None,
        }],
        evidence_sources: vec![frona::memory::pkm::TranscriptEvidenceSource {
            handle: "m1".into(),
            text: "I will give Buddy ear medication next week.".into(),
            kind: frona::memory::pkm::TranscriptEvidenceKind::UserMessage {
                message_id: "message-1".into(),
                chat_id: "test-chat".into(),
            },
        }],
        recall: Default::default(),
        timezone: "America/Los_Angeles".into(),
    };

    let batch = service
        .mine_window(scope, transcript, test_harness(&db, &config, mock))
        .await
        .unwrap();

    let repo = PkmRepo::new(db.clone(), memory_config.pkm_search_top_k);
    commit_checkpointed_extract_patch(&repo, "test-user", &batch, None, &[]).await;
    assert!(
        repo.entity_by_path("test-user", "dogs/buddy")
            .await
            .unwrap()
            .is_none()
    );
    let record = repo
        .latest_consolidation_record("test-user")
        .await
        .unwrap()
        .unwrap();
    let effective = frona::db::repo::pkm::PkmConsolidationStore::new(Arc::new(repo.clone()))
        .scoped(&record.consolidation_id, "test-user");
    let page = effective
        .working_entity("dogs/buddy")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.attributes,
        json!({}),
        "an episode created no current attributes"
    );
    assert_eq!(page.identity_evidence.len(), 1);
    let memories = repo.list_all_memories("test-user").await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].kind, MemoryKind::Episodic);
    assert_eq!(
        memories[0].evidence[0].strength,
        frona::memory::pkm::model::EvidenceStrength::Explicit
    );
    assert!(matches!(&memories[0].evidence[0].source,
        frona::memory::pkm::model::EvidenceSource::UserMessage { message_id, chat_id, quote }
        if message_id == "message-1" && chat_id == "test-chat"
            && quote == "I will give Buddy ear medication next week"));
    let episode = memories[0]
        .episode
        .as_ref()
        .expect("episodic metadata persisted");
    assert_eq!(
        episode.status,
        frona::memory::pkm::model::EpisodeStatus::Planned
    );
    assert_eq!(episode.anchor.quote, "next week");
    assert_eq!(
        episode.resolved_start,
        Some(
            chrono::DateTime::parse_from_rfc3339("2030-01-07T08:00:00Z")
                .unwrap()
                .into()
        )
    );
    assert_eq!(
        episode.resolved_end,
        Some(
            chrono::DateTime::parse_from_rfc3339("2030-01-14T08:00:00Z")
                .unwrap()
                .into()
        )
    );
}

/// Task lifecycle evidence already contains authoritative timestamps. Extract must return
/// those timestamps as structured episode time instead of leaving the durable memory undated.
#[tokio::test]
async fn task_lifecycle_episodes_require_the_applicable_task_dates() {
    let db = test_db().await;
    seed_identity(&db).await;
    let (_tmp, config) = tmp_config();
    let memory_config = frona::core::config::MemoryConfig::default();
    let missing_dates = json!({
        "new_entities": [{
            "id": "page1",
            "path": "pets/buddy",
            "name": "Buddy",
            "description": "A dog with scheduled ear medication reminders.",
            "aliases": [],
            "sources": [{
                "message": "m1", "quote": "", "strength": "derived",
                "confirmation": false
            }],
            "candidate_attributes": []
        }],
        "existing_entity_updates": [],
        "playbooks": [],
        "memories": [
            {
                "id": "planned1",
                "kind": "episodic",
                "sources": [{
                    "message": "m1", "quote": "", "strength": "derived",
                    "confirmation": false
                }],
                "episode": {
                    "status": "planned",
                    "anchor": {"message": "m1", "quote": ""},
                    "duration": null,
                    "absolute": null
                },
                "content": "A reminder for Buddy's ear medication was scheduled.",
                "entities": ["pets/buddy"]
            },
            {
                "id": "occurred1",
                "kind": "episodic",
                "sources": [{
                    "message": "m2", "quote": "", "strength": "derived",
                    "confirmation": false
                }],
                "episode": {
                    "status": "occurred",
                    "anchor": {"message": "m2", "quote": ""},
                    "duration": null,
                    "absolute": null
                },
                "content": "The reminder system completed Buddy's ear medication reminder.",
                "entities": ["pets/buddy"]
            }
        ],
        "research_dispositions": []
    });
    let corrected_dates = json!({
        "new_entities": [{
            "id": "page1",
            "path": "pets/buddy",
            "name": "Buddy",
            "description": "A dog with scheduled ear medication reminders.",
            "aliases": [],
            "sources": [{
                "message": "m1", "quote": "", "strength": "derived",
                "confirmation": false
            }],
            "candidate_attributes": []
        }],
        "existing_entity_updates": [],
        "playbooks": [],
        "memories": [
            {
                "id": "planned1",
                "kind": "episodic",
                "sources": [{
                    "message": "m1", "quote": "", "strength": "derived",
                    "confirmation": false
                }],
                "episode": {
                    "status": "planned",
                    "anchor": {"message": "m1", "quote": ""},
                    "duration": null,
                    "absolute": {
                        "year": 2030, "month": 1, "day": 2, "hour": 10, "minute": 0
                    }
                },
                "content": "A reminder for Buddy's ear medication was scheduled.",
                "entities": ["pets/buddy"]
            },
            {
                "id": "occurred1",
                "kind": "episodic",
                "sources": [{
                    "message": "m2", "quote": "", "strength": "derived",
                    "confirmation": false
                }],
                "episode": {
                    "status": "occurred",
                    "anchor": {"message": "m2", "quote": ""},
                    "duration": null,
                    "absolute": {
                        "year": 2030, "month": 1, "day": 3, "hour": 10, "minute": 0
                    }
                },
                "content": "The reminder system completed Buddy's ear medication reminder.",
                "entities": ["pets/buddy"]
            }
        ],
        "research_dispositions": []
    });
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![("extract-1".into(), "submit".into(), missing_dates)]),
        MockResponse::ToolCalls(vec![("extract-2".into(), "submit".into(), corrected_dates)]),
    ]));
    let registry = Arc::new(test_registry_with_group(
        "mock",
        mock.clone(),
        &memory_config.model_group,
        test_model_group(),
    ));
    let service = PkmService::new(
        db.clone(),
        StorageService::new(&config),
        registry,
        frona::agent::prompt::PromptLoader::new(resources_prompts()),
        memory_config.clone(),
        test_user_service(&db),
        ontology_base(),
    );
    let scheduled_at = chrono::DateTime::parse_from_rfc3339("2030-01-01T10:00:00.000Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let target_at = chrono::DateTime::parse_from_rfc3339("2030-01-02T10:00:00.000Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let completed_at = chrono::DateTime::parse_from_rfc3339("2030-01-03T10:00:00.000Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let transcript = concat!(
        "[m1] [task scheduled event_at=2030-01-01T10:00:00.000Z ",
        "target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication\n",
        "[m2] [task completed event_at=2030-01-03T10:00:00.000Z ",
        "target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication",
    );
    let scope = ConsolidationScope {
        user_id: "test-user".into(),
        user_name: "Casey Owner".into(),
        agent_id: "test-agent".into(),
        chat_id: Some("test-chat".into()),
        vault: service.storage().vault_scope(frona::handle!("testuser"), "Memory").unwrap(),
        temporal_sources: vec![
            frona::memory::pkm::TemporalSource {
                handle: "m1".into(),
                text: "[task scheduled event_at=2030-01-01T10:00:00.000Z target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication".into(),
                created_at: scheduled_at,
                task_event_at: Some(scheduled_at),
                task_target_at: Some(target_at),
            },
            frona::memory::pkm::TemporalSource {
                handle: "m2".into(),
                text: "[task completed event_at=2030-01-03T10:00:00.000Z target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication".into(),
                created_at: completed_at,
                task_event_at: Some(completed_at),
                task_target_at: Some(target_at),
            },
        ],
        evidence_sources: vec![
            frona::memory::pkm::TranscriptEvidenceSource {
                handle: "m1".into(),
                text: "[task scheduled event_at=2030-01-01T10:00:00.000Z target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication".into(),
                kind: frona::memory::pkm::TranscriptEvidenceKind::TaskLifecycle {
                    message_id: "message-1".into(), chat_id: "test-chat".into(), task_id: "task-1".into(),
                },
            },
            frona::memory::pkm::TranscriptEvidenceSource {
                handle: "m2".into(),
                text: "[task completed event_at=2030-01-03T10:00:00.000Z target_at=2030-01-02T10:00:00.000Z] Buddy's ear medication".into(),
                kind: frona::memory::pkm::TranscriptEvidenceKind::TaskLifecycle {
                    message_id: "message-2".into(), chat_id: "test-chat".into(), task_id: "task-1".into(),
                },
            },
        ],
        recall: Default::default(),
        timezone: "America/Los_Angeles".into(),
    };

    let batch = service
        .mine_window(scope, transcript, test_harness(&db, &config, mock.clone()))
        .await
        .unwrap();

    let repo = PkmRepo::new(db, memory_config.pkm_search_top_k);
    commit_checkpointed_extract_patch(&repo, "test-user", &batch, None, &[]).await;
    let memories = repo.list_all_memories("test-user").await.unwrap();
    assert_eq!(memories.len(), 2);
    let planned = memories
        .iter()
        .find(|memory| {
            memory.episode.as_ref().is_some_and(|episode| {
                episode.status == frona::memory::pkm::model::EpisodeStatus::Planned
            })
        })
        .expect("planned task episode");
    let occurred = memories
        .iter()
        .find(|memory| {
            memory.episode.as_ref().is_some_and(|episode| {
                episode.status == frona::memory::pkm::model::EpisodeStatus::Occurred
            })
        })
        .expect("occurred task episode");
    assert_eq!(
        planned.episode.as_ref().unwrap().absolute,
        Some(frona::memory::pkm::model::AbsoluteTime {
            year: Some(2030),
            month: Some(1),
            day: Some(2),
            hour: Some(10),
            minute: Some(0),
        })
    );
    assert_eq!(
        occurred.episode.as_ref().unwrap().absolute,
        Some(frona::memory::pkm::model::AbsoluteTime {
            year: Some(2030),
            month: Some(1),
            day: Some(3),
            hour: Some(10),
            minute: Some(0),
        })
    );
    let histories = format!("{:?}", mock.histories());
    assert!(
        histories
            .matches("task_episode_missing_absolute_time")
            .count()
            >= 2,
        "Extract did not return both missing task dates in one correction: {histories}",
    );
}
