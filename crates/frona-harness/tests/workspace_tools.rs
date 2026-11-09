use std::{io::Cursor, path::Path};

use frona_harness::{
    ToolContent, ToolContext, ToolError, ToolOutput, ToolRegistry, workspace_read_tools,
};
use rig_core::completion::message::{DocumentSourceKind, ImageMediaType};
use serde_json::{Value, json};

async fn call(
    tools: &mut ToolRegistry<'_>,
    name: &str,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    tools
        .get_mut(name)
        .unwrap()
        .call(args, &ToolContext::default())
        .await
}

fn text(output: ToolOutput) -> String {
    match &output.content[..] {
        [ToolContent::Text(text)] => text.text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn explicit_bundle_contains_only_read_and_glob_and_requires_a_directory() {
    assert!(ToolRegistry::default().is_empty());
    let workspace = tempfile::tempdir().unwrap();
    let tools = workspace_read_tools(workspace.path()).unwrap();
    assert_eq!(
        tools
            .definitions()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["glob", "read"]
    );
    assert!(workspace_read_tools(workspace.path().join("missing")).is_err());
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(workspace_read_tools(file.path()).is_err());
}

#[tokio::test]
async fn read_preserves_pagination_caps_and_ordinary_errors() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "one\ntwo\nthree\nfour").unwrap();
    std::fs::write(workspace.path().join("empty.txt"), "").unwrap();
    std::fs::write(workspace.path().join("binary"), [0xff, 0xfe]).unwrap();
    std::fs::write(workspace.path().join("large"), "line\n".repeat(2001)).unwrap();
    let mut tools = workspace_read_tools(workspace.path()).unwrap();
    assert_eq!(
        text(
            call(
                &mut tools,
                "read",
                json!({"path":"notes.txt","offset":2,"limit":2})
            )
            .await
            .unwrap()
        ),
        "two\nthree\n\n[Showing lines 2-3 of 4. Use offset=4 to continue.]"
    );
    assert_eq!(
        text(
            call(
                &mut tools,
                "read",
                json!({"path":"notes.txt","offset":2,"limit":usize::MAX})
            )
            .await
            .unwrap()
        ),
        "two\nthree\nfour"
    );
    assert_eq!(
        text(
            call(&mut tools, "read", json!({"path":"empty.txt","offset":99}))
                .await
                .unwrap()
        ),
        ""
    );
    assert!(
        text(
            call(&mut tools, "read", json!({"path":"large"}))
                .await
                .unwrap()
        )
        .ends_with("Use offset=2001 to continue.]")
    );
    for (path, message) in [("missing", "file not found"), ("binary", "binary file")] {
        let error = call(&mut tools, "read", json!({"path":path}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed(_)));
        assert!(error.to_string().contains(message));
        if path == "binary" {
            let feedback = error.to_string();
            assert!(feedback.contains("Choose a text file or a supported image"));
            assert!(!feedback.contains("produce_file"));
            assert!(!feedback.contains("CliTool"));
        }
    }
    assert!(
        call(&mut tools, "read", json!({"path":"notes.txt","offset":100}))
            .await
            .unwrap_err()
            .to_string()
            .contains("past end")
    );
}

#[tokio::test]
async fn directory_guidance_stays_bounded_and_quotes_names() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a\nb.txt"), "text").unwrap();
    std::fs::create_dir(workspace.path().join("subdir")).unwrap();
    let mut tools = workspace_read_tools(workspace.path()).unwrap();
    let listing = call(&mut tools, "read", json!({"path":"."}))
        .await
        .unwrap_err()
        .to_string();
    assert!(listing.contains("Read got a directory"));
    assert!(listing.contains("\"a\\nb.txt\""));
    assert!(listing.contains("\"subdir/\""));
    for i in 0..129 {
        std::fs::write(workspace.path().join(format!("file-{i}")), "").unwrap();
    }
    let listing = call(&mut tools, "read", json!({"path":"."}))
        .await
        .unwrap_err()
        .to_string();
    assert!(listing.contains("Listing omitted"));
    assert!(listing.len() < 300);
}

