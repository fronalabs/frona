use std::sync::Arc;

use chrono::{Duration, Utc};
use frona::auth::AuthService;
use frona::auth::User;
use frona::auth::UserService;
use frona::auth::jwt::JwtService;
use frona::auth::token::models::{ApiToken, CreatePatRequest};
use frona::auth::token::repository::TokenRepository;
use frona::auth::token::service::TokenService;
use frona::core::config::CacheConfig;
use frona::core::error::AppError;
use frona::core::repository::Repository;
use frona::credential::keypair::service::KeyPairService;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::policy::service::PolicyService;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_policy_service(db: &Surreal<Db>) -> PolicyService {
    let schema = frona::policy::schema::build_schema();
    let repo: Arc<dyn frona::policy::repository::PolicyRepository> =
        Arc::new(SurrealRepo::<frona::policy::models::Policy>::new(
            db.clone(),
        ));
    let tool_fixture = helpers::app_state::build(db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();
    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    let service = PolicyService::new(repo, schema, tool_manager, storage, user_service);
    // Mirror production startup so the admins-group permission rule is
    // present when we exercise UserInfo.permissions.list_users.
    service.sync_base_policies().await.unwrap();
    service
}

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_user() -> User {
    let now = Utc::now();
    User {
        id: frona::core::repository::new_id(),
        handle: frona::handle!("testuser"),
        email: "test@example.com".to_string(),
        name: "Test User".to_string(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$test$test".to_string(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn setup_services(db: &Surreal<Db>) -> (KeyPairService, TokenService) {
    let keypair_service =
        KeyPairService::new("test-jwt-secret", Arc::new(SurrealRepo::new(db.clone())));
    let jwt_service = JwtService::new();
    let user_service = frona::auth::user_service::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    let token_service = TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        jwt_service,
        user_service,
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
        handle: frona::handle!("testuser"),
        email: "test@example.com".to_string(),
        exp: (Utc::now().timestamp() + 3600) as usize,
        iat: Utc::now().timestamp() as usize,
        token_id: "tok-1".to_string(),
        token_type: "access".to_string(),
        principal: frona::core::Principal::user("test-456"),
        scopes: None,
        extensions: None,
    };

    let token = jwt_svc.sign(&claims, &encoding_key, &kid).unwrap();
    let verified = jwt_svc
        .verify::<frona::auth::models::Claims>(&token, &decoding_key)
        .unwrap();
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

    let access_claims = token_svc.validate(&keypair_svc, &access_jwt).await.unwrap();
    assert_eq!(access_claims.sub, user.id);
    assert_eq!(access_claims.token_type, "access");

    let refresh_claims = token_svc
        .validate(&keypair_svc, &refresh_jwt)
        .await
        .unwrap();
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

    let (new_access, new_refresh, claims) =
        token_svc.refresh(&keypair_svc, &refresh_jwt).await.unwrap();

    assert_eq!(claims.sub, user.id);

    let new_access_claims = token_svc.validate(&keypair_svc, &new_access).await.unwrap();
    assert_eq!(new_access_claims.sub, user.id);

    let old_result = token_svc.validate(&keypair_svc, &access_jwt).await;
    assert!(old_result.is_err());

    let old_refresh_result = token_svc.validate(&keypair_svc, &refresh_jwt).await;
    assert!(old_refresh_result.is_err());
    assert!(token_svc.refresh(&keypair_svc, &refresh_jwt).await.is_err());
    assert_eq!(
        token_svc
            .repo()
            .find_by_user_id(&user.id)
            .await
            .unwrap()
            .len(),
        2
    );

    let new_refresh_claims = token_svc
        .validate(&keypair_svc, &new_refresh)
        .await
        .unwrap();
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

    let claims = token_svc.validate(&keypair_svc, &access_jwt).await.unwrap();

    let token_record = token_svc
        .repo()
        .find_active_by_id(&claims.token_id)
        .await
        .unwrap()
        .unwrap();
    let pair_id = token_record.refresh_pair_id.unwrap();
    token_svc.revoke_session(&pair_id).await.unwrap();

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
                principal: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(pat.name, "My API Key");
    assert!(!pat.token.is_empty());

    let claims = token_svc.validate(&keypair_svc, &pat.token).await.unwrap();
    assert_eq!(claims.sub, user.id);
    assert_eq!(claims.token_type, "pat");
    assert_eq!(claims.scopes, Some(vec!["read".to_string()]));

    let pats = token_svc.list_pats(&user.id).await.unwrap();
    assert_eq!(pats.len(), 1);
    assert_eq!(pats[0].name, "My API Key");

    token_svc.delete_pat(&user.id, &pat.id).await.unwrap();
    let pats = token_svc.list_pats(&user.id).await.unwrap();
    assert!(pats.is_empty());

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
                principal: None,
            },
        )
        .await
        .unwrap();

    let result = token_svc.delete_pat("other-user-id", &pat.id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_jwks_listing() {
    let db = test_db().await;
    let (keypair_svc, _) = setup_services(&db);

    let jwks = keypair_svc.list_jwks().await.unwrap();
    assert!(jwks.is_empty());

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
    let user_service = UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default());
    let (keypair_svc, token_svc) = setup_services(&db);
    let policy_svc = test_policy_service(&db).await;
    let auth_svc = AuthService::new();

    let (register_resp, register_refresh) = auth_svc
        .register(
            &user_service,
            &keypair_svc,
            &token_svc,
            &policy_svc,
            frona::auth::models::RegisterRequest {
                handle: "newuser".to_string(),
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
    assert!(
        register_resp.user.permissions.list_users,
        "first registered user should be auto-promoted to admin and see list_users=true",
    );

    let claims = token_svc
        .validate(&keypair_svc, &register_resp.token)
        .await
        .unwrap();
    assert_eq!(claims.email, "new@example.com");

    let (login_resp, _login_refresh) = auth_svc
        .login(
            &user_service,
            &keypair_svc,
            &token_svc,
            &policy_svc,
            frona::auth::models::LoginRequest {
                identifier: "new@example.com".to_string(),
                password: "password123".to_string(),
            },
        )
        .await
        .unwrap();

    assert!(!login_resp.token.is_empty());
    assert_eq!(login_resp.user.email, "new@example.com");
    assert!(
        login_resp.user.permissions.list_users,
        "login response should carry the same Cedar-computed permissions as /me",
    );
}

#[tokio::test]
async fn test_login_wrong_password() {
    let db = test_db().await;
    let user_service = UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default());
    let (keypair_svc, token_svc) = setup_services(&db);
    let policy_svc = test_policy_service(&db).await;
    let auth_svc = AuthService::new();

    auth_svc
        .register(
            &user_service,
            &keypair_svc,
            &token_svc,
            &policy_svc,
            frona::auth::models::RegisterRequest {
                handle: "wrongpwtest".to_string(),
                email: "test@example.com".to_string(),
                name: "Test".to_string(),
                password: "correct-password".to_string(),
            },
        )
        .await
        .unwrap();

    let result = auth_svc
        .login(
            &user_service,
            &keypair_svc,
            &token_svc,
            &policy_svc,
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
    let user_service = UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default());
    let (keypair_svc, token_svc) = setup_services(&db);
    let policy_svc = test_policy_service(&db).await;
    let auth_svc = AuthService::new();

    let req = frona::auth::models::RegisterRequest {
        handle: "dupuser".to_string(),
        email: "dup@example.com".to_string(),
        name: "User".to_string(),
        password: "password123".to_string(),
    };

    auth_svc
        .register(&user_service, &keypair_svc, &token_svc, &policy_svc, req)
        .await
        .unwrap();

    let req2 = frona::auth::models::RegisterRequest {
        handle: "dupuser2".to_string(),
        email: "dup@example.com".to_string(),
        name: "User 2".to_string(),
        password: "password456".to_string(),
    };

    let result = auth_svc
        .register(&user_service, &keypair_svc, &token_svc, &policy_svc, req2)
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

    let refresh_claims = token_svc
        .validate(&keypair_svc, &refresh_jwt)
        .await
        .unwrap();

    drop(token_svc);
    drop(keypair_svc);

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

    let (keypair_svc2, token_svc2) = setup_services(&db);

    let (new_access, new_refresh, claims) = token_svc2
        .refresh(&keypair_svc2, &refresh_jwt)
        .await
        .unwrap();

    assert_eq!(claims.sub, user.id);

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
        id: frona::core::repository::new_id(),
        user_id: "user-1".to_string(),
        name: "short-lived".to_string(),
        token_type: TokenType::Access,
        principal: frona::core::Principal::user("user-1"),
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

    let (_access_jwt, refresh_jwt) = token_svc
        .create_session_pair(&keypair_svc, &user)
        .await
        .unwrap();

    let cookie_header = make_refresh_cookie(&refresh_jwt, token_svc.refresh_expiry_secs(), false);
    let cookie_str = cookie_header.to_str().unwrap();

    let extracted = extract_refresh_token_from_cookie_header(cookie_str)
        .expect("refresh_token must be extractable from cookie");
    assert_eq!(extracted, refresh_jwt);

    drop(token_svc);
    drop(keypair_svc);
    let (keypair_svc2, token_svc2) = setup_services(&db);

    let (new_access, new_refresh, claims) = token_svc2
        .refresh(&keypair_svc2, extracted)
        .await
        .expect("refresh with cookie-extracted token must succeed after restart");

    assert_eq!(claims.sub, user.id);
    assert_eq!(claims.token_type, "refresh");

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

mod helpers;

// Keep the real database implementation and gate only entry to consumption, so
// both service instances have validated and read the same live pair before racing.
struct GatedTokenRepo {
    inner: SurrealRepo<ApiToken>,
    gate: Arc<tokio::sync::Barrier>,
}

#[async_trait::async_trait]
impl Repository<ApiToken> for GatedTokenRepo {
    async fn create(&self, token: &ApiToken) -> Result<ApiToken, AppError> {
        self.inner.create(token).await
    }
    async fn find_by_id(&self, id: &str) -> Result<Option<ApiToken>, AppError> {
        self.inner.find_by_id(id).await
    }
    async fn update(&self, token: &ApiToken) -> Result<ApiToken, AppError> {
        self.inner.update(token).await
    }
    async fn delete(&self, id: &str) -> Result<(), AppError> {
        self.inner.delete(id).await
    }
}

#[async_trait::async_trait]
impl TokenRepository for GatedTokenRepo {
    async fn find_by_user_id(&self, id: &str) -> Result<Vec<ApiToken>, AppError> {
        self.inner.find_by_user_id(id).await
    }
    async fn find_active_by_id(&self, id: &str) -> Result<Option<ApiToken>, AppError> {
        self.inner.find_active_by_id(id).await
    }
    async fn find_by_refresh_pair(&self, id: &str) -> Result<Vec<ApiToken>, AppError> {
        self.inner.find_by_refresh_pair(id).await
    }
    async fn update_last_used(&self, id: &str) -> Result<(), AppError> {
        self.inner.update_last_used(id).await
    }
    async fn delete_expired(&self) -> Result<u64, AppError> {
        self.inner.delete_expired().await
    }
    async fn consume_refresh_pair(&self, token_id: &str, pair_id: &str) -> Result<bool, AppError> {
        self.gate.wait().await;
        self.inner.consume_refresh_pair(token_id, pair_id).await
    }
    async fn delete_by_refresh_pair(&self, id: &str) -> Result<(), AppError> {
        self.inner.delete_by_refresh_pair(id).await
    }
    async fn delete_by_user_id(&self, id: &str) -> Result<(), AppError> {
        self.inner.delete_by_user_id(id).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_has_one_winner_across_independent_services_on_rocksdb() {
    let directory = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::RocksDb>(directory.path().join("db"))
        .await
        .unwrap();
    db::setup_schema(&db).await.unwrap();
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);

    // Each instance has its own service, repository, and caches. Only persistent
    // storage and a test barrier are shared; production has no process-local lock.
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let make_contender = || {
        let (keys, _) = setup_services(&db);
        let service = TokenService::new(
            Arc::new(GatedTokenRepo {
                inner: SurrealRepo::new(db.clone()),
                gate: gate.clone(),
            }),
            JwtService::new(),
            UserService::new(SurrealRepo::new(db.clone()), &CacheConfig::default()),
            900,
            604800,
        );
        (keys, service)
    };
    let (keys_a, contender_a) = make_contender();
    let (keys_b, contender_b) = make_contender();

    for _ in 0..12 {
        let (old_access, old_refresh) = service.create_session_pair(&keys, &user).await.unwrap();
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(
                contender_a.refresh(&keys_a, &old_refresh),
                contender_b.refresh(&keys_b, &old_refresh)
            )
        })
        .await
        .expect("both contenders must reach consumption");
        assert_eq!(
            usize::from(a.is_ok()) + usize::from(b.is_ok()),
            1,
            "a={a:?}, b={b:?}"
        );
        let (access, refresh, _) = a.or(b).unwrap();
        assert!(service.validate(&keys, &old_access).await.is_err());
        assert!(service.refresh(&keys, &old_refresh).await.is_err());
        service.validate(&keys, &access).await.unwrap();
        service.validate(&keys, &refresh).await.unwrap();
        let rows = service.repo().find_by_user_id(&user.id).await.unwrap();
        assert_eq!(rows.len(), 2, "only the winning replacement pair may exist");
        service
            .revoke_session(rows[0].refresh_pair_id.as_ref().unwrap())
            .await
            .unwrap();
    }
}

// Upgrade sentinel for the RocksDB primitive used by consume_refresh_pair.
// Keep these two DELETE statements and typed bindings aligned with that method.
// The service race above checks its caller; this test forces both database
// transactions to delete the same row provisionally before either can commit.
#[tokio::test]
async fn refresh_consumption_conflicts_at_commit_on_rocksdb() {
    use frona::auth::token::models::TokenType;
    use surrealdb::types::{RecordId, Value};

    let directory = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::RocksDb>(directory.path().join("db"))
        .await
        .unwrap();
    db::setup_schema(&db).await.unwrap();
    db.query("DEFINE TABLE refresh_commit_marker SCHEMALESS")
        .await
        .unwrap()
        .check()
        .unwrap();
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);

    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        for reverse in [false, true] {
            service.create_session_pair(&keys, &user).await.unwrap();
            let old_pair = service.repo().find_by_user_id(&user.id).await.unwrap();
            assert_eq!(old_pair.len(), 2);
            let refresh = old_pair
                .iter()
                .find(|token| token.token_type == TokenType::Refresh)
                .unwrap();
            let refresh_id = RecordId::new("api_token", refresh.id.clone());
            let pair_id = refresh.refresh_pair_id.clone().unwrap();
            for contender in ["a", "b"] {
                db.query("CREATE $marker SET contender = $contender, reverse = $reverse, committed = false")
                    .bind(("marker", RecordId::new("refresh_commit_marker", format!("{reverse}-{contender}"))))
                    .bind(("contender", contender.to_owned()))
                    .bind(("reverse", reverse))
                    .await.unwrap().check().unwrap();
            }
            let a = db.clone().begin().await.unwrap();
            let b = db.clone().begin().await.unwrap();

            for (transaction, contender) in [(&a, "a"), (&b, "b")] {
                let mut response = transaction
                    .query(
                        "DELETE api_token WHERE id = $id AND refresh_pair_id = $pair_id
                     AND token_type = $refresh_type AND expires_at > $now
                     RETURN BEFORE",
                    )
                    .bind(("id", refresh_id.clone()))
                    .bind(("pair_id", pair_id.clone()))
                    .bind(("refresh_type", TokenType::Refresh))
                    .bind(("now", Utc::now()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let deleted: Vec<Value> = response.take(0).unwrap();
                assert_eq!(
                    deleted.len(),
                    1,
                    "both transactions must provisionally consume the token"
                );
                let Value::Object(deleted) = &deleted[0] else {
                    panic!("RETURN BEFORE must return the deleted record");
                };
                assert_eq!(
                    deleted.get("id"),
                    Some(&Value::RecordId(refresh_id.clone()))
                );
                transaction
                    .query("DELETE api_token WHERE refresh_pair_id = $pair_id")
                    .bind(("pair_id", pair_id.clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();

                let mut response = transaction
                    .query(
                        "SELECT VALUE meta::id(id) FROM api_token WHERE refresh_pair_id = $pair_id",
                    )
                    .bind(("pair_id", pair_id.clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let remaining: Vec<String> = response.take(0).unwrap();
                assert!(
                    remaining.is_empty(),
                    "each transaction must have provisionally deleted the whole pair"
                );
                // Updating distinct existing, unindexed records cannot itself
                // cause the conflict. The loser's marker update must roll back.
                transaction.query("UPDATE $marker SET committed = true")
                    .bind(("marker", RecordId::new("refresh_commit_marker", format!("{reverse}-{contender}"))))
                    .await.unwrap().check().unwrap();
            }

            let visible = service.repo().find_by_refresh_pair(&pair_id).await.unwrap();
            assert_eq!(
                visible.len(),
                2,
                "neither provisional deletion may be visible outside its transaction"
            );
            assert!(
                old_pair
                    .iter()
                    .all(|old| visible.iter().any(|row| row.id == old.id))
            );

            let (winner, loser, expected_marker) = if reverse { (b, a, "b") } else { (a, b, "a") };
            winner
                .commit()
                .await
                .expect("the first commit must succeed");
            let conflict = loser
                .commit()
                .await
                .expect_err("the second commit must reject the stale deletion");
            // The 3.2.4 local SDK's Commit path wraps the engine error with
            // std_error_to_types_error, dropping QueryError::TransactionConflict.
            // Assert its conflict message rather than accepting any commit error.
            assert!(conflict.message().starts_with("Transaction conflict:"), "{conflict:?}");
            assert!(
                service
                    .repo()
                    .find_by_refresh_pair(&pair_id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let mut response = db
                .query("SELECT VALUE contender FROM refresh_commit_marker WHERE reverse = $reverse AND committed = true")
                .bind(("reverse", reverse))
                .await
                .unwrap()
                .check()
                .unwrap();
            let markers: Vec<String> = response.take(0).unwrap();
            assert_eq!(
                markers,
                [expected_marker],
                "only the winner's other writes may be committed"
            );
        }
    })
    .await
    .expect("overlapping deletions and commits must complete without blocking");
}

#[tokio::test]
async fn refresh_requires_a_live_pair_even_after_validation() {
    let db = test_db().await;
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);
    let (_, refresh) = service.create_session_pair(&keys, &user).await.unwrap();
    let claims = service.validate(&keys, &refresh).await.unwrap();
    let mut token = service
        .repo()
        .find_active_by_id(&claims.token_id)
        .await
        .unwrap()
        .unwrap();
    let pair_id = token.refresh_pair_id.take().unwrap();
    service.repo().update(&token).await.unwrap();
    assert!(service.refresh(&keys, &refresh).await.is_err());
    assert_eq!(
        service
            .repo()
            .find_by_user_id(&user.id)
            .await
            .unwrap()
            .len(),
        2
    );

    assert!(
        !service
            .repo()
            .consume_refresh_pair(&claims.token_id, &pair_id)
            .await
            .unwrap()
    );
    service.revoke_session(&pair_id).await.unwrap();
    // The refresh row now has no pair, so revoke the row separately.
    service.repo().delete(&claims.token_id).await.unwrap();
    service
        .repo()
        .update_last_used(&claims.token_id)
        .await
        .unwrap();
    assert!(
        service
            .repo()
            .find_by_id(&claims.token_id)
            .await
            .unwrap()
            .is_none(),
        "a late usage update must not resurrect a consumed row"
    );
}

#[tokio::test]
async fn consumption_rejects_missing_expired_and_non_refresh_rows() {
    let db = test_db().await;
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);
    let (access, refresh) = service.create_session_pair(&keys, &user).await.unwrap();
    let access = service.validate(&keys, &access).await.unwrap();
    let refresh = service.validate(&keys, &refresh).await.unwrap();
    let mut row = service
        .repo()
        .find_active_by_id(&refresh.token_id)
        .await
        .unwrap()
        .unwrap();
    let pair = row.refresh_pair_id.clone().unwrap();
    assert!(
        !service
            .repo()
            .consume_refresh_pair("missing", &pair)
            .await
            .unwrap()
    );
    assert!(
        !service
            .repo()
            .consume_refresh_pair(&access.token_id, &pair)
            .await
            .unwrap()
    );
    assert!(
        !service
            .repo()
            .consume_refresh_pair(&refresh.token_id, "other-pair")
            .await
            .unwrap()
    );
    row.expires_at = Utc::now() - Duration::seconds(1);
    service.repo().update(&row).await.unwrap();
    assert!(
        !service
            .repo()
            .consume_refresh_pair(&refresh.token_id, &pair)
            .await
            .unwrap()
    );
    assert_eq!(
        service
            .repo()
            .find_by_user_id(&user.id)
            .await
            .unwrap()
            .len(),
        2
    );
    service.revoke_session(&pair).await.unwrap();
    service.revoke_session(&pair).await.unwrap();
    assert!(
        !service
            .repo()
            .consume_refresh_pair(&refresh.token_id, &pair)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_pair_deletion_rolls_back_and_never_mints_or_hides_revocation_errors() {
    let db = test_db().await;
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);
    let (access, refresh) = service.create_session_pair(&keys, &user).await.unwrap();
    let claims = service.validate(&keys, &refresh).await.unwrap();
    let pair = service
        .repo()
        .find_active_by_id(&claims.token_id)
        .await
        .unwrap()
        .unwrap()
        .refresh_pair_id
        .unwrap();
    // Fail the second deletion, after the refresh row was provisionally consumed.
    db.query("DEFINE EVENT reject_delete ON api_token WHEN $event = 'DELETE' AND $before.name = 'session' THEN { THROW 'injected delete failure'; }")
        .await.unwrap().check().unwrap();
    assert!(matches!(
        service.refresh(&keys, &refresh).await,
        Err(AppError::Database(_))
    ));
    assert!(matches!(
        service.revoke_session(&pair).await,
        Err(AppError::Database(_))
    ));
    assert!(matches!(
        service.repo().delete_by_user_id(&user.id).await,
        Err(AppError::Database(_))
    ));
    assert_eq!(
        service
            .repo()
            .find_by_user_id(&user.id)
            .await
            .unwrap()
            .len(),
        2
    );
    service.validate(&keys, &access).await.unwrap();
    service.validate(&keys, &refresh).await.unwrap();
    db.query("REMOVE EVENT reject_delete ON api_token")
        .await
        .unwrap()
        .check()
        .unwrap();
    service.refresh(&keys, &refresh).await.unwrap();
}

#[tokio::test]
async fn malformed_stored_refresh_token_fails_closed() {
    let db = test_db().await;
    let user = test_user();
    SurrealRepo::new(db.clone()).create(&user).await.unwrap();
    let (keys, service) = setup_services(&db);
    let (_, refresh) = service.create_session_pair(&keys, &user).await.unwrap();
    db.query("UPDATE api_token SET scopes = 42 WHERE name = 'refresh'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(matches!(
        service.refresh(&keys, &refresh).await,
        Err(AppError::Database(_))
    ));
    let mut result = db
        .query("SELECT VALUE token_type FROM api_token")
        .await
        .unwrap();
    let types: Vec<frona::auth::token::models::TokenType> = result.take(0).unwrap();
    assert_eq!(types.len(), 2);
}
