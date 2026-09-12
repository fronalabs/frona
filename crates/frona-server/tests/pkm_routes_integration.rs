//! HTTP route-layer tests for `/api/memory/pkm/*` - the auth-scope gate (403),
//! the PKM-inactive gate (404), and the happy-path JSON DTOs. Real requests via
//! `tower::oneshot` against a `AppState` built with the chosen memory backend.

mod helpers;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use frona::core::config::{Config, MemoryBackend};
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::storage::StorageService;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use helpers::seed_asserted_entity_link;

async fn test_state(backend: MemoryBackend) -> (AppState, tempfile::TempDir) {
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let mut config = Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        storage: frona::core::config::StorageConfig {
            data_dir: base.clone(),
            // Prompts come from the repo's bundled resources.
            shared_config_dir: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../resources")
                .to_string_lossy()
                .into_owned(),
            // No ontology catalogue: a checkout has none, and the server no longer
            // needs one to boot. These tests exercise config routes, not reasoning.
            ..Default::default()
        },
        ..Default::default()
    };
    config.memory.backend = Some(backend);
    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics = frona::core::metrics::setup_metrics_recorder();
    let state = AppState::new(
        db.clone(),
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );
    (state, tmp)
}

/// Create a user + mint a PAT carrying `scopes`; returns the Bearer token.
async fn pat(state: &AppState, scopes: Vec<String>) -> String {
    pat_with_groups(state, scopes, vec![]).await
}

