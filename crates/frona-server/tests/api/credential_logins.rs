use super::*;
use frona::{
    core::error::AppError,
    credential::managed::login::{ManagedLoginService, provider::*},
};
use serde_json::json;
use std::sync::Arc;
struct Provider;
struct Session;
#[async_trait::async_trait]
impl LoginProvider for Provider {
    fn id(&self) -> &'static str {
        "fixture"
    }
    async fn start(&self) -> Result<StartedLogin, AppError> {
        Ok(StartedLogin {
            challenge: LoginChallenge::Redirect {
                url: "https://login.example/authorize".into(),
                message: None,
            },
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            session: Box::new(Session),
        })
    }
}
#[async_trait::async_trait]
impl LoginSession for Session {
    async fn advance(&mut self, code: Option<&str>) -> Result<LoginProgress, AppError> {
        Ok(match code {
            None => LoginProgress::Pending,
            Some(_) => LoginProgress::Authenticated(LoginDocument {
                integration: "static",
                secret: json!({"API_KEY":"private-key","ACCOUNT_ID":"account"}),
            }),
        })
    }
}
async fn state_with_login(provider: Arc<dyn LoginProvider>) -> (AppState, tempfile::TempDir) {
    let (mut state, _tmp) = test_app_state_with_sandbox(true).await;
    state.vault_service.sync_config_connections().await.unwrap();
    let config = state.config_service.active();
    let managed_vault = frona::credential::managed::ManagedVault::new(
        Arc::new(frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(state.db.clone())),
        &config.auth.encryption_secret,
        "managed".into(),
    );
    state.vault_service = frona::credential::vault::service::VaultService::new(
        Arc::new(frona::db::repo::generic::SurrealRepo::new(state.db.clone())),
        Arc::new(frona::db::repo::generic::SurrealRepo::new(state.db.clone())),
        Arc::new(frona::db::repo::generic::SurrealRepo::new(state.db.clone())),
        Arc::new(frona::db::repo::generic::SurrealRepo::new(state.db.clone())),
        Arc::new(frona::db::repo::generic::SurrealRepo::new(state.db.clone())),
        &config.auth.encryption_secret,
        config.vault.clone(),
        config.storage.data_dir.clone().into(),
        frona::storage::StorageService::new(&config),
        state.user_service.clone(),
        managed_vault,
        Arc::new(frona::credential::managed::resolver::ManagedResolver::new(
            frona::credential::managed::integration::registered(),
        )),
        ManagedLoginService::new([provider]).unwrap(),
    );
    (state, _tmp)
}

