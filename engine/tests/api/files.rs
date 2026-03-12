use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::fs;
use tower::ServiceExt;

use super::*;

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_file_returns_attachment() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "uploader", "uploader@example.com", "password123").await;

    let att = upload_test_file(&state, &token, "hello.txt", b"hello world").await;
    assert_eq!(att["filename"], "hello.txt");
    assert_eq!(att["content_type"], "text/plain");
    assert_eq!(att["size_bytes"], 11);
    assert!(att["owner"].as_str().unwrap().starts_with("user:"));
    assert_eq!(att["path"], "hello.txt");
}

#[tokio::test]
async fn upload_file_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let boundary = "----testboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.txt\"\r\nContent-Type: text/plain\r\n\r\ndata\r\n--{boundary}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn upload_file_with_relative_path() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "pathuser", "pathuser@example.com", "password123").await;

    let app = build_app(state);
    let req = multipart_upload_with_path(&token, "doc.txt", b"content", "docs/readme/doc.txt");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["filename"], "doc.txt");
    assert!(json["path"].as_str().unwrap().starts_with("docs/readme/"));
}

#[tokio::test]
async fn upload_file_missing_file_field_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "nofield", "nofield@example.com", "password123").await;

    let app = build_app(state);
    let boundary = "----testboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\nsome/path\r\n--{boundary}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_deduplicates_filename() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "dedup", "dedup@example.com", "password123").await;

    let first = upload_test_file(&state, &token, "dup.txt", b"first").await;
    assert_eq!(first["filename"], "dup.txt");

    let second = upload_test_file(&state, &token, "dup.txt", b"second").await;
    assert_eq!(second["filename"], "dup-1.txt");
}

// ---------------------------------------------------------------------------
// Download user files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_user_file_returns_content() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "dluser", "dluser@example.com", "password123").await;

    // Write file directly to the storage path
    let user_dir = tmp.path().join("files").join("dluser");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("test.txt"), b"file content").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/user/dluser/test.txt", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"file content");
}

#[tokio::test]
async fn download_user_file_other_user_returns_403() {
    let (state, tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "dl-owner", "dlowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "dl-other", "dlother@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("dl-owner");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("secret.txt"), b"private").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/user/dl-owner/secret.txt", &token_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Owner can access
    let _ = token_a;
}

#[tokio::test]
async fn download_user_file_not_found_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "dl-miss", "dlmiss@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/user/dl-miss/nonexistent.txt", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Download agent files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_agent_file_returns_content() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "agentdl", "agentdl@example.com", "password123").await;
    let agent = create_agent(&state, &token, "DlAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("output.csv"), b"col1,col2").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/agent/{agent_id}/output.csv"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"col1,col2");
}

#[tokio::test]
async fn download_agent_file_other_user_returns_error() {
    let (state, tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "ag-owner", "agowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "ag-other", "agother@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "PrivAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("data.txt"), b"secret").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/agent/{agent_id}/data.txt"),
            &token_b,
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Delete user files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_user_file_removes_file() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "delfile", "delfile@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("delfile");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("remove.txt"), b"bye").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete("/api/files/user/delfile/remove.txt", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!user_dir.join("remove.txt").exists());
}

#[tokio::test]
async fn delete_user_file_removes_directory() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "deldir", "deldir@example.com", "password123").await;

    let sub_dir = tmp.path().join("files").join("deldir").join("mydir");
    fs::create_dir_all(&sub_dir).await.unwrap();
    fs::write(sub_dir.join("inner.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete("/api/files/user/deldir/mydir", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!sub_dir.exists());
}

#[tokio::test]
async fn delete_user_file_not_found_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "del-miss", "delmiss@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete("/api/files/user/del-miss/nope.txt", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_user_file_other_user_returns_403() {
    let (state, tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "del-own", "delown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "del-oth", "deloth@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("del-own");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("mine.txt"), b"private").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete("/api/files/user/del-own/mine.txt", &token_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = token_a;
}

// ---------------------------------------------------------------------------
// Browse user files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_user_files_empty_root() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "browse-empty", "browseempty@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/browse/user", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_user_files_returns_entries() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "browse-files", "browsefiles@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("browse-files");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("a.txt"), b"aaa").await.unwrap();
    fs::create_dir_all(user_dir.join("subdir")).await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/browse/user", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json.as_array().unwrap();
    assert_eq!(entries.len(), 2);

    let types: Vec<&str> = entries.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"file"));
    assert!(types.contains(&"folder"));
}

#[tokio::test]
async fn list_user_files_subdirectory() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "browse-sub", "browsesub@example.com", "password123").await;

    let sub_dir = tmp.path().join("files").join("browse-sub").join("docs");
    fs::create_dir_all(&sub_dir).await.unwrap();
    fs::write(sub_dir.join("nested.md"), b"# Hello").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/browse/user/docs", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "/docs/nested.md");
    assert_eq!(entries[0]["type"], "file");
    assert_eq!(entries[0]["parent"], "/docs");
}

