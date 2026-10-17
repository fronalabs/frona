use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::fs;
use tower::ServiceExt;

use super::super::*;

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn signed_request_path(url: &str) -> String {
    let url = url::Url::parse(url).unwrap();
    format!("{}?{}", url.path(), url.query().unwrap())
}

#[tokio::test]
async fn file_preview_and_download_modes_preserve_content_and_authorization() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "display", "display@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Dashboard").await;
    let agent_id = agent["id"].as_str().unwrap();
    let roots = [
        (
            format!("user:{user_id}"),
            "/api/files/user/display".to_string(),
            tmp.path().join("users/display/files"),
        ),
        (
            format!("agent:{agent_id}"),
            format!("/api/files/agent/{agent_id}"),
            tmp.path()
                .join("users/display/agents")
                .join(agent["handle"].as_str().unwrap()),
        ),
    ];
    let app = build_app(state.clone());
    for (owner, route, root) in roots {
        fs::create_dir_all(&root).await.unwrap();
        for (filename, content_type, sandboxed) in [
            ("dashboard.html", "text/html", true),
            ("dashboard.HTM", "text/html", true),
            ("chart.svg", "image/svg+xml", true),
            ("document.xml", "application/xml", true),
            ("image.png", "image/png", false),
            ("image.jpg", "image/jpeg", false),
            ("document.pdf", "application/pdf", false),
            ("video.mp4", "video/mp4", false),
            ("sound.mp3", "audio/mpeg", false),
            ("notes.txt", "text/plain", false),
            ("data.bin", "application/octet-stream", false),
        ] {
            // Serving must preserve the exact bytes, including dashboard scripts.
            let content = b"<script>document.body.dataset.dashboard = 'ready';</script>";
            fs::write(root.join(filename), content).await.unwrap();
            let signed = state
                .presign_service
                .sign_by_user_id(&owner, filename, &user_id)
                .await
                .unwrap();
            let signed = signed_request_path(&signed);
            for (query, disposition) in [
                ("", "inline"),
                ("download=false", "inline"),
                ("download=true", "attachment"),
            ] {
                for request in [
                    auth_get(&format!("{route}/{filename}?{query}"), &token),
                    get(&format!("{signed}&{query}")),
                ] {
                    let response = app.clone().oneshot(request).await.unwrap();
                    assert_eq!(response.status(), StatusCode::OK, "{owner}/{filename}");
                    let headers = response.headers();
                    assert_eq!(headers["content-type"], content_type);
                    assert_eq!(
                        headers["content-disposition"],
                        format!("{disposition}; filename=\"{filename}\"")
                    );
                    assert_eq!(headers["x-content-type-options"], "nosniff");
                    assert_eq!(headers["referrer-policy"], "no-referrer");
                    if sandboxed {
                        assert_eq!(headers["content-security-policy"], "sandbox allow-scripts");
                    } else {
                        // In particular, don't sandbox the browser's PDF/media viewer.
                        assert!(!headers.contains_key("content-security-policy"));
                    }
                    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                        .await
                        .unwrap();
                    assert_eq!(bytes.as_ref(), content);
                }
            }
        }
    }
}

#[tokio::test]
async fn file_download_headers_handle_unicode_quotes_and_control_characters() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "filename", "filename@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Filenames").await;
    let agent_id = agent["id"].as_str().unwrap();
    let roots = [
        (
            "user/filename".to_string(),
            format!("user:{user_id}"),
            tmp.path().join("users/filename/files"),
        ),
        (
            format!("agent/{agent_id}"),
            format!("agent:{agent_id}"),
            tmp.path()
                .join("users/filename/agents")
                .join(agent["handle"].as_str().unwrap()),
        ),
    ];
    let app = build_app(state.clone());
    for (route, owner, root) in roots {
        fs::create_dir_all(&root).await.unwrap();
        for (filename, expected) in [
            (
                "résumé \"Q4\".html",
                "attachment; filename=\"r_sum_ _Q4_.html\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20%22Q4%22.html",
            ),
            (
                "%2F report.txt",
                "attachment; filename=\"_2F report.txt\"; filename*=UTF-8''%252F%20report.txt",
            ),
            ("sales; Q4.txt", "attachment; filename=\"sales; Q4.txt\""),
            (
                "notes\r\nX-Injected: yes\\file.txt",
                "attachment; filename=\"notes__X-Injected: yes_file.txt\"",
            ),
        ] {
            fs::write(root.join(filename), b"file content")
                .await
                .unwrap();
            let encoded: String = url::form_urlencoded::byte_serialize(filename.as_bytes())
                .collect::<String>()
                .replace('+', "%20");
            let uri = format!("/api/files/{route}/{encoded}");
            let signed = state
                .presign_service
                .sign_by_user_id(&owner, filename, &user_id)
                .await
                .unwrap();
            let presign = signed.split("?presign=").nth(1).unwrap();
            for request in [
                auth_get(&format!("{uri}?download=true"), &token),
                get(&format!("{uri}?presign={presign}&download=true")),
            ] {
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{route}/{filename}");
                assert_eq!(response.headers()["content-disposition"], expected);
                assert!(!response.headers().contains_key("x-injected"));
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap();
                assert_eq!(bytes.as_ref(), b"file content");
            }
        }
    }
}

