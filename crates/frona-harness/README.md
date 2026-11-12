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

```no_run
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

## Annotated tools and context

Tools are ordinary async functions registered by function reference. The harness
recognizes `ToolContext` by type and excludes it from the model's argument schema.
Tools are installed explicitly; application authorization belongs in the tool or
in a `before_tool` callback.

```no_run
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

## Structured feedback loops

Add `serde` with its `derive` feature and `schemars` to your dependencies. The
inspector can be asynchronous and can return a different accepted result type.

```no_run
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

Only explicit acceptance completes refinement. A retained candidate in an
`Incomplete` outcome is unfinished work and needs application revalidation before
any domain commit.

## Custom loops and execution policy

Call `.manual()` on a configured run to obtain a session. Use `call_model()` to
request a turn, inspect its actions, and `execute()` or `reject()` each proposed
action. `finish()` returns the completed result. Automatic and manual execution
share conversation management, tool-call pairing, and budget admission.

Reuse a `DefaultBudget` across runs or implement `Budget`. Admission happens before
every model/tool call; already-admitted calls may finish after exhaustion.
Compaction is enabled by default and can be disabled with `disable_compaction()`;
context overflow checks remain active. `subscribe()` adds optional bounded progress.

## Features and recovery

| Feature | Default | Purpose |
|---|---|---|
| `jsonl` | Off | File-backed history and explicit resume |

Runs work in memory without storage. With `jsonl`, history can be loaded without
execution and continued through `Harness::resume`. The caller restores model/tool
dependencies and budget policy, coordinates writers, and makes interrupted tools
safe to repeat. Checkpoints do not restore budget usage or grant acceptance.

## Examples

The package includes `basic`, `tools`, `refinement`, `manual`, `budgets`, `workspace`,
`progress`, `rig`, and `jsonl` examples. From a source checkout:

```sh
cargo run -p frona-harness --example tools
cargo run -p frona-harness --example manual
cargo run -p frona-harness --example jsonl --features jsonl
```

The scripted examples run without provider credentials; `rig` demonstrates a live
provider client. See the [API reference](https://docs.rs/frona-harness).

## License

MIT. See `LICENSE`. This license covers this crate; dependencies have their own licenses.
