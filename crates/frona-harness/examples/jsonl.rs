//! Save application state beside a stopped dialogue; load first, then explicitly resume.
mod support;
use frona_harness::{Decision, DefaultBudget, Harness, Jsonl};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct Draft {
    title: String,
}
#[derive(Serialize, Deserialize)]
struct ReviewState {
    inspections: usize,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?; // Use an application-owned durable path in production.
    let path = directory.path().join("review.jsonl");
    let harness = Harness::new(support::model(vec![support::submission(
        json!({"title":"Candidate"}),
    )])?);
    let mut state = ReviewState { inspections: 0 };
    let outcome = harness
        .run("Suggest a title")
        .jsonl(Jsonl::create(&path)?)
        .structured::<Draft>()
        .inspect(async |draft| {
            state.inspections += 1;
            Ok::<_, String>(Decision::stop("application checkpoint boundary").retain(draft.title))
        })
        .await;
    let unfinished = match outcome {
        Ok(_) => return Err("the checkpoint example unexpectedly completed".into()),
        Err(unfinished) => unfinished,
    };
    let mut conversation = *unfinished.conversation;
    conversation.save_checkpoint("title-review", 1, &state)?;
    drop(conversation); // Release this writer before loading again.

    let loaded = Jsonl::open(&path)?; // Pure load: no tools, models, inspector, or writes.
    let mut restored: ReviewState = loaded
        .load_checkpoint("title-review", 1)?
        .ok_or("missing application checkpoint")?
        .payload;
    assert_eq!(restored.inspections, 1);
    // Restore model, tools (none here), schema, inspector state, and budget explicitly.
    let resumed_harness = Harness::new(support::model(vec![])?);
    let budget = DefaultBudget::default(); // Prior budget consumption is not restored.
    let accepted = resumed_harness
        .resume(loaded)
        .budget(&budget)
        .structured::<Draft>()
        .inspect(async |draft| {
            restored.inspections += 1;
            // A real application rechecks the candidate against its current domain state.
            Ok::<_, String>(if draft.title.trim().is_empty() {
                Decision::revise("Supply a nonempty title")
            } else {
                Decision::accept(draft.title.trim().to_owned())
            })
        })
        .await?;
    assert_eq!(accepted, "Candidate");
    assert_eq!(restored.inspections, 2); // Checkpointed retention did not imply acceptance.
    assert_eq!(budget.snapshot().model_attempts, 0); // Pending submission was inspected first.
    println!("Explicitly accepted after resume: {accepted}");
    Ok(())
}
