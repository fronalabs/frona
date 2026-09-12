mod helpers;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use frona::api::routes;
use frona::auth::models::RegisterRequest;
use frona::chat::message::models::{Message, MessageRole};
use frona::chat::models::Chat;
use frona::core::config::{Config, MemoryBackend};
use frona::core::metrics::setup_metrics_recorder;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init::setup_schema;
use frona::db::repo::chats::SurrealChatRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::db::repo::pkm::PkmRepo;
use frona::inference::config::ModelRegistryConfig;
use frona::memory::pkm::PkmService;
use frona::storage::StorageService;
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;
use tower::ServiceExt;

use helpers::{
    MockModelProvider, MockResponse, test_harness, test_model_group, test_model_service_with_group,
};

fn resources() -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap()
        .ancestors()
        .find(|path| path.join("resources/prompts").exists())
        .unwrap()
        .join("resources")
}

async fn register(state: &AppState, handle: &str) -> (String, String, frona::core::Handle) {
    let (response, _) = state
        .auth_service
        .register(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            &state.policy_service,
            RegisterRequest {
                handle: handle.into(),
                email: format!("{handle}@example.com"),
                name: handle.into(),
                password: "password123".into(),
            },
        )
        .await
        .unwrap();
    (response.token, response.user.id, response.user.handle)
}

async fn rows(
    db: &Surreal<surrealdb::engine::local::Db>,
    table: &str,
    user_id: &str,
) -> Vec<serde_json::Value> {
    let mut response = db
        .query(format!("SELECT * FROM {table} WHERE user_id = $user_id"))
        .bind(("user_id", user_id.to_string()))
        .await
        .unwrap();
    response.take(0).unwrap()
}

