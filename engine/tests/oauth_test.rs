use chrono::Utc;
use frona::api::db;
use frona::api::repo::generic::SurrealRepo;
use frona::auth::oauth::models::OAuthIdentity;
use frona::auth::oauth::repository::OAuthRepository;
use frona::core::models::User;
use frona::core::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

#[tokio::test]
async fn test_oauth_identity_create_and_find_by_sub() {
    let db = test_db().await;
    let repo: SurrealRepo<OAuthIdentity> = SurrealRepo::new(db.clone());

    let now = Utc::now();
    let identity = OAuthIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        external_sub: "google-sub-123".to_string(),
        external_email: Some("user@gmail.com".to_string()),
        external_name: Some("Test User".to_string()),
        created_at: now,
        updated_at: now,
    };

    repo.create(&identity).await.unwrap();

    let found = repo.find_identity_by_sub("google-sub-123").await.unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.user_id, "user-1");
    assert_eq!(found.external_email, Some("user@gmail.com".to_string()));
}

#[tokio::test]
async fn test_oauth_identity_find_by_sub_not_found() {
    let db = test_db().await;
    let repo: SurrealRepo<OAuthIdentity> = SurrealRepo::new(db.clone());

    let found = repo.find_identity_by_sub("nonexistent").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_oauth_identity_find_by_user() {
    let db = test_db().await;
    let repo: SurrealRepo<OAuthIdentity> = SurrealRepo::new(db.clone());

    let now = Utc::now();
    let identity = OAuthIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-2".to_string(),
        external_sub: "provider-sub-456".to_string(),
        external_email: None,
        external_name: None,
        created_at: now,
        updated_at: now,
    };

    repo.create(&identity).await.unwrap();

    let identities = repo.find_identities_by_user("user-2").await.unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].external_sub, "provider-sub-456");

    let empty = repo.find_identities_by_user("nonexistent").await.unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn test_oauth_identity_unique_sub() {
    let db = test_db().await;
    let repo: SurrealRepo<OAuthIdentity> = SurrealRepo::new(db.clone());

    let now = Utc::now();
    let identity1 = OAuthIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        external_sub: "same-sub".to_string(),
        external_email: None,
        external_name: None,
        created_at: now,
        updated_at: now,
    };

    repo.create(&identity1).await.unwrap();

    // Creating another with the same external_sub should still work at the repo level
    // (the unique index would reject it in SurrealDB)
    let identity2 = OAuthIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-2".to_string(),
        external_sub: "same-sub".to_string(),
        external_email: None,
        external_name: None,
        created_at: now,
        updated_at: now,
    };

    let result = repo.create(&identity2).await;
    // The unique index on external_sub should cause a failure
    assert!(result.is_err());
}

#[tokio::test]
async fn test_email_matching_flow() {
    let db = test_db().await;
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    let oauth_repo: SurrealRepo<OAuthIdentity> = SurrealRepo::new(db.clone());

    // Create a user with a specific email
    let now = Utc::now();
    let user = User {
        id: uuid::Uuid::new_v4().to_string(),
        username: "existinguser".to_string(),
        email: "existing@example.com".to_string(),
        name: "Existing User".to_string(),
        password_hash: "hash".to_string(),
        created_at: now,
        updated_at: now,
    };
    user_repo.create(&user).await.unwrap();

    // Simulate SSO login — no identity exists yet, but email matches
    let identity_by_sub = oauth_repo
        .find_identity_by_sub("sso-sub-new")
        .await
        .unwrap();
    assert!(identity_by_sub.is_none());

    // User found by email — link identity
    use frona::auth::UserRepository;
    let found_user = user_repo.find_by_email("existing@example.com").await.unwrap();
    assert!(found_user.is_some());
    let found_user = found_user.unwrap();
    assert_eq!(found_user.id, user.id);

    // Create the identity link
    let identity = OAuthIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: found_user.id.clone(),
        external_sub: "sso-sub-new".to_string(),
        external_email: Some("existing@example.com".to_string()),
        external_name: Some("Existing User".to_string()),
        created_at: now,
        updated_at: now,
    };
    oauth_repo.create(&identity).await.unwrap();

    // Now finding by sub should return the linked identity
    let linked = oauth_repo
        .find_identity_by_sub("sso-sub-new")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(linked.user_id, user.id);
}
