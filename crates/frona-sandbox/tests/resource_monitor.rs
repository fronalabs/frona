#![cfg(unix)]
use frona_sandbox::{ResourceMonitor, Sandbox, SandboxFactory};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Monitor {
    events: Mutex<Vec<String>>,
    killed: AtomicBool,
    registered: tokio::sync::Notify,
}
impl ResourceMonitor for Monitor {
    fn register(&self, pid: u32, owner: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("register:{pid}:{owner}"));
        self.registered.notify_one();
    }
    fn is_resource_killed(&self, _: u32) -> bool {
        self.killed.load(Ordering::Relaxed)
    }
    fn unregister(&self, pid: u32) {
        self.events
            .lock()
            .unwrap()
            .push(format!("unregister:{pid}"));
        self.killed.store(false, Ordering::Relaxed);
    }
}
fn sandbox(dir: &std::path::Path, monitor: Arc<Monitor>) -> Sandbox {
    SandboxFactory::new(true)
        .with_resource_monitor(monitor)
        .get_sandbox(dir.to_path_buf(), "worker", false, vec![])
        .without_node()
        .without_venv()
}
fn assert_pair(monitor: &Monitor) {
    let events = monitor.events.lock().unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    let pid = events[0].split(':').nth(1).unwrap();
    assert_eq!(events[1], format!("unregister:{pid}"));
}

#[cfg(target_os = "linux")]
async fn assert_process_dead(pid: u32) {
    for _ in 0..100 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        if status.is_err() || status.unwrap().contains(") Z ") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process {pid} survived cleanup");
}

#[tokio::test]
async fn execute_registers_and_unregisters_once() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let output = sandbox(dir.path(), monitor.clone())
        .execute("true", &[], 2, None, None, None)
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_pair(&monitor);
}

#[tokio::test]
async fn spawn_failure_does_not_register() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    assert!(
        sandbox(dir.path(), monitor.clone())
            .execute("/nonexistent-program-frona", &[], 2, None, None, None)
            .await
            .is_err()
    );
    assert!(monitor.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn closed_stdout_keeps_monitor_and_cancellation_active_until_exit() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let cancel = CancellationToken::new();
    let run = sandbox.execute(
        "sh",
        &["-c", "exec 1>&- 2>&-; sleep 30"],
        3,
        None,
        None,
        Some(cancel.clone()),
    );
    let cancellation = async {
        monitor.registered.notified().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(monitor.events.lock().unwrap().len(), 1);
        monitor.killed.store(true, Ordering::Relaxed);
        cancel.cancel();
    };
    let (output, ()) = tokio::join!(run, cancellation);
    let output = output.unwrap();
    assert!(output.cancelled);
    assert!(output.resource_killed);
    assert_pair(&monitor);
}

#[tokio::test]
async fn dropping_execute_future_unregisters_and_stops_child() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let mut run = Box::pin(sandbox.execute("sleep", &["30"], 0, None, None, None));
    tokio::select! { _ = &mut run => panic!("unexpected exit"), _ = monitor.registered.notified() => {} }
    drop(run);
    assert_pair(&monitor);
    #[cfg(target_os = "linux")]
    {
        let pid = monitor.events.lock().unwrap()[0]
            .split(':')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert_process_dead(pid).await;
    }
}

#[tokio::test]
async fn spawned_child_wait_and_drop_unregister_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let mut child = sandbox
        .spawn(
            "true",
            &[],
            None,
            vec![],
            None,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
        )
        .await
        .unwrap();
    child.wait().await.unwrap();
    assert_pair(&monitor);
    assert!(child.id().is_none());
    child.try_wait().unwrap();
    drop(child);
    assert_pair(&monitor);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn timeout_kills_descendant_holding_pipes_after_leader_exit() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let output = sandbox
        .execute(
            "sh",
            &["-c", "sleep 30 & echo $!; exit 0"],
            1,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(output.timed_out);
    assert_pair(&monitor);
    let pid: u32 = output.stdout.trim().parse().unwrap();
    assert_process_dead(pid).await;
}

#[tokio::test]
async fn dropping_wait_with_output_kills_live_group_and_unregisters() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let mut child = sandbox
        .spawn(
            "sh",
            &["-c", "sleep 30 & echo $!; exit 0"],
            None,
            vec![],
            None,
            std::process::Stdio::piped(),
            std::process::Stdio::piped(),
        )
        .await
        .unwrap();
    use tokio::io::AsyncBufReadExt;
    let stdout = child.stdout.take().unwrap();
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let descendant: u32 = line.trim().parse().unwrap();
    child.stdout = Some(reader.into_inner());
    let result = tokio::time::timeout(Duration::from_millis(100), child.wait_with_output()).await;
    #[cfg(target_os = "linux")]
    assert_process_dead(descendant).await;
    #[cfg(not(target_os = "linux"))]
    let _ = descendant;
    assert!(result.is_err());
    assert_pair(&monitor);
}

#[tokio::test]
async fn resource_kill_status_survives_unregister() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    monitor.killed.store(true, Ordering::Relaxed);
    let output = sandbox(dir.path(), monitor.clone())
        .execute("true", &[], 2, None, None, None)
        .await
        .unwrap();
    assert!(output.resource_killed);
    assert_eq!(output.exit_code, None);
    assert!(!monitor.killed.load(Ordering::Relaxed));
    assert_pair(&monitor);
}

#[tokio::test]
async fn dropping_spawned_child_unregisters_once() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let child = sandbox(dir.path(), monitor.clone())
        .spawn(
            "sleep",
            &["30"],
            None,
            vec![],
            None,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
        )
        .await
        .unwrap();
    let pid = child.id().unwrap();
    drop(child);
    assert_pair(&monitor);
    #[cfg(target_os = "linux")]
    {
        for _ in 0..100 {
            let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if status.is_err() || status.unwrap().contains(") Z ") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("child {pid} survived drop");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[tokio::test]
async fn stderr_is_drained_while_child_runs() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let output = sandbox(dir.path(), monitor.clone())
        .execute(
            "sh",
            &["-c", "head -c 131072 /dev/zero >&2; echo done"],
            3,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout.trim(), "done");
    assert_eq!(output.stderr.len(), 131072);
    assert_pair(&monitor);
}

#[tokio::test]
async fn stalled_stdout_consumer_does_not_block_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let monitor = Arc::new(Monitor::default());
    let sandbox = sandbox(dir.path(), monitor.clone());
    let cancel = CancellationToken::new();
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let run = sandbox.execute(
        "sh",
        &["-c", "printf 'one\\ntwo\\nthree\\n'; sleep 30"],
        3,
        Some(sender),
        None,
        Some(cancel.clone()),
    );
    let cancel_after_registration = async {
        monitor.registered.notified().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (output, ()) = tokio::join!(run, cancel_after_registration);
    assert!(output.unwrap().cancelled);
    assert_pair(&monitor);
}
