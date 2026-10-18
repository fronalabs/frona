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

#[tokio::test]
async fn root_equivalent_sources_are_rejected_before_any_changes() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "root-equivalents",
        "root-equivalents@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Root Equivalents").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let destination_agent = create_agent(&state, &token, "Root Destination").await;
    let destination_handle = destination_agent["handle"].as_str().unwrap();
    let user_root = tmp.path().join("users/root-equivalents/files");
    let agents_root = tmp.path().join("users/root-equivalents/agents");
    let agent_root = agents_root.join(agent_handle);
    let destination = agents_root.join(destination_handle).join("not-created");
    for root in [&user_root, &agent_root] {
        fs::create_dir_all(root).await.unwrap();
        fs::write(root.join("keep.txt"), b"keep").await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, root.join("root-link")).unwrap();
    }
    let app = build_app(state);

    for (prefix, root) in [
        (String::new(), &user_root),
        ("user://root-equivalents/".to_owned(), &user_root),
        (format!("agent://{agent_handle}/"), &agent_root),
    ] {
        let suffixes = ["", "/", "///", ".", "./", "./.", "././", ".//.///", "/././"];
        #[cfg(unix)]
        let suffixes = [suffixes.as_slice(), &["root-link", "root-link/./"]].concat();
        for suffix in suffixes {
            let source = format!("{prefix}{suffix}");
            for operation in ["rename", "delete", "copy", "move"] {
                // A valid source comes first, so a later root rejection must
                // leave earlier sources untouched and not create a destination.
                let sources = serde_json::json!([format!("{prefix}keep.txt"), source]);
                let body = match operation {
                    "rename" => serde_json::json!({
                        "path": source, "new_name": "renamed-root"
                    }),
                    "delete" => serde_json::json!({"paths": sources}),
                    _ => serde_json::json!({
                        "sources": sources,
                        "destination": format!("agent://{destination_handle}/not-created")
                    }),
                };
                let response = app
                    .clone()
                    .oneshot(auth_post_json(
                        &format!("/api/files/{operation}"),
                        &token,
                        body,
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{operation} source {source:?}"
                );
                let error = body_json(response).await;
                // A slash immediately after a virtual-path namespace is an
                // absolute path, rejected by the existing containment check.
                if prefix.is_empty() || !suffix.starts_with('/') {
                    assert!(
                        error
                            .to_string()
                            .contains(&format!("Cannot {operation} a workspace root")),
                        "{operation} source {source:?}: {error}"
                    );
                }
                assert_eq!(fs::read(root.join("keep.txt")).await.unwrap(), b"keep");
                assert!(!root.parent().unwrap().join("renamed-root").exists());
                assert!(!destination.exists());
            }
        }
    }
}

