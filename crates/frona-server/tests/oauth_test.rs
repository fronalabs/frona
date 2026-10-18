use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use frona::auth::UserService;
use frona::auth::jwt::JwtService;
use frona::auth::oauth::service::OAuthService;
use frona::auth::token::service::TokenService;
use frona::core::config::{CacheConfig, Config};
use frona::core::error::{AppError, AuthErrorCode};
use frona::credential::keypair::service::KeyPairService;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use chrono::Utc;
use frona::auth::User;
use frona::auth::oauth::models::OAuthIdentity;
use frona::auth::oauth::repository::OAuthRepository;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

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
        id: frona::core::repository::new_id(),
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
        id: frona::core::repository::new_id(),
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
        id: frona::core::repository::new_id(),
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
        id: frona::core::repository::new_id(),
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

    let now = Utc::now();
    let user = User {
        id: frona::core::repository::new_id(),
        handle: frona::handle!("existinguser"),
        email: "existing@example.com".to_string(),
        name: "Existing User".to_string(),
        password_hash: "hash".to_string(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
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
    let found_user = user_repo
        .find_by_email("existing@example.com")
        .await
        .unwrap();
    assert!(found_user.is_some());
    let found_user = found_user.unwrap();
    assert_eq!(found_user.id, user.id);

    let identity = OAuthIdentity {
        id: frona::core::repository::new_id(),
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

struct Fixture {
    oauth: OAuthService,
    users: UserService,
    keys: KeyPairService,
    tokens: TokenService,
}

impl Fixture {
    fn new(db: Surreal<Db>, authority: &str) -> Self {
        let mut config = Config::default();
        config.sso.authority = Some(authority.into());
        config.sso.client_id = Some("test-client".into());
        config.sso.client_secret = Some("test-secret".into());
        config.server.backend_url = Some("http://localhost:3001".into());
        let users = UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default());
        Self {
            oauth: OAuthService::new(&config, Arc::new(SurrealRepo::new(db.clone()))).unwrap(),
            keys: KeyPairService::new("test-key", Arc::new(SurrealRepo::new(db.clone()))),
            tokens: TokenService::new(
                Arc::new(SurrealRepo::new(db)),
                JwtService::new(),
                users.clone(),
                900,
                604800,
            ),
            users,
        }
    }

    async fn callback(&self, state: &str) -> Result<(User, bool), AppError> {
        self.oauth
            .handle_callback("test-code", state, &self.users, &self.keys, &self.tokens)
            .await
    }
}

fn assert_csrf<T: std::fmt::Debug>(result: Result<T, AppError>) {
    assert!(
        matches!(
            result,
            Err(AppError::Auth {
                code: AuthErrorCode::CsrfFailed,
                ..
            })
        ),
        "expected rejected SSO state, got {result:?}"
    );
}

async fn mock_provider(fixture: &Fixture, server: &MockServer) -> jsonwebtoken::EncodingKey {
    let (signing_key, kid) = fixture.keys.get_signing_key("provider").await.unwrap();
    let public_key = fixture.keys.get_or_create("provider").await.unwrap();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{
                "kty": "OKP", "crv": "Ed25519", "use": "sig",
                "alg": "EdDSA", "kid": kid,
                "x": URL_SAFE_NO_PAD.encode(public_key.public_key_bytes)
            }]
        })))
        .mount(server)
        .await;
    signing_key
}

async fn provider_fixture() -> (Fixture, MockServer, jsonwebtoken::EncodingKey) {
    let server = MockServer::start().await;
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    let fixture = Fixture::new(db, &server.uri());
    let key = mock_provider(&fixture, &server).await;
    (fixture, server, key)
}

async fn token_response(server: &MockServer, key: &jsonwebtoken::EncodingKey, nonce: &str) {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
    header.kid = Some("provider".into());
    let now = Utc::now().timestamp();
    let id_token = jsonwebtoken::encode(
        &header,
        &json!({
            "iss": server.uri(), "sub": "external-user", "aud": "test-client",
            "exp": now + 3600, "iat": now, "nonce": nonce,
            "email": "sso@example.com", "email_verified": true, "name": "SSO User"
        }),
        key,
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "provider-access", "token_type": "Bearer",
            "id_token": id_token
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn expired_and_unknown_callbacks_do_not_contact_provider() {
    let (fixture, server, _) = provider_fixture().await;
    for elapsed in [
        Duration::from_secs(10 * 60),
        Duration::from_secs(10 * 60) + Duration::from_secs(1),
    ] {
        let (_, state, _) = fixture.oauth.get_authorization_url().await.unwrap();
        let request_count = server.received_requests().await.unwrap().len();
        tokio::time::pause();
        tokio::time::advance(elapsed).await;
        assert_csrf(fixture.callback(&state).await);
        assert_csrf(fixture.callback(&state).await);
        assert_csrf(fixture.callback("unknown").await);
        tokio::time::resume();
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            request_count
        );
    }
}

#[tokio::test]
async fn concurrent_callbacks_exchange_once_and_preserve_valid_nonce_login() {
    let (fixture, server, key) = provider_fixture().await;
    let (url, state, nonce) = fixture.oauth.get_authorization_url().await.unwrap();
    let url = url::Url::parse(&url).unwrap();
    assert!(url.query_pairs().any(|(k, v)| k == "nonce" && v == nonce));
    token_response(&server, &key, &nonce).await;
    let (first, second) = tokio::join!(fixture.callback(&state), fixture.callback(&state));
    let (winner, loser) = if first.is_ok() {
        (first, second)
    } else {
        (second, first)
    };
    let (user, created) = winner.unwrap();
    assert_eq!(user.email, "sso@example.com");
    assert!(created);
    assert_csrf(loser);
    assert_csrf(fixture.callback(&state).await);
    server.verify().await;
}

#[tokio::test]
async fn wrong_nonce_rejects_login_and_still_consumes_state() {
    let (fixture, server, key) = provider_fixture().await;
    let (_, state, _) = fixture.oauth.get_authorization_url().await.unwrap();
    token_response(&server, &key, "wrong-nonce").await;
    let result = fixture.callback(&state).await;
    assert!(
        matches!(
            result,
            Err(AppError::Auth {
                code: AuthErrorCode::TokenInvalid,
                ..
            })
        ),
        "{result:?}"
    );
    assert!(
        fixture
            .users
            .find_by_email("sso@example.com")
            .await
            .unwrap()
            .is_none()
    );
    assert_csrf(fixture.callback(&state).await);
    server.verify().await;
}
