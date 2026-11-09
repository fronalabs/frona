//! Optional observation and a spawned owned operation. The final result is authoritative.
mod support;
use frona_harness::{Harness, Progress};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // All captured state is owned. Local borrowing examples need no spawn or Arc wrappers.
    let harness = Harness::new(support::model(vec![support::text("A complete answer")])?);
    let result = tokio::spawn(async move {
        let run = harness.run("Answer briefly");
        let report = run.report();
        let mut progress = run.subscribe(); // Subscription alone starts no work.
        let observe = async {
            while let Some(event) = progress.recv().await {
                match event {
                    Progress::Text { text, .. } => println!("Update: {text}"),
                    Progress::Missed(count) => println!("Missed {count} optional notifications"),
                    _ => {}
                }
            }
        };
        let (outcome, ()) = tokio::join!(async { run.await }, observe);
        (outcome, report.snapshot())
    })
    .await?;
    let (outcome, report) = result;
    assert_eq!(outcome?, "A complete answer");
    assert_eq!(report.provider_attempts(), 1);
    println!("Final outcome: {:?}", report.outcome);
    Ok(())
}