#[tokio::test]
async fn personal_login_is_independent_of_models_and_isolated_from_other_users() {
    let (state, _tmp) = state_with_login(Arc::new(Provider)).await;
    let (token, user) =
        register_user(&state, "loginuser", "login@example.com", "password123").await;
    let (other, other_user) = register_user(
        &state,
        "otherlogin",
        "otherlogin@example.com",
        "password123",
    )
    .await;
    let connection = state
        .vault_service
        .create_connection(
            &user,
            frona::credential::vault::models::CreateVaultConnectionRequest {
                name: "Personal".into(),
                provider: frona::credential::vault::models::VaultProviderType::Managed,
                config: frona::credential::vault::models::VaultConnectionConfig::Managed {},
            },
        )
        .await
        .unwrap();
    let personal = connection.id;
    let request = json!({"provider":"fixture","target":{"kind":"create","name":"account"}});
    let start = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/vaults/{personal}/logins"),
            &token,
            request.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::OK);
    let attempt = body_json(start).await["id"].as_str().unwrap().to_owned();
    let second = state
        .vault_service
        .create_connection(
            &user,
            frona::credential::vault::models::CreateVaultConnectionRequest {
                name: "Second".into(),
                provider: frona::credential::vault::models::VaultProviderType::Managed,
                config: frona::credential::vault::models::VaultConnectionConfig::Managed {},
            },
        )
        .await
        .unwrap();
    let wrong_connection = build_app(state.clone())
        .oneshot(auth_get(
            &format!("/api/vaults/{}/logins/{attempt}", second.id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(wrong_connection.status(), StatusCode::NOT_FOUND);

    let path = format!("/api/vaults/{personal}/logins/{attempt}");
    let stolen = build_app(state.clone())
        .oneshot(auth_get(&path, &other))
        .await
        .unwrap();
    assert_eq!(stolen.status(), StatusCode::FORBIDDEN);
    let global = build_app(state.clone())
        .oneshot(auth_post_json(
            "/api/vaults/managed/logins",
            &other,
            request.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(global.status(), StatusCode::FORBIDDEN);
    let completed = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("{path}/complete"),
            &token,
            json!({"code":"approval"}),
        ))
        .await
        .unwrap();
    assert_eq!(completed.status(), StatusCode::OK);
    let completed = body_json(completed).await;
    assert!(!completed.to_string().contains("private-key"));
    let id = completed["credential_id"].as_str().unwrap();
    let secret = state
        .vault_service
        .get_secret(&user, &personal, id)
        .await
        .unwrap();
    assert_eq!(secret.fields["API_KEY"], "private-key");
    assert_eq!(secret.fields["ACCOUNT_ID"], "account");
    assert!(
        state
            .vault_service
            .get_secret(&other_user, &personal, id)
            .await
            .is_err()
    );
    let duplicate = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/vaults/{personal}/logins"),
            &token,
            request,
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let restart = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/vaults/{personal}/logins"),
            &token,
            json!({"provider":"fixture","target":{"kind":"replace","credential_id":id}}),
        ))
        .await
        .unwrap();
    assert_eq!(restart.status(), StatusCode::OK);
    let restart = body_json(restart).await["id"].as_str().unwrap().to_owned();
    let replaced = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/vaults/{personal}/logins/{restart}/complete"),
            &token,
            json!({"code":"approval"}),
        ))
        .await
        .unwrap();
    assert_eq!(replaced.status(), StatusCode::OK);
    assert_eq!(body_json(replaced).await["credential_id"], id);
    // A global credential may share a display name with a model-provider handle.
    let global = build_app(state.clone())
        .oneshot(auth_post_json(
            "/api/vaults/managed/logins",
            &token,
            json!({"provider":"fixture","target":{"kind":"create","name":"account"}}),
        ))
        .await
        .unwrap();
    assert_eq!(global.status(), StatusCode::OK);
    let global_attempt = body_json(global).await["id"].as_str().unwrap().to_owned();
    let global = build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/vaults/managed/logins/{global_attempt}/complete"),
            &token,
            json!({"code":"approval"}),
        ))
        .await
        .unwrap();
    assert_eq!(global.status(), StatusCode::OK);
    let global_id = body_json(global).await["credential_id"]
        .as_str()
        .unwrap()
        .to_owned();
    state
        .model_provider_service
        .config_service
        .save(json!({"providers":{"account":null}}), None)
        .await
        .unwrap();
    assert_eq!(
        state
            .vault_service
            .get_secret(&user, "managed", &global_id)
            .await
            .unwrap()
            .fields["API_KEY"],
        "private-key"
    );
    let providers = &state.model_provider_service;
    let id = uuid::Uuid::parse_str(&global_id).unwrap();
    assert!(
        providers
            .saved_credentials()
            .await
            .unwrap()
            .iter()
            .any(|credential| credential.credential_id == id)
    );
    let patch = json!({"providers":{"account":{"provider":"openai","credential_id":global_id}}});
    assert!(
        providers
            .config_service
            .save(patch.clone(), Some("stale-after-login"))
            .await
            .is_err()
    );
    assert!(
        providers
            .saved_credentials()
            .await
            .unwrap()
            .iter()
            .any(|credential| credential.credential_id == id)
    );
    let saved = providers.config_service.save(patch, None).await.unwrap();
    assert_eq!(
        saved.config["providers"]["account"]["credential_id"],
        global_id
    );
    providers.config_service.save(json!({"providers":{"account":null,"renamed":{"provider":"openai","credential_id":global_id}}}), None).await.unwrap();
    assert_eq!(
        providers
            .store
            .vault()
            .status_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .metadata["name"],
        "account"
    );
    providers
        .config_service
        .save(json!({"providers":{"renamed":null}}), None)
        .await
        .unwrap();
    assert!(
        providers
            .store
            .vault()
            .status_by_id(id)
            .await
            .unwrap()
            .is_some()
    );
}

struct PausedProvider {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}
struct PausedSession {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}
#[async_trait::async_trait]
impl LoginProvider for PausedProvider {
    fn id(&self) -> &'static str {
        "paused"
    }
    async fn start(&self) -> Result<StartedLogin, AppError> {
        Ok(StartedLogin {
            challenge: LoginChallenge::DeviceCode {
                url: "https://login.example/device".into(),
                user_code: "fixture".into(),
                message: None,
            },
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            session: Box::new(PausedSession {
                entered: self.entered.clone(),
                release: self.release.clone(),
            }),
        })
    }
}
#[async_trait::async_trait]
impl LoginSession for PausedSession {
    async fn advance(&mut self, _: Option<&str>) -> Result<LoginProgress, AppError> {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(LoginProgress::Authenticated(LoginDocument {
            integration: "static",
            secret: json!({"API_KEY":"new-fixture-key"}),
        }))
    }
}