// ---------------------------------------------------------------------------
// Browse agent files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_agent_files_root() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "ag-browse", "agbrowse@example.com", "password123").await;
    let agent = create_agent(&state, &token, "BrowseAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("file.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/browse/agent/{agent_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_agent_files_subdir() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "ag-sub", "agsub@example.com", "password123").await;
    let agent = create_agent(&state, &token, "SubAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let sub = tmp
        .path()
        .join("workspaces")
        .join(agent_id)
        .join("output");
    fs::create_dir_all(&sub).await.unwrap();
    fs::write(sub.join("result.json"), b"{}").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/browse/agent/{agent_id}/output"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "/output/result.json");
}

#[tokio::test]
async fn list_agent_files_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "ag-list-own", "aglistown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "ag-list-oth", "aglistoth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "PrivList").await;
    let agent_id = agent["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/browse/agent/{agent_id}"),
            &token_b,
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_files_finds_matching() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "searcher", "searcher@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("searcher");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("report.pdf"), b"pdf").await.unwrap();
    fs::write(user_dir.join("notes.txt"), b"txt").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/search?q=report", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0]["id"].as_str().unwrap().contains("report"));
}

#[tokio::test]
async fn search_files_empty_query_returns_empty() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "search-empty", "searchempty@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/search?q=", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn search_files_user_scope() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "search-scope", "searchscope@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("search-scope");
    let sub = user_dir.join("docs");
    fs::create_dir_all(&sub).await.unwrap();
    fs::write(sub.join("readme.md"), b"hi").await.unwrap();
    fs::write(user_dir.join("readme.txt"), b"other").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            "/api/files/search?q=readme&scope=user:docs",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0]["id"].as_str().unwrap().contains("readme.md"));
}

#[tokio::test]
async fn search_files_agent_scope() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "search-agent", "searchagent@example.com", "password123").await;
    let agent = create_agent(&state, &token, "SearchAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("found.csv"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/files/search?q=found&scope=agent:{agent_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
}

// ---------------------------------------------------------------------------
// Rename
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rename_user_file_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "renamer", "renamer@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("renamer");
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
async fn rename_file_not_found_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "rename-miss", "renamemiss@example.com", "password123").await;

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

    let user_dir = tmp.path().join("files").join("rename-bad");
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

    let user_dir = tmp.path().join("files").join("rename-dup");
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

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn copy_files_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "copier", "copier@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("copier");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("src.txt"), b"source data").await.unwrap();

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
async fn copy_directory_recursive() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "copydir", "copydir@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("copydir");
    let src_dir = user_dir.join("project");
    fs::create_dir_all(src_dir.join("sub")).await.unwrap();
    fs::write(src_dir.join("root.txt"), b"root").await.unwrap();
    fs::write(src_dir.join("sub").join("deep.txt"), b"deep").await.unwrap();

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
    assert!(user_dir.join("copy-dest").join("project").join("root.txt").exists());
    assert!(user_dir
        .join("copy-dest")
        .join("project")
        .join("sub")
        .join("deep.txt")
        .exists());
}

#[tokio::test]
async fn copy_to_agent_workspace_returns_403() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "copy-ag", "copyag@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("copy-ag");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("f.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/copy",
            &token,
            serde_json::json!({
                "sources": ["/f.txt"],
                "destination": "agent://some-agent/out"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Move
// ---------------------------------------------------------------------------

#[tokio::test]
async fn move_files_succeeds() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "mover", "mover@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("mover");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("moveme.txt"), b"moving").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["/moveme.txt"],
                "destination": "/moved"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!user_dir.join("moveme.txt").exists());
    assert!(user_dir.join("moved").join("moveme.txt").exists());
}

#[tokio::test]
async fn move_to_agent_workspace_returns_403() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "move-ag", "moveag@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("move-ag");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("f.txt"), b"data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["/f.txt"],
                "destination": "agent://ag/out"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn move_from_agent_workspace_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "move-from-ag", "movefromag@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/move",
            &token,
            serde_json::json!({
                "sources": ["agent://ag/file.txt"],
                "destination": "/dest"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Mkdir
// ---------------------------------------------------------------------------

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
    assert!(tmp.path().join("files").join("mkdir-user").join("new-folder").join("sub").is_dir());
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

// ---------------------------------------------------------------------------
// Presign
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_file_returns_url() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "presigner", "presigner@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("presigner");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("doc.pdf"), b"pdf content").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": format!("user:{user_id}"),
                "path": "doc.pdf"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let url = json["url"].as_str().unwrap();
    assert!(url.contains("presign="));
    assert!(url.contains("doc.pdf"));
}

#[tokio::test]
async fn presign_other_user_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "pre-own", "preown@example.com", "password123").await;
    let (_, user_id_b) =
        register_user(&state, "pre-oth", "preoth@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token_a,
            serde_json::json!({
                "owner": format!("user:{user_id_b}"),
                "path": "file.txt"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn presign_invalid_owner_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "pre-bad", "prebad@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": "invalid:prefix",
                "path": "file.txt"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Presign for agent files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_agent_file_returns_url() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "pre-agent", "preagent@example.com", "password123").await;
    let agent = create_agent(&state, &token, "PreAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("out.csv"), b"csv data").await.unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": format!("agent:{agent_id}"),
                "path": "out.csv"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let url = json["url"].as_str().unwrap();
    assert!(url.contains("presign="));
    assert!(url.contains("out.csv"));
}

// ---------------------------------------------------------------------------
// Presigned URL downloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_user_file_with_presigned_url() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "pre-dl", "predl@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("pre-dl");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("presigned.txt"), b"presigned content")
        .await
        .unwrap();

    // Get a presigned URL
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": format!("user:{user_id}"),
                "path": "presigned.txt"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let url = json["url"].as_str().unwrap();

    // Extract the presign token from the URL
    let presign_token = url.split("presign=").nth(1).unwrap();

    // Download using presigned URL (no auth header)
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/files/user/pre-dl/presigned.txt?presign={presign_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"presigned content");
}

#[tokio::test]
async fn download_agent_file_with_presigned_url() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "pre-ag-dl", "preagdl@example.com", "password123").await;
    let agent = create_agent(&state, &token, "PreDlAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("report.csv"), b"agent data")
        .await
        .unwrap();

    // Get a presigned URL for agent file
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": format!("agent:{agent_id}"),
                "path": "report.csv"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let url = json["url"].as_str().unwrap();
    let presign_token = url.split("presign=").nth(1).unwrap();

    // Download using presigned URL
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/files/agent/{agent_id}/report.csv?presign={presign_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"agent data");
}

