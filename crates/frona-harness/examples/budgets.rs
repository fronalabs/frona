//! Reuse one allowance across outer runs and nested model calls inside a tool.
mod support;

use frona_harness::{
    BudgetLimits, DefaultBudget, ExecutionStop, Harness, ManualError, ModelRequest, ToolContext,
    ToolError, ToolOutput, tool,
};
use serde_json::json;

/// Research a question under the caller's shared execution budget.
#[tool]
async fn research(question: String, context: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
    // This fixture creates a local fake client. An application may call its own
    // service here; use tool_with_context for a closure capturing a prepared client.
    let model = support::model(vec![support::text("A researched fact")])?;
    let answer = context
        .complete(&model, ModelRequest::new(question))
        .await?;
    let text: String = answer
        .content
        .into_iter()
        .filter_map(|content| {
            if let rig_core::completion::AssistantContent::Text(text) = content {
                Some(text.text)
            } else {
                None
            }
        })
        .collect();
    Ok(ToolOutput::text(text))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(3),
        ..Default::default()
    });
    let harness = Harness::new(support::model(vec![
        support::output(vec![support::call(
            "research",
            "research",
            json!({"question":"Research the question"}),
        )]),
        support::text("Answer based on the researched fact"),
    ])?);
    let answer = harness
        .run("Investigate")
        .budget(budget.clone())
        .tool(research)
        .await?;
    assert_eq!(budget.snapshot().model_attempts, 3); // Outer + nested + outer.
    assert_eq!(budget.snapshot().tool_calls, 1);
    println!("{answer}");

    let next = Harness::new(support::model(vec![])?)
        .run("Another loop")
        .budget(&budget)
        .await;
    let stopped = next.expect_err("the shared allowance is exhausted");
    assert!(matches!(
        stopped.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(budget.snapshot().model_attempts, 3); // The refused call consumed nothing.
    Ok(())
}
