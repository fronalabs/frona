#![cfg(unix)]
use frona_sandbox::{
    ExecutionContext, ExecutionHook, ExecutionLease, HookFuture, Sandbox, SandboxFactory,
};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Guard {
    path: PathBuf,
    dropped: Arc<AtomicUsize>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).unwrap();
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Hook {
    directory: PathBuf,
    calls: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    ready: tokio::sync::Notify,
    pending: bool,
    fail: bool,
}
impl ExecutionHook for Hook {
    fn prepare<'a>(&'a self, context: &'a ExecutionContext<'a>) -> HookFuture<'a> {
        Box::pin(async move {
            assert_eq!(context.owner_id, "hook-test");
            assert_eq!(context.workspace, context.working_directory);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let path = self.directory.join(format!("token-{call}"));
            std::fs::write(&path, format!("token-{call}")).unwrap();
            let guard = Guard {
                path: path.clone(),
                dropped: self.dropped.clone(),
            };
            self.ready.notify_one();
            if self.pending {
                std::future::pending::<()>().await;
            }
            if self.fail {
                return Err(std::io::Error::other("hook refused").into());
            }
            Ok(ExecutionLease::new()
                .env("TOKEN", path.to_string_lossy())
                .read_file(path)
                .retain(guard))
        })
    }
}
fn setup(pending: bool, fail: bool) -> (tempfile::TempDir, Sandbox, Arc<Hook>) {
    let directory = tempfile::tempdir().unwrap();
    let hook = Arc::new(Hook {
        directory: directory.path().into(),
        calls: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
        ready: tokio::sync::Notify::new(),
        pending,
        fail,
    });
    let sandbox = SandboxFactory::new(true)
        .get_sandbox(
            directory.path().join("workspace"),
            "hook-test",
            false,
            vec![],
        )
        .without_venv()
        .without_node()
        .with_extra_env_vars(vec![("TOKEN".into(), "wrong".into())])
        .with_execution_hook(hook.clone());
    (directory, sandbox, hook)
}

#[tokio::test]
async fn fresh_lease_each_execution_overrides_env_and_cleans_up() {
    let (_dir, sandbox, hook) = setup(false, false);
    assert!(sandbox.is_readable(sandbox.path()));
    assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
    for call in 0..2 {
        let output = sandbox
            .execute("sh", &["-c", "cat \"$TOKEN\""], 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(output.stdout.trim(), format!("token-{call}"));
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(hook.dropped.load(Ordering::SeqCst), call + 1);
    }
}

#[tokio::test]
async fn spawned_child_retains_lease_after_sandbox_drop_and_releases_on_wait() {
    let (_dir, sandbox, hook) = setup(false, false);
    let mut child = sandbox
        .spawn(
            "sh",
            &["-c", "cat \"$TOKEN\""],
            None,
            vec![],
            None,
            Stdio::null(),
            Stdio::null(),
        )
        .await
        .unwrap();
    drop(sandbox);
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 0);
    assert!(child.wait().await.unwrap().success());
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spawn_failure_releases_prepared_lease() {
    let (_dir, sandbox, hook) = setup(false, false);
    assert!(
        sandbox
            .spawn(
                "/missing-hook-test-program",
                &[],
                None,
                vec![],
                None,
                Stdio::null(),
                Stdio::null()
            )
            .await
            .is_err()
    );
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hook_error_prevents_spawn_and_releases_acquired_resources() {
    let (dir, sandbox, hook) = setup(false, true);
    let marker = dir.path().join("spawned");
    assert!(
        sandbox
            .execute("touch", &[marker.to_str().unwrap()], 5, None, None, None)
            .await
            .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_prepare_releases_resources_without_spawning() {
    let (dir, sandbox, hook) = setup(true, false);
    let marker = dir.path().join("spawned");
    let cancel = CancellationToken::new();
    let args = [marker.to_str().unwrap()];
    let run = sandbox.execute("touch", &args, 5, None, None, Some(cancel.clone()));
    let cancel_ready = async {
        hook.ready.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(run, cancel_ready);
    assert!(result.unwrap().cancelled);
    assert!(!marker.exists());
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_child_releases_lease() {
    let (_dir, sandbox, hook) = setup(false, false);
    let child = sandbox
        .spawn(
            "sleep",
            &["30"],
            None,
            vec![],
            None,
            Stdio::null(),
            Stdio::null(),
        )
        .await
        .unwrap();
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 0);
    drop(child);
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_prepare_future_releases_resources() {
    let (_dir, sandbox, hook) = setup(true, false);
    let mut run = Box::pin(sandbox.spawn(
        "sleep",
        &["30"],
        None,
        vec![],
        None,
        Stdio::null(),
        Stdio::null(),
    ));
    tokio::select! { biased; _ = &mut run => panic!("prepare should remain pending"), _ = hook.ready.notified() => {} }
    drop(run);
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn hook_file_grant_is_applied_before_enforced_spawn_without_sibling_access() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    if canonical.starts_with("/tmp") {
        eprintln!("skipping exact hook grant isolation: checkout is under implicitly granted /tmp");
        return;
    }
    let hook = Arc::new(Hook {
        directory: canonical.clone(),
        calls: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
        ready: tokio::sync::Notify::new(),
        pending: false,
        fail: false,
    });
    let sibling = canonical.join("sibling");
    std::fs::write(&sibling, "must-not-read").unwrap();
    let factory = SandboxFactory::new(false);
    assert_ne!(
        factory.driver_id(),
        "noop",
        "test requires real enforcement"
    );
    let sandbox = factory
        .get_sandbox(canonical.join("workspace"), "hook-test", false, vec![])
        .without_venv()
        .without_node()
        .with_execution_hook(hook.clone());
    let output = sandbox
        .execute(
            "sh",
            &[
                "-c",
                "cat \"$TOKEN\"; if cat \"$1\"; then exit 9; fi",
                "sh",
                sibling.to_str().unwrap(),
            ],
            5,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
    assert_eq!(output.stdout.trim(), "token-0");
    assert_eq!(hook.dropped.load(Ordering::SeqCst), 1);
}
