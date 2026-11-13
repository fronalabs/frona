# frona-sandbox

Process sandbox execution with platform-selected isolation, workspace setup,
output capture, cancellation and optional caller-owned resource monitoring. Callers supply
paths, environment and opaque workload identifiers; no Frona server, identity,
policy engine or database is required.

```rust,no_run
use frona_sandbox::SandboxFactory;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let factory = SandboxFactory::new(false);
let sandbox = factory
    .get_sandbox("/tmp/build-job".into(), "build-job-42", false, vec![])
    .without_venv()
    .without_node()
    .with_extra_env_vars(vec![("BUILD_LABEL".into(), "nightly".into())]);

let result = sandbox
    .execute("sh", &["-c", "printf '%s\\n' \"$BUILD_LABEL\""], 10, None, None, None)
    .await?;
assert_eq!(result.exit_code, Some(0));
println!("{}", result.stdout);
# Ok(())
# }
```

## Platform selection and enforcement

`SandboxFactory::new(false)` chooses Syd on Linux when its availability probe
succeeds, otherwise Landlock. macOS uses the external `sandbox-exec` executable.
Other platforms select the noop driver. Passing `true` explicitly selects noop
on every platform. Inspect `driver_id()` and use `verify_sandbox` at application
startup as a basic probe. The probe is not a complete capability test and does
not establish every requested restriction. Noop execution is not an isolation test.

The backends preserve the existing implementation's behavior; they do **not**
provide equivalent restrictions:

- Linux Syd requires the `syd` executable. It implements the configured path
  masks and network destination rules. Hostnames are resolved to IP grants and
  cached; this is not hostname/SNI isolation, and shared IPs can host other names.
- Linux Landlock requires kernel support. The current driver grants workspace
  and additional filesystem paths; it uses Landlock network restrictions when
  network access is disabled. It does not implement destination allowlists,
  denied-path masks, blocked-network lists or per-port binding grants. Landlock
  network coverage is not a blanket restriction on every socket family/protocol.
- macOS requires `sandbox-exec`. Its profile supports filesystem grants and its
  existing network rules, but does not consume all Linux restriction fields,
  including denied-path and blocked-network lists.
- Noop imposes no filesystem or network isolation.

The workspace is implicitly readable and writable. Platform runtime paths and
selected temporary/device paths also have built-in grants. Additional write
paths grant read/write access. A true `network_access` with an empty destination
list means unrestricted network access, not deny-all. Enforce application policy
before constructing the sandbox; this crate does not interpret Cedar policies or
reject every backend capability mismatch.

Additional read/write paths, exact read-file grants (including hook grants), and
denied paths resolve relative to the host process's current directory, not the
sandbox workspace or command working directory. Before execution they are
canonicalized, preserving missing path tails, just like the application-side
checks. Use absolute paths to make the intended base explicit.

`is_readable` and `is_writable` perform application-side path checks using
canonicalization, including unresolved path tails. They are not atomic file-open
operations, cannot prevent symlink races, and can be stricter than a backend
that does not implement the configured denial fields.

## Execution and lifecycle

`execute` captures stdout/stderr and accepts optional line-streaming stdout,
stdin-channel and cancellation-token arguments. Its timeout is supplied explicitly
per execution; zero disables it. `with_default_timeout` stores a value for callers
to retrieve, rather than overriding the explicit execution timeout. Output has a
512 KiB default returned-output truncation limit; `execute_with_options` accepts
an `ExecutionOptions` value to override it. This is not a streaming memory
limit. Stdout is line-oriented and stderr is drained concurrently. The deadline
and cancellation stay active while waiting for process exit, draining output or
waiting for a stdout consumer. With no timeout, surviving descendants that retain
output pipes can keep an execution open.

Await `execute` to completion to receive output and termination status, or use its
cancellation token for cooperative supervision. Dropping its future initiates
process-group cleanup while the leader remains tracked and releases the optional
monitor registration and any execution lease. Normal
execution also registers and unregisters the child with that monitor. On Unix,
timeout/cancellation signals the process group; this is not containment of a
process that deliberately escapes that group.

`spawn` is asynchronous and returns a `TrackedChild` for callers that manage long-lived processes
and their stdio. Waiting or observing exit unregisters the child from its monitor.
Dropping a live tracked child initiates process-group cleanup and releases its
monitor registration and execution lease. A hook can retain credential guards
for the child; independently managed credentials remain the caller's responsibility. Process-group cleanup cannot contain descendants that deliberately
escape the group. Once the leader is reaped, the wrapper no longer signals its
former group; consumers must separately supervise any surviving descendants.

## Execution hooks

Attach one trusted `ExecutionHook` with `with_execution_hook`. Installing another
hook replaces the previous hook. Each `execute` or awaited `spawn` calls
`prepare` afresh before constructing the backend command. Path checks such as
`is_readable` do not call the hook. The context supplies the command, arguments,
workspace, working directory and opaque owner ID.