#[tokio::test]
async fn image_content_keeps_mime_and_encoded_bytes() {
    use base64::Engine;
    let workspace = tempfile::tempdir().unwrap();
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    std::fs::write(workspace.path().join("image.png"), png.get_ref()).unwrap();
    let mut tools = workspace_read_tools(workspace.path()).unwrap();
    let result = call(&mut tools, "read", json!({"path":"image.png"}))
        .await
        .unwrap();
    let [ToolContent::Text(caption), ToolContent::Image(image)] = &result.content[..] else {
        panic!("missing image: {result:?}")
    };
    assert_eq!(caption.text, "Read image file [image/png]");
    assert_eq!(image.media_type, Some(ImageMediaType::PNG));
    let DocumentSourceKind::Base64(encoded) = &image.data else {
        panic!("expected base64")
    };
    assert_eq!(
        &base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap(),
        png.get_ref()
    );
}

#[tokio::test]
async fn glob_uses_existing_ignore_walker_and_relative_result_format() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("notes")).unwrap();
    for name in [
        "notes/one.txt",
        "notes/two.md",
        "ignored.txt",
        ".hidden.txt",
    ] {
        std::fs::write(workspace.path().join(name), "text").unwrap();
    }
    std::fs::write(workspace.path().join(".gitignore"), "ignored.txt\n").unwrap();
    let mut tools = workspace_read_tools(workspace.path()).unwrap();
    assert_eq!(
        text(
            call(&mut tools, "glob", json!({"pattern":"**/*.txt"}))
                .await
                .unwrap()
        ),
        "notes/one.txt"
    );
    assert_eq!(
        text(
            call(
                &mut tools,
                "glob",
                json!({"pattern":"*.txt","path":"notes"})
            )
            .await
            .unwrap()
        ),
        "one.txt"
    );
    assert_eq!(
        text(
            call(&mut tools, "glob", json!({"pattern":"*.rs"}))
                .await
                .unwrap()
        ),
        "No matches for pattern *.rs under .."
    );
    assert!(
        call(&mut tools, "glob", json!({"pattern":"["}))
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid glob")
    );
    assert!(
        call(
            &mut tools,
            "glob",
            json!({"pattern":"*","path":"notes/one.txt"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("scope must be a directory")
    );
}

async fn denied_for_both(tools: &mut ToolRegistry<'_>, path: &Path) {
    for tool in ["read", "glob"] {
        let error = call(tools, tool, json!({"path":path,"pattern":"*"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed(_)));
        assert!(
            error.to_string().contains("Path escapes allowed directory"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn absolute_parent_and_prefix_sibling_paths_cannot_escape_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    let sibling = parent.path().join("workspace-other");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    std::fs::write(root.join("inside"), "allowed").unwrap();
    std::fs::write(sibling.join("secret"), "forbidden").unwrap();
    let mut tools = workspace_read_tools(&root).unwrap();
    denied_for_both(&mut tools, &sibling).await;
    denied_for_both(&mut tools, Path::new("../workspace-other")).await;
    denied_for_both(&mut tools, Path::new("missing/../../workspace-other")).await;
    assert_eq!(
        text(
            call(&mut tools, "read", json!({"path":root.join("inside")}))
                .await
                .unwrap()
        ),
        "allowed"
    );
    for path in ["", "agent://other/file", "user://me/file"] {
        for tool in ["read", "glob"] {
            assert!(matches!(
                call(&mut tools, tool, json!({"path":path,"pattern":"*"})).await,
                Err(ToolError::Failed(_))
            ));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_destinations_are_checked_and_glob_does_not_follow_links() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir_all(root.join("subdir")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "forbidden").unwrap();
    std::fs::write(root.join("subdir/inside"), "allowed").unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    symlink(root.join("subdir"), root.join("alias")).unwrap();
    symlink(outside.join("missing"), root.join("dangling")).unwrap();
    let mut tools = workspace_read_tools(&root).unwrap();
    denied_for_both(&mut tools, Path::new("escape")).await;
    denied_for_both(&mut tools, Path::new("escape/../outside")).await;
    assert!(
        call(&mut tools, "read", json!({"path":"escape/secret"}))
            .await
            .is_err()
    );
    assert!(
        call(&mut tools, "read", json!({"path":"dangling"}))
            .await
            .is_err()
    );
    assert_eq!(
        text(
            call(&mut tools, "read", json!({"path":"alias/inside"}))
                .await
                .unwrap()
        ),
        "allowed"
    );
    assert_eq!(
        text(
            call(&mut tools, "glob", json!({"pattern":"**/*"}))
                .await
                .unwrap()
        ),
        "subdir/inside"
    );
}