#[tokio::test]
async fn download_user_file_presigned_wrong_path_returns_403() {
    let (state, tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "pre-wrong", "prewrong@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("pre-wrong");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("real.txt"), b"real").await.unwrap();
    fs::write(user_dir.join("other.txt"), b"other").await.unwrap();

    // Get presigned URL for real.txt
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/files/presign",
            &token,
            serde_json::json!({
                "owner": format!("user:{user_id}"),
                "path": "real.txt"
            }),
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let url = json["url"].as_str().unwrap();
    let presign_token = url.split("presign=").nth(1).unwrap();

    // Try to use it for other.txt — should be forbidden
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/files/user/pre-wrong/other.txt?presign={presign_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Search default scope (user + agent)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_files_default_scope_includes_agents() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "search-def", "searchdef@example.com", "password123").await;
    let agent = create_agent(&state, &token, "DefAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    // Create files in both user and agent dirs
    let user_dir = tmp.path().join("files").join("search-def");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("userfile.txt"), b"u").await.unwrap();

    let agent_dir = tmp.path().join("workspaces").join(agent_id);
    fs::create_dir_all(&agent_dir).await.unwrap();
    fs::write(agent_dir.join("agentfile.txt"), b"a").await.unwrap();

    // Search without scope — should find both
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/search?q=file", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 2);

    let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert!(ids.iter().any(|id| id.contains("userfile")));
    assert!(ids.iter().any(|id| id.contains("agentfile")));
}

// ---------------------------------------------------------------------------
// Copy/move cross-user via user:// prefix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn copy_from_other_user_via_prefix_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "copy-own-a", "copyowna@example.com", "password123").await;
    let (_, _) =
        register_user(&state, "copy-own-b", "copyownb@example.com", "password123").await;

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
async fn move_from_other_user_via_prefix_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "move-own-a", "moveowna@example.com", "password123").await;
    let (_, _) =
        register_user(&state, "move-own-b", "moveownb@example.com", "password123").await;

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

// ---------------------------------------------------------------------------
// Path traversal validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn browse_path_traversal_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "trav-browse", "travbrowse@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/files/browse/user/../../../etc", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_with_traversal_path_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "trav-upload", "travupload@example.com", "password123").await;

    let app = build_app(state);
    let req =
        multipart_upload_with_path(&token, "evil.txt", b"data", "../../etc/evil.txt");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Validate relative path edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_absolute_path_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "abs-path", "abspath@example.com", "password123").await;

    // Upload with path starting with '/' triggers validate_relative_path
    let app = build_app(state);
    let req = multipart_upload_with_path(&token, "file.txt", b"data", "/absolute/path.txt");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
async fn rename_path_traversal_returns_400() {
    let (state, tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "rename-trav", "renametrav@example.com", "password123").await;

    let user_dir = tmp.path().join("files").join("rename-trav");
    fs::create_dir_all(&user_dir).await.unwrap();
    fs::write(user_dir.join("ok.txt"), b"data").await.unwrap();

    // new_name with slash should be rejected
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

// ---------------------------------------------------------------------------
// Upload with unknown multipart field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_with_unknown_field_only_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "unk-field", "unkfield@example.com", "password123").await;

    let boundary = "----testboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"unknown\"\r\n\r\nvalue\r\n--{boundary}--\r\n"
    );

    let app = build_app(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// No-auth coverage for file endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_endpoints_reject_no_auth() {
    let (state, _tmp) = test_app_state().await;

    let cases: Vec<(&str, &str)> = vec![
        ("GET", "/api/files/browse/user"),
        ("GET", "/api/files/search?q=test"),
        ("POST", "/api/files/rename"),
        ("POST", "/api/files/copy"),
        ("POST", "/api/files/move"),
        ("POST", "/api/files/mkdir"),
        ("POST", "/api/files/presign"),
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
