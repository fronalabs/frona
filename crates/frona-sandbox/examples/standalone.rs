//! A consumer with no application services, identities, policy engine or database.
use frona_sandbox::{ExecutionContext, ExecutionHook, ExecutionLease, HookFuture, SandboxFactory};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct InputHook {
    created_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl ExecutionHook for InputHook {
    fn prepare<'a>(&'a self, _context: &'a ExecutionContext<'a>) -> HookFuture<'a> {
        Box::pin(async move {
            let mut input = tempfile::NamedTempFile::new()?;
            writeln!(input, "prepared-input")?;
            let path = input.path().to_owned();
            self.created_paths.lock().unwrap().push(path.clone());
            Ok(ExecutionLease::new()
                .env("PREPARED_INPUT", path.to_string_lossy().into_owned())
                .read_file(path)
                .retain(input))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Disabled execution is an explicit smoke-test mode, never an isolation test.
    let disabled = std::env::args().skip(1).any(|arg| arg == "--disabled");
    let workspace = tempfile::tempdir()?;
    let factory = SandboxFactory::new(disabled);
    eprintln!("backend: {} (disabled={disabled})", factory.driver_id());

    let created_paths = Arc::new(Mutex::new(Vec::new()));
    let sandbox = factory
        .get_sandbox(workspace.path().to_owned(), "build-job-42", false, vec![])
        .without_venv()
        .without_node()
        .with_extra_env_vars(vec![("BUILD_LABEL".into(), "standalone".into())])
        .with_execution_hook(Arc::new(InputHook {
            created_paths: created_paths.clone(),
        }));

    let result = sandbox
        .execute(
            "sh",
            &[
                "-c",
                "printf '%s\\n' \"$BUILD_LABEL\"; pwd; cat \"$PREPARED_INPUT\"",
            ],
            5,
            None,
            None,
            None,
        )
        .await?;
    assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
    let mut lines = result.stdout.lines();
    assert_eq!(lines.next(), Some("standalone"));
    let cwd = std::path::PathBuf::from(lines.next().expect("command printed cwd"));
    assert_eq!(cwd.canonicalize()?, workspace.path().canonicalize()?);

    assert_eq!(lines.next(), Some("prepared-input"));
    assert_eq!(created_paths.lock().unwrap().len(), 1);
    assert!(
        created_paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| !path.exists())
    );

    let result = sandbox
        .execute("sh", &["-c", "exec sleep 30"], 1, None, None, None)
        .await?;
    assert!(result.timed_out, "expected timeout, got {result:?}");
    assert_eq!(result.exit_code, None);
    assert_eq!(created_paths.lock().unwrap().len(), 2);
    assert!(
        created_paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| !path.exists())
    );
    println!("command, environment, workspace, hook lease and timeout checks passed");
    Ok(())
}
