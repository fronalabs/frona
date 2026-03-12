use std::sync::Arc;

use chrono::{Duration, Utc};
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::auth::jwt::JwtService;
use frona::auth::token::models::CreatePatRequest;
use frona::auth::token::repository::TokenRepository;
use frona::auth::token::service::TokenService;
use frona::auth::AuthService;
use frona::core::models::User;
use frona::core::repository::Repository;
use frona::credential::keypair::service::KeyPairService;
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
        username: "testuser".to_string(),
        email: "test@example.com".to_string(),
        name: "Test User".to_string(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$test$test".to_string(),
        created_at: now,
        updated_at: now,
    }
}

fn setup_services(db: &Surreal<Db>) -> (KeyPairService, TokenService) {
    let keypair_service = KeyPairService::new(
        "test-jwt-secret",
        Arc::new(SurrealRepo::new(db.clone())),
    );
    let jwt_service = JwtService::new();
    let token_service = TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        jwt_service,
        900,    // 15 min
        604800, // 7 days
    );
    (keypair_service, token_service)
}

#[tokio::test]
async fn test_keypair_get_or_create() {
    let db = test_db().await;
    let (keypair_svc, _) = setup_services(&db);

    let owner = "user:test-123";
    let kp1 = keypair_svc.get_or_create(owner).await.unwrap();
    assert_eq!(kp1.owner, owner);
    assert!(kp1.active);
    assert_eq!(kp1.public_key_bytes.len(), 32);

    // Calling again returns the same keypair
    let kp2 = keypair_svc.get_or_create(owner).await.unwrap();
    assert_eq!(kp1.id, kp2.id);
}

#[tokio::test]
async fn test_keypair_signing_and_verifying() {
    let db = test_db().await;
    let (keypair_svc, _) = setup_services(&db);

    let owner = "user:test-456";
    let (encoding_key, kid) = keypair_svc.get_signing_key(owner).await.unwrap();
    assert_eq!(kid, owner);

    let decoding_key = keypair_svc.get_verifying_key(&kid).await.unwrap();

    let jwt_svc = JwtService::new();
    let claims = frona::auth::models::Claims {
        sub: "test-456".to_string(),
        username: "testuser".to_string(),
        email: "test@example.com".to_string(),
        exp: (Utc::now().timestamp() + 3600) as usize,
        iat: Utc::now().timestamp() as usize,
        token_id: "tok-1".to_string(),
        token_type: "access".to_string(),
        agent_id: None,
        scopes: None,
    };

    let token = jwt_svc.sign(&claims, &encoding_key, &kid).unwrap();
    let verified = jwt_svc.verify::<frona::auth::models::Claims>(&token, &decoding_key).unwrap();
    assert_eq!(verified.sub, "test-456");
    assert_eq!(verified.token_id, "tok-1");
}

#[tokio::test]
async fn test_session_pair_creation() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    let (access_jwt, refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    // Validate access token
    let access_claims = token_svc.validate(&keypair_svc, &access_jwt).await.unwrap();
    assert_eq!(access_claims.sub, user.id);
    assert_eq!(access_claims.token_type, "access");

    // Validate refresh token
    let refresh_claims = token_svc.validate(&keypair_svc, &refresh_jwt).await.unwrap();
    assert_eq!(refresh_claims.sub, user.id);
    assert_eq!(refresh_claims.token_type, "refresh");
}

#[tokio::test]
async fn test_token_refresh_rotation() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    let (access_jwt, refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    // Refresh the token
    let (new_access, new_refresh, claims) = token_svc
        .refresh(&keypair_svc, &refresh_jwt)
        .await
        .unwrap();

    assert_eq!(claims.sub, user.id);

    // New tokens should be valid
    let new_access_claims = token_svc.validate(&keypair_svc, &new_access).await.unwrap();
    assert_eq!(new_access_claims.sub, user.id);

    // Old tokens should be revoked
    let old_result = token_svc.validate(&keypair_svc, &access_jwt).await;
    assert!(old_result.is_err());

    let old_refresh_result = token_svc.validate(&keypair_svc, &refresh_jwt).await;
    assert!(old_refresh_result.is_err());

    // New refresh should work
    let new_refresh_claims = token_svc.validate(&keypair_svc, &new_refresh).await.unwrap();
    assert_eq!(new_refresh_claims.token_type, "refresh");
}

