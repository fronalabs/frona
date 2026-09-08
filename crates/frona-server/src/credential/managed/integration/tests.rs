use super::*;
use crate::credential::managed::{ManagedVault, resolver::ManagedResolver};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn vault() -> ManagedVault {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    crate::credential::managed::test_support::vault(&db, "credentials", "fixture-secret").await
}

fn jwt() -> String {
    format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp":(Utc::now()+chrono::Duration::hours(1)).timestamp(),"https://api.openai.com/auth":{"chatgpt_account_id":"account"}})).unwrap()))
}

#[tokio::test]
async fn codex_refresh_is_shared_and_only_renewable_changes_are_persisted() {
    for rotate in [false, true] {
        let server = MockServer::start().await;
        let access = jwt();
        Mock::given(method("POST")).and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":access,"refresh_token":if rotate {"rotated-private"} else {"private"}})))
            .expect(1).mount(&server).await;
        let integration = openai_codex::OpenAiCodexIntegration {
            auth_endpoint: server.uri(),
            ..Default::default()
        };
        let resolver = Arc::new(ManagedResolver::new(
            register([(
                "openai_codex".into(),
                Arc::new(integration)
                    as Arc<dyn crate::credential::managed::integration::RegisteredIntegration>,
            )])
            .unwrap(),
        ));
        let vault = vault().await;
        let original = json!({"access_token":"expired","refresh_token":"private","expires_at":Utc::now()-chrono::Duration::hours(1),"account_id":"account","scopes":[]});
        let first = vault
            .create("openai_codex", json!({}), original.clone())
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            resolver.resolve(&vault, first.item_id, || async { Ok(()) }),
            resolver.resolve(&vault, first.item_id, || async { Ok(()) })
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.1.to_env().unwrap(), b.1.to_env().unwrap());
        assert_eq!(a.1.to_env().unwrap()["ACCESS_TOKEN"], access);
        assert!(!a.1.to_env().unwrap().contains_key("REFRESH_TOKEN"));
        let (current, doc) = vault.document_by_id(first.item_id).await.unwrap();
        assert_eq!(current.item_id, first.item_id);
        assert_eq!(current.version != first.version, rotate);
        if rotate {
            assert_eq!(doc["refresh_token"], "rotated-private");
        } else {
            assert_eq!(doc, original);
        }
        vault
            .delete_by_id(current.item_id, current.version)
            .await
            .unwrap();
        assert!(
            resolver
                .resolve(&vault, first.item_id, || async { Ok(()) })
                .await
                .is_err()
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn static_composite_documents_invalidate_on_update_and_authorization_runs_on_cache_hits() {
    let vault = vault().await;
    let resolver = ManagedResolver::new(registered());
    let first = vault
        .create(
            "static",
            json!({}),
            json!({"USERNAME":"one","PASSWORD":"secret"}),
        )
        .await
        .unwrap();
    let (_, resolved) = resolver
        .resolve(&vault, first.item_id, || async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(resolved.to_env().unwrap().len(), 2);
    assert_eq!(resolved.cache, CachePolicy::UntilChanged);
    let typed = resolved.credentials::<static_secret::Document>().unwrap();
    assert_eq!(typed["USERNAME"], "one");
    assert!(
        resolved
            .credentials::<rig_core::providers::chatgpt::ChatGPTAuth>()
            .is_err()
    );
    let (_, cached) = resolver
        .resolve(&vault, first.item_id, || async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(resolved.identity, cached.identity);
    assert!(std::ptr::eq(
        typed,
        cached.credentials::<static_secret::Document>().unwrap()
    ));
    assert!(
        resolver
            .resolve(&vault, first.item_id, || async {
                Err(crate::core::error::AppError::Forbidden("revoked".into()))
            })
            .await
            .is_err()
    );
    let second = vault
        .replace_by_id(
            first.item_id,
            first.version,
            "static",
            json!({}),
            json!({"USERNAME":"two","PASSWORD":"new"}),
        )
        .await
        .unwrap();
    assert_eq!(first.item_id, second.item_id);
    assert_eq!(
        resolver
            .resolve(&vault, first.item_id, || async { Ok(()) })
            .await
            .unwrap()
            .1
            .to_env()
            .unwrap()["USERNAME"],
        "two"
    );
}

#[test]
fn resolved_exports_preserve_fields_and_redact_diagnostics() {
    use rig_core::providers::{chatgpt::ChatGPTAuth, copilot::CopilotAuth};

    let expiry = Utc::now() + chrono::Duration::hours(1);
    let secret = ResolvedSecret {
        credentials: ChatGPTAuth::AccessToken {
            access_token: "private-access".into(),
            account_id: Some("private-account".into()),
        },
        expires_at: Some(expiry),
        cache: CachePolicy::Until(expiry),
    };
    assert_eq!(
        secret.to_env().unwrap(),
        HashMap::from([
            ("ACCESS_TOKEN".into(), "private-access".into()),
            ("ACCOUNT_ID".into(), "private-account".into()),
            ("EXPIRES_AT".into(), expiry.to_rfc3339()),
        ])
    );
    assert!(!format!("{secret:?}").contains("private"));
    let erased = secret.erase();
    assert!(!format!("{erased:?}").contains("private"));
    let copilot = ResolvedSecret {
        credentials: CopilotAuth::ApiKey("short-lived".into()),
        expires_at: Some(expiry),
        cache: CachePolicy::Until(expiry),
    };
    assert_eq!(
        copilot.to_env().unwrap(),
        HashMap::from([
            ("ACCESS_TOKEN".into(), "short-lived".into()),
            ("EXPIRES_AT".into(), expiry.to_rfc3339()),
        ])
    );
    assert!(ChatGPTAuth::OAuth.to_env().is_err());
    assert!(
        ChatGPTAuth::AccessToken {
            access_token: "token".into(),
            account_id: None
        }
        .to_env()
        .is_err()
    );
    assert!(CopilotAuth::OAuth.to_env().is_err());
    assert!(
        CopilotAuth::GitHubAccessToken("bootstrap".into())
            .to_env()
            .is_err()
    );
    assert!(CopilotAuth::ApiKey(String::new()).to_env().is_err());
}

#[test]
fn resolved_export_errors_do_not_expose_integration_details() {
    struct FailingExport;

    impl SecretEnv for FailingExport {
        fn to_env(&self) -> Result<HashMap<String, String>, crate::core::error::AppError> {
            Err(crate::core::error::AppError::Validation(
                "private-refresh-token".into(),
            ))
        }
    }

    let secret = ResolvedSecret {
        credentials: FailingExport,
        expires_at: None,
        cache: CachePolicy::NoCache,
    };
    assert!(
        !secret
            .to_env()
            .unwrap_err()
            .to_string()
            .contains("private-refresh-token")
    );
    assert!(
        !secret
            .erase()
            .to_env()
            .unwrap_err()
            .to_string()
            .contains("private-refresh-token")
    );
}