#[tokio::test]
async fn explicit_download_cannot_bypass_auth_or_signed_file_claims() {
    use frona::auth::jwt::JwtService;
    use frona::storage::PresignClaims;

    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "private", "private@example.com", "password123").await;
    let (other_token, _) =
        register_user(&state, "stranger", "stranger@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Private").await;
    let agent_id = agent["id"].as_str().unwrap();
    let roots = [
        (
            format!("user:{user_id}"),
            "/api/files/user/private".to_string(),
            tmp.path().join("users/private/files"),
        ),
        (
            format!("agent:{agent_id}"),
            format!("/api/files/agent/{agent_id}"),
            tmp.path()
                .join("users/private/agents")
                .join(agent["handle"].as_str().unwrap()),
        ),
    ];
    let (key, kid) = state
        .keypair_service
        .get_signing_key(&format!("user:{user_id}"))
        .await
        .unwrap();
    let app = build_app(state.clone());
    for (owner, route, root) in roots {
        fs::create_dir_all(&root).await.unwrap();
        fs::write(root.join("private.html"), b"private dashboard")
            .await
            .unwrap();
        let expired = JwtService::new()
            .sign(
                &PresignClaims {
                    sub: user_id.clone(),
                    owner: owner.clone(),
                    path: "private.html".into(),
                    exp: 1,
                },
                &key,
                &kid,
            )
            .unwrap();
        let other_file = state
            .presign_service
            .sign_by_user_id(&owner, "different.html", &user_id)
            .await
            .unwrap();
        let other_file = other_file.split("?presign=").nth(1).unwrap();
        let uri = format!("{route}/private.html?download=true");
        for request in [
            get(&uri),
            auth_get(&uri, &other_token),
            get(&format!("{uri}&presign=invalid")),
            get(&format!("{uri}&presign={expired}")),
            get(&format!("{uri}&presign={other_file}")),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ));
            assert!(!response.headers().contains_key("content-disposition"));
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("private dashboard"));
        }
    }
}

#[tokio::test]
async fn file_shares_preserve_preview_download_and_visibility() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "share", "share@example.com", "password123").await;
    let owner = format!("user:{user_id}");
    let root = tmp.path().join("users/share/files");
    fs::create_dir_all(&root).await.unwrap();
    fs::write(root.join("dashboard.html"), b"<script>1 + 1</script>")
        .await
        .unwrap();
    let private = state
        .share_service
        .issue_file(&owner, "dashboard.html", &user_id, 3600)
        .await
        .unwrap();
    let public = state
        .share_service
        .issue_file_public(&owner, "dashboard.html", &user_id, 3600)
        .await
        .unwrap();
    let app = build_app(state);
    for (share, public) in [(private, false), (public, true)] {
        for (query, disposition) in [
            ("", "inline"),
            ("?download=false", "inline"),
            ("?download=true", "attachment"),
        ] {
            let response = app
                .clone()
                .oneshot(get(&format!("/s/{share}{query}")))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            let location = response.headers()["location"].to_str().unwrap();
            assert_eq!(location.contains("presign="), public);
            assert_eq!(
                location.contains("download=true"),
                disposition == "attachment"
            );
            let target = if public {
                signed_request_path(location)
            } else {
                location.to_string()
            };
            if !public {
                let response = app.clone().oneshot(get(&target)).await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            let request = if public {
                get(&target)
            } else {
                auth_get(&target, &token)
            };
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["content-disposition"],
                format!("{disposition}; filename=\"dashboard.html\"")
            );
            assert_eq!(
                response.headers()["content-security-policy"],
                "sandbox allow-scripts"
            );
        }
    }
}

#[tokio::test]
async fn download_flag_does_not_reactivate_expired_shares_or_change_chat_shares() {
    let (state, _tmp) = test_app_state().await;
    let (_, user_id) = register_user(&state, "expired", "expired@example.com", "password123").await;
    let expired = state
        .share_service
        .issue_file_public(&format!("user:{user_id}"), "secret.html", &user_id, 0)
        .await
        .unwrap();
    let chat = state
        .share_service
        .lookup_or_issue_chat("chat-id", &user_id, 3600)
        .await
        .unwrap();
    let app = build_app(state);
    for share in [expired.as_str(), "missing"] {
        let response = app
            .clone()
            .oneshot(get(&format!("/s/{share}?download=true")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key("location"));
    }
    let response = app
        .oneshot(get(&format!("/s/{chat}?download=true")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/chats/chat-id");
}
