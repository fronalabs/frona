//! Choose individual actions and the next model while the SDK keeps history and pairing.
mod support;

use frona_harness::{ActionKind, Harness, tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct Lookup {
    key: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = support::model(vec![support::output(vec![
        support::call("one", "lookup", json!({"key":"public"})),
        support::call("two", "lookup", json!({"key":"private"})),
    ])])?;
    let second = support::model(vec![support::text("Used only the permitted record")])?;
    let mut looked_up = Vec::new();
    let lookup = tool("lookup", "Read an application record", |args: Lookup| {
        looked_up.push(args.key.clone());
        async move { Ok(json!({"key":args.key,"value":"example record"})) }
    })?;
    let harness = Harness::new(first);
    let mut session = harness
        .run("Answer using permitted records")
        .tool(lookup)
        .manual();
    let turn = session.call_model().await?;
    for action in turn.actions {
        let proposal = session.action(&action)?;
        assert_eq!(proposal.kind, ActionKind::Tool);
        if proposal.call.function.arguments["key"] == "public" {
            session.execute(action).await?;
        } else {
            session.reject(action, "That record is outside this operation's scope")?;
        }
    }
    // Overrides apply to this call only; raw native history may restrict model switching.
    session.call_model().model(&second).await?;
    let answer = session.finish()?;
    println!("{answer}");
    drop(session); // Releases the tool's borrow before reading application state.
    assert_eq!(looked_up, ["public"]);
    Ok(())
}
