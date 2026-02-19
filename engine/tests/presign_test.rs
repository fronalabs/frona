use std::sync::Arc;

use chrono::Utc;
use frona::api::db;
use frona::api::files::{Attachment, PresignClaims, presign_attachment, presign_message};
use frona::api::repo::generic::SurrealRepo;
use frona::auth::jwt::JwtService;
use frona::auth::models::Claims;
use frona::chat::message::models::{MessageResponse, MessageRole};
use frona::credential::keypair::service::KeyPairService;
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
        created_at: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// presign_attachment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_attachment_populates_url() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("user:uid-1", "photo.png");
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400)
        .await
        .unwrap();

    let url = att.url.as_ref().expect("url should be populated");
    assert!(url.starts_with("http://localhost:3001/api/files/user/uid-1/photo.png?presign="));
}

#[tokio::test]
async fn presign_attachment_agent_path() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("agent:dev", "output.csv");
    att.filename = "output.csv".to_string();
    att.content_type = "text/csv".to_string();
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "https://app.example.com", 3600)
        .await
        .unwrap();

    let url = att.url.as_ref().expect("url should be populated");
    assert!(url.starts_with("https://app.example.com/api/files/agent/dev/output.csv?presign="));
}

#[tokio::test]
async fn presign_attachment_skips_invalid_scheme() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("invalid:x", "y");
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400)
        .await
        .unwrap();

    assert!(att.url.is_none());
}

// ---------------------------------------------------------------------------
// presign JWT token round-trip verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_token_verifies_successfully() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("user:uid-1", "photo.png");
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400)
        .await
        .unwrap();

    let url = att.url.unwrap();
    let token = url.split("?presign=").nth(1).expect("token in URL");

    let header = jwt_svc.decode_unverified_header(token).unwrap();
    let kid = header.kid.expect("kid in header");
    let decoding_key = kp_svc.get_verifying_key(&kid).await.unwrap();

    let claims = jwt_svc.verify::<PresignClaims>(token, &decoding_key).unwrap();
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
        exp: 1, // epoch second 1 — long expired
    };
    let token = jwt_svc.sign(&expired_claims, &encoding_key, &kid).unwrap();

    let decoding_key = kp_svc.get_verifying_key(&kid).await.unwrap();
    let result = jwt_svc.verify::<PresignClaims>(&token, &decoding_key);
    assert!(result.is_err());
}

#[tokio::test]
async fn presign_token_cannot_be_used_as_auth_token() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("user:uid-1", "photo.png");
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400)
        .await
        .unwrap();

    let url = att.url.unwrap();
    let token = url.split("?presign=").nth(1).unwrap();

    let header = jwt_svc.decode_unverified_header(token).unwrap();
    let kid = header.kid.unwrap();
    let decoding_key = kp_svc.get_verifying_key(&kid).await.unwrap();

    // Attempting to decode as auth Claims should fail because PresignClaims
    // lacks the required fields (token_id, token_type, etc.)
    let result = jwt_svc.verify::<Claims>(token, &decoding_key);
    assert!(result.is_err());
}

#[tokio::test]
async fn presign_token_wrong_key_rejected() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = make_attachment("user:uid-1", "photo.png");
    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400)
        .await
        .unwrap();

    let url = att.url.unwrap();
    let token = url.split("?presign=").nth(1).unwrap();

    // Create a different keypair for a different user
    let (_, other_kid) = kp_svc.get_signing_key("user:other-user").await.unwrap();
    let other_key = kp_svc.get_verifying_key(&other_kid).await.unwrap();

    // Verifying with the wrong key should fail
    let result = jwt_svc.verify::<PresignClaims>(token, &other_key);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// presign_message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_message_presigns_all_attachments() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut msg = make_message_response(vec![
        make_attachment("user:uid-1", "a.png"),
        make_attachment("user:uid-1", "b.jpg"),
    ]);
    msg.attachments[1].filename = "b.jpg".to_string();
    msg.attachments[1].content_type = "image/jpeg".to_string();

    presign_message(&mut msg, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400).await;

    for att in &msg.attachments {
        assert!(att.url.is_some(), "each attachment should have a presigned URL");
        assert!(att.url.as_ref().unwrap().contains("?presign="));
    }
}

#[tokio::test]
async fn presign_message_no_attachments_is_noop() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut msg = make_message_response(vec![]);
    presign_message(&mut msg, &kp_svc, &jwt_svc, "uid-1", "uid-1", "http://localhost:3001", 86400).await;
    assert!(msg.attachments.is_empty());
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

    // Both keys should produce tokens that verify with the same decoding key
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

    // First call populates cache, second uses it
    let dk1 = kp_svc.get_verifying_key(&kid).await.unwrap();
    let dk2 = kp_svc.get_verifying_key(&kid).await.unwrap();

    // Both should successfully verify
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

// ---------------------------------------------------------------------------
// URL structure validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presigned_url_contains_correct_path_segment() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att = Attachment {
        filename: "report.pdf".into(),
        content_type: "application/pdf".into(),
        size_bytes: 2048,
        owner: "user:uid-5".into(),
        path: "sub/dir/report.pdf".into(),
        url: None,
    };

    presign_attachment(&mut att, &kp_svc, &jwt_svc, "uid-5", "uid-5", "https://example.com", 3600)
        .await
        .unwrap();

    let url = att.url.unwrap();
    assert!(url.starts_with("https://example.com/api/files/user/uid-5/sub/dir/report.pdf?presign="));
}

#[tokio::test]
async fn presign_different_users_get_different_tokens() {
    let db = test_db().await;
    let kp_svc = keypair_service(&db);
    let jwt_svc = JwtService::new();

    let mut att1 = make_attachment("user:user-a", "file.png");
    let mut att2 = make_attachment("user:user-b", "file.png");

    presign_attachment(&mut att1, &kp_svc, &jwt_svc, "user-a", "user-a", "http://localhost", 86400)
        .await
        .unwrap();
    presign_attachment(&mut att2, &kp_svc, &jwt_svc, "user-b", "user-b", "http://localhost", 86400)
        .await
        .unwrap();

    let token1 = att1.url.unwrap().split("?presign=").nth(1).unwrap().to_string();
    let token2 = att2.url.unwrap().split("?presign=").nth(1).unwrap().to_string();

    assert_ne!(token1, token2);

    // Each token should only verify with its owner's kid
    let h1 = jwt_svc.decode_unverified_header(&token1).unwrap();
    let h2 = jwt_svc.decode_unverified_header(&token2).unwrap();
    assert_ne!(h1.kid, h2.kid);
}