#[tokio::test]
async fn copy_and_move_accept_root_equivalent_destinations() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "root-destinations",
        "root-destinations@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Root Destinations").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let user_root = tmp.path().join("users/root-destinations/files");
    let agent_root = tmp
        .path()
        .join("users/root-destinations/agents")
        .join(agent_handle);
    let app = build_app(state);

    for (prefix, root) in [
        (String::new(), &user_root),
        ("user://root-destinations/".to_owned(), &user_root),
        (format!("agent://{agent_handle}/"), &agent_root),
    ] {
        fs::create_dir_all(root.join("nested")).await.unwrap();
        for (index, suffix) in ["", ".", "./", "././", ".//.///"].iter().enumerate() {
            for operation in ["copy", "move"] {
                let filename = format!("{operation}-{index}.txt");
                let source = root.join("nested").join(&filename);
                fs::write(&source, b"contents").await.unwrap();
                let response = app
                    .clone()
                    .oneshot(auth_post_json(
                        &format!("/api/files/{operation}"),
                        &token,
                        serde_json::json!({
                            "sources": [format!("{prefix}nested/{filename}")],
                            "destination": format!("{prefix}{suffix}")
                        }),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{operation} to {prefix}{suffix}"
                );
                assert_eq!(fs::read(root.join(&filename)).await.unwrap(), b"contents");
                assert_eq!(source.exists(), operation == "copy");
                fs::remove_file(root.join(&filename)).await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn dot_named_entries_remain_valid_operation_sources() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "dot-entries",
        "dot-entries@example.com",
        "password123",
    )
    .await;
    let root = tmp.path().join("users/dot-entries/files");
    fs::create_dir_all(&root).await.unwrap();
    let app = build_app(state);
    for name in [".hidden", "...", "report..txt"] {
        fs::write(root.join(name), b"contents").await.unwrap();
        for (operation, body) in [
            (
                "copy",
                serde_json::json!({"sources": [format!("./{name}")], "destination": "copies"}),
            ),
            (
                "move",
                serde_json::json!({"sources": [format!("copies/./{name}")], "destination": "moved"}),
            ),
            (
                "rename",
                serde_json::json!({"path": format!("moved/./{name}"), "new_name": "renamed.txt"}),
            ),
            (
                "delete",
                serde_json::json!({"paths": ["moved/./renamed.txt"]}),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(auth_post_json(
                    &format!("/api/files/{operation}"),
                    &token,
                    body,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{operation} of {name}");
        }
        assert_eq!(fs::read(root.join(name)).await.unwrap(), b"contents");
        assert!(!root.join("copies").join(name).exists());
        assert!(!root.join("moved").join(name).exists());
        assert!(!root.join("moved/renamed.txt").exists());
    }
}

#[tokio::test]
async fn missing_workspace_roots_remain_protected() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "missing-roots",
        "missing-roots@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Missing Root").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let user_root = tmp.path().join("users/missing-roots/files");
    let agent_root = tmp
        .path()
        .join("users/missing-roots/agents")
        .join(agent_handle);
    for root in [&user_root, &agent_root] {
        if root.exists() {
            fs::remove_dir_all(root).await.unwrap();
        }
    }
    let app = build_app(state);
    for source in [
        "./".to_owned(),
        "user://missing-roots/./".to_owned(),
        format!("agent://{agent_handle}/./"),
    ] {
        for operation in ["rename", "delete", "copy", "move"] {
            let body = match operation {
                "rename" => serde_json::json!({"path": source, "new_name": "renamed"}),
                "delete" => serde_json::json!({"paths": [source]}),
                _ => serde_json::json!({"sources": [source], "destination": "new-destination"}),
            };
            let response = app
                .clone()
                .oneshot(auth_post_json(
                    &format!("/api/files/{operation}"),
                    &token,
                    body,
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{operation} {source}"
            );
            let error = body_json(response).await;
            assert!(
                error
                    .to_string()
                    .contains(&format!("Cannot {operation} a workspace root"))
            );
            assert!(!user_root.exists());
            assert!(!agent_root.exists());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn non_root_symlink_operations_preserve_the_referent() {
    let (state, tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "symlink-entries",
        "symlink-entries@example.com",
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, "Symlink Entries").await;
    let agent_handle = agent["handle"].as_str().unwrap();
    let user_root = tmp.path().join("users/symlink-entries/files");
    let agent_root = tmp
        .path()
        .join("users/symlink-entries/agents")
        .join(agent_handle);
    let app = build_app(state);
    for (prefix, root) in [
        ("user://symlink-entries/".to_owned(), user_root),
        (format!("agent://{agent_handle}/"), agent_root),
    ] {
        fs::create_dir_all(root.join("folder")).await.unwrap();
        fs::write(root.join("folder/keep.txt"), b"keep")
            .await
            .unwrap();
        fs::write(root.join("file.txt"), b"contents").await.unwrap();
        for target in ["folder", "file.txt"] {
            std::os::unix::fs::symlink(root.join(target), root.join("link")).unwrap();
            for (operation, body, link_path) in [
                (
                    "rename",
                    serde_json::json!({"path": format!("{prefix}link"), "new_name": "renamed"}),
                    "renamed",
                ),
                (
                    "move",
                    serde_json::json!({"sources": [format!("{prefix}renamed")], "destination": format!("{prefix}moved")}),
                    "moved/renamed",
                ),
                (
                    "delete",
                    serde_json::json!({"paths": [format!("{prefix}moved/renamed")]}),
                    "moved/renamed",
                ),
            ] {
                let response = app
                    .clone()
                    .oneshot(auth_post_json(
                        &format!("/api/files/{operation}"),
                        &token,
                        body,
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{operation} link to {target}"
                );
                if operation == "delete" {
                    assert!(fs::symlink_metadata(root.join(link_path)).await.is_err());
                } else {
                    assert!(
                        fs::symlink_metadata(root.join(link_path))
                            .await
                            .unwrap()
                            .is_symlink()
                    );
                }
                assert_eq!(
                    fs::read(root.join("folder/keep.txt")).await.unwrap(),
                    b"keep"
                );
                assert_eq!(fs::read(root.join("file.txt")).await.unwrap(), b"contents");
            }
            assert!(fs::symlink_metadata(root.join("link")).await.is_err());
            assert!(fs::symlink_metadata(root.join("renamed")).await.is_err());
        }
    }
}
