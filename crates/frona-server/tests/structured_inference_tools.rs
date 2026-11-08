//! The prepared provider group uses the shared SDK tool loop and typed submission.
mod helpers;
use frona_harness::{Harness, Model, ToolOutput, inference::ModelGroup, tool};
use helpers::{MockModelProvider, MockResponse, test_model_group, test_providers};
use serde_json::json;
use std::sync::Arc;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Query {
    pattern: String,
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Resolution {
    path: String,
}

#[tokio::test]
async fn structured_inference_with_tools_loops_then_submits() {
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "t1".into(),
            "grep".into(),
            json!({"pattern":"amazon"}),
        )]),
        MockResponse::ToolCalls(vec![(
            "t2".into(),
            "submit".into(),
            json!({"path":"organizations/amazon"}),
        )]),
    ]));
    let sdk = Harness::new(Model::from_group(ModelGroup {
        providers: test_providers("mock", mock.clone()),
        ..test_model_group()
    }));
    let mut searched = Vec::new();
    let lookup = tool("grep", "Find a page", |query: Query| {
        searched.push(query.pattern);
        async { Ok(ToolOutput::text("match: organizations/amazon")) }
    })
    .unwrap();
    let result = sdk
        .run("Proposed: Amazon Inc")
        .instructions("Resolve the entity to a canonical page path.")
        .tool(lookup)
        .model_responses(5)
        .structured::<Resolution>()
        .await
        .unwrap();
    assert_eq!(result.path, "organizations/amazon");
    assert_eq!(searched, ["amazon"]);
    assert_eq!(mock.calls(), 2);
    assert!(format!("{:?}", mock.last_history()).contains("match: organizations/amazon"));
}
