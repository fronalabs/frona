use frona::tool::workspace::WorkspaceManager;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn test_manager() -> WorkspaceManager {
    let base = std::env::temp_dir()
        .join("frona_test_venv_integration")
        .join(uuid::Uuid::new_v4().to_string());
    WorkspaceManager::new(base, false)
}

#[tokio::test]
async fn test_execute_uses_venv_python() {
    if !python3_available() {
        eprintln!("python3 not found, skipping");
        return;
    }

    let mgr = test_manager();
    let ws = mgr.get_workspace("agent-venv-prefix", false, vec![]);

    let output = ws
        .execute(
            "python3",
            &["-c", "import sys; print(sys.prefix)"],
            None,
            30,
        )
        .await
        .unwrap();

    assert!(
        output.exit_code == Some(0),
        "python3 should succeed: exit={:?} stderr={:?}",
        output.exit_code,
        output.stderr,
    );
    let prefix = output.stdout.trim();
    assert!(
        prefix.contains(".venv"),
        "sys.prefix should point to the venv, got: {prefix}"
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_shell_uses_venv_python() {
    if !python3_available() {
        eprintln!("python3 not found, skipping");
        return;
    }

    let mgr = test_manager();
    let ws = mgr.get_workspace("agent-venv-which", false, vec![]);

    let output = ws
        .execute("which", &["python3"], None, 30)
        .await
        .unwrap();

    let which_path = output.stdout.trim();
    assert!(
        which_path.contains(".venv/bin/python3"),
        "which python3 should resolve to .venv/bin/python3, got: {which_path}"
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_pip_install_isolated() {
    if !python3_available() {
        eprintln!("python3 not found, skipping");
        return;
    }

    let mgr = test_manager();
    let ws_a = mgr.get_workspace("agent-pip-a", true, vec![]);
    let ws_b = mgr.get_workspace("agent-pip-b", false, vec![]);

    let install = ws_a
        .execute("pip", &["install", "cowsay"], None, 60)
        .await
        .unwrap();
    assert!(
        install.exit_code == Some(0),
        "pip install should succeed: {}",
        install.stderr
    );

    let import_a = ws_a
        .execute(
            "python3",
            &["-c", "import cowsay; print('ok')"],
            None,
            30,
        )
        .await
        .unwrap();
    assert_eq!(import_a.stdout.trim(), "ok");

    let import_b = ws_b
        .execute(
            "python3",
            &["-c", "import cowsay; print('ok')"],
            None,
            30,
        )
        .await
        .unwrap();
    assert!(
        import_b.exit_code != Some(0),
        "agent B should NOT be able to import cowsay"
    );

    let _ = std::fs::remove_dir_all(ws_a.path());
    let _ = std::fs::remove_dir_all(ws_b.path());
}
