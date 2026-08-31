use axum::http::StatusCode;
use tokio::fs;
use tower::ServiceExt;

use super::super::*;

#[tokio::test]
async fn empty_file_operation_batches_return_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "empty-operations",
        "empty-operations@example.com",
        "password123",
    )
    .await;
    let app = build_app(state);

    for (route, body) in [
        (
            "/api/files/copy",
            serde_json::json!({"sources": [], "destination": "/destination"}),
        ),
        (
            "/api/files/move",
            serde_json::json!({"sources": [], "destination": "/destination"}),
        ),
        ("/api/files/delete", serde_json::json!({"paths": []})),
    ] {
        let resp = app
            .clone()
            .oneshot(auth_post_json(route, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "route: {route}");
    }
}

#[tokio::test]
async fn rename_user_file_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "renamer", "renamer@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("renamer").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("old.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({"path": "old.txt", "new_name": "new.txt"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!user_dir.join("old.txt").exists());
    assert!(user_dir.join("new.txt").exists());
}

#[tokio::test]
async fn rename_owned_agent_file_persists() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "agent-rename",
        "agent-rename@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Rename Target").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("agent-rename")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("old.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({
                "path": format!("agent://{agent_handle}/old.txt"),
                "new_name": "new.txt"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!agent_dir.join("old.txt").exists());
    assert!(agent_dir.join("new.txt").exists());
}

#[tokio::test]
async fn rename_rejects_agent_workspace_root() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "rename-root",
        "rename-root@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Rename Root").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("rename-root")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({
                "path": format!("agent://{agent_handle}/"),
                "new_name": "renamed-root"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(agent_dir.exists());
}

#[tokio::test]
async fn rename_file_not_found_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "rename-miss",
        "renamemiss@example.com",
        "password123",
    )
    .await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({"path": "nonexistent.txt", "new_name": "x.txt"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_file_invalid_name_returns_400() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "rename-bad", "renamebad@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("rename-bad").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("file.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({"path": "file.txt", "new_name": "../escape.txt"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rename_file_destination_exists_returns_400() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "rename-dup", "renamedup@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("rename-dup").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("a.txt"), b"a").await.unwrap();
    fs::write(user_dir.join("b.txt"), b"b").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({"path": "a.txt", "new_name": "b.txt"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rename_path_traversal_returns_400() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "rename-trav",
        "renametrav@example.com",
        "password123",
    )
    .await;

    let user_dir = tmp.path().join("users").join("rename-trav").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("ok.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/rename",
            &token,
            serde_json::json!({"path": "ok.txt", "new_name": "sub/escape.txt"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn copy_files_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "copier", "copier@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("copier").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("src.txt"), b"source data")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["/src.txt"],
                "destination": "/backup"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(user_dir.join("backup").join("src.txt").exists());
    // Original still exists
    assert!(user_dir.join("src.txt").exists());
}

#[tokio::test]
async fn copy_preserves_existing_file_with_deduplicated_name() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "copy-collision",
        "copy-collision@example.com",
        "password123",
    )
    .await;
    let user_dir = tmp
        .path()
        .join("users")
        .join("copy-collision")
        .join("files");
    let destination = user_dir.join("destination");
    fs::create_dir_all(&destination).await.unwrap();
    fs::write(user_dir.join("report.txt"), b"new")
        .await
        .unwrap();
    fs::write(destination.join("report.txt"), b"existing")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["user://copy-collision/report.txt"],
                "destination": "user://copy-collision/destination"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        fs::read(destination.join("report.txt")).await.unwrap(),
        b"existing"
    );
    assert_eq!(
        fs::read(destination.join("report-1.txt")).await.unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn copy_missing_source_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "copy-missing",
        "copy-missing@example.com",
        "password123",
    )
    .await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["user://copy-missing/not-there.txt"],
                "destination": "user://copy-missing/destination"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn copy_directory_recursive() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "copydir", "copydir@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("copydir").join("files");
    let src_dir = user_dir.join("project");
    fs::create_dir_all(src_dir.join("sub")).await.unwrap();
    fs::write(src_dir.join("root.txt"), b"root").await.unwrap();
    fs::write(src_dir.join("sub").join("deep.txt"), b"deep")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["/project"],
                "destination": "/copy-dest"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        user_dir
            .join("copy-dest")
            .join("project")
            .join("root.txt")
            .exists()
    );
    assert!(
        user_dir
            .join("copy-dest")
            .join("project")
            .join("sub")
            .join("deep.txt")
            .exists()
    );
}

#[tokio::test]
async fn copy_directory_into_its_descendant_returns_400() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "copy-self", "copy-self@example.com", "password123").await;
    let project = tmp
        .path()
        .join("users")
        .join("copy-self")
        .join("files")
        .join("project");
    fs::create_dir_all(&project).await.unwrap();
    fs::write(project.join("file.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["user://copy-self/project"],
                "destination": "user://copy-self/project/subfolder"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn copy_between_owned_agent_workspaces_persists() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "copy-ag", "copyag@example.com", "password123").await;
    let source_agent = create_agent(&state, &token, "Copy Source").await;
    let destination_agent = create_agent(&state, &token, "Copy Destination").await;
    let source_handle = source_agent["handle"].as_str().unwrap();
    let destination_handle = destination_agent["handle"].as_str().unwrap();
    let agents_dir = tmp.path().join("users").join("copy-ag").join("agents");
    let source_dir = agents_dir.join(source_handle);
    let destination_dir = agents_dir.join(destination_handle).join("out");
    fs::create_dir_all(&source_dir).await.unwrap();
    fs::write(source_dir.join("f.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": [format!("agent://{source_handle}/f.txt")],
                "destination": format!("agent://{destination_handle}/out")
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        fs::read(destination_dir.join("f.txt")).await.unwrap(),
        b"data"
    );
}

#[tokio::test]
async fn copy_rejects_agent_workspace_root_source() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "copy-root", "copy-root@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Copy Root").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("copy-root")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("keep.txt"), b"keep")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": [format!("agent://{agent_handle}/")],
                "destination": "/copied"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(agent_dir.join("keep.txt").exists());
}

