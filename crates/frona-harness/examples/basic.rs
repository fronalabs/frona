//! Minimal text and typed runs. No session, store, events, identities, or tools to set up.
mod support;

use frona_harness::Harness;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct Summary {
    title: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = support::model(vec![
        support::text("Hello"),
        support::submission(json!({"title":"Notes"})),
    ])?;
    let harness = Harness::new(model);
    let answer = harness.run("Say hello").await?;
    let summary = harness
        .run("Give these notes a title")
        .structured::<Summary>()
        .await?;
    assert_eq!(answer, "Hello");
    assert_eq!(summary.title, "Notes");
    println!("{answer}: {}", summary.title);
    Ok(())
}
