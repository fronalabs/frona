use std::sync::Arc;

use chrono::Utc;
use frona::db::init as db;
use frona::api::files::{Attachment, PresignClaims};
use frona::db::repo::generic::SurrealRepo;
use frona::auth::jwt::JwtService;
use frona::auth::models::Claims;
use frona::auth::UserService;
use frona::chat::message::models::{MessageResponse, MessageRole};
use frona::auth::User;
use frona::core::config::CacheConfig;
use frona::core::repository::Repository;
use frona::credential::keypair::service::KeyPairService;
use frona::credential::presign::{PresignService, presign_response, presign_response_by_user_id};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn keypair_service(db: &Surreal<Db>) -> KeyPairService {
    KeyPairService::new("test-jwt-secret", Arc::new(SurrealRepo::new(db.clone())))
}

fn presign_service(db: &Surreal<Db>) -> PresignService {
    let kp_svc = keypair_service(db);
    let user_service = UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default());
    PresignService::new(
        kp_svc,
        user_service,
        "http://localhost:3001".to_string(),
        86400,
    )
}

async fn create_user(db: &Surreal<Db>, id: &str, username: &str) {
    let repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    let user = User {
        id: id.to_string(),
        username: username.to_string(),
        email: format!("{username}@test.com"),
        name: username.to_string(),
        password_hash: String::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    repo.create(&user).await.unwrap();
}

fn make_attachment(owner: &str, path: &str) -> Attachment {
    Attachment {
        filename: "photo.png".to_string(),
        content_type: "image/png".to_string(),
        size_bytes: 1024,
        owner: owner.to_string(),
        path: path.to_string(),
        url: None,
    }
}

fn make_message_response(attachments: Vec<Attachment>) -> MessageResponse {
    MessageResponse {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id: "chat-1".to_string(),
        role: MessageRole::User,
        content: "hello".to_string(),
        agent_id: None,
        tool_calls: None,
        tool_call_id: None,
        tool: None,
        attachments,
        contact_id: None,
        created_at: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// PresignService.sign
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_service_sign_user_file() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url = svc.sign("user:uid-1", "photo.png", "uid-1", "jdoe").await.unwrap();
    assert!(url.starts_with("http://localhost:3001/api/files/user/jdoe/photo.png?presign="));
}

#[tokio::test]
async fn presign_service_sign_agent_file() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url = svc.sign("agent:dev", "output.csv", "uid-1", "jdoe").await.unwrap();
    assert!(url.starts_with("http://localhost:3001/api/files/agent/dev/output.csv?presign="));
}

#[tokio::test]
async fn presign_service_sign_invalid_owner() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url = svc.sign("invalid:x", "y", "uid-1", "jdoe").await.unwrap();
    assert!(url.is_empty());
}

#[tokio::test]
async fn presign_service_sign_nested_path() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url = svc.sign("user:uid-5", "sub/dir/report.pdf", "uid-5", "jdoe").await.unwrap();
    assert!(url.starts_with("http://localhost:3001/api/files/user/jdoe/sub/dir/report.pdf?presign="));
}

// ---------------------------------------------------------------------------
// PresignService.verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_service_verify_round_trip() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url = svc.sign("user:uid-1", "photo.png", "uid-1", "jdoe").await.unwrap();
    let token = url.split("?presign=").nth(1).expect("token in URL");

    let claims = svc.verify(token).await.unwrap();
    assert_eq!(claims.sub, "uid-1");
    assert_eq!(claims.owner, "user:uid-1");
    assert_eq!(claims.path, "photo.png");
    assert!(claims.exp > Utc::now().timestamp() as usize);
}

#[tokio::test]
async fn presign_token_rejected_when_expired() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let owner = "user:uid-1";
    let (encoding_key, kid) = kp_svc.get_signing_key(owner).await.unwrap();

    let expired_claims = PresignClaims {
        sub: "uid-1".to_string(),
        owner: "user:uid-1".to_string(),
        path: "photo.png".to_string(),
        exp: 1,
    };
    let token = jwt_svc.sign(&expired_claims, &encoding_key, &kid).unwrap();

    let svc = presign_service(&db);
    let result = svc.verify(&token).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn presign_token_cannot_be_used_as_auth_token() {
    let db = test_db().await;
    let svc = presign_service(&db);
    let jwt_svc = JwtService::new();

    let url = svc.sign("user:uid-1", "photo.png", "uid-1", "jdoe").await.unwrap();
    let token = url.split("?presign=").nth(1).unwrap();

    let header = jwt_svc.decode_unverified_header(token).unwrap();
    let kid = header.kid.unwrap();
    let kp_svc = keypair_service(&db);
    let decoding_key = kp_svc.get_verifying_key(&kid).await.unwrap();

    let result = jwt_svc.verify::<Claims>(token, &decoding_key);
    assert!(result.is_err());
}