#[tokio::test]
async fn delete_owned_agent_file_persists() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "agent-delete",
        "agent-delete@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Delete Target").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("agent-delete")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("remove-me.txt"), b"data")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/delete",
            &token,
            serde_json::json!({
                "paths": [format!("agent://{agent_handle}/remove-me.txt")]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!agent_dir.join("remove-me.txt").exists());
}

#[tokio::test]
async fn delete_missing_path_does_not_delete_other_paths() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "delete-atomic",
        "delete-atomic@example.com",
        "password123",
    )
    .await;
    let user_dir = tmp.path().join("users").join("delete-atomic").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("keep.txt"), b"keep").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/delete",
            &token,
            serde_json::json!({"paths": ["/keep.txt", "/missing.txt"]}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(user_dir.join("keep.txt").exists());
}

#[tokio::test]
async fn delete_rejects_agent_owned_by_another_user() {
    let (state, tmp) = test_app_state().await;
    let (token_a, _) = register_user(
        &state,
        "delete-owner-a",
        "delete-owner-a@example.com",
        "password123",
    )
    .await;
    let (token_b, _) = register_user(
        &state,
        "delete-owner-b",
        "delete-owner-b@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token_b, "Private Agent").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let private_file = tmp
        .path()
        .join("users")
        .join("delete-owner-b")
        .join("agents")
        .join(agent_handle)
        .join("private.txt");
    fs::create_dir_all(private_file.parent().unwrap())
        .await
        .unwrap();
    fs::write(&private_file, b"private").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/delete",
            &token_a,
            serde_json::json!({
                "paths": [format!("agent://{agent_handle}/private.txt")]
            }),
        ))
        .await
        .unwrap();

    assert_ne!(resp.status(), StatusCode::OK);
    assert!(private_file.exists());
}

#[tokio::test]
async fn delete_rejects_agent_workspace_root() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "root-delete",
        "root-delete@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Protected Root").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("root-delete")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("keep.txt"), b"data")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/delete",
            &token,
            serde_json::json!({"paths": [format!("agent://{agent_handle}/")]}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(agent_dir.join("keep.txt").exists());
}

#[tokio::test]
async fn copy_from_other_user_via_prefix_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "copy-own-a", "copyowna@example.com", "password123").await;
    let (_, _) = register_user(&state, "copy-own-b", "copyownb@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token_a,
            serde_json::json!({
                "sources": ["user://copy-own-b/secret.txt"],
                "destination": "/stolen"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn move_files_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "mover", "mover@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("mover").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("moveme.txt"), b"moving")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["/moveme.txt"],
                "destination": "/archive"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!user_dir.join("moveme.txt").exists());
    assert!(user_dir.join("archive").join("moveme.txt").exists());
}

#[tokio::test]
async fn move_collision_does_not_move_any_source() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "move-atomic",
        "move-atomic@example.com",
        "password123",
    )
    .await;
    let user_dir = tmp.path().join("users").join("move-atomic").join("files");
    let destination = user_dir.join("destination");
    fs::create_dir_all(&destination).await.unwrap();
    fs::write(user_dir.join("first.txt"), b"first")
        .await
        .unwrap();
    fs::write(user_dir.join("second.txt"), b"second")
        .await
        .unwrap();
    fs::write(destination.join("second.txt"), b"existing")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["/first.txt", "/second.txt"],
                "destination": "/destination"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(user_dir.join("first.txt").exists());
    assert!(user_dir.join("second.txt").exists());
    assert!(!destination.join("first.txt").exists());
    assert_eq!(
        fs::read(destination.join("second.txt")).await.unwrap(),
        b"existing"
    );
}

