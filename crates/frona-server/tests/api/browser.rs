use super::*;
use frona::core::config::BrowserConfig;
use frona::credential::vault::models::CreateLocalItemRequest;
use frona::tool::browser::session::BrowserSessionManager;
use std::sync::Arc;
use wiremock::matchers::{path, query_param, query_param_is_missing};

#[tokio::test]
async fn debugger_auth_preserves_the_ui_and_never_forwards_upstream_diagnostics() {
    let (mut state, _directory) = test_app_state_with_sandbox(true).await;
    let (user_token, user_id) =
        register_user(&state, "browseruser", "browser@example.com", "password123").await;
    let (other_token, _) = register_user(
        &state,
        "otherbrowser",
        "otherbrowser@example.com",
        "password123",
    )
    .await;
    let credential = state
        .vault_service
        .create_credential(
            &user_id,
            CreateLocalItemRequest::BrowserProfile {
                name: "Browser".into(),
            },
        )
        .await
        .unwrap();
    let route = format!("/api/browser/debugger/{}", credential.id);
    let upstream = MockServer::start().await;
    const SECRET: &str = "browser-private +/&?#=é";
    const HTML: &str = "<!doctype html><title>Existing debugger UI</title>";
    let logs = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_env_filter("frona::api::routes::browser=debug,frona::api::error=error")
        .with_writer(logs.reopen().unwrap())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    for configured in [Some(SECRET), None] {
        state.browser_session_manager = Arc::new(BrowserSessionManager::new(Some(BrowserConfig {
            ws_url: format!(
                "{}/prefix/?region=a%26b&--user-data-dir=stale",
                upstream.uri().replacen("http://", "ws://", 1)
            ),
            api_token: configured.map(str::to_owned),
            profiles_path: "/profiles +&#".into(),
            ..Default::default()
        })));
        for status in [200, 302, 401, 500] {
            let mut mock = Mock::given(method("GET"))
                .and(path("/prefix/debugger/"))
                .and(query_param("region", "a&b"))
                .and(query_param(
                    "--user-data-dir",
                    "/profiles +&#/browseruser/browser",
                ));
            mock = match configured {
                Some(token) => mock.and(query_param("token", token)),
                None => mock.and(query_param_is_missing("token")),
            };
            let response = if status == 200 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(HTML)
            } else {
                ResponseTemplate::new(status)
                    .insert_header("location", "/debugger/?token=browser-private")
                    .insert_header("x-error-detail", "browser-private")
                    .set_body_string(SECRET)
            };
            mock.respond_with(response).expect(1).mount(&upstream).await;
            let response = build_app(state.clone())
                .oneshot(auth_get(&route, &user_token))
                .await
                .unwrap();
            let expected = if status == 200 {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            assert_eq!(response.status(), expected);
            assert!(!response.headers().contains_key("location"));
            assert!(!response.headers().contains_key("x-error-detail"));
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            if status == 200 {
                assert_eq!(text, HTML);
            }
            assert!(!text.contains("browser-private"));
            let text = std::fs::read_to_string(logs.path()).unwrap();
            assert!(!text.contains("browser-private"));
            if status != 200 {
                assert!(
                    text.contains("Browserless debugger returned HTTP"),
                    "{text}"
                );
            }
            upstream.verify().await;
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0]
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key == "--user-data-dir")
                    .count(),
                1
            );
            upstream.reset().await;
        }
        let denied = build_app(state.clone())
            .oneshot(auth_get(&route, &other_token))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }
}
