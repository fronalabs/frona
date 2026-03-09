use frona::api::db;
use frona::api::repo::generic::SurrealRepo;
use frona::app::models::{App, AppStatus};
use frona::app::repository::AppRepository;
use frona::core::repository::Repository;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_app(id: &str, agent_id: &str, user_id: &str, status: AppStatus) -> App {
    let now = chrono::Utc::now();
    App {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        user_id: user_id.to_string(),
        name: format!("App {id}"),
        description: None,
        kind: "service".to_string(),
        command: Some("python app.py".to_string()),
        static_dir: None,
        port: Some(4000),
        status,
        pid: Some(12345),
        manifest: serde_json::json!({"id": id, "name": format!("App {id}")}),
        last_accessed_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_create_and_find_by_id() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);
    let app = make_app("app-1", "agent-1", "user-1", AppStatus::Running);

    let created = repo.create(&app).await.unwrap();
    assert_eq!(created.id, "app-1");
    assert_eq!(created.name, "App app-1");

    let found = repo.find_by_id("app-1").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().agent_id, "agent-1");
}

#[tokio::test]
async fn test_find_by_agent_id() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    repo.create(&make_app("a1", "agent-1", "user-1", AppStatus::Running))
        .await
        .unwrap();
    repo.create(&make_app("a2", "agent-1", "user-1", AppStatus::Stopped))
        .await
        .unwrap();
    repo.create(&make_app("a3", "agent-2", "user-1", AppStatus::Running))
        .await
        .unwrap();

    let agent1_apps = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(agent1_apps.len(), 2);

    let agent2_apps = repo.find_by_agent_id("agent-2").await.unwrap();
    assert_eq!(agent2_apps.len(), 1);
}

#[tokio::test]
async fn test_find_by_user_id() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    repo.create(&make_app("a1", "agent-1", "user-1", AppStatus::Running))
        .await
        .unwrap();
    repo.create(&make_app("a2", "agent-2", "user-2", AppStatus::Running))
        .await
        .unwrap();

    let user1_apps = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(user1_apps.len(), 1);
    assert_eq!(user1_apps[0].id, "a1");
}

#[tokio::test]
async fn test_find_running() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    repo.create(&make_app("a1", "agent-1", "user-1", AppStatus::Running))
        .await
        .unwrap();
    repo.create(&make_app("a2", "agent-1", "user-1", AppStatus::Stopped))
        .await
        .unwrap();
    repo.create(&make_app("a3", "agent-1", "user-1", AppStatus::Serving))
        .await
        .unwrap();
    repo.create(&make_app("a4", "agent-1", "user-1", AppStatus::Hibernated))
        .await
        .unwrap();
    repo.create(&make_app("a5", "agent-1", "user-1", AppStatus::Failed))
        .await
        .unwrap();

    let running = repo.find_running().await.unwrap();
    assert_eq!(running.len(), 3);
    let ids: Vec<&str> = running.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"a1"));
    assert!(ids.contains(&"a3"));
    assert!(ids.contains(&"a4"));
}

#[tokio::test]
async fn test_update_status() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    let app = make_app("a1", "agent-1", "user-1", AppStatus::Starting);
    repo.create(&app).await.unwrap();

    let mut app = repo.find_by_id("a1").await.unwrap().unwrap();
    assert!(matches!(app.status, AppStatus::Starting));

    app.status = AppStatus::Running;
    app.port = Some(4001);
    repo.update(&app).await.unwrap();

    let updated = repo.find_by_id("a1").await.unwrap().unwrap();
    assert!(matches!(updated.status, AppStatus::Running));
    assert_eq!(updated.port, Some(4001));
}

#[tokio::test]
async fn test_manifest_id_as_app_id() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    let app = make_app("gold-dash", "agent-1", "user-1", AppStatus::Serving);
    repo.create(&app).await.unwrap();

    let found = repo.find_by_id("gold-dash").await.unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.id, "gold-dash");
    assert_eq!(found.name, "App gold-dash");
    assert!(matches!(found.status, AppStatus::Serving));
}

#[tokio::test]
async fn test_delete() {
    let db = test_db().await;
    let repo = SurrealRepo::<App>::new(db);

    repo.create(&make_app("a1", "agent-1", "user-1", AppStatus::Running))
        .await
        .unwrap();

    repo.delete("a1").await.unwrap();
    assert!(repo.find_by_id("a1").await.unwrap().is_none());
}