#[tokio::test]
async fn revocation_during_exchange_prevents_create_and_replace_for_completion_and_polling() {
    for global in [false, true] {
        for replace in [false, true] {
            for polling in [false, true] {
                let entered = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let (state, _tmp) = state_with_login(Arc::new(PausedProvider {
                    entered: entered.clone(),
                    release: release.clone(),
                }))
                .await;
                let (token, user_id) =
                    register_user(&state, "owner", "owner@example.com", "password123").await;
                let original_user = state
                    .user_service
                    .find_by_id(&user_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(original_user.groups.iter().any(|g| g == "admins"));
                // Keep an administrator available so the repository's last-admin
                // invariant permits revoking the initiating user's access.
                let (_, other_id) = register_user(
                    &state,
                    "otheradmin",
                    "otheradmin@example.com",
                    "password123",
                )
                .await;
                let mut other = state
                    .user_service
                    .find_by_id(&other_id)
                    .await
                    .unwrap()
                    .unwrap();
                other.groups.push(frona::auth::models::ADMINS_GROUP.into());
                state.user_service.update(&other).await.unwrap();
                let connection_id = if global {
                    "managed".to_owned()
                } else {
                    state.vault_service.create_connection(&user_id, frona::credential::vault::models::CreateVaultConnectionRequest {
                        name: "Personal".into(), provider: frona::credential::vault::models::VaultProviderType::Managed,
                        config: frona::credential::vault::models::VaultConnectionConfig::Managed {},
                    }).await.unwrap().id
                };
                let vault = frona::credential::managed::ManagedVault::new(
                    Arc::new(
                        frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(
                            state.db.clone(),
                        ),
                    ),
                    &state.config.auth.encryption_secret,
                    connection_id.clone(),
                );
                let previous = if replace {
                    Some(
                        vault
                            .create(
                                "static",
                                json!({"name":"account"}),
                                json!({"API_KEY":"old-fixture-key"}),
                            )
                            .await
                            .unwrap(),
                    )
                } else {
                    None
                };
                let target = match &previous {
                    Some(previous) => json!({"kind":"replace","credential_id":previous.item_id}),
                    None => json!({"kind":"create","name":"account"}),
                };
                let response = build_app(state.clone())
                    .oneshot(auth_post_json(
                        &format!("/api/vaults/{connection_id}/logins"),
                        &token,
                        json!({"provider":"paused","target":target}),
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let attempt = body_json(response).await["id"].as_str().unwrap().to_owned();
                let path = format!("/api/vaults/{connection_id}/logins/{attempt}");
                let request = if polling {
                    auth_get(&path, &token)
                } else {
                    auth_post_json(
                        &format!("{path}/complete"),
                        &token,
                        json!({"code":"fixture"}),
                    )
                };
                let app = build_app(state.clone());
                let mut completion =
                    tokio::spawn(async move { app.oneshot(request).await.unwrap() });
                // Synchronize on the exchange, not a wall-clock startup budget:
                // authentication and database work can be slow under suite load.
                tokio::select! {
                    _ = entered.notified() => {}
                    response = &mut completion => {
                        let response = response.expect("login request task failed before exchange");
                        panic!(
                            "login request returned {} before entering the exchange (global={global}, replace={replace}, polling={polling})",
                            response.status()
                        );
                    }
                }
                if global {
                    let mut revoked = original_user.clone();
                    revoked.groups.clear();
                    state.user_service.update(&revoked).await.unwrap();
                } else {
                    state.user_service.deactivate(&user_id).await.unwrap();
                }
                release.add_permits(1);
                let response = completion.await.unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
                let rows = vault.list().await.unwrap();
                assert_eq!(rows.len(), usize::from(replace));
                if let Some(previous) = previous {
                    let current = vault.status_by_id(previous.item_id).await.unwrap().unwrap();
                    assert_eq!(current.version, previous.version);
                    assert_eq!(current.generation, previous.generation);
                }
                state.user_service.update(&original_user).await.unwrap();
                let retry = build_app(state.clone())
                    .oneshot(auth_post_json(
                        &format!("{path}/complete"),
                        &token,
                        json!({"code":"retry"}),
                    ))
                    .await
                    .unwrap();
                assert_eq!(retry.status(), StatusCode::CONFLICT);
            }
        }
    }
}
