mod helpers;

use frona::tool::files::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
use frona::tool::{AgentTool, InferenceContext};
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

async fn fixture() -> (helpers::app_state::Fixture, InferenceContext) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let fixture = helpers::app_state::build(&db).await;
    let ctx = helpers::mock_context();
    fixture.state.user_service.create(&ctx.user).await.unwrap();
    (fixture, ctx)
}

#[tokio::test]
async fn native_file_tools_accept_pwd_and_contained_parent_paths() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    tokio::fs::create_dir_all(workspace.join("reports"))
        .await
        .unwrap();
    let pwd = std::fs::canonicalize(&workspace).unwrap();
    let writer = WriteTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for (path, expected) in [
        ("reports/../relative.txt".to_string(), "relative.txt"),
        (
            pwd.join("reports/../absolute.txt")
                .to_string_lossy()
                .into_owned(),
            "absolute.txt",
        ),
        ("missing/../new/file.txt".to_string(), "new/file.txt"),
    ] {
        let output = writer
            .execute("write", json!({"path": path, "content": "accepted"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert_eq!(
            tokio::fs::read_to_string(workspace.join(expected))
                .await
                .unwrap(),
            "accepted"
        );
        let output = reader
            .execute("read", json!({"path": path}), &ctx)
            .await
            .unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert_eq!(output.text_content(), "accepted");
    }
    assert!(!workspace.join("missing").exists());
    let editor = EditTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let output = editor.execute("edit", json!({"path": "reports/../relative.txt", "old_string": "accepted", "new_string": "edited"}), &ctx).await.unwrap();
    assert!(output.is_success(), "{}", output.text_content());
    assert_eq!(
        tokio::fs::read_to_string(workspace.join("relative.txt"))
            .await
            .unwrap(),
        "edited"
    );
    let glob = GlobTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let grep = GrepTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for scope in [None, Some("reports/.."), Some(pwd.to_str().unwrap())] {
        let mut glob_args = json!({"pattern": "**/*.txt"});
        let mut grep_args = json!({"pattern": "edited"});
        if let Some(path) = scope {
            glob_args["path"] = json!(path);
            grep_args["path"] = json!(path);
        }
        let output = glob.execute("glob", glob_args, &ctx).await.unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert!(output.text_content().contains("relative.txt"));
        let output = grep.execute("grep", grep_args, &ctx).await.unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert!(output.text_content().contains("relative.txt:1:edited"));
    }
}

#[tokio::test]
async fn native_file_tools_enforce_existing_denials_on_resolved_destinations() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    let denied = workspace.parent().unwrap().join("denied-sibling");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(denied.join("nested"))
        .await
        .unwrap();
    tokio::fs::write(denied.join("secret.txt"), b"secret")
        .await
        .unwrap();
    tokio::fs::symlink(denied.join("nested"), workspace.join("link"))
        .await
        .unwrap();
    // Explicit denial avoids an ambient /tmp permission masking a path escape.
    state
        .policy_service
        .create_policy(
            &ctx.user.id,
            &format!(
                "@id(\"deny-sibling\") forbid(principal, action, resource == Policy::Path::{});",
                serde_json::to_string(&denied.to_string_lossy()).unwrap()
            ),
        )
        .await
        .unwrap();
    let writer = WriteTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for path in [
        "../denied-sibling/secret.txt".to_string(),
        "link/../secret.txt".to_string(),
        workspace
            .join("missing/../../denied-sibling/secret.txt")
            .to_string_lossy()
            .into_owned(),
        denied.join("secret.txt").to_string_lossy().into_owned(),
    ] {
        let output = reader
            .execute("read", json!({"path": path}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_success(), "read unexpectedly allowed: {path}");
        assert!(output.text_content().contains("denied by sandbox policy"));
    }
    for path in [
        "link/../new/file.txt".to_string(),
        workspace
            .join("missing/../../denied-sibling/new/file.txt")
            .to_string_lossy()
            .into_owned(),
    ] {
        let output = writer
            .execute("write", json!({"path": path, "content": "forbidden"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_success(), "write unexpectedly allowed: {path}");
        assert!(output.text_content().contains("denied by sandbox policy"));
    }
    assert!(!denied.join("new").exists());
    assert!(!workspace.join("missing").exists());
    assert_eq!(
        tokio::fs::read_to_string(denied.join("secret.txt"))
            .await
            .unwrap(),
        "secret"
    );
}

#[tokio::test]
async fn native_file_tools_preserve_inward_links_and_explicit_grants() {
    let (fixture, mut ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    tokio::fs::create_dir_all(workspace.join("real/nested"))
        .await
        .unwrap();
    tokio::fs::symlink(workspace.join("real/nested"), workspace.join("link"))
        .await
        .unwrap();
    let sandbox = state.sandbox_manager.for_tool(&ctx).await.unwrap();
    // /tmp may have an ambient grant. Prefer the checkout's target directory;
    // if the checkout itself is temporary, use the user's home instead.
    let parents =
        std::iter::once(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
            .chain(std::env::var_os("HOME").map(std::path::PathBuf::from));
    let grant_dir = parents
        .filter_map(|parent| tempfile::tempdir_in(parent).ok())
        .find(|dir| {
            let target = dir.path().join("absolute.txt");
            !sandbox.is_readable(&target) && !sandbox.is_writable(&target)
        })
        .expect("test requires a writable fixture directory outside ambient sandbox grants");
    let granted_file = grant_dir.path().join("absolute.txt");
    tokio::fs::write(&granted_file, "before grant")
        .await
        .unwrap();
    let writer = WriteTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    let output = reader
        .execute("read", json!({"path": granted_file}), &ctx)
        .await
        .unwrap();
    assert!(!output.is_success());
    assert!(output.text_content().contains("denied by sandbox policy"));
    let output = writer
        .execute(
            "write",
            json!({"path": granted_file, "content": "forbidden", "overwrite": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!output.is_success());
    assert!(output.text_content().contains("denied by sandbox policy"));
    assert_eq!(
        tokio::fs::read_to_string(&granted_file).await.unwrap(),
        "before grant"
    );

    ctx.file_paths
        .push(grant_dir.path().to_string_lossy().into_owned());
    for (path, target) in [
        (
            "link/../file.txt".to_string(),
            workspace.join("real/file.txt"),
        ),
        (granted_file.to_string_lossy().into_owned(), granted_file),
    ] {
        let output = writer
            .execute(
                "write",
                json!({"path": path, "content": "allowed", "overwrite": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert_eq!(tokio::fs::read_to_string(target).await.unwrap(), "allowed");
        let output = reader
            .execute("read", json!({"path": path}), &ctx)
            .await
            .unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert_eq!(output.text_content(), "allowed");
    }
    assert!(
        !workspace.join("file.txt").exists(),
        "symlink/.. was collapsed lexically"
    );
}

#[tokio::test]
async fn edit_tool_replaces_mixed_matches_atomically_with_correct_counts() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let editor = EditTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for (name, ending) in [("lf", "\n"), ("crlf", "\r\n")] {
        let path = workspace.join(name);
        let original = "before\n    “x”\n    \"x\"\n    “x”\n    after\n".replace('\n', ending);
        tokio::fs::write(&path, &original).await.unwrap();
        let original_link = workspace.join(format!("{name}-original"));
        tokio::fs::hard_link(&path, &original_link).await.unwrap();
        let output = editor.execute("edit", json!({
            "path": name, "old_string": "    \"x\"\n", "new_string": "    y\n", "replace_all": true,
        }), &ctx).await.unwrap();
        assert!(output.is_success(), "{}", output.text_content());
        assert!(output.text_content().contains("(3 replacements)"));
        assert_eq!(
            tokio::fs::read(&path).await.unwrap(),
            "before\n    y\n    y\n    y\n    after\n"
                .replace('\n', ending)
                .as_bytes()
        );
        // Atomic replacement leaves the original inode untouched.
        assert_eq!(
            tokio::fs::read(&original_link).await.unwrap(),
            original.as_bytes()
        );
    }
}

#[tokio::test]
async fn edit_tool_failures_leave_original_bytes_unchanged() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let path = workspace.join("unchanged.txt");
    let original = "中\r\n  \"x\"\r\n  “x”\r\n  \"x\"\r\n";
    tokio::fs::write(&path, original).await.unwrap();
    let editor = EditTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for (old, expected_error) in [
        ("", "old_string must not be empty"),
        ("missing", "old_string not found"),
        ("\"x\"", "matches 2 locations"),
        ("＂x＂", "matches 3 locations"),
    ] {
        let output = editor
            .execute(
                "edit",
                json!({
                    "path": "unchanged.txt", "old_string": old, "new_string": "changed",
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_success());
        assert!(
            output.text_content().contains(expected_error),
            "{}",
            output.text_content()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), original.as_bytes());
    }
}

#[tokio::test]
async fn native_image_reads_share_validation_and_resize_behavior() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for width in [1, 2001, 8193] {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(width, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let path = format!("image-{width}.png");
        tokio::fs::write(workspace.join(&path), encoded.into_inner())
            .await
            .unwrap();
        let output = reader
            .execute("read", json!({"path": path}), &ctx)
            .await
            .unwrap();
        if width > 8192 {
            assert!(!output.is_success());
            assert!(output.images().is_empty());
            assert!(output.text_content().contains("decoding limits"));
        } else {
            assert!(output.is_success());
            assert_eq!(output.images().len(), 1);
            assert_eq!(output.images()[0].media_type, "image/png");
            assert_eq!(
                image::load_from_memory(&output.images()[0].bytes)
                    .unwrap()
                    .width(),
                width.min(2000)
            );
        }
    }
}

#[tokio::test]
async fn read_directories_offer_sorted_entries_and_file_guidance() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    tokio::fs::create_dir_all(workspace.join("empty"))
        .await
        .unwrap();
    let empty = reader
        .execute("read", json!({"path": "empty"}), &ctx)
        .await
        .unwrap();
    assert!(!empty.is_success());
    assert!(empty.text_content().contains("Directory is empty"));
    assert!(empty.text_content().contains("Choose a file"));

    tokio::fs::create_dir_all(workspace.join("reports/nested"))
        .await
        .unwrap();
    for name in [
        "z.txt",
        "a.txt",
        "é.txt",
        "line\nbreak.txt",
        "nested/inside.txt",
    ] {
        tokio::fs::write(workspace.join("reports").join(name), "first\nsecond\nthird")
            .await
            .unwrap();
    }
    let first = reader
        .execute("read", json!({"path": "reports"}), &ctx)
        .await
        .unwrap();
    assert!(!first.is_success());
    assert_eq!(
        first.text_content().lines().skip(1).collect::<Vec<_>>(),
        [
            "\"a.txt\"",
            "\"line\\nbreak.txt\"",
            "\"nested/\"",
            "\"z.txt\"",
            "\"é.txt\""
        ]
    );
    assert!(!first.text_content().contains("inside.txt"));
    // Filesystem enumeration order and Read's file-only pagination arguments
    // must not change the deterministic directory guidance.
    for _ in 0..3 {
        let again = reader
            .execute(
                "read",
                json!({"path": "reports", "offset": 99, "limit": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(again.text_content(), first.text_content());
    }
    let nested = reader
        .execute(
            "read",
            json!({"path": "reports/nested/inside.txt", "offset": 2, "limit": 1}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(nested.is_success());
    assert_eq!(
        nested.text_content(),
        "second\n\n[Showing lines 2-2 of 3. Use offset=3 to continue.]"
    );
    let missing = reader
        .execute("read", json!({"path": "reports/missing.txt"}), &ctx)
        .await
        .unwrap();
    assert!(!missing.is_success());
    assert!(missing.text_content().contains("file not found"));
    for path in [
        "",
        "agent://system/file.txt",
        "reports/a.txt/child",
        "reports/invalid\0name",
    ] {
        assert!(matches!(
            reader.execute("read", json!({"path": path}), &ctx).await,
            Err(frona::core::error::AppError::Validation(_))
        ));
    }
}

#[tokio::test]
async fn read_directory_listing_bounds_are_deterministic() {
    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for (directory, count, padding) in [
        ("display", 51, 0),
        ("scan", 129, 0),
        ("names", 80, 220),
        ("bytes", 40, 220),
    ] {
        tokio::fs::create_dir_all(workspace.join(directory))
            .await
            .unwrap();
        for index in (0..count).rev() {
            let name = format!("{index:03}{}", "x".repeat(padding));
            tokio::fs::write(workspace.join(directory).join(name), "")
                .await
                .unwrap();
        }
        let first = reader
            .execute("read", json!({"path": directory}), &ctx)
            .await
            .unwrap();
        assert!(!first.is_success());
        assert!(first.text_content().len() <= 8 * 1024);
        assert!(
            first
                .text_content()
                .contains("Use Glob with a narrower pattern")
        );
        if directory == "scan" || directory == "names" {
            assert!(first.text_content().contains("Listing omitted"));
            assert_eq!(first.text_content().lines().count(), 2);
        } else {
            assert!(first.text_content().contains("Listing truncated"));
            assert!(
                first
                    .text_content()
                    .lines()
                    .nth(1)
                    .unwrap()
                    .starts_with("\"000")
            );
            assert!(first.text_content().lines().count() <= 52);
            if directory == "display" {
                assert!(first.text_content().contains("\"049\""));
                assert!(!first.text_content().contains("\"050\""));
            }
        }
        let again = reader
            .execute("read", json!({"path": directory}), &ctx)
            .await
            .unwrap();
        assert_eq!(first.text_content(), again.text_content());
    }
}

#[tokio::test]
async fn read_unreadable_and_forbidden_directories_do_not_leak_entries() {
    use std::os::unix::fs::PermissionsExt;

    let (fixture, ctx) = fixture().await;
    let state = &fixture.state;
    let workspace = state
        .storage_service
        .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);
    let reader = ReadTool::new(
        state.storage_service.clone(),
        state.sandbox_manager.clone(),
        state.prompts.clone(),
    );
    for name in ["unreadable", "forbidden"] {
        tokio::fs::create_dir_all(workspace.join(name))
            .await
            .unwrap();
        tokio::fs::write(workspace.join(name).join("hidden-name.txt"), "secret")
            .await
            .unwrap();
    }
    tokio::fs::write(workspace.join("unreadable.txt"), "secret")
        .await
        .unwrap();
    for path in ["unreadable", "unreadable.txt"] {
        tokio::fs::set_permissions(workspace.join(path), std::fs::Permissions::from_mode(0))
            .await
            .unwrap();
        let result = reader.execute("read", json!({"path": path}), &ctx).await;
        // Restore permissions before assertions so failed tests remain cleanable.
        tokio::fs::set_permissions(workspace.join(path), std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let output = result.unwrap();
        assert!(
            !output.is_success(),
            "permission fixture requires an unprivileged test user"
        );
        assert!(output.text_content().contains("Permission denied"));
        assert!(!output.text_content().contains("empty"));
        assert!(!output.text_content().contains("hidden-name.txt"));
        if path == "unreadable" {
            assert!(output.text_content().contains("could not list entries"));
        }
    }
    state
        .policy_service
        .create_policy(
            &ctx.user.id,
            &format!(
                "@id(\"deny-directory\") forbid(principal, action, resource == Policy::Path::{});",
                serde_json::to_string(&workspace.join("forbidden").to_string_lossy()).unwrap()
            ),
        )
        .await
        .unwrap();
    let denied = reader
        .execute("read", json!({"path": "forbidden"}), &ctx)
        .await
        .unwrap();
    assert!(!denied.is_success());
    assert!(denied.text_content().contains("denied by sandbox policy"));
    assert!(!denied.text_content().contains("hidden-name.txt"));
    assert!(!denied.text_content().contains("Choose a file"));
}