#[tokio::test]
async fn test_token_revocation() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    let (access_jwt, _refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    // Token should be valid
    let claims = token_svc.validate(&keypair_svc, &access_jwt).await.unwrap();

    // Revoke the session
    let token_record = token_svc.repo().find_active_by_id(&claims.token_id).await.unwrap().unwrap();
    let pair_id = token_record.refresh_pair_id.unwrap();
    token_svc.revoke_session(&pair_id).await.unwrap();

    // Token should now be invalid
    let result = token_svc.validate(&keypair_svc, &access_jwt).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_pat_creation_and_validation() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    let pat = token_svc
        .create_pat(
            &keypair_svc,
            &user,
            CreatePatRequest {
                name: "My API Key".to_string(),
                expires_in_days: Some(30),
                scopes: Some(vec!["read".to_string()]),
            },
        )
        .await
        .unwrap();

    assert_eq!(pat.name, "My API Key");
    assert!(!pat.token.is_empty());

    // Validate PAT
    let claims = token_svc.validate(&keypair_svc, &pat.token).await.unwrap();
    assert_eq!(claims.sub, user.id);
    assert_eq!(claims.token_type, "pat");
    assert_eq!(claims.scopes, Some(vec!["read".to_string()]));

    // List PATs
    let pats = token_svc.list_pats(&user.id).await.unwrap();
    assert_eq!(pats.len(), 1);
    assert_eq!(pats[0].name, "My API Key");

    // Delete PAT
    token_svc.delete_pat(&user.id, &pat.id).await.unwrap();
    let pats = token_svc.list_pats(&user.id).await.unwrap();
    assert!(pats.is_empty());

    // Deleted PAT should be invalid
    let result = token_svc.validate(&keypair_svc, &pat.token).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_pat_ownership_check() {
    let db = test_db().await;
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user1 = test_user();
    user_repo.create(&user1).await.unwrap();

    let pat = token_svc
        .create_pat(
            &keypair_svc,
            &user1,
            CreatePatRequest {
                name: "Token".to_string(),
                expires_in_days: None,
                scopes: None,
            },
        )
        .await
        .unwrap();

    // Another user cannot delete it
    let result = token_svc.delete_pat("other-user-id", &pat.id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_jwks_listing() {
    let db = test_db().await;
    let (keypair_svc, _) = setup_services(&db);

    // No keys initially
    let jwks = keypair_svc.list_jwks().await.unwrap();
    assert!(jwks.is_empty());

    // Create a keypair
    keypair_svc.get_or_create("user:test-1").await.unwrap();

    let jwks = keypair_svc.list_jwks().await.unwrap();
    assert_eq!(jwks.len(), 1);
    assert_eq!(jwks[0]["kty"], "OKP");
    assert_eq!(jwks[0]["crv"], "Ed25519");
    assert_eq!(jwks[0]["alg"], "EdDSA");
    assert_eq!(jwks[0]["kid"], "user:test-1");
}

#[tokio::test]
async fn test_register_and_login_flow() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);
    let auth_svc = AuthService::new();

    // Register
    let (register_resp, register_refresh) = auth_svc
        .register(
            &user_repo,
            &keypair_svc,
            &token_svc,
            frona::auth::models::RegisterRequest {
                username: "newuser".to_string(),
                email: "new@example.com".to_string(),
                name: "New User".to_string(),
                password: "password123".to_string(),
            },
        )
        .await
        .unwrap();

    assert!(!register_resp.token.is_empty());
    assert!(!register_refresh.is_empty());
    assert_eq!(register_resp.user.email, "new@example.com");

    // Access token should be valid
    let claims = token_svc
        .validate(&keypair_svc, &register_resp.token)
        .await
        .unwrap();
    assert_eq!(claims.email, "new@example.com");

    // Login with same credentials
    let (login_resp, _login_refresh) = auth_svc
        .login(
            &user_repo,
            &keypair_svc,
            &token_svc,
            frona::auth::models::LoginRequest {
                identifier: "new@example.com".to_string(),
                password: "password123".to_string(),
            },
        )
        .await
        .unwrap();

    assert!(!login_resp.token.is_empty());
    assert_eq!(login_resp.user.email, "new@example.com");
}

#[tokio::test]
async fn test_login_wrong_password() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);
    let auth_svc = AuthService::new();

    // Register first
    auth_svc
        .register(
            &user_repo,
            &keypair_svc,
            &token_svc,
            frona::auth::models::RegisterRequest {
                username: "wrongpwtest".to_string(),
                email: "test@example.com".to_string(),
                name: "Test".to_string(),
                password: "correct-password".to_string(),
            },
        )
        .await
        .unwrap();

    // Login with wrong password
    let result = auth_svc
        .login(
            &user_repo,
            &keypair_svc,
            &token_svc,
            frona::auth::models::LoginRequest {
                identifier: "test@example.com".to_string(),
                password: "wrong".to_string(),
            },
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_duplicate_registration() {
    let db = test_db().await;
    let user_repo = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);
    let auth_svc = AuthService::new();

    let req = frona::auth::models::RegisterRequest {
        username: "dupuser".to_string(),
        email: "dup@example.com".to_string(),
        name: "User".to_string(),
        password: "password123".to_string(),
    };

    auth_svc
        .register(&user_repo, &keypair_svc, &token_svc, req)
        .await
        .unwrap();

    let req2 = frona::auth::models::RegisterRequest {
        username: "dupuser2".to_string(),
        email: "dup@example.com".to_string(),
        name: "User 2".to_string(),
        password: "password456".to_string(),
    };

    let result = auth_svc
        .register(&user_repo, &keypair_svc, &token_svc, req2)
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_refresh_survives_service_restart() {
    let db = test_db().await;
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    let (_access_jwt, refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    // Grab the refresh token's claims before dropping services
    let refresh_claims = token_svc
        .validate(&keypair_svc, &refresh_jwt)
        .await
        .unwrap();

    // Drop services to clear all in-memory caches (simulates server restart)
    drop(token_svc);
    drop(keypair_svc);

    // Verify the api_token record still exists in the DB via raw query
    let raw: Option<String> = db
        .query("SELECT VALUE meta::id(id) FROM api_token WHERE meta::id(id) = $id")
        .bind(("id", refresh_claims.token_id.clone()))
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert!(
        raw.is_some(),
        "api_token record missing from DB after service drop"
    );

    // Create fresh services from the same DB (restart)
    let (keypair_svc2, token_svc2) = setup_services(&db);

    // Refresh must succeed with new service instances
    let (new_access, new_refresh, claims) = token_svc2
        .refresh(&keypair_svc2, &refresh_jwt)
        .await
        .unwrap();

    assert_eq!(claims.sub, user.id);

    // New tokens should be valid
    let access_claims = token_svc2
        .validate(&keypair_svc2, &new_access)
        .await
        .unwrap();
    assert_eq!(access_claims.sub, user.id);
    assert_eq!(access_claims.token_type, "access");

    let refresh_claims = token_svc2
        .validate(&keypair_svc2, &new_refresh)
        .await
        .unwrap();
    assert_eq!(refresh_claims.sub, user.id);
    assert_eq!(refresh_claims.token_type, "refresh");
}

#[tokio::test]
async fn test_expired_token_not_found_by_find_active() {
    use frona::auth::token::models::{ApiToken, TokenType};

    let db = test_db().await;
    let token_repo: SurrealRepo<ApiToken> = SurrealRepo::new(db.clone());

    let now = Utc::now();
    let token = ApiToken {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        name: "short-lived".to_string(),
        token_type: TokenType::Access,
        agent_id: None,
        scopes: vec![],
        prefix: "test...".to_string(),
        expires_at: now + Duration::seconds(1),
        last_used_at: None,
        refresh_pair_id: None,
        created_at: now,
        updated_at: now,
    };

    token_repo.create(&token).await.unwrap();

    let found = TokenRepository::find_active_by_id(&token_repo, &token.id)
        .await
        .unwrap();
    assert!(found.is_some(), "token should be active before expiry");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let found = TokenRepository::find_active_by_id(&token_repo, &token.id)
        .await
        .unwrap();
    assert!(found.is_none(), "token should not be found after expiry");
}

#[tokio::test]
async fn test_refresh_cookie_round_trip() {
    use frona::api::cookie::{extract_refresh_token_from_cookie_header, make_refresh_cookie};

    let db = test_db().await;
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    let (keypair_svc, token_svc) = setup_services(&db);

    let user = test_user();
    user_repo.create(&user).await.unwrap();

    // Step 1: Create session pair (same as login/register handler)
    let (_access_jwt, refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    // Step 2: Build Set-Cookie header (same as HTTP handler)
    let cookie_header = make_refresh_cookie(&refresh_jwt, token_svc.refresh_expiry_secs(), false);
    let cookie_str = cookie_header.to_str().unwrap();

    // Step 3: Extract refresh token from cookie (same as refresh handler)
    let extracted = extract_refresh_token_from_cookie_header(cookie_str)
        .expect("refresh_token must be extractable from cookie");
    assert_eq!(extracted, refresh_jwt);

    // Step 4: Simulate server restart — drop and recreate services
    drop(token_svc);
    drop(keypair_svc);
    let (keypair_svc2, token_svc2) = setup_services(&db);

    // Step 5: Use extracted cookie value to refresh (same as refresh handler)
    let (new_access, new_refresh, claims) = token_svc2
        .refresh(&keypair_svc2, extracted)
        .await
        .expect("refresh with cookie-extracted token must succeed after restart");

    assert_eq!(claims.sub, user.id);
    assert_eq!(claims.token_type, "refresh");

    // Step 6: Verify new tokens work
    let access_claims = token_svc2
        .validate(&keypair_svc2, &new_access)
        .await
        .unwrap();
    assert_eq!(access_claims.token_type, "access");

    let refresh_claims = token_svc2
        .validate(&keypair_svc2, &new_refresh)
        .await
        .unwrap();
    assert_eq!(refresh_claims.token_type, "refresh");
}