async fn pat_with_groups(state: &AppState, scopes: Vec<String>, groups: Vec<String>) -> String {
    let user = frona::auth::User {
        id: "u1".into(),
        handle: frona::handle!("testuser"),
        email: "t@t.com".into(),
        name: "Test".into(),
        password_hash: String::new(),
        timezone: None,
        groups,
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let repo = frona::db::repo::generic::SurrealRepo::<frona::auth::User>::new(state.db.clone());
    repo.create(&user).await.unwrap();
    state
        .token_service
        .create_pat(
            &state.keypair_service,
            &user,
            frona::auth::token::models::CreatePatRequest {
                name: "test".into(),
                expires_in_days: Some(1),
                scopes: Some(scopes),
                principal: None,
            },
        )
        .await
        .unwrap()
        .token
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn pkm_router() -> axum::Router<AppState> {
    frona::api::routes::memory_pkm::router().merge(frona::api::routes::memory_pkm_read::router())
}

fn post(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn reset_requires_the_memory_scope_for_a_pat() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec![]).await;
    let app = pkm_router().with_state(state);
    let response = app
        .oneshot(post("/api/memory/pkm/reset", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn reset_is_not_available_when_pkm_is_inactive() {
    let (state, _tmp) = test_state(MemoryBackend::Basic).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let app = pkm_router().with_state(state);
    let response = app
        .oneshot(post("/api/memory/pkm/reset", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reset_accepts_an_ordinary_user_and_clears_memory_in_the_background() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let repo = frona::db::repo::pkm::PkmRepo::new(state.db.clone(), 20);
    repo.upsert_entity_skeleton(
        "u1",
        "projects/reset-me",
        frona::memory::pkm::model::EntityCategory::Concept,
        &[],
        "Reset me",
        "Derived memory",
        &[],
    )
    .await
    .unwrap();

    let app = pkm_router().with_state(state.clone());
    let response = app
        .oneshot(post("/api/memory/pkm/reset", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "pending");
    assert!(json["requestId"].is_string());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while repo
            .entity_by_path("u1", "projects/reset-me")
            .await
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reset worker did not finish");
    assert!(
        repo.entity_by_path("u1", "projects/reset-me")
            .await
            .unwrap()
            .is_none()
    );
    assert!(state.user_service.find_by_id("u1").await.unwrap().is_some());
}

#[tokio::test]
async fn config_returns_forbidden_without_memory_scope() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec![]).await; // no memory scope
    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/sync/config", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "missing scope → 403");
}

#[tokio::test]
async fn config_returns_not_found_when_pkm_backend_is_inactive() {
    let (state, _tmp) = test_state(MemoryBackend::Basic).await; // PKM not the active backend
    let token = pat(&state, vec!["memory".into()]).await;
    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/sync/config", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "PKM inactive → 404");
}

#[tokio::test]
async fn config_returns_unauthorized_without_token() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let app = pkm_router().with_state(state);
    let req = Request::builder()
        .method("GET")
        .uri("/api/memory/pkm/sync/config")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "no token → 401");
}

#[tokio::test]
async fn config_get_and_set_round_trip() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let app = pkm_router().with_state(state);

    // GET → default directory.
    let resp = app
        .clone()
        .oneshot(get("/api/memory/pkm/sync/config", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["memoryDirectory"], "Memory", "default directory");

    // POST → set it; response reflects the new value.
    let req = Request::builder()
        .method("POST")
        .uri("/api/memory/pkm/sync/config")
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"memoryDirectory":"Brain"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["memoryDirectory"], "Brain",
        "directory updated via POST"
    );
}

#[tokio::test]
async fn manifest_empty_for_fresh_user() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/sync/manifest", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["pages"].as_array().unwrap().len(), 0, "no pages yet");
}

#[tokio::test]
async fn graph_returns_the_user_scoped_memory_network() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let repo = frona::db::repo::pkm::PkmRepo::new(state.db.clone(), 20);
    repo.upsert_entity_skeleton(
        "u1",
        "people/me",
        frona::memory::pkm::model::EntityCategory::Concept,
        &["https://schema.org/Person".into()],
        "Test",
        "The account owner's page",
        &[],
    )
    .await
    .unwrap();
    repo.upsert_entity_skeleton(
        "u1",
        "projects/frona",
        frona::memory::pkm::model::EntityCategory::Concept,
        &["https://schema.org/Project".into()],
        "Frona",
        "A personal memory system",
        &[],
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &state.db,
        "u1",
        "people/me",
        "projects/frona",
        "schema:creator",
    )
    .await
    .unwrap();

    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/graph", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["selfPath"], "people/me");
    assert_eq!(json["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(json["edges"].as_array().unwrap().len(), 1);
    let me = json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["path"] == "people/me")
        .unwrap();
    assert_eq!(me["relationStats"]["outgoing"], 1);
    assert_eq!(me["relationStats"]["incoming"], 0);
    assert_eq!(me["useCount"], 0);
}

#[tokio::test]
async fn graph_adds_shared_memory_edges_only_without_a_direct_relation() {
    use frona::memory::pkm::model::{EntityCategory, MemoryKind};

    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let repo = frona::db::repo::pkm::PkmRepo::new(state.db.clone(), 20);
    for (path, name) in [("people/a", "A"), ("people/b", "B"), ("people/c", "C")] {
        repo.upsert_entity_skeleton(
            "u1",
            path,
            EntityCategory::Concept,
            &[],
            name,
            "Person",
            &[],
        )
        .await
        .unwrap();
    }
    repo.create_sourced_memory(
        "u1",
        MemoryKind::Fact,
        "A and B share a fact.",
        &["people/a".into(), "people/b".into()],
        vec![],
    )
    .await
    .unwrap();
    repo.create_sourced_memory(
        "u1",
        MemoryKind::Fact,
        "B and C share a fact.",
        &["people/b".into(), "people/c".into()],
        vec![],
    )
    .await
    .unwrap();
    seed_asserted_entity_link(&state.db, "u1", "people/b", "people/c", "schema:knows")
        .await
        .unwrap();

    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/graph", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let edges = json["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    let shared = edges
        .iter()
        .find(|edge| edge["origin"] == "memory")
        .unwrap();
    assert_eq!(shared["fromPath"], "people/a");
    assert_eq!(shared["toPath"], "people/b");
    assert_eq!(shared["sourceMemoryIds"].as_array().unwrap().len(), 1);
    assert!(edges.iter().any(|edge| {
        edge["origin"] == "asserted"
            && edge["fromPath"] == "people/b"
            && edge["toPath"] == "people/c"
    }));
}

#[tokio::test]
async fn page_exposes_article_structure_relations_and_memory_evidence() {
    use frona::memory::pkm::model::{
        EntityCategory, EvidenceSource, EvidenceStrength, MemoryEvidence, MemoryKind,
    };

    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let repo = frona::db::repo::pkm::PkmRepo::new(state.db.clone(), 20);
    repo.upsert_entity_skeleton(
        "u1",
        "people/me",
        EntityCategory::Concept,
        &["https://schema.org/Person".into()],
        "Test",
        "Owner",
        &[],
    )
    .await
    .unwrap();
    repo.upsert_entity_skeleton(
        "u1",
        "projects/frona",
        EntityCategory::Concept,
        &["https://schema.org/Project".into()],
        "Frona",
        "Memory system",
        &[],
    )
    .await
    .unwrap();
    repo.set_page_body("u1", "people/me", "# Test\n\nBuilds [[projects/frona]].")
        .await
        .unwrap();
    state.db.query(
        "UPDATE knowledge_entity SET attributes = $attrs WHERE user_id = 'u1' AND path = 'people/me'"
    ).bind(("attrs", serde_json::json!({"schema:email": "test@example.com"})))
        .await.unwrap();
    seed_asserted_entity_link(
        &state.db,
        "u1",
        "people/me",
        "projects/frona",
        "schema:creator",
    )
    .await
    .unwrap();
    repo.create_sourced_memory(
        "u1",
        MemoryKind::Fact,
        "Test builds Frona.",
        &["people/me".into()],
        vec![MemoryEvidence {
            strength: EvidenceStrength::Explicit,
            source: EvidenceSource::UserMessage {
                message_id: "m1".into(),
                chat_id: "c1".into(),
                quote: "I build Frona".into(),
            },
        }],
    )
    .await
    .unwrap();

    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/entity?path=people%2Fme", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["entity"]["body"],
        "# Test\n\nBuilds [[projects/frona]]."
    );
    assert_eq!(json["attributes"][0]["property"], "schema:email");
    assert_eq!(json["outgoingRelations"][0]["toPath"], "projects/frona");
    assert_eq!(json["memories"][0]["content"], "Test builds Frona.");
    assert_eq!(
        json["memories"][0]["evidence"][0]["source"]["user_message"]["message_id"],
        "m1"
    );
}

#[tokio::test]
async fn search_uses_the_existing_ranked_entity_search() {
    let (state, _tmp) = test_state(MemoryBackend::Pkm).await;
    let token = pat(&state, vec!["memory".into()]).await;
    let repo = frona::db::repo::pkm::PkmRepo::new(state.db.clone(), 20);
    repo.upsert_entity_skeleton(
        "u1",
        "projects/frona",
        frona::memory::pkm::model::EntityCategory::Concept,
        &["https://schema.org/Project".into()],
        "Frona",
        "A private personal memory system",
        &[],
    )
    .await
    .unwrap();

    let app = pkm_router().with_state(state);
    let resp = app
        .oneshot(get("/api/memory/pkm/search?q=personal", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["results"][0]["path"], "projects/frona");
    assert_eq!(json["results"][0]["name"], "Frona");
}