A hook returns an `ExecutionLease`: environment additions, single-file read
grants and any application-owned resources to retain for that execution. Hook
environment values override sandbox environment values; later values within the
lease win. File grants are added before backend command construction and remain
subject to that backend's denial behavior and limitations described above.
Hooks are trusted host code, not an authorization boundary.

```rust,no_run
use std::{io::Write, sync::Arc};
use frona_sandbox::{
    ExecutionContext, ExecutionHook, ExecutionLease, HookFuture, SandboxFactory,
};

struct PreparedInput;

impl ExecutionHook for PreparedInput {
    fn prepare<'a>(&'a self, _context: &'a ExecutionContext<'a>) -> HookFuture<'a> {
        Box::pin(async move {
            let mut input = tempfile::NamedTempFile::new()?;
            writeln!(input, "input for this execution")?;
            let path = input.path().to_owned();
            Ok(ExecutionLease::new()
                .env("INPUT_FILE", path.to_string_lossy().into_owned())
                .read_file(path)
                .retain(input))
        })
    }
}

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let sandbox = SandboxFactory::new(false)
    .get_sandbox("/tmp/build-job".into(), "job-42", false, vec![])
    .without_venv()
    .without_node()
    .with_execution_hook(Arc::new(PreparedInput));

let output = sandbox
    .execute("sh", &["-c", "cat \"$INPUT_FILE\""], 10, None, None, None)
    .await?;
// The temporary file has been removed by the retained value's Drop.
assert_eq!(output.exit_code, Some(0));
# Ok(())
# }
```

The tracked child retains the lease until it is reaped or dropped. Preparation
or spawn errors release acquired resources. Cancellation during `execute`
preparation drops that preparation future; a hook must use RAII for resources it
acquires before returning a lease. Dropping an awaited `spawn` future also drops
its preparation. The execution timeout covers the child runtime, not hook
preparation; hooks should bound their own external requests. Cleanup is synchronous
`Drop`, with no async cleanup callback. Resource monitoring is separate and can
be used with or without a hook.

## Resource accounting

Resource monitoring is optional. `SandboxFactory::new(false)` runs without a
monitor and does not impose CPU or memory quotas. Applications that need resource
accounting implement `ResourceMonitor` and attach it to the factory. The crate
notifies that monitor when a process starts and when normal `execute` supervision
finishes; it also queries whether the monitor killed a process so the result can
report `resource_killed`. Monitor callbacks must be thread-safe and nonblocking.
Once `is_resource_killed(pid)` returns true, the monitor must retain that status
until `unregister(pid)`, including after the process exits. The child wrapper
retains the final status after unregistering.

The monitor owns quota policy, CPU/memory sampling, process termination and its
own background-task lifecycle. The crate does not include a concrete system
monitor or depend on Frona's resource manager. Workload identifiers supplied to
`get_sandbox` are opaque strings: build-job IDs, worker IDs or any other
application attribution key.

A monitor may collect lifecycle events without enforcing quotas:

```rust,no_run
use std::sync::Arc;
use frona_sandbox::{ResourceMonitor, SandboxFactory};

struct LifecycleLog;

impl ResourceMonitor for LifecycleLog {
    fn register(&self, pid: u32, owner_id: &str) {
        eprintln!("started {pid} for {owner_id}");
    }

    fn is_resource_killed(&self, _pid: u32) -> bool {
        false // This monitor only records lifecycle events.
    }

    fn unregister(&self, pid: u32) {
        eprintln!("finished {pid}");
    }
}

let factory = SandboxFactory::new(false)
    .with_resource_monitor(Arc::new(LifecycleLog));
```

## Workspace runtime setup

Execution creates the workspace if needed. By default, setup attempts to create
a Python virtual environment using `python3 -m venv --system-site-packages`, and
a local Node prefix/package using `npm init -y`. These setup commands run on the
host before the sandboxed workload; provide trusted workspace paths. Missing
Python/npm setup is logged rather than always failing execution. Use
`without_venv()` and `without_node()` when the caller manages its own toolchain.
The executable being run, and any interpreter it uses, must already be installed.

## Standalone consumer verification

The isolated fixture resolves only this crate and its consumer dependencies:

```nu
cargo run --manifest-path crates/frona-sandbox/tests/fixtures/standalone/Cargo.toml
```

It checks command output, caller environment, workspace selection, per-execution
hook preparation and lease cleanup on normal completion and timeout.
It requires `sh` and `sleep`. An explicit `-- --disabled` runs only the execution
smoke checks without isolation; passing that run proves no backend enforcement.
The crate also includes platform-specific enforcement tests. Record unavailable
kernel/executable prerequisites when reporting test results; Linux results do not
establish macOS coverage.
