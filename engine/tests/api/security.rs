use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use frona::api::error::ApiError;
use frona::core::error::AppError;
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn all_protected_endpoints_reject_no_auth() {
    let (state, _tmp) = test_app_state().await;

    let cases: Vec<(&str, &str)> = vec![
        ("GET", "/api/auth/me"),
        ("GET", "/api/agents"),
        ("POST", "/api/agents"),
        ("GET", "/api/chats"),
        ("POST", "/api/chats"),
        ("GET", "/api/spaces"),
        ("POST", "/api/spaces"),
        ("GET", "/api/tasks"),
        ("POST", "/api/tasks"),
    ];

    for (method, uri) in cases {
        let app = build_app(state.clone());
        let body = if method == "POST" {
            Body::from("{}")
        } else {
            Body::empty()
        };
        let mut builder = Request::builder().method(method).uri(uri);
        if method == "POST" {
            builder = builder.header("content-type", "application/json");
        }
        let req = builder.body(body).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} should return 401 without auth"
        );
    }
}

#[tokio::test]
async fn api_error_maps_all_variants_correctly() {
    let cases: Vec<(AppError, StatusCode)> = vec![
        (AppError::Auth("x".into()), StatusCode::UNAUTHORIZED),
        (AppError::NotFound("x".into()), StatusCode::NOT_FOUND),
        (AppError::Validation("x".into()), StatusCode::BAD_REQUEST),
        (AppError::Forbidden("x".into()), StatusCode::FORBIDDEN),
        (
            AppError::Database("x".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            AppError::Internal("x".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (AppError::Inference("x".into()), StatusCode::BAD_GATEWAY),
        (AppError::Browser("x".into()), StatusCode::BAD_GATEWAY),
        (
            AppError::Tool("x".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            AppError::Decryption("x".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            AppError::Http {
                status: 503,
                message: "unavailable".into(),
            },
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ];

    for (err, expected_status) in cases {
        let label = format!("{err:?}");
        let resp = ApiError(err).into_response();
        assert_eq!(resp.status(), expected_status, "Failed for: {label}");
    }
}
