use axum::body::Body;
use axum::http::{Request, StatusCode};
use frona::auth::jwt::JwtService;
use frona::storage::PresignClaims;
use tokio::fs;
use tower::ServiceExt;

use super::super::*;

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn presigns_bind_subject_owner_handle_and_exact_path() {
    let (state, tmp) = test_app_state().await;
    let (_, alice) = register_user(&state, "alice", "alice@example.com", "password123").await;
    let (_, bob) = register_user(&state, "bob", "bob@example.com", "password123").await;
    for handle in ["alice", "bob"] {
        let root = tmp.path().join("users").join(handle).join("files");
        fs::create_dir_all(root.join("nested")).await.unwrap();
        for path in ["report.html", "nested/report.html", "other.html"] {
            fs::write(root.join(path), format!("{handle}'s private content"))
                .await
                .unwrap();
        }
    }
    let future = (chrono::Utc::now().timestamp() + 3600) as usize;
    let alice_owner = format!("user:{alice}");
    let bob_owner = format!("user:{bob}");
    let unknown = "missing-user";
    // Every case has a valid signature from the named signer. In particular,
    // the unknown-subject case has its own key, so a key check cannot mask a
    // missing authoritative-user lookup.
    let cases = [
        (
            "valid",
            alice.as_str(),
            alice.as_str(),
            alice_owner.as_str(),
            "report.html",
            "alice/report.html",
            future,
            StatusCode::OK,
        ),
        (
            "URL handle swapped",
            &alice,
            &alice,
            &alice_owner,
            "report.html",
            "bob/report.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "wrong owner",
            &alice,
            &alice,
            &bob_owner,
            "report.html",
            "alice/report.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "owner uses handle instead of ID",
            &alice,
            &alice,
            "user:alice",
            "report.html",
            "alice/report.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "wrong owner kind",
            &alice,
            &alice,
            "agent:alice",
            "report.html",
            "alice/report.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "wrong subject",
            &alice,
            &bob,
            &alice_owner,
            "report.html",
            "alice/report.html",
            future,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "wrong signer with consistent claims",
            &alice,
            &bob,
            &bob_owner,
            "report.html",
            "bob/report.html",
            future,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "unknown subject",
            unknown,
            unknown,
            "user:missing-user",
            "report.html",
            "alice/report.html",
            future,
            StatusCode::NOT_FOUND,
        ),
        (
            "different filename",
            &alice,
            &alice,
            &alice_owner,
            "report.html",
            "alice/other.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "same filename in another directory",
            &alice,
            &alice,
            &alice_owner,
            "nested/report.html",
            "alice/report.html",
            future,
            StatusCode::FORBIDDEN,
        ),
        (
            "expired",
            &alice,
            &alice,
            &alice_owner,
            "report.html",
            "alice/report.html",
            1,
            StatusCode::UNAUTHORIZED,
        ),
    ];
    let app = build_app(state.clone());
    for (case, signer, sub, owner, path, requested, exp, expected) in cases {
        let (key, kid) = state
            .keypair_service
            .get_signing_key(&format!("user:{signer}"))
            .await
            .unwrap();
        let signed = JwtService::new()
            .sign(
                &PresignClaims {
                    sub: sub.to_owned(),
                    owner: owner.to_owned(),
                    path: path.to_owned(),
                    exp,
                },
                &key,
                &kid,
            )
            .unwrap();
        for download in [false, true] {
            let response = app
                .clone()
                .oneshot(get(&format!(
                    "/api/files/user/{requested}?presign={signed}&download={download}"
                )))
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{case}, download={download}");
            if expected != StatusCode::OK {
                assert!(
                    !response.headers().contains_key("content-disposition"),
                    "{case}"
                );
            }
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&bytes);
            if expected == StatusCode::OK {
                assert_eq!(body, "alice's private content", "{case}");
            } else {
                assert!(!body.contains("private content"), "{case}: {body}");
            }
        }
    }
}

#[tokio::test]
async fn minted_private_and_public_share_links_cannot_switch_users() {
    let (state, tmp) = test_app_state().await;
    let (token, alice) = register_user(&state, "alice", "alice@example.com", "password123").await;
    register_user(&state, "bob", "bob@example.com", "password123").await;
    for handle in ["alice", "bob"] {
        let root = tmp.path().join("users").join(handle).join("files");
        fs::create_dir_all(root.join("nested")).await.unwrap();
        fs::write(
            root.join("nested/dashboard.html"),
            format!("{handle}'s dashboard"),
        )
        .await
        .unwrap();
    }
    let owner = format!("user:{alice}");
    let share = state
        .share_service
        .issue_file_public(&owner, "nested/dashboard.html", &alice, 3600)
        .await
        .unwrap();
    let app = build_app(state);
    let response = app
        .clone()
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({"owner": owner, "path": "nested/dashboard.html"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let private_url = json["url"].as_str().unwrap();

    for download in [false, true] {
        let response = app
            .clone()
            .oneshot(get(&format!("/s/{share}?download={download}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let public_url = response.headers()["location"].to_str().unwrap();
        for (source, signed_url) in [
            (
                "private presign",
                format!("{private_url}&download={download}"),
            ),
            ("public share", public_url.to_owned()),
        ] {
            let url = url::Url::parse(&signed_url).unwrap();
            assert_eq!(url.path(), "/api/files/user/alice/nested/dashboard.html");
            let request_path = format!("{}?{}", url.path(), url.query().unwrap());
            let response = app.clone().oneshot(get(&request_path)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{source}");
            let disposition = if download { "attachment" } else { "inline" };
            assert_eq!(
                response.headers()["content-disposition"],
                format!("{disposition}; filename=\"dashboard.html\"")
            );
            assert_eq!(
                response.headers()["content-security-policy"],
                "sandbox allow-scripts"
            );
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert_eq!(&bytes[..], b"alice's dashboard", "{source}");

            let swapped = request_path.replacen("/user/alice/", "/user/bob/", 1);
            let response = app.clone().oneshot(get(&swapped)).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{source}");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("bob's dashboard"),
                "{source}"
            );
        }
    }
}
