use chrono::Utc;
use frona::api::db;
use frona::api::repo::users::SurrealUserRepo;
use frona::auth::UserRepository;
use frona::core::repository::Repository;
use frona::core::models::User;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_user() -> User {
    let now = Utc::now();
    User {
        id: uuid::Uuid::new_v4().to_string(),
        email: "test@example.com".to_string(),
        name: "Test User".to_string(),
        password_hash: "hashed_password".to_string(),
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_create_and_find_by_id() {
    let db = test_db().await;
    let repo = SurrealUserRepo::new(db);
    let user = test_user();

    let created = repo.create(&user).await.unwrap();
    assert_eq!(created.id, user.id);
    assert_eq!(created.email, user.email);
    assert_eq!(created.name, user.name);
    assert_eq!(created.password_hash, user.password_hash);
    assert_eq!(created.created_at, user.created_at);
    assert_eq!(created.updated_at, user.updated_at);

    let found = repo.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(found.id, user.id);
    assert_eq!(found.email, user.email);
    assert_eq!(found.created_at, user.created_at);
    assert_eq!(found.updated_at, user.updated_at);
}

#[tokio::test]
async fn test_find_by_email() {
    let db = test_db().await;
    let repo = SurrealUserRepo::new(db);
    let user = test_user();

    repo.create(&user).await.unwrap();

    let found = repo.find_by_email("test@example.com").await.unwrap().unwrap();
    assert_eq!(found.id, user.id);
    assert_eq!(found.email, user.email);
    assert_eq!(found.created_at, user.created_at);
}

#[tokio::test]
async fn test_find_by_email_not_found() {
    let db = test_db().await;
    let repo = SurrealUserRepo::new(db);

    let found = repo.find_by_email("nonexistent@example.com").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_find_by_id_not_found() {
    let db = test_db().await;
    let repo = SurrealUserRepo::new(db);

    let found = repo.find_by_id("nonexistent-id").await.unwrap();
    assert!(found.is_none());
}
