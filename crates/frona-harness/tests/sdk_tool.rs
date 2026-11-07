use std::{
    future::ready,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use frona_harness::{
    ExecutionStop, Tool, ToolContent, ToolContext, ToolError, ToolOutput, ToolRegistrationError,
    ToolRegistry, ToolResult, tool, tool_with_context,
};
use rig_core::completion::{
    message::{DocumentSourceKind, Image, ImageMediaType},
    request::ToolDefinition,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[schemars(range(min = 1))]
    count: usize,
}

#[tokio::test]
async fn typed_tool_borrows_client_validates_arguments_and_serializes_output() {
    let calls = AtomicUsize::new(0);
    {
        let borrowed = &calls;
        let typed = tool(
            "count",
            "Count items",
            move |arguments: Arguments| async move {
                ready(()).await;
                borrowed.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"count": arguments.count}))
            },
        )
        .unwrap();
        let mut tools = ToolRegistry::default();
        assert!(tools.is_empty());
        tools.register(typed).unwrap();
        let definition = tools.definitions().next().unwrap();
        assert_eq!(definition.parameters["properties"]["count"]["minimum"], 1);
        let context = ToolContext::default();
        let registered = tools.get_mut("count").unwrap();
        for invalid in [
            json!({}),
            json!({"count": "bad"}),
            json!({"count": 0}),
            json!({"count": 1,"extra": true}),
        ] {
            assert!(matches!(
                registered.call(invalid, &context).await,
                Err(ToolError::Failed(_))
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let output = registered
            .call(json!({"count": 2}), &context)
            .await
            .unwrap();
        assert_eq!(
            output.content,
            vec![ToolContent::Json {
                value: json!({"count":2})
            }]
        );
    }
    assert_eq!(calls.into_inner(), 1);
}

struct MutableTool<'a> {
    definition: ToolDefinition,
    observations: &'a mut Vec<String>,
}

#[async_trait]
impl Tool for MutableTool<'_> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&mut self, arguments: Value, context: &ToolContext<'_>) -> ToolResult {
        let state = &mut self.observations;
        ready(()).await;
        state.push(format!(
            "{}:{arguments}",
            context.call_id.as_deref().unwrap_or("local")
        ));
        Ok(ToolOutput::text("recorded"))
    }
}

#[tokio::test]
async fn lending_tool_can_mutate_borrowed_state_across_await() {
    let mut observations = Vec::new();
    {
        let mut tools = ToolRegistry::default();
        tools
            .register(MutableTool {
                definition: ToolDefinition {
                    name: "observe".into(),
                    description: "Observe".into(),
                    parameters: json!({"type":"object"}),
                },
                observations: &mut observations,
            })
            .unwrap();
        tools
            .get_mut("observe")
            .unwrap()
            .call(json!({}), &ToolContext::default().with_call_id("call-1"))
            .await
            .unwrap();
    }
    assert_eq!(observations, ["call-1:{}"]);
    observations.push("application still owns state".into());
}

#[tokio::test]
async fn rich_content_and_context_are_not_flattened_or_discarded() {
    let image = Image {
        data: DocumentSourceKind::Raw(vec![1, 2, 3]),
        media_type: Some(ImageMediaType::PNG),
        detail: None,
        additional_params: None,
    };
    let expected = ToolOutput::content(vec![
        ToolContent::Text("caption".into()),
        ToolContent::Image(image),
        ToolContent::Json {
            value: json!({"result":true}),
        },
    ])
    .with_metadata(json!({"source":"application","signature":"opaque"}));
    let output = expected.clone();
    let mut typed = tool_with_context(
        "inspect",
        "Inspect",
        move |_: Arguments, context: ToolContext<'_>| {
            let output = output.clone();
            Box::pin(async move {
                assert_eq!(context.call_id.as_deref(), Some("opaque-call"));
                assert!(context.cancellation.is_cancelled());
                Ok(output)
            })
        },
    )
    .unwrap();
    let context = ToolContext::default().with_call_id("opaque-call");
    context.cancellation.cancel();
    // This primitive passes context; admission/cancellation policy is HSDK-10.
    assert_eq!(
        typed.call(json!({"count":1}), &context).await.unwrap(),
        expected
    );
}

#[tokio::test]
async fn typed_control_stops_are_not_converted_to_ordinary_failure() {
    let mut stopped = tool("stop", "Stop", |_: Arguments| async {
        Err::<Value, _>(ExecutionStop::BudgetExhausted("shared tokens".into()).into())
    })
    .unwrap();
    assert!(
        matches!(stopped.call(json!({"count":1}), &ToolContext::default()).await,
        Err(ToolError::Stop(ExecutionStop::BudgetExhausted(reason))) if reason == frona_harness::BudgetRefusal::Denied("shared tokens".into()))
    );
    let mut failed = tool("fail", "Fail", |_: Arguments| async {
        Err::<Value, _>(ToolError::failed("cancelled is ordinary error text"))
    })
    .unwrap();
    assert!(matches!(
        failed
            .call(json!({"count":1}), &ToolContext::default())
            .await,
        Err(ToolError::Failed(_))
    ));
    let boxed: frona_harness::ToolFailure = Box::new(ExecutionStop::Cancelled("explicit".into()));
    assert!(
        matches!(ToolError::failed(boxed), ToolError::Stop(ExecutionStop::Cancelled(reason)) if reason == "explicit")
    );
    let nested = ToolError::from(ExecutionStop::BudgetExhausted("nested".into()));
    assert!(
        matches!(ToolError::failed(nested), ToolError::Stop(ExecutionStop::BudgetExhausted(reason)) if reason == frona_harness::BudgetRefusal::Denied("nested".into()))
    );
}

fn simple_tool(name: &str, response: usize) -> impl Tool + use<> {
    tool(
        name,
        "simple",
        move |_: Arguments| async move { Ok(response) },
    )
    .unwrap()
}

#[tokio::test]
async fn registration_rejects_collisions_without_replacing_existing_tools() {
    let mut state = Vec::new();
    let mut tools = ToolRegistry::default();
    tools.register(simple_tool("lookup", 1)).unwrap();
    assert!(
        matches!(tools.register(simple_tool("lookup", 2)), Err(ToolRegistrationError::Duplicate(name)) if name == "lookup")
    );
    assert!(matches!(
        tool("submit", "reserved", |_: Arguments| async { Ok(1) }),
        Err(ToolRegistrationError::ReservedName(_))
    ));
    let reserved = MutableTool {
        definition: ToolDefinition {
            name: "submit".into(),
            description: String::new(),
            parameters: json!({}),
        },
        observations: &mut state,
    };
    assert!(matches!(
        tools.register(reserved),
        Err(ToolRegistrationError::ReservedName(_))
    ));
    assert_eq!(tools.definitions().count(), 1);
    let result = tools
        .get_mut("lookup")
        .unwrap()
        .call(json!({"count":1}), &ToolContext::default())
        .await
        .unwrap();
    assert_eq!(result.content[0].as_json(), Some(&json!(1)));
}

#[tokio::test]
async fn owned_tool_and_output_can_run_in_an_application_spawned_task() {
    let output = tokio::spawn(async move {
        let mut owned = tool(
            "owned",
            "Owned",
            |args: Arguments| async move { Ok(args.count) },
        )
        .unwrap();
        owned
            .call(json!({"count":3}), &ToolContext::default())
            .await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output.content[0].as_json(), Some(&json!(3)));
}
