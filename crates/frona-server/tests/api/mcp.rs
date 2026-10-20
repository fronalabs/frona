use axum::http::StatusCode;
use http_body_util::BodyExt;
use std::io::Write;
use std::time::Duration;
use tower::ServiceExt;

use super::*;

fn sample_manifest() -> serde_json::Value {
    serde_json::json!({
        "name": "io.test/echo-mcp",
        "description": "A test MCP server",
        "version": "1.0.0",
        "packages": [{
            "registry_type": "npm",
            "identifier": "@test/echo-mcp",
            "version": "1.0.0",
            "transport": { "type": "stdio" },
            "environment_variables": []
        }]
    })
}

fn sample_manifest_with_secret() -> serde_json::Value {
    serde_json::json!({
        "name": "io.test/secret-mcp",
        "description": "Needs a secret",
        "version": "1.0.0",
        "packages": [{
            "registry_type": "npm",
            "identifier": "@test/secret-mcp",
            "version": "1.0.0",
            "transport": { "type": "stdio" },
            "environment_variables": [{
                "name": "API_KEY",
                "is_required": true,
                "is_secret": true
            }]
        }]
    })
}

async fn install_via_manifest(
    state: &AppState,
    token: &str,
    manifest: serde_json::Value,
) -> serde_json::Value {
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/mcp/servers",
            token,
            serde_json::json!({
                "manifest": manifest,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "install should succeed");
    body_json(resp).await
}

#[tokio::test]
async fn list_servers_empty() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "mcp-list", "mcplist@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/mcp/servers", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_servers_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp/servers")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn install_server_with_manifest_no_secrets() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "mcp-install",
        "mcpinstall@example.com",
        "password123",
    )
    .await;

    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    assert!(server["id"].is_string());
    assert_eq!(server["display_name"], "echo-mcp");
    assert_eq!(server["status"], "installed");
    assert_eq!(server["command"], "npx");

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/mcp/servers", &token))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn install_server_allows_missing_secret_binding() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mcp-nobind", "mcpnobind@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/mcp/servers",
            &token,
            serde_json::json!({
                "manifest": sample_manifest_with_secret(),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn uninstall_server() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "mcp-uninstall",
        "mcpuninstall@example.com",
        "password123",
    )
    .await;

    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/mcp/servers/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/mcp/servers", &token))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn uninstall_nonexistent_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mcp-del404", "mcpdel404@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete("/api/mcp/servers/nonexistent-id", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_server_extra_env() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mcp-update", "mcpupdate@example.com", "password123").await;

    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_patch_json(
            &format!("/api/mcp/servers/{id}"),
            &token,
            serde_json::json!({
                "extra_env": { "LOG_LEVEL": "debug" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["restart_required"], false);
}

#[tokio::test]
async fn invalid_environment_names_are_rejected_without_partial_updates_or_secret_errors() {
    use frona::credential::vault::models::{
        BindingScope, CreateLocalItemRequest, CredentialTarget, GrantDuration, VaultField,
    };
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "mcp-env", "mcpenv@example.com", "password123").await;
    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();
    let route = format!("/api/mcp/servers/{id}");
    let principal = frona::core::Principal::mcp_server(id);
    let credential = state
        .vault_service
        .create_credential(
            &user_id,
            CreateLocalItemRequest::UsernamePassword {
                name: "Bound secret".into(),
                username: "user".into(),
                password: "vault-secret-value".into(),
            },
        )
        .await
        .unwrap();
    state
        .vault_service
        .create_grant(
            &user_id,
            principal.clone(),
            "local",
            &credential.id,
            "bound",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    state
        .vault_service
        .create_binding(
            &user_id,
            principal.clone(),
            "bound",
            "local",
            &credential.id,
            CredentialTarget::Single {
                env_var: "BOUND_TOKEN".into(),
                field: VaultField::Password,
            },
            BindingScope::Durable,
            None,
        )
        .await
        .unwrap();
    for name in ["", "1BAD", "BAD-NAME", "BAD=NAME", "BAD\0NAME", "SÉCRET"] {
        let response = build_app(state.clone()).oneshot(auth_patch_json(&route, &token, serde_json::json!({
            "description":"must not be saved", "credentials":[], "extra_env":{(name):"submitted-secret-value"}
        }))).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await.to_string();
        assert!(body.contains("Environment variable names"));
        assert!(!body.contains("submitted-secret-value"));
        let persisted = body_json(
            build_app(state.clone())
                .oneshot(auth_get(&route, &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(persisted["env"], server["env"]);
        assert_eq!(persisted["description"], server["description"]);
        assert_eq!(
            state
                .vault_service
                .list_bindings_for_principal(&user_id, &principal)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let response = build_app(state.clone()).oneshot(auth_post_json("/api/mcp/servers", &token,
        serde_json::json!({"manifest":sample_manifest(), "extra_env":{"BAD=KEY":"install-secret-value"}})
    )).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !body_json(response)
            .await
            .to_string()
            .contains("install-secret-value")
    );
    let servers = body_json(
        build_app(state)
            .oneshot(auth_get("/api/mcp/servers", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(servers.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn update_wrong_owner_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (owner_token, _) =
        register_user(&state, "mcp-owner", "mcpowner@example.com", "password123").await;
    let (attacker_token, _) = register_user(
        &state,
        "mcp-attacker",
        "mcpattacker@example.com",
        "password123",
    )
    .await;

    let server = install_via_manifest(&state, &owner_token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_patch_json(
            &format!("/api/mcp/servers/{id}"),
            &attacker_token,
            serde_json::json!({ "extra_env": {} }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn stop_installed_server_returns_ok() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "mcp-stop", "mcpstop@example.com", "password123").await;

    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/mcp/servers/{id}/stop"),
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn log_path(state: &AppState, server_id: &str) -> std::path::PathBuf {
    let server = state.mcp_service.find_by_id(server_id).await.unwrap();
    let dir = std::path::PathBuf::from(server.workspace_dir).join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("server.log")
}

async fn read_log_event(body: &mut axum::body::Body) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut text = String::new();
        loop {
            let frame = body.frame().await.unwrap().unwrap();
            if let Some(data) = frame.data_ref() {
                text.push_str(std::str::from_utf8(data).unwrap());
                if text.contains("\n\n") {
                    return text;
                }
            }
        }
    })
    .await
    .expect("log event should arrive promptly")
}

#[tokio::test]
async fn logs_reject_foreign_server_before_reading_or_streaming() {
    let (state, _tmp) = test_app_state().await;
    let (owner_token, _) =
        register_user(&state, "log-owner", "log-owner@example.com", "password123").await;
    let (other_token, _) =
        register_user(&state, "log-other", "log-other@example.com", "password123").await;
    let server = install_via_manifest(&state, &owner_token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();
    let path = log_path(&state, id).await;
    std::fs::write(&path, "private-existing-line\n").unwrap();

    for suffix in ["logs", "logs/stream"] {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            build_app(state.clone()).oneshot(auth_get(
                &format!("/api/mcp/servers/{id}/{suffix}"),
                &other_token,
            )),
        )
        .await
        .expect("unauthorized logs should reject promptly")
        .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{suffix}");
        assert_ne!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "private-appended-line").unwrap();
        let body = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .expect("rejected response should finish without following logs")
            .unwrap()
            .to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(!text.contains("private-existing-line"));
        assert!(!text.contains("private-appended-line"));
    }
}

#[tokio::test]
async fn logs_reject_missing_server_before_reading_or_streaming() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "log-missing",
        "log-missing@example.com",
        "password123",
    )
    .await;

    for suffix in ["logs", "logs/stream"] {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            build_app(state.clone()).oneshot(auth_get(
                &format!("/api/mcp/servers/nonexistent-id/{suffix}"),
                &token,
            )),
        )
        .await
        .expect("missing server should reject promptly")
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{suffix}");
        tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .expect("missing server response should finish")
            .unwrap();
    }
}

#[tokio::test]
async fn owner_can_read_logs_and_empty_log_file() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "log-read", "log-read@example.com", "password123").await;
    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();
    let path = log_path(&state, id).await;

    // Installed servers may not have emitted a log file yet.
    assert!(!path.exists());
    let response = build_app(state.clone())
        .oneshot(auth_get(&format!("/api/mcp/servers/{id}/logs"), &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["logs"], "");

    std::fs::write(path, "owner-log-line\n").unwrap();
    let response = build_app(state)
        .oneshot(auth_get(&format!("/api/mcp/servers/{id}/logs"), &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["logs"], "owner-log-line\n");
}

#[tokio::test]
async fn owner_can_stream_existing_and_appended_log_lines() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "log-stream",
        "log-stream@example.com",
        "password123",
    )
    .await;
    let server = install_via_manifest(&state, &token, sample_manifest()).await;
    let id = server["id"].as_str().unwrap();
    let path = log_path(&state, id).await;
    std::fs::write(&path, "existing-owner-line\n").unwrap();

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        build_app(state).oneshot(auth_get(
            &format!("/api/mcp/servers/{id}/logs/stream"),
            &token,
        )),
    )
    .await
    .expect("owner stream should open promptly")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let mut body = response.into_body();
    assert_eq!(
        read_log_event(&mut body).await,
        "data: existing-owner-line\n\n"
    );

    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "appended-owner-line").unwrap();
    assert_eq!(
        read_log_event(&mut body).await,
        "data: appended-owner-line\n\n"
    );
}