#[tokio::test]
async fn move_between_owned_agent_workspaces_persists() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "move-ag", "moveag@example.com", "password123").await;
    let source_agent = create_agent(&state, &token, "Move Source").await;
    let destination_agent = create_agent(&state, &token, "Move Destination").await;
    let source_handle = source_agent["handle"].as_str().unwrap();
    let destination_handle = destination_agent["handle"].as_str().unwrap();
    let agents_dir = tmp.path().join("users").join("move-ag").join("agents");
    let source_dir = agents_dir.join(source_handle);
    let destination_dir = agents_dir.join(destination_handle);
    fs::create_dir_all(&source_dir).await.unwrap();
    fs::create_dir_all(&destination_dir).await.unwrap();
    fs::write(source_dir.join("file.txt"), b"move me")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": [format!("agent://{source_handle}/file.txt")],
                "destination": format!("agent://{destination_handle}/")
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!source_dir.join("file.txt").exists());
    assert_eq!(
        fs::read(destination_dir.join("file.txt")).await.unwrap(),
        b"move me"
    );
}

#[tokio::test]
async fn move_rejects_agent_workspace_root_source() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "move-root", "move-root@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Move Root").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let agent_dir = tmp
        .path()
        .join("users")
        .join("move-root")
        .join("agents")
        .join(agent_handle);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("keep.txt"), b"keep")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": [format!("agent://{agent_handle}/")],
                "destination": "/moved"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(agent_dir.join("keep.txt").exists());
}

#[tokio::test]
async fn move_to_unknown_agent_workspace_returns_404() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "move-ag2", "moveag2@example.com", "password123").await;

    let user_dir = tmp.path().join("users").join("move-ag2").join("files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("f.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["/f.txt"],
                "destination": "agent://some-agent/out"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(user_dir.join("f.txt").exists());
}

#[tokio::test]
async fn move_from_other_user_via_prefix_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "move-own-a", "moveowna@example.com", "password123").await;
    let (_, _) = register_user(&state, "move-own-b", "moveownb@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token_a,
            serde_json::json!({
                "sources": ["user://move-own-b/secret.txt"],
                "destination": "/stolen"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn create_folder_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mkdir-user", "mkdiruser@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/mkdir",
            &token,
            serde_json::json!({"path": "new-folder/sub"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        tmp.path()
            .join("users")
            .join("mkdir-user")
            .join("files")
            .join("new-folder")
            .join("sub")
            .is_dir()
    );
}

#[tokio::test]
async fn create_folder_in_owned_agent_workspace_persists() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "agent-folder",
        "agent-folder@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Folder Target").await;
    let agent_handle = agent["handle"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/mkdir",
            &token,
            serde_json::json!({
                "path": format!("agent://{agent_handle}/new-folder/subfolder")
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        tmp.path()
            .join("users")
            .join("agent-folder")
            .join("agents")
            .join(agent_handle)
            .join("new-folder")
            .join("subfolder")
            .is_dir()
    );
}

#[tokio::test]
async fn create_folder_path_traversal_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mkdir-trav", "mkdirtrav@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/mkdir",
            &token,
            serde_json::json!({"path": "../escape"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn create_folder_rejects_symlink_escape() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "mkdir-symlink",
        "mkdir-symlink@example.com",
        "password123",
    )
    .await;
    let user_dir = tmp.path().join("users").join("mkdir-symlink").join("files");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::create_dir_all(&outside).await.unwrap();
    std::os::unix::fs::symlink(&outside, user_dir.join("escape")).unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/mkdir",
            &token,
            serde_json::json!({"path": "/escape/created-outside"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(!outside.join("created-outside").exists());
}

#[tokio::test]
async fn mkdir_null_char_in_path_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mkdir-null", "mkdirnull@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/mkdir",
            &token,
            serde_json::json!({"path": "test\u{0000}dir"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
