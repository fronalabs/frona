//! Annotate ordinary async functions and register them by name.
mod support;

use frona_harness::{Harness, ToolContext, ToolError, ToolOutput, tool};
use serde_json::json;

/// Add two integers.
#[tool]
async fn add(left: i64, right: i64) -> Result<i64, ToolError> {
    Ok(left + right)
}

/// Identify the current tool invocation.
#[tool]
async fn identify(prefix: String, execution: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
    // The harness supplies execution; only prefix appears in the model's schema.
    Ok(ToolOutput::text(format!(
        "{prefix}:{}",
        execution.call_id.as_deref().unwrap_or("direct call")
    )))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // These are genuine functions and remain callable outside the harness.
    assert_eq!(add(2, 3).await?, 5);
    let harness = Harness::new(support::model(vec![
        support::output(vec![
            support::call("sum", "add", json!({"left":2,"right":3})),
            support::call("trace", "identify", json!({"prefix":"operation"})),
        ]),
        support::text("The sum is 5"),
    ])?);
    let answer = harness
        .run("Add two and three, then identify this operation")
        .tool(add)
        .tool(identify)
        .await?;
    assert_eq!(answer, "The sum is 5");
    println!("{answer}");
    Ok(())
}