#[tokio::test]
async fn presign_token_wrong_key_rejected() {
    let db = test_db().await;
    let svc = presign_service(&db);
    let kp_svc = keypair_service(&db);

    let url = svc.sign("user:uid-1", "photo.png", "uid-1", "jdoe").await.unwrap();
    let token = url.split("?presign=").nth(1).unwrap();

    let (_, other_kid) = kp_svc.get_signing_key("user:other-user").await.unwrap();
    let other_key = kp_svc.get_verifying_key(&other_kid).await.unwrap();

    let jwt_svc = JwtService::new();
    let result = jwt_svc.verify::<PresignClaims>(token, &other_key);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// presign_response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_response_presigns_all_attachments() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let mut msg = make_message_response(vec![
        make_attachment("user:uid-1", "a.png"),
        make_attachment("user:uid-1", "b.jpg"),
    ]);

    presign_response(&svc, &mut msg, "uid-1", "jdoe").await;

    for att in &msg.attachments {
        assert!(att.url.is_some(), "each attachment should have a presigned URL");
        assert!(att.url.as_ref().unwrap().contains("?presign="));
    }
}

#[tokio::test]
async fn presign_response_no_attachments_is_noop() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let mut msg = make_message_response(vec![]);
    presign_response(&svc, &mut msg, "uid-1", "jdoe").await;
    assert!(msg.attachments.is_empty());
}

// ---------------------------------------------------------------------------
// sign_by_user_id (resolves username from DB)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_by_user_id_resolves_username() {
    let db = test_db().await;
    create_user(&db, "uid-10", "alice").await;
    let svc = presign_service(&db);

    let url = svc.sign_by_user_id("user:uid-10", "doc.pdf", "uid-10").await.unwrap();
    assert!(url.contains("/user/alice/doc.pdf?presign="));
}

#[tokio::test]
async fn sign_by_user_id_caches_username() {
    let db = test_db().await;
    create_user(&db, "uid-20", "bob").await;
    let svc = presign_service(&db);

    let url1 = svc.sign_by_user_id("user:uid-20", "a.txt", "uid-20").await.unwrap();
    let url2 = svc.sign_by_user_id("user:uid-20", "b.txt", "uid-20").await.unwrap();

    assert!(url1.contains("/user/bob/a.txt"));
    assert!(url2.contains("/user/bob/b.txt"));
}

#[tokio::test]
async fn sign_by_user_id_unknown_user_returns_error() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let result = svc.sign_by_user_id("user:nonexistent", "file.txt", "nonexistent").await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// presign_response_by_user_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_response_by_user_id_resolves_and_presigns() {
    let db = test_db().await;
    create_user(&db, "uid-30", "carol").await;
    let svc = presign_service(&db);

    let mut msg = make_message_response(vec![
        make_attachment("user:uid-30", "photo.png"),
        make_attachment("agent:dev", "output.csv"),
    ]);

    presign_response_by_user_id(&svc, &mut msg, "uid-30").await;

    assert!(msg.attachments[0].url.as_ref().unwrap().contains("/user/carol/photo.png"));
    assert!(msg.attachments[1].url.as_ref().unwrap().contains("/agent/dev/output.csv"));
}

// ---------------------------------------------------------------------------
// presign_response with mixed valid/invalid owners
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_response_skips_invalid_owners() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let mut msg = make_message_response(vec![
        make_attachment("user:uid-1", "valid.png"),
        make_attachment("invalid:x", "skip.txt"),
        make_attachment("agent:dev", "also_valid.csv"),
    ]);

    presign_response(&svc, &mut msg, "uid-1", "jdoe").await;

    assert!(msg.attachments[0].url.is_some());
    assert!(msg.attachments[1].url.is_none());
    assert!(msg.attachments[2].url.is_some());
}

