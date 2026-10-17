//! Verifies that the `AuthUser` extractor round-trips the `principal` claim
//! for each of the three principal-carrying token kinds: a standard access
//! token (Principal::User), an agent-bound PAT (Principal::Agent), and an
//! ephemeral token (Principal::Agent with no DB row).

use std::sync::Arc;

use frona::auth::User;
use frona::auth::ephemeral_token::EphemeralTokenGuard;
use frona::auth::jwt::JwtService;
use frona::auth::token::models::CreatePatRequest;
use frona::auth::token::service::TokenService;
use frona::core::repository::Repository;
use frona::core::{Principal, PrincipalKind};
use frona::credential::keypair::service::KeyPairService;
use frona::db::init::setup_schema;
use frona::db::repo::generic::SurrealRepo;

async fn setup() -> (TokenService, KeyPairService, User, tempfile::TempDir) {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    setup_schema(&db).await.unwrap();

    let keypair = KeyPairService::new("test-secret", Arc::new(SurrealRepo::new(db.clone())));
    let user_service = frona::auth::user_service::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    let tokens = TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        JwtService::new(),
        user_service,
        900,
        604_800,
    );
    let user = User {
        id: "user-zzz".into(),
        handle: frona::handle!("bob"),
        email: "b@example.com".into(),
        name: "Bob".into(),
        password_hash: String::new(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    // Persist the user so refresh/access DB checks pass.
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db);
    user_repo.create(&user).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    (tokens, keypair, user, tmp)
}

#[tokio::test]
async fn session_access_token_validates_as_user_principal() {
    let (tokens, keypair, user, _tmp) = setup().await;

    let (access, _refresh) = tokens.create_session_pair(&keypair, &user).await.unwrap();

    let claims = tokens.validate_api(&keypair, &access).await.unwrap();
    assert_eq!(claims.token_type, "access");
    assert_eq!(claims.principal.kind, PrincipalKind::User);
    assert_eq!(claims.principal.id, user.id);
}

#[tokio::test]
async fn user_scoped_pat_defaults_to_user_principal() {
    let (tokens, keypair, user, _tmp) = setup().await;

    let pat = tokens
        .create_pat(
            &keypair,
            &user,
            CreatePatRequest {
                name: "test".into(),
                expires_in_days: Some(30),
                scopes: Some(vec!["read".into()]),
                principal: None,
            },
        )
        .await
        .unwrap();

    let claims = tokens.validate_api(&keypair, &pat.token).await.unwrap();
    assert_eq!(claims.token_type, "pat");
    assert_eq!(claims.principal.kind, PrincipalKind::User);
    assert_eq!(claims.principal.id, user.id);
}

#[tokio::test]
async fn agent_bound_pat_carries_agent_principal() {
    let (tokens, keypair, user, _tmp) = setup().await;

    let pat = tokens
        .create_pat(
            &keypair,
            &user,
            CreatePatRequest {
                name: "agent-pat".into(),
                expires_in_days: Some(30),
                scopes: None,
                principal: Some(Principal::agent("agent-123")),
            },
        )
        .await
        .unwrap();

    let claims = tokens.validate_api(&keypair, &pat.token).await.unwrap();
    assert_eq!(claims.principal.kind, PrincipalKind::Agent);
    assert_eq!(claims.principal.id, "agent-123");
}

#[tokio::test]
async fn ephemeral_token_validates_without_db_row_but_access_does_not() {
    let (tokens, keypair, user, tmp) = setup().await;

    // Ephemeral: skips DB check — validates even with no row.
    let guard = EphemeralTokenGuard::issue(
        &tokens,
        &keypair,
        &user,
        Principal::agent("agent-e"),
        60,
        tmp.path(),
    )
    .await
    .unwrap();
    let jwt = tokio::fs::read_to_string(guard.path()).await.unwrap();
    let claims = tokens.validate_api(&keypair, jwt.trim()).await.unwrap();
    assert_eq!(claims.principal.kind, PrincipalKind::Agent);

    // Access: requires DB row. Issue an access token, delete the row, then
    // re-validate — should fail.
    let (access_jwt, _) = tokens.create_session_pair(&keypair, &user).await.unwrap();
    let claims = tokens.validate_api(&keypair, &access_jwt).await.unwrap();
    tokens.repo().delete(&claims.token_id).await.unwrap();
    let err = tokens
        .validate_api(&keypair, &access_jwt)
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("revoked") || format!("{err:?}").contains("TokenInvalid"),
        "expected access token with missing DB row to fail validation, got {err:?}"
    );
}
