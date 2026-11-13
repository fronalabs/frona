use frona_sandbox::{ExecutionOptions, SandboxFactory};

#[cfg(unix)]
#[tokio::test]
async fn public_options_control_output_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let factory = SandboxFactory::new(true);
    let sandbox = factory
        .get_sandbox(dir.path().to_path_buf(), "job", false, vec![])
        .without_venv()
        .without_node();
    let output = sandbox
        .execute_with_options(
            "sh",
            &["-c", "printf '1234567890\\n'"],
            ExecutionOptions {
                timeout_secs: 5,
                max_output_bytes: 4,
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert!(output.stdout.starts_with("1234"));
    assert!(!output.stdout.contains("567890"));
    assert!(output.stdout.contains("truncated"));
}