// ---------------------------------------------------------------------------
// Different users get different tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_different_users_get_different_tokens() {
    let db = test_db().await;
    let svc = presign_service(&db);

    let url1 = svc.sign("user:user-a", "file.png", "user-a", "alice").await.unwrap();
    let url2 = svc.sign("user:user-b", "file.png", "user-b", "bob").await.unwrap();

    let token1 = url1.split("?presign=").nth(1).unwrap();
    let token2 = url2.split("?presign=").nth(1).unwrap();
    assert_ne!(token1, token2);
}

// ---------------------------------------------------------------------------
// Key caching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signing_key_cache_returns_same_key() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let owner = "user:cache-test";
    let (key1, kid1) = kp_svc.get_signing_key(owner).await.unwrap();
    let (key2, kid2) = kp_svc.get_signing_key(owner).await.unwrap();

    assert_eq!(kid1, kid2);

    let claims = PresignClaims {
        sub: "x".into(),
        owner: "user:x".into(),
        path: "f".into(),
        exp: (Utc::now().timestamp() + 3600) as usize,
    };

    let t1 = jwt_svc.sign(&claims, &key1, &kid1).unwrap();
    let t2 = jwt_svc.sign(&claims, &key2, &kid2).unwrap();

    let dk = kp_svc.get_verifying_key(&kid1).await.unwrap();
    jwt_svc.verify::<PresignClaims>(&t1, &dk).unwrap();
    jwt_svc.verify::<PresignClaims>(&t2, &dk).unwrap();
}

#[tokio::test]
async fn verifying_key_cache_returns_same_key() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let owner = "user:verify-cache";
    let (enc_key, kid) = kp_svc.get_signing_key(owner).await.unwrap();

    let claims = PresignClaims {
        sub: "x".into(),
        owner: "user:x".into(),
        path: "f".into(),
        exp: (Utc::now().timestamp() + 3600) as usize,
    };
    let token = jwt_svc.sign(&claims, &enc_key, &kid).unwrap();

    let dk1 = kp_svc.get_verifying_key(&kid).await.unwrap();
    let dk2 = kp_svc.get_verifying_key(&kid).await.unwrap();

    jwt_svc.verify::<PresignClaims>(&token, &dk1).unwrap();
    jwt_svc.verify::<PresignClaims>(&token, &dk2).unwrap();
}

// ---------------------------------------------------------------------------
// Generic JWT sign/verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generic_jwt_sign_verify_with_presign_claims() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let owner = "user:generic-test";
    let (enc_key, kid) = kp_svc.get_signing_key(owner).await.unwrap();

    let claims = PresignClaims {
        sub: "uid-42".into(),
        owner: "user:uid-42".into(),
        path: "doc.pdf".into(),
        exp: (Utc::now().timestamp() + 3600) as usize,
    };

    let token = jwt_svc.sign(&claims, &enc_key, &kid).unwrap();
    let dec_key = kp_svc.get_verifying_key(&kid).await.unwrap();
    let verified = jwt_svc.verify::<PresignClaims>(&token, &dec_key).unwrap();

    assert_eq!(verified.sub, "uid-42");
    assert_eq!(verified.owner, "user:uid-42");
    assert_eq!(verified.path, "doc.pdf");
}

#[tokio::test]
async fn generic_jwt_still_works_with_auth_claims() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let owner = "user:auth-compat";
    let (enc_key, kid) = kp_svc.get_signing_key(owner).await.unwrap();

    let claims = Claims {
        sub: "uid-99".to_string(),
        username: "testuser".to_string(),
        email: "test@example.com".to_string(),
        exp: (Utc::now().timestamp() + 3600) as usize,
        iat: Utc::now().timestamp() as usize,
        token_id: "tok-1".to_string(),
        token_type: "access".to_string(),
        agent_id: None,
        scopes: None,
    };

    let token = jwt_svc.sign(&claims, &enc_key, &kid).unwrap();
    let dec_key = kp_svc.get_verifying_key(&kid).await.unwrap();
    let verified = jwt_svc.verify::<Claims>(&token, &dec_key).unwrap();

    assert_eq!(verified.sub, "uid-99");
    assert_eq!(verified.token_type, "access");
}
