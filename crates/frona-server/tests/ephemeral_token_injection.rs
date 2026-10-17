//! Verifies that `EphemeralTokenGuard::issue` produces a per-invocation
//! token file on disk, that the JWT inside decodes with the expected
//! `TokenType::Ephemeral` and principal, and that dropping the guard
//! unlinks the file.

use std::sync::Arc;

use frona::auth::User;
use frona::auth::ephemeral_token::EphemeralTokenGuard;
use frona::auth::jwt::JwtService;
use frona::auth::models::Claims;
use frona::auth::token::service::TokenService;
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
        id: "user-abc".into(),
        handle: frona::handle!("alice"),
        email: "a@example.com".into(),
        name: "Alice".into(),
        password_hash: String::new(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let tmp = tempfile::tempdir().unwrap();
    (tokens, keypair, user, tmp)
}

#[tokio::test]
async fn guard_writes_file_with_agent_principal_and_unlinks_on_drop() {
    let (tokens, keypair, user, tmp) = setup().await;
    let root = tmp.path().to_path_buf();

    let path = {
        let guard = EphemeralTokenGuard::issue(
            &tokens,
            &keypair,
            &user,
            Principal::agent("agent-xyz"),
            60,
            &root,
        )
        .await
        .unwrap();

        // File exists while the guard is alive.
        assert!(
            guard.path().exists(),
            "token file should exist during guard lifetime"
        );

        // JWT inside decodes with the expected shape.
        let jwt = tokio::fs::read_to_string(guard.path()).await.unwrap();
        let claims = tokens
            .validate_api(&keypair, jwt.trim())
            .await
            .expect("ephemeral token should validate without any DB row");
        assert_eq!(claims.token_type, "ephemeral");
        assert_eq!(claims.principal.kind, PrincipalKind::Agent);
        assert_eq!(claims.principal.id, "agent-xyz");
        assert_eq!(claims.sub, user.id);

        guard.path().to_path_buf()
    };

    // Dropped at end of block — file must be gone.
    assert!(
        !path.exists(),
        "dropping the guard must unlink the token file"
    );
}

#[tokio::test]
async fn each_invocation_gets_a_distinct_path() {
    let (tokens, keypair, user, tmp) = setup().await;
    let root = tmp.path().to_path_buf();

    let g1 = EphemeralTokenGuard::issue(&tokens, &keypair, &user, Principal::agent("a"), 60, &root)
        .await
        .unwrap();
    let g2 = EphemeralTokenGuard::issue(&tokens, &keypair, &user, Principal::agent("a"), 60, &root)
        .await
        .unwrap();

    assert_ne!(
        g1.path(),
        g2.path(),
        "sibling invocations must write to distinct paths so neither sees the other's JWT"
    );

    // Both paths share the same parent directory (the canonicalized root).
    assert_eq!(g1.path().parent(), g2.path().parent());
}

#[tokio::test]
async fn ephemeral_token_type_is_stateless_and_others_are_not() {
    use frona::auth::token::models::TokenType;
    assert!(TokenType::Ephemeral.is_stateless());
    assert!(!TokenType::Access.is_stateless());
    assert!(!TokenType::Refresh.is_stateless());
    assert!(!TokenType::Pat.is_stateless());
}

#[tokio::test]
async fn claims_schema_round_trip() {
    let (tokens, keypair, user, tmp) = setup().await;
    let guard = EphemeralTokenGuard::issue(
        &tokens,
        &keypair,
        &user,
        Principal::mcp_server("srv-1"),
        60,
        tmp.path(),
    )
    .await
    .unwrap();

    let jwt = tokio::fs::read_to_string(guard.path()).await.unwrap();
    // Decode unverified payload (base64 middle segment) to inspect JSON shape.
    let payload = jwt
        .trim()
        .split('.')
        .nth(1)
        .expect("jwt should have payload");
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
        .expect("payload must be valid base64");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["token_type"], "ephemeral");
    assert_eq!(json["principal"]["kind"], "mcp_server");
    assert_eq!(json["principal"]["id"], "srv-1");

    let _: Claims = serde_json::from_value(json).unwrap();
}
