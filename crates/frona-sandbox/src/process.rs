use std::{
    io,
    process::{ExitStatus, Output},
    sync::Arc,
};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

/// Caller-owned accounting for sandbox process leaders. Implementations must be
/// thread-safe and nonblocking. Killing processes and maintaining quotas are the
/// monitor's responsibility; the sandbox only reports lifecycle and reads status.
pub trait ResourceMonitor: Send + Sync {
    fn register(&self, pid: u32, owner_id: &str);
    /// A true result must remain available until `unregister`, even after exit.
    fn is_resource_killed(&self, pid: u32) -> bool;
    fn unregister(&self, pid: u32);
}

/// Owns a sandbox child and its monitor registration. Waiting/reaping unregisters
/// exactly once. Dropping a live child kills its process group and unregisters;
/// callers cannot detach a process while silently discarding its accounting.
pub struct TrackedChild {
    child: Child,
    lease: Option<crate::ExecutionLease>,
    registration: Option<(Arc<dyn ResourceMonitor>, u32)>,
    resource_killed: bool,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl TrackedChild {
    pub(crate) fn new(
        mut child: Child,
        monitor: Option<Arc<dyn ResourceMonitor>>,
        owner: &str,
        lease: crate::ExecutionLease,
    ) -> Self {
        let registration = monitor.zip(child.id());
        if let Some((monitor, pid)) = &registration {
            monitor.register(*pid, owner);
        }
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            lease: Some(lease),
            registration,
            resource_killed: false,
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Reports the monitor's kill flag, retaining it after the child is reaped.
    pub fn is_resource_killed(&self) -> bool {
        self.resource_killed
            || self
                .registration
                .as_ref()
                .is_some_and(|(monitor, pid)| monitor.is_resource_killed(*pid))
    }

    fn finish_tracking(&mut self) {
        self.lease.take();
        if let Some((monitor, pid)) = self.registration.take() {
            self.resource_killed |= monitor.is_resource_killed(pid);
            monitor.unregister(pid);
        }
    }

    /// Wait for the process leader. After reaping, surviving descendants are
    /// caller-owned; use `wait_with_output` when inherited pipes must be drained.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.stdin.take();
        let status = self.child.wait().await;
        if status.is_ok() {
            self.finish_tracking();
        }
        status
    }

    /// Poll the process leader, ending tracking once it is reaped.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.finish_tracking();
        }
        Ok(status)
    }

    pub fn start_kill(&mut self) -> io::Result<()> {
        // Use the live Child ID, never a cached registration PID after reaping.
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        self.child.start_kill()
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await.map(|_| ())
    }

    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        use tokio::io::AsyncReadExt;
        self.stdin.take();
        let mut stdout = self.stdout.take();
        let mut stderr = self.stderr.take();
        let read_stdout = async {
            let mut bytes = Vec::new();
            if let Some(pipe) = &mut stdout {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok::<_, io::Error>(bytes)
        };
        let read_stderr = async {
            let mut bytes = Vec::new();
            if let Some(pipe) = &mut stderr {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok::<_, io::Error>(bytes)
        };
        let (stdout, stderr) = tokio::try_join!(read_stdout, read_stderr)?;
        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            let _ = self.start_kill();
        }
        self.finish_tracking();
    }
}