async fn seed_chat_sources(
    db: &Surreal<surrealdb::engine::local::Db>,
    user_id: &str,
) -> (String, String) {
    let agent_id = format!("agent-{user_id}");
    SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
        .create(&frona::agent::models::Agent {
            id: agent_id.clone(),
            user_id: user_id.into(),
            handle: frona::handle!("memory-agent"),
            name: "Memory Agent".into(),
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
        .await
        .unwrap();
    let old = Utc::now() - Duration::hours(1);
    let chat = Chat {
        id: frona::core::repository::new_id(),
        user_id: user_id.into(),
        space_id: None,
        task_id: None,
        agent_id,
        title: Some("Saved memory source".into()),
        archived_at: None,
        channel_id: None,
        channel_external_id: None,
        metadata: Default::default(),
        created_at: old,
        updated_at: old,
    };
    SurrealChatRepo::new(db.clone())
        .create(&chat)
        .await
        .unwrap();
    let mut message = Message::builder(
        &chat.id,
        MessageRole::User,
        "Alice uses the reset service.".into(),
    )
    .build();
    message.created_at = old;
    let message_id = message.id.clone();
    SurrealMessageRepo::new(db.clone())
        .create(&message)
        .await
        .unwrap();
    (chat.id, message_id)
}

fn rebuild_responses() -> Vec<MockResponse> {
    vec![
        MockResponse::ToolCalls(vec![(
            "extract".into(),
            "submit".into(),
            json!({
                "new_entities": [{
                    "id": "reset-rebuilt-page",
                    "path": "services/reset-service",
                    "kind": "service",
                    "name": "Reset Service",
                    "description": "A service used by Alice",
                    "sources": [{"message": "m1", "quote": "reset service", "strength": "explicit"}]
                }],
                "memories": [{
                    "kind": "fact",
                    "content": "Alice uses the reset service.",
                    "entities": ["services/reset-service"],
                    "sources": [{"message": "m1", "quote": "Alice uses the reset service", "strength": "explicit"}]
                }]
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "classify-self".into(),
            "submit".into(),
            json!({
                "entity": {
                    "name": "reset-user",
                    "description": "The account owner.",
                    "aliases": []
                },
                "classes": [{"class": "schema:Person"}],
                "relations": []
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "classify-service".into(),
            "submit".into(),
            json!({
                "entity": {"name": "Reset Service", "description": "A service used by Alice", "aliases": []},
                "classes": [{"class": "schema:SoftwareApplication"}],
                "relations": []
            }),
        )]),
        MockResponse::ToolCalls(vec![(
            "reconcile".into(),
            "submit".into(),
            json!({
                "name": "Reset Service",
                "description": "A service used by Alice",
                "relations": [],
                "entity_relations": [],
                "outdated": [],
                "attributes": {},
                "attribute_sources": [],
                "moves": []
            }),
        )]),
        MockResponse::Text("# Reset Service\n\nAlice uses this service.".into()),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reset_rebuilds_only_the_authenticated_users_memory_on_a_later_sweep() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    setup_schema(&db).await.unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().to_string_lossy().to_string();
    let mut config = Config::default();
    config.auth.encryption_secret = "pkm-reset-e2e-secret".into();
    config.memory.backend = Some(MemoryBackend::Pkm);
    config.memory.model_group = "test".into();
    config.storage.data_dir = base.clone();
    config.storage.shared_config_dir = resources().to_string_lossy().into_owned();
    config.storage.skills_dir = format!("{base}/skills");
    config.storage.cache_dir = format!("{base}/cache");
    config.storage.ontology_dir = format!("{base}/ontology");
    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let mut state = AppState::new(
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
        setup_metrics_recorder(),
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );
    state.policy_service.sync_base_policies().await.unwrap();

    let (_, other_user_id, other_handle) = register(&state, "other-user").await;
    let (token, user_id, handle) = register(&state, "reset-user").await;
    let (chat_id, message_id) = seed_chat_sources(&db, &user_id).await;

    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::PendingWithDropDelay(std::time::Duration::from_millis(300)),
    ]));
    let registry = Arc::new(
        test_model_service_with_group("mock", mock.clone(), "test", test_model_group()).await,
    );
    let prompts = frona::agent::prompt::PromptLoader::new(resources().join("prompts"));
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    let pkm = PkmService::new(
        db.clone(),
        state.storage_service.clone(),
        registry,
        prompts,
        config.memory.clone(),
        state.user_service.clone(),
        frona::memory::pkm::ontology::Roots {
            release: fixture.join("standard"),
            user: temporary.path().join("user-ontology"),
        },
    );
    state.pkm_service = Some(pkm.clone());
    let harness = test_harness(&db, &config, mock.clone()).await;
    let seeded_repo = PkmRepo::new(db.clone(), 8);
    seeded_repo
        .upsert_entity_skeleton(
            &user_id,
            "people/alice",
            frona::memory::pkm::model::EntityCategory::Concept,
            &[],
            "Alice",
            "Derived memory",
            &[],
        )
        .await
        .unwrap();
    seeded_repo
        .create_memory_with_entities(
            &user_id,
            "source-agent",
            &chat_id,
            frona::memory::pkm::model::MemoryKind::Fact,
            "Alice has derived memory.",
            &["people/alice".into()],
        )
        .await
        .unwrap();
    seeded_repo
        .remember(&user_id, &chat_id, "source")
        .await
        .unwrap();
    seeded_repo
        .upsert_entity_skeleton(
            &other_user_id,
            "people/bob",
            frona::memory::pkm::model::EntityCategory::Concept,
            &[],
            "Bob",
            "Other derived memory",
            &[],
        )
        .await
        .unwrap();
    seeded_repo
        .create_memory_with_entities(
            &other_user_id,
            "other-agent",
            "other-chat",
            frona::memory::pkm::model::MemoryKind::Fact,
            "Bob has other derived memory.",
            &["people/bob".into()],
        )
        .await
        .unwrap();
    seeded_repo
        .remember(&other_user_id, "other-chat", "other source")
        .await
        .unwrap();
    db.query("UPDATE knowledge_short_memory SET validated = true WHERE user_id IN [$user, $other]")
        .bind(("user", user_id.clone()))
        .bind(("other", other_user_id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let user_root = state.storage_service.user_pkm_path(&handle);
    let other_root = state.storage_service.user_pkm_path(&other_handle);
    std::fs::create_dir_all(user_root.join("Memory")).unwrap();
    std::fs::create_dir_all(user_root.join("Work Notes")).unwrap();
    std::fs::create_dir_all(other_root.join("Memory")).unwrap();
    std::fs::write(user_root.join("Memory/alice.md"), "managed").unwrap();
    std::fs::write(user_root.join("Work Notes/source.md"), "external").unwrap();
    std::fs::write(other_root.join("Memory/bob.md"), "other managed").unwrap();

    state
        .set_runtime_config("pkm-reset-e2e-keep", "plain-source-value")
        .await
        .unwrap();
    assert!(
        pkm.ontology_manager().is_ready(),
        "ontology fixture did not load"
    );
    let candidates = PkmRepo::new(db.clone(), 8)
        .chats_needing_consolidation(Utc::now() - Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        candidates.contains(&chat_id),
        "seeded chat is not eligible: {candidates:?}"
    );

    let running = {
        let pkm = pkm.clone();
        let chat_service = state.chat_service.clone();
        let contact_service = state.contact_service.clone();
        let agent_service = state.agent_service.clone();
        let harness = harness.clone();
        tokio::spawn(async move {
            pkm.run_consolidation_sweep(&chat_service, &contact_service, &agent_service, &harness)
                .await
        })
    };
    let started = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while mock.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if started.is_err() {
        if running.is_finished() {
            panic!(
                "controlled consolidation stopped before inference: {:?}",
                running.await
            );
        }
        panic!("controlled consolidation did not reach inference");
    }
    db.query(
        "CREATE tool_call SET chat_id = $chat, message_id = $message, name = 'source-tool';
         CREATE chat_summary SET user_id = $user, chat_id = $chat, content = 'derived summary';",
    )
    .bind(("chat", chat_id.clone()))
    .bind(("message", message_id.clone()))
    .bind(("user", user_id.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();

    let app = Router::new()
        .merge(routes::memory_pkm_read::router())
        .with_state(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/memory/pkm/reset")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(
        !running.is_finished(),
        "the endpoint waited for the consolidation guard"
    );

    let active_status = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/memory/pkm/status")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let active_body = axum::body::to_bytes(active_status.into_body(), 4096)
        .await
        .unwrap();
    let active_body: serde_json::Value = serde_json::from_slice(&active_body).unwrap();
    assert!(matches!(
        active_body["reset"]["status"].as_str(),
        Some("pending" | "running")
    ));

    tokio::time::timeout(std::time::Duration::from_secs(2), running)
        .await
        .expect("consolidation did not stop after cancellation")
        .unwrap()
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if rows(&db, "knowledge_entity", &user_id).await.is_empty()
                && !user_root.join("Memory").exists()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background reset did not complete");

    assert!(rows(&db, "knowledge_memory", &user_id).await.is_empty());
    let short = rows(&db, "knowledge_short_memory", &user_id).await;
    assert_eq!(short.len(), 1);
    assert_eq!(short[0]["validated"], false);
    assert!(user_root.join("Work Notes/source.md").exists());
    assert_eq!(rows(&db, "chat", &user_id).await.len(), 1);
    let messages: Vec<serde_json::Value> = {
        let mut response = db
            .query("SELECT * FROM message WHERE chat_id = $chat")
            .bind(("chat", chat_id.clone()))
            .await
            .unwrap();
        response.take(0).unwrap()
    };
    assert_eq!(messages.len(), 1);
    let tool_calls: Vec<serde_json::Value> = {
        let mut response = db
            .query("SELECT * FROM tool_call WHERE chat_id = $chat")
            .bind(("chat", chat_id.clone()))
            .await
            .unwrap();
        response.take(0).unwrap()
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        state
            .get_runtime_config("pkm-reset-e2e-keep")
            .await
            .unwrap()
            .as_deref(),
        Some("plain-source-value")
    );
    assert!(rows(&db, "chat_summary", &user_id).await.is_empty());

    assert_eq!(rows(&db, "knowledge_entity", &other_user_id).await.len(), 1);
    assert_eq!(rows(&db, "knowledge_memory", &other_user_id).await.len(), 1);
    assert_eq!(
        rows(&db, "knowledge_short_memory", &other_user_id).await[0]["validated"],
        true
    );
    assert!(other_root.join("Memory/bob.md").exists());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(rows(&db, "knowledge_entity", &user_id).await.is_empty());
    assert!(
        PkmRepo::new(db.clone(), 8)
            .consolidation_watermark(&chat_id)
            .await
            .unwrap()
            .is_none()
    );

    let status = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/memory/pkm/status")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let body = axum::body::to_bytes(status.into_body(), 4096)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"available": true, "reset": null, "consolidation": null})
    );

    for response in rebuild_responses() {
        mock.enqueue(response);
    }
    pkm.run_consolidation_sweep(
        &state.chat_service,
        &state.contact_service,
        &state.agent_service,
        &harness,
    )
    .await
    .unwrap();
    let repo = PkmRepo::new(db.clone(), 8);
    assert!(
        repo.entity_by_path(&user_id, "services/reset-service")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.consolidation_watermark(&chat_id)
            .await
            .unwrap()
            .is_some()
    );
    let short = rows(&db, "knowledge_short_memory", &user_id).await;
    assert_eq!(short.len(), 1);
    assert_eq!(short[0]["validated"], true);
}
