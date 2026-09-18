//! Issue 022 process-mechanics prototype. Never linked into a production provider.
//! Synthetic events below are contract probes, not captured authenticated CLI runs.
#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, time::Duration};

use serde_json::Value;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

const LIMIT: usize = 4096;

#[test]
fn failed_gates_leave_both_managed_auth_methods_unavailable() {
    use frona::{
        core::{Handle, config::ModelProviderConfig},
        inference::{credential::store::CredentialMethod, provider::platform::ProviderPlatform},
    };
    for brand in ["anthropic", "google"] {
        let connection = ProviderPlatform::resolve(
            &Handle::try_new(brand).unwrap(),
            &ModelProviderConfig::default(),
        )
        .unwrap();
        assert!(
            connection
                .auth_methods
                .iter()
                .all(|method| method.method == CredentialMethod::ApiKey)
        );
    }
}

#[derive(Debug, PartialEq)]
enum Failure {
    Malformed,
    TooLarge,
    AgentOwnsTools,
    Exit,
    Timeout,
    Cancelled,
}

#[derive(Default)]
struct Frames {
    buffer: Vec<u8>,
    total: usize,
    events: Vec<Value>,
}

impl Frames {
    fn push(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        self.total += bytes.len();
        if self.total > LIMIT {
            return Err(Failure::TooLarge);
        }
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=end).collect();
            let event: Value = serde_json::from_slice(&line).map_err(|_| Failure::Malformed)?;
            let kind = event["type"].as_str().ok_or(Failure::Malformed)?;
            // A notification that a tool ran is not a return of control to Frona.
            // Claude wraps tool results inside user messages; Gemini emits them directly.
            if kind == "tool_result"
                || event["message"]["content"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["type"] == "tool_result"))
            {
                return Err(Failure::AgentOwnsTools);
            }
            if !matches!(kind, "assistant" | "message" | "tool_use" | "result") {
                return Err(Failure::Malformed);
            }
            self.events.push(event);
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<Value>, Failure> {
        if !self.buffer.is_empty()
            || self
                .events
                .last()
                .is_none_or(|event| event["type"] != "result")
        {
            return Err(Failure::Malformed);
        }
        Ok(self.events)
    }
}

struct Probe {
    directory: tempfile::TempDir,
    child: tokio::process::Child,
}

impl Probe {
    fn spawn(script: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("CLAUDE_CONFIG_DIR", directory.path())
            .env("GEMINI_CLI_HOME", directory.path())
            .env("GEMINI_FORCE_FILE_STORAGE", "true")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Never put raw CLI stderr in an administrator response.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self { directory, child }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().to_owned()
    }

    async fn run(
        mut self,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>, Failure> {
        let mut stdout = self.child.stdout.take().unwrap();
        let mut frames = Frames::default();
        let read = async {
            let mut chunk = [0; 37];
            loop {
                let size = stdout.read(&mut chunk).await.map_err(|_| Failure::Exit)?;
                if size == 0 {
                    return frames.finish();
                }
                frames.push(&chunk[..size])?;
            }
        };
        let result = tokio::select! {
            result = read => result,
            _ = tokio::time::sleep(timeout) => Err(Failure::Timeout),
            _ = cancel.cancelled() => Err(Failure::Cancelled),
        };
        if result.is_err() {
            let _ = self.child.start_kill();
        }
        // Also bound a process that closed stdout but never exits.
        let status = match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => status.map_err(|_| Failure::Exit),
            Err(_) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                Err(Failure::Timeout)
            }
        };
        // Explicitly reap before dropping the private directory. The fixtures do
        // not spawn descendants; real process-tree cleanup is an unmet go condition.
        let events = result?;
        if !status?.success() {
            return Err(Failure::Exit);
        }
        Ok(events)
    }
}

#[test]
fn split_frames_preserve_tool_ids_arguments_usage_and_structured_data() {
    let transcript = b"{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"external\",\"arguments\":{\"items\":[null,2]}}\n{\"type\":\"result\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3},\"structured_output\":{\"ok\":true}}\n";
    for split in 1..transcript.len() {
        let mut frames = Frames::default();
        frames.push(&transcript[..split]).unwrap();
        frames.push(&transcript[split..]).unwrap();
        let events = frames.finish().unwrap();
        assert_eq!(events[0]["id"], "c1");
        assert_eq!(
            events[0]["arguments"]["items"],
            serde_json::json!([null, 2])
        );
        assert_eq!(events[1]["usage"]["output_tokens"], 3);
        assert_eq!(events[1]["structured_output"]["ok"], true);
    }
}

#[tokio::test]
async fn fake_processes_reject_cli_owned_tool_results_for_each_candidate() {
    for script in [
        r#"printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"c1"}]}}' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"executed"}]}}'"#,
        r#"printf '%s\n' '{"type":"tool_use","tool_id":"c1"}' '{"type":"tool_result","tool_id":"c1","output":"executed"}'"#,
    ] {
        let probe = Probe::spawn(script);
        let directory = probe.path();
        assert_eq!(
            probe
                .run(Duration::from_secs(2), CancellationToken::new())
                .await,
            Err(Failure::AgentOwnsTools)
        );
        assert!(!directory.exists());
    }
}

#[tokio::test]
async fn fake_process_frames_and_private_credential_cleanup() {
    let probe = Probe::spawn(
        r#"umask 077
test "$GEMINI_FORCE_FILE_STORAGE" = true || exit 2
test "$CLAUDE_CONFIG_DIR" = "$PWD" || exit 2
test "$GEMINI_CLI_HOME" = "$PWD" || exit 2
test -z "$CLAUDE_CODE_OAUTH_TOKEN" || exit 2
printf '%s' 'synthetic-secret' > .credentials.json
printf '%s' '{"type":"mes'
printf '%s\n' 'sage","content":"hello"}' '{"type":"result","usage":{"input_tokens":2}}'"#,
    );
    let directory = probe.path();
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let events = probe
        .run(Duration::from_secs(2), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(events[0]["content"], "hello");
    assert!(!directory.exists());
}

#[tokio::test]
async fn malformed_truncated_oversized_and_failed_processes_are_sanitized() {
    for (script, expected) in [
        ("printf 'not-json\\n'", Failure::Malformed),
        (r#"printf '%s' '{"type":"result"}'"#, Failure::Malformed),
        (
            "i=0; while [ $i -lt 5000 ]; do printf x; i=$((i+1)); done",
            Failure::TooLarge,
        ),
        (
            r#"printf '%s\n' '{"type":"result"}'; printf 'synthetic-secret' >&2; exit 9"#,
            Failure::Exit,
        ),
    ] {
        let probe = Probe::spawn(script);
        let directory = probe.path();
        let result = probe
            .run(Duration::from_secs(2), CancellationToken::new())
            .await;
        assert!(!format!("{result:?}").contains("synthetic-secret"));
        assert_eq!(result, Err(expected));
        assert!(!directory.exists());
    }
}

#[tokio::test]
async fn timeout_and_cancellation_reap_the_child_and_clean_private_state() {
    for cancelled in [false, true] {
        let probe = Probe::spawn("umask 077; printf secret > .credentials.json; exec sleep 30");
        let directory = probe.path();
        let pid = probe.child.id().unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        if cancelled {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                trigger.cancel();
            });
        }
        let result = probe.run(Duration::from_millis(100), cancel).await;
        assert_eq!(
            result,
            Err(if cancelled {
                Failure::Cancelled
            } else {
                Failure::Timeout
            })
        );
        assert!(!directory.exists());
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}
