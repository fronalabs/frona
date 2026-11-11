use crate::prompt::{PromptError, PromptLoader};

#[test]
fn source_precedence_and_live_directory_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.md");
    let loader = PromptLoader::embedded([("test.md", "embedded")]).with_directory(dir.path());
    assert_eq!(
        loader.try_read("test.md").unwrap().as_deref(),
        Some("embedded")
    );
    std::fs::write(&path, "directory").unwrap();
    assert_eq!(
        loader.try_read("test.md").unwrap().as_deref(),
        Some("directory")
    );
    std::fs::write(&path, "updated").unwrap();
    assert_eq!(
        loader.try_read("test.md").unwrap().as_deref(),
        Some("updated")
    );
    let loader = loader.with_override("test.md", "explicit");
    assert_eq!(
        loader.try_read("test.md").unwrap().as_deref(),
        Some("explicit")
    );
}

#[test]
fn invalid_overrides_never_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.md");
    let loader = PromptLoader::embedded([("test.md", "embedded")]).with_directory(dir.path());
    std::fs::write(&path, [0xff]).unwrap();
    assert!(matches!(
        loader.try_read("test.md"),
        Err(PromptError::Read { .. })
    ));
    std::fs::write(&path, "{{missing}}").unwrap();
    assert!(matches!(
        loader.try_read("test.md"),
        Err(PromptError::Render { name: Some(_), .. })
    ));
    assert!(loader.read("test.md").is_none());
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        loader.try_read_raw("test.md"),
        Err(PromptError::Read { .. })
    ));
    let loader = loader.with_override("test.md", "{{missing}}");
    assert!(matches!(
        loader.try_read("test.md"),
        Err(PromptError::Render { .. })
    ));
}

#[test]
fn variables_override_defaults_case_insensitively_without_mutating_them() {
    let loader = PromptLoader::embedded([("test.md", "{{NAME}} from {{location}}")])
        .with_var("name", "Default")
        .with_var("Location", "Home");
    assert_eq!(
        loader
            .read_with_vars("test.md", &[("Name", "Caller")])
            .unwrap(),
        "Caller from Home"
    );
    assert_eq!(loader.read("test.md").unwrap(), "Default from Home");
    assert_eq!(loader.defaults()["location"], "Home");
    assert_eq!(
        loader.read_raw("test.md").unwrap(),
        "{{NAME}} from {{location}}"
    );
}

#[test]
fn missing_is_distinct_from_empty() {
    let loader = PromptLoader::embedded([("empty.md", "")]);
    assert_eq!(loader.try_read("absent.md").unwrap(), None);
    assert_eq!(loader.try_read("empty.md").unwrap(), Some(String::new()));
    assert!(
        PromptLoader::new("/nonexistent")
            .read("missing.md")
            .is_none()
    );
}

#[test]
fn listing_merges_sources_deduplicates_and_only_lists_immediate_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("tools")).unwrap();
    std::fs::write(dir.path().join("tools/b.md"), "b").unwrap();
    std::fs::write(dir.path().join("tools/a.md"), "a").unwrap();
    std::fs::create_dir(dir.path().join("tools/sub")).unwrap();
    let loader = PromptLoader::embedded([
        ("tools/a.md", "a"),
        ("tools/c.md", "c"),
        ("tools/sub/d.md", "d"),
    ])
    .with_directory(dir.path())
    .with_override("tools/d.md", "d");
    assert_eq!(
        loader.list_dir("tools"),
        ["tools/a.md", "tools/b.md", "tools/c.md", "tools/d.md"]
    );
}
