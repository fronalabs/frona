//! Borrow mutable inspector state; accept a different type or return unfinished retention.
mod support;

use frona_harness::{BudgetLimits, Decision, DefaultBudget, Harness};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct Draft {
    title: String,
}
struct PublishedTitle(String); // Application result need not implement Serialize or JsonSchema.

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::new(support::model(vec![
        support::submission(json!({"title":""})),
        support::submission(json!({"title":"  A useful title  "})),
        support::submission(json!({"title":"A candidate needing review"})),
    ])?);
    let mut inspections = 0;
    let title = harness
        .run("Name the article")
        .structured::<Draft>()
        .inspect(async |draft| {
            inspections += 1;
            tokio::task::yield_now().await; // A real validator may query application state.
            let normalized = draft.title.trim();
            Ok::<_, String>(if normalized.is_empty() {
                Decision::revise("Supply a nonempty title")
            } else {
                Decision::accept(PublishedTitle(normalized.to_owned()))
            })
        })
        .await?;
    assert_eq!(inspections, 2);
    assert_eq!(title.0, "A useful title");

    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(1),
        ..Default::default()
    });
    let outcome = harness
        .run("Find a better title")
        .budget(&budget)
        .structured::<Draft>()
        .inspect(async |draft| {
            Ok::<_, String>(
                Decision::revise("Check this against the latest article")
                    .retain(PublishedTitle(draft.title)),
            )
        })
        .await;
    let unfinished = match outcome {
        Ok(_) => return Err("the bounded example unexpectedly completed".into()),
        Err(unfinished) => unfinished,
    };
    assert!(unfinished.retained.is_some());
    println!(
        "Unfinished: {}; retained candidate needs application revalidation",
        unfinished.reason
    );
    // The application can inspect/revalidate retained progress. It is not an accepted result.
    // An outer workflow decides whether to retry or commit an independently validated subset.
    Ok(())
}
