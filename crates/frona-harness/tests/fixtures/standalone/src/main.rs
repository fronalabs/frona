use frona_harness::{Model, ModelRequest, ToolContext, ToolRegistry, UsageReport, tool};
use frona_standalone_consumer::ApplicationClient;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct Arguments {
    value: String,
}

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
