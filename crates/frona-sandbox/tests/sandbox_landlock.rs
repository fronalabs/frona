#![cfg(target_os = "linux")]

use frona_sandbox::SandboxFactory;
use std::path::PathBuf;

fn test_manager() -> (SandboxFactory, PathBuf) {
    let base = std::env::temp_dir()
        .join("frona_test_landlock")
        .join(uuid::Uuid::new_v4().to_string());
    (SandboxFactory::new(false), base)
}

fn relative_path_manager() -> (SandboxFactory, PathBuf) {
    let base = PathBuf::from(format!("target/test_landlock_{}", uuid::Uuid::new_v4()));
    (SandboxFactory::new(false), base)
}

#[tokio::test]
async fn test_sandbox_allows_read_system_paths() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-read-sys", false, vec![]);

    let output = ws
        .execute("cat", &["/etc/hostname"], 10, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to read /etc/hostname: stderr={}",
        output.stderr
    );
    assert!(!output.stdout.trim().is_empty());

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_blocks_write_to_system_paths() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-write-sys", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &["-c", "echo hacked > /etc/landlock_test_file"],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(
        output.exit_code,
        Some(0),
        "should NOT be able to write to /etc: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_allows_write_to_workspace() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-write-ws", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &["-c", "echo hello > testfile.txt && cat testfile.txt"],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to write in workspace: stderr={}",
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "hello");

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_blocks_read_outside_allowed_paths() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-block-read", false, vec![]);

    let output = ws
        .execute("ls", &["/root"], 10, None, None, None)
        .await
        .unwrap();

    assert_ne!(
        output.exit_code,
        Some(0),
        "should NOT be able to read /root: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_allows_write_to_tmp() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-write-tmp", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &[
                "-c",
                "echo test > /tmp/frona_landlock_test && cat /tmp/frona_landlock_test && rm /tmp/frona_landlock_test",
            ],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to write to /tmp: stderr={}",
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "test");

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_blocks_write_outside_workspace() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-block-write", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &["-c", "echo hacked > /usr/landlock_test_file"],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(
        output.exit_code,
        Some(0),
        "should NOT be able to write to /usr: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_python_in_workspace() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-python", false, vec![]);

    let output = ws
        .execute(
            "python3",
            &["-c", "print('sandboxed python works')"],
            30,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "python3 should work in sandbox: stderr={}",
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "sandboxed python works");

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_python_cannot_write_system() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-py-no-write", false, vec![]);

    let output = ws
        .execute(
            "python3",
            &["-c", "open('/etc/landlock_test', 'w').write('hacked')"],
            30,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(
        output.exit_code,
        Some(0),
        "python should NOT be able to write to /etc: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

/// Workspace on a relative path (resolves to /app/target/... on the named volume).
/// This verifies landlock works on ext4 named volumes.
#[tokio::test]
async fn test_sandbox_write_with_relative_workspace_path() {
    let (mgr, base) = relative_path_manager();
    let ws = mgr.get_sandbox(base, "landlock-relative", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &["-c", "echo hello > testfile.txt && cat testfile.txt"],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to write in workspace (relative path): stderr={}",
        output.stderr
    );
    assert!(
        output.stdout.contains("hello"),
        "stdout should contain 'hello', got: {}",
        output.stdout
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

/// Tests writing a CSV file via heredoc — matches the actual agent usage pattern.
#[tokio::test]
async fn test_sandbox_heredoc_write() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-heredoc", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &[
                "-c",
                "cat <<'EOF' > data.csv\nname,date\nAlice,2024-01-01\nBob,2024-02-02\nEOF\ncat data.csv",
            ],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "heredoc write should work in workspace: stderr={}",
        output.stderr
    );
    assert!(
        output.stdout.contains("Alice"),
        "stdout should contain CSV data, got: {}",
        output.stdout
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

/// Tests Python writing a file inside the workspace.
#[tokio::test]
async fn test_sandbox_python_write_to_workspace() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-py-write-ws", false, vec![]);

    let output = ws
        .execute(
            "python3",
            &[
                "-c",
                "with open('output.txt', 'w') as f: f.write('hello from python')\nwith open('output.txt') as f: print(f.read())",
            ],
            30,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "python should be able to write files in workspace: stderr={}",
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "hello from python");

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_blocks_read_etc_shadow() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-etc-shadow", false, vec![]);

    let output = ws
        .execute("cat", &["/etc/shadow"], 10, None, None, None)
        .await
        .unwrap();

    assert_ne!(
        output.exit_code,
        Some(0),
        "should NOT be able to read /etc/shadow"
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_allows_read_etc_passwd() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-etc-passwd", false, vec![]);

    let output = ws
        .execute("cat", &["/etc/passwd"], 10, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to read /etc/passwd: stderr={}",
        output.stderr
    );
    assert!(!output.stdout.trim().is_empty());

    let _ = std::fs::remove_dir_all(ws.path());
}

#[tokio::test]
async fn test_sandbox_allows_read_etc_ssl() {
    let (mgr, base) = test_manager();
    let ws = mgr.get_sandbox(base, "landlock-etc-ssl", false, vec![]);

    let output = ws
        .execute("ls", &["/etc/ssl"], 10, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should be able to read /etc/ssl: stderr={}",
        output.stderr
    );

    let _ = std::fs::remove_dir_all(ws.path());
}

/// Verifies that the sandbox gracefully falls back (allows writes) when the
/// workspace is on a filesystem that doesn't support landlock enforcement
/// (e.g. Docker Desktop's VirtioFS fakeowner mounts).
#[tokio::test]
async fn test_sandbox_fallback_on_unsupported_fs() {
    let base = PathBuf::from("/app/data/workspaces/test_fallback");
    if std::fs::create_dir_all(&base).is_err() {
        eprintln!(
            "cannot create {}, skipping (not in Docker?)",
            base.display()
        );
        return;
    }

    let mgr = SandboxFactory::new(false);
    let ws = mgr.get_sandbox(base, "landlock-fallback", false, vec![]);

    let output = ws
        .execute(
            "bash",
            &["-c", "echo works > test.txt && cat test.txt"],
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        output.exit_code,
        Some(0),
        "should fall back gracefully on unsupported fs: stderr={}",
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "works");

    let _ = std::fs::remove_dir_all(ws.path());
}
