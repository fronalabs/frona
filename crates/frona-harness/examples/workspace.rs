//! Opt into the read/glob bundle for one directory; default runs have no application tools.
mod support;
use frona_harness::{Harness, ToolDecision, workspace_read_tools};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    std::fs::write(workspace.path().join("notes.txt"), "A note to summarize")?;
    let harness = Harness::new(support::model(vec![
        support::output(vec![support::call(
            "read-note",
            "read",
            json!({"path":"notes.txt"}),
        )]),
        support::text("The notes contain one example sentence"),
    ])?);
    let answer = harness
        .run("Summarize notes.txt")
        .tools(workspace_read_tools(workspace.path())?)
        .before_tool(|call| async move {
            Ok::<_, String>(if call.function.name == "read" {
                ToolDecision::Execute
            } else {
                ToolDecision::Reject("This operation already knows the note path".into())
            })
        })
        .disable_compaction() // Optional; overflow still fails explicitly.
        .await?;
    println!("{answer}");
    Ok(())
}
