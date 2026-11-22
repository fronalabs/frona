# frona-harness

Embed model and tool execution in a Rust application. Start with a text run, add
structured output and feedback, or take control of individual model and tool calls.
The harness does not require a Frona server, database, User, Chat, or Agent record.

## Install

```toml
[dependencies]
frona-harness = "0.1"
rig-core = { version = "0.46", features = ["reqwest"] }
tokio = { version = "1", features = ["macros", "rt"] }
```

## Run a model

This example uses a Rig OpenAI client and reads `OPENAI_API_KEY` from the environment.
The model name is chosen by the application.

```rust no_run
use frona_harness::{Harness, Model};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let model = Model::from_rig(client.responses("gpt-5-mini"))?;
    let harness = Harness::new(model);
    let answer = harness.run("Explain Rust ownership in two sentences").await?;
    println!("{answer}");
    Ok(())
}
```

Use `ModelAdapter` for an application-owned model implementation. Reuse a `Harness`
across runs; each run configures its own instructions, tools, and execution policy.

A host factory can use `Harness::bind(user_id, chat_id, initialize)` to return a
single-use SDK harness. `chat_id` is optional. Calling `run(input)` consumes the
binding. Its async initializer receives the SDK run and input on first execution,
so dropping the harness or unpolled run performs no host writes. For manual
driving, explicitly await `run.start()` to initialize it.

Use `.on_start(|token| ...)` to register activity for the lifetime of a run. The
hook receives the configured cancellation token and returns a guard. The SDK
holds the guard through model calls, tool calls, and verification, then drops it
when execution ends or the running future is dropped. Creating a builder leaves
the hook pending. Opening a manual session starts it.

## Annotated tools and context

Tools are ordinary async functions registered by function reference. The harness
recognizes `ToolContext` by type and excludes it from the model's argument schema.
Tools are installed explicitly; application authorization belongs in the tool or
in a `before_tool` callback. Each `before_tool` or `tool_call_hook` registration
appends a hook. Before invocation, hooks run in registration order until one
rejects, stops, or fails. After the tool result commits, result hooks run in
registration order until one stops the run.

```rust no_run
use frona_harness::{Harness, ToolContext, ToolError, tool};

/// Add two integers.
#[tool]
async fn add(left: i64, right: i64) -> Result<i64, ToolError> {
    Ok(left + right)
}

/// Return this invocation's identifier.
#[tool]
async fn invocation(ctx: ToolContext<'_>) -> Result<String, ToolError> {
    Ok(ctx.call_id.clone().unwrap_or_default())
}

# async fn example(harness: &Harness<'_>) -> Result<(), Box<dyn std::error::Error>> {
let answer = harness.run("Add 2 and 3").tool(add).tool(invocation).await?;
# Ok(())
# }
```

For application state, closure tools can borrow local values. See the `tools` and
`manual` examples. Spawning execution requires an owned, `Send`, `'static` future.

## Concurrent tool execution

Automatic runs execute sibling tool calls concurrently, up to the available CPU
count clamped to 2–8. Override the limit with
`.max_concurrent_tool_calls(NonZeroUsize::new(4).unwrap())`; use `1` for sequential
execution. Manual loops can use `session.execute_tools(&actions).await`.

Distinct tool instances overlap. Repeated calls to the same registered mutable
`Tool` stay serial, preserving borrowed application state. Tools finish in any
order; the next model request retains proposal order. Cancellation and terminal
failures leave interrupted calls unfinished. Budget refusal lets already admitted
calls finish, without starting queued calls.

The server uses separate invocation instances, allowing repeated calls to the
same server tool to overlap. Configure `inference.max_concurrent_tool_calls` in
server settings or YAML; zero is invalid. The limit applies per response batch,
not globally across chats or nested runs. Tools sharing external mutable resources
must coordinate access, or the application can select sequential execution.

## Structured feedback loops

