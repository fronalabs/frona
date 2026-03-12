use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn create_task_returns_pending() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "taskuser", "taskuser@example.com", "password123").await;
    let agent = create_agent(&state, &token, "TaskAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let task = create_task(&state, &token, agent_id, "My Task").await;
    assert!(task["id"].is_string());
    assert_eq!(task["title"], "My Task");
    assert_eq!(task["status"], "pending");
}

#[tokio::test]
async fn create_task_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"agent_id": "fake", "title": "X"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_tasks_returns_only_own() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "task-a", "taska@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "task-b", "taskb@example.com", "password123").await;

    let agent_a = create_agent(&state, &token_a, "TA").await;
    let agent_b = create_agent(&state, &token_b, "TB").await;

    create_task(&state, &token_a, agent_a["id"].as_str().unwrap(), "TaskA").await;
    create_task(&state, &token_b, agent_b["id"].as_str().unwrap(), "TaskB").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/tasks", &token_a))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let tasks = json.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "TaskA");
}

#[tokio::test]
async fn get_task_by_id() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "gettask", "gettask@example.com", "password123").await;
    let agent = create_agent(&state, &token, "GT").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "FindMe").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/tasks/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["title"], "FindMe");
}

#[tokio::test]
async fn get_task_other_user_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "tk-owner", "tkowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "tk-other", "tkother@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "TO").await;
    let task = create_task(&state, &token_a, agent["id"].as_str().unwrap(), "Private").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/tasks/{id}"), &token_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_task() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "deltask", "deltask@example.com", "password123").await;
    let agent = create_agent(&state, &token, "DT").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "Remove").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/tasks/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/tasks/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_task_title() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "uptask", "uptask@example.com", "password123").await;
    let agent = create_agent(&state, &token, "UT").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "Original").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/tasks/{id}"),
            &token,
            serde_json::json!({"title": "Updated Title"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["title"], "Updated Title");
}

#[tokio::test]
async fn update_task_status() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "upstatus", "upstatus@example.com", "password123").await;
    let agent = create_agent(&state, &token, "US").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "Status").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/tasks/{id}"),
            &token,
            serde_json::json!({"status": "completed"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "completed");
}

#[tokio::test]
async fn update_task_other_user_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "ut-owner", "utowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "ut-other", "utother@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "UTO").await;
    let task = create_task(&state, &token_a, agent["id"].as_str().unwrap(), "Mine").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/tasks/{id}"),
            &token_b,
            serde_json::json!({"title": "Hacked"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cancel_pending_task() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "canceltask", "canceltask@example.com", "password123").await;
    let agent = create_agent(&state, &token, "CT").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "Cancel Me").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/tasks/{id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "cancelled");
}

#[tokio::test]
async fn cancel_completed_task_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "cancelcomp", "cancelcomp@example.com", "password123").await;
    let agent = create_agent(&state, &token, "CC").await;
    let task = create_task(&state, &token, agent["id"].as_str().unwrap(), "Done").await;
    let id = task["id"].as_str().unwrap();

    let app = build_app(state.clone());
    app.oneshot(auth_put_json(
        &format!("/api/tasks/{id}"),
        &token,
        serde_json::json!({"status": "completed"}),
    ))
    .await
    .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/tasks/{id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
