use frona_harness::{
    Decision, Harness, Model, ModelRequest, ToolContext, ToolDecision, ToolRegistry, UsageReport,
    tool,
};
use frona_standalone_consumer::{Answer as Arguments, ApplicationClient};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The production builder prepares a client without network calls or identities.
    let _prepared = Model::builder("openai", "application-model")
        .api_key("fixture-key")
        .build()?;
    let application_state = String::from("application response");
    let model = Model::from_adapter(
        "application",
        "local",
        ApplicationClient(&application_state),
    )?;
    let output = model.complete(ModelRequest::new("hello")).await?;
    assert!(matches!(output.usage, UsageReport::Unavailable));
    assert_eq!(output.content.len(), 1);

    let harness = Harness::new(model);
    assert_eq!(
        harness.run("automatic standalone call").await?,
        application_state
    );
    let answer: Arguments = harness.run("typed standalone call").structured().await?;
    assert_eq!(answer.value, application_state);
    let mut inspections = 0;
    let accepted: String = harness
        .run("inspected standalone call")
        .structured::<Arguments>()
        .inspect(async |answer| {
            inspections += 1;
            tokio::task::yield_now().await;
            Ok::<_, ()>(Decision::accept(answer.value.to_uppercase()))
        })
        .await?;
    assert_eq!(accepted, application_state.to_uppercase());
    assert_eq!(inspections, 1);
    let mut invoked = false;
    assert_eq!(
        harness
            .run("hooked standalone call")
            .tool(tool("echo", "Echo a value", |args: Arguments| {
                invoked = true;
                async move { Ok(args.value) }
            })?)
            .before_tool(|call| async move {
                assert_eq!(call.function.name, "echo");
                Ok::<_, ()>(ToolDecision::Reject("Not needed for this fixture".into()))
            })
            .await?,
        application_state
    );
    assert!(!invoked);
    let mut session = harness.run("manual standalone call").manual();
    assert!(session.call_model().await?.actions.is_empty());
    assert_eq!(session.finish()?, application_state);

    let mut tools = ToolRegistry::default();
    tools.register(tool(
        "echo",
        "Echo an input",
        |args: Arguments| async move { Ok(json!({"echo": args.value})) },
    )?)?;
    let result = tools
        .get_mut("echo")
        .unwrap()
        .call(json!({"value": "hello"}), &ToolContext::default())
        .await?;
    assert!(!result.content.is_empty());
    Ok(())
}