Add `serde` with its `derive` feature and `schemars` to your dependencies. The
inspector can be asynchronous and can return a different accepted result type.

```rust no_run
use frona_harness::{Decision, Harness};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Draft { title: String }

# async fn example(harness: &Harness<'_>) -> Result<(), Box<dyn std::error::Error>> {
let title = harness.run("Give this article a title")
    .structured::<Draft>()
    .inspect(async |draft| {
        Ok::<_, String>(if draft.title.trim().is_empty() {
            Decision::revise("Supply a nonempty title")
        } else {
            Decision::accept(draft.title)
        })
    })
    .await?;
# Ok(())
# }
```

Use `.verify(check)` when the caller wants the submitted value unchanged. The
check borrows the submission. Return `Ok(Ok(()))` to accept it, `Ok(Err(feedback))`
to request a revision, or an outer `Err` if verification itself failed.

Only explicit acceptance completes refinement. A retained candidate in an
`Incomplete` outcome is unfinished work and needs application revalidation before
any domain commit.

## Custom loops and execution policy

Call `.manual()` on a configured run to obtain a session. Use `call_model()` to
request a turn, inspect its actions, and `execute()` or `reject()` each proposed
action. Await state-changing operations, including `reject()`, `accept()`, and
`finish()`: successful completion includes the required history write. Automatic and manual execution
share conversation management, tool-call pairing, and budget admission.

Use `.budget_limits(limits)` for a fresh per-run budget. Reuse a `DefaultBudget` across runs or implement `Budget` for a custom policy. Admission happens before
every model/tool call; already-admitted calls may finish after exhaustion.
Compaction is enabled by default and can be disabled with `disable_compaction()`;
context overflow checks remain active. `subscribe()` adds optional bounded progress.

## Features and recovery

| Feature | Default | Purpose |
|---|---|---|
| `jsonl` | Off | File-backed history and explicit resume |

Runs work in memory by default. Attach an owned `HistoryWriter` with
`.history_writer(writer)` to require asynchronous persistence. Database adapters
and the JSONL writer share the same commit ordering and failure tracking; no
separate flush is required. A failed or dropped commit retains an uncertain
pending record and blocks further transitions until explicit reconciliation.

Use `HistoryReader::read_messages().await` for the model-facing messages from a
JSONL file or a database-backed chat. The JSONL reader opens and replays the file
when read. The chat reader projects stored messages and tool calls through its
conversation builder. Use `Jsonl::open(path).await` for file-specific recovery
state such as checkpoints and pending actions.

File creation is synchronous. Replay and durable commits are awaited. Dropping a commit does not guarantee that its underlying
file or database operation stopped. Coordinate storage before resuming it.

With `jsonl`, history can be loaded without
execution and continued through `Harness::resume`. The caller restores model/tool
dependencies and budget policy, coordinates writers, and makes interrupted tools
safe to repeat. Checkpoints do not restore budget usage or grant acceptance.

## Examples

The package includes [basic][example-basic], [tools][example-tools], [refinement][example-refinement], [manual][example-manual], [budgets][example-budgets], [workspace][example-workspace],
[progress][example-progress], [rig][example-rig], and [jsonl][example-jsonl] examples.

From a source checkout:

```sh
cargo run -p frona-harness --example tools
cargo run -p frona-harness --example manual
cargo run -p frona-harness --example jsonl --features jsonl
```

The scripted examples run without provider credentials; `rig` demonstrates a live
provider client. See the API reference.

## License

MIT. See `LICENSE`. This license covers this crate; dependencies have their own licenses.

[example-basic]: examples/basic.rs
[example-budgets]: examples/budgets.rs
[example-jsonl]: examples/jsonl.rs
[example-manual]: examples/manual.rs
[example-progress]: examples/progress.rs
[example-refinement]: examples/refinement.rs
[example-rig]: examples/rig.rs
[example-tools]: examples/tools.rs
[example-workspace]: examples/workspace.rs
