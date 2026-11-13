//! Per-invocation ephemeral token files. A short-lived JWT is written to a
//! uniquely-named file outside the sandbox workspace; the sandbox is granted
//! read access to only that file. The file is unlinked when the
//! [`EphemeralTokenGuard`] drops.

use std::path::{Path, PathBuf};

use super::User;
use super::token::models::TokenType;
use super::token::service::{CreateTokenRequest, TokenService};
use crate::core::Principal;
use crate::core::error::AppError;
use crate::credential::keypair::service::KeyPairService;

pub struct EphemeralTokenGuard {
    path: PathBuf,
}

impl EphemeralTokenGuard {
    pub async fn issue(
        token_service: &TokenService,
        keypair_service: &KeyPairService,
        user: &User,
        principal: Principal,
        ttl_secs: u64,
        root: &Path,
    ) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(root).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to create runtime tokens dir {}: {e}",
                root.display()
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await;
        }

        // Sandbox drivers (syd, landlock) need absolute paths in allowlists.
        let root = tokio::fs::canonicalize(root).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to canonicalize runtime tokens dir {}: {e}",
                root.display()
            ))
        })?;

        let created = token_service
            .create_token(
                keypair_service,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Ephemeral,
                    principal,
                    ttl_secs,
                    name: String::new(),
                    scopes: Vec::new(),
                    refresh_pair_id: None,
                    extensions: None,
                },
            )
            .await?;

        let path = root.join(&created.token_id);
        write_atomic_0600(&path, created.jwt.as_bytes()).await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralTokenGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sweeps stale token files left after a prior crash.
pub fn prepare_runtime_dir(users_root: &Path) {
    let Ok(entries) = std::fs::read_dir(users_root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let tokens_dir = entry.path().join("tokens");
        cleanup_stale(&tokens_dir);
    }
}

fn cleanup_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// The blocking operation owns both its temporary file and the returned guard.
// If the awaiting preparation future is cancelled, Tokio drops the detached
// task's output, which removes the final token once the write finishes.
async fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<EphemeralTokenGuard, AppError> {
    let path = path.to_path_buf();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || write_atomic_0600_blocking(&path, &bytes))
        .await
        .map_err(|e| AppError::Internal(format!("Token writer failed: {e}")))?
}

fn write_atomic_0600_blocking(path: &Path, bytes: &[u8]) -> Result<EphemeralTokenGuard, AppError> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        AppError::Internal(format!("Token path has no parent: {}", path.display()))
    })?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        AppError::Internal(format!("Failed to create ephemeral token tempfile: {e}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| {
                AppError::Internal(format!("Failed to restrict token permissions: {e}"))
            })?;
    }
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .map_err(|e| AppError::Internal(format!("Failed to write ephemeral token: {e}")))?;
    file.persist_noclobber(path)
        .map_err(|e| AppError::Internal(format!("Failed to persist ephemeral token: {e}")))?;
    Ok(EphemeralTokenGuard {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_stale_is_idempotent_on_missing_dir() {
        let missing = std::env::temp_dir().join("nonexistent-frona-ephemeral-xxx");
        let _ = std::fs::remove_dir_all(&missing);
        cleanup_stale(&missing);
    }

    #[tokio::test]
    async fn write_atomic_creates_0600_file() {
        let dir = std::env::temp_dir().join(format!("frona-eph-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("tok");
        let _guard = write_atomic_0600(&path, b"jwt-content").await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"jwt-content");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tokio::fs::metadata(&path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "token file must be owner-only");
        }

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn abandoned_token_write_cleans_up_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let worker_path = path.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let guard = write_atomic_0600_blocking(&worker_path, b"secret").unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            guard
        });
        ready_rx.await.unwrap();
        assert!(path.exists());
        drop(worker);
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn failed_token_write_removes_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing");
        std::fs::write(&path, b"original").unwrap();
        assert!(write_atomic_0600(&path, b"replacement").await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cleanup_stale_removes_files() {
        let dir = std::env::temp_dir().join(format!("frona-eph-cleanup-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a"), b"x").await.unwrap();
        tokio::fs::write(dir.join("b"), b"y").await.unwrap();

        cleanup_stale(&dir);

        let remaining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(remaining.is_empty(), "stale files should be removed");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}

#[cfg(all(test, unix))]
mod sandbox_lifetime_tests {
    use super::*;
    use crate::tool::sandbox::resource_monitor::SystemResourceManager;
    use crate::tool::sandbox::{Sandbox, SandboxFactory};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct TokenHook {
        path: PathBuf,
    }

    impl frona_sandbox::ExecutionHook for TokenHook {
        fn prepare<'a>(
            &'a self,
            _: &'a frona_sandbox::ExecutionContext<'a>,
        ) -> frona_sandbox::HookFuture<'a> {
            Box::pin(async move {
                let guard = write_atomic_0600(&self.path, b"test-token").await?;
                Ok(frona_sandbox::ExecutionLease::new()
                    .env("TOKEN_PATH", self.path.to_string_lossy())
                    .read_file(&self.path)
                    .retain(guard))
            })
        }
    }

    fn sandbox(root: &Path, token: &Path) -> Sandbox {
        SandboxFactory::new(true)
            .with_resource_monitor(Arc::new(SystemResourceManager::new(0.0, 0.0, 0.0, 0.0)))
            .get_sandbox(root.to_path_buf(), "test", false, vec![])
            .without_venv()
            .without_node()
            .with_execution_hook(Arc::new(TokenHook {
                path: token.to_path_buf(),
            }))
    }

    #[tokio::test]
    async fn sandbox_retains_token_through_execution_and_builder_then_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let sandbox =
            sandbox(dir.path(), &token).with_read_files(vec![token.to_string_lossy().into_owned()]);
        let output = sandbox
            .execute("sh", &["-c", "cat \"$TOKEN_PATH\""], 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(output.stdout.trim(), "test-token");
        assert!(!token.exists());
        drop(sandbox);
        assert!(!token.exists());
    }

    #[tokio::test]
    async fn sandbox_cleans_token_after_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let sandbox = sandbox(dir.path(), &token);
        assert!(
            sandbox
                .execute(
                    "/nonexistent-frona-sandbox-program",
                    &[],
                    5,
                    None,
                    None,
                    None
                )
                .await
                .is_err()
        );
        assert!(!token.exists());
        drop(sandbox);
        assert!(!token.exists());
    }

    #[tokio::test]
    async fn sandbox_retains_token_until_cancelled_execution_returns() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let sandbox = sandbox(dir.path(), &token);
        let cancel = CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let run = sandbox.execute(
            "sh",
            &["-c", "cat \"$TOKEN_PATH\"; echo; sleep 30"],
            10,
            Some(tx),
            None,
            Some(cancel.clone()),
        );
        let cancel_after_read = async {
            assert_eq!(rx.recv().await.as_deref(), Some("test-token"));
            assert!(token.exists());
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_after_read);
        assert!(result.unwrap().cancelled);
        drop(sandbox);
        assert!(!token.exists());
    }
}
