//! Unit-level exercise of `structured_inference_with_tools`: the model calls a
//! tool for one turn, its result is fed back, then the model `submit`s the
//! structured verdict - the loop must execute the tool and return the parsed `T`.

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

use frona::core::error::AppError;
use frona::tool::registry::AgentToolRegistry;
use frona::tool::{AgentTool, InferenceContext, ToolDefinition, ToolOutput};

use helpers::{
    MockModelProvider, MockResponse, mock_context, test_model_group, test_providers,
    test_usage_ctx, test_usage_service,
};

/// A trivial read-only "tool" the resolver would use to navigate the vault.
struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        Ok(ToolOutput::text("match: organizations/amazon"))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Resolution {
    path: String,
}

#[tokio::test]
async fn structured_inference_with_tools_loops_then_submits() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();

    // Turn 1: model calls `grep`. Turn 2: model calls `submit` with the verdict.
    let mock = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "t1".into(),
            "grep".into(),
            json!({"pattern": "amazon"}),
        )]),
        MockResponse::ToolCalls(vec![(
            "t2".into(),
            "submit".into(),
            json!({"path": "organizations/amazon"}),
        )]),
    ]));
    let registry = test_providers("mock", mock.clone());

    let mut tools: HashMap<String, Arc<dyn AgentTool>> = HashMap::new();
    tools.insert("grep".into(), Arc::new(EchoTool));
    let tool_registry = AgentToolRegistry::new(tools, HashMap::new(), vec![], false);

    let ctx = mock_context();
    let usage = test_usage_service(&db);
    let usage_ctx = test_usage_ctx();

    let result: Resolution = frona::inference::structured_inference_with_tools(
        &frona::inference::ModelGroup {
            providers: registry.clone(),
            ..(test_model_group()).clone()
        },
        "Resolve the entity to a canonical page path.",
        vec![rig_core::completion::Message::user("Proposed: Amazon Inc")],
        &tool_registry,
        &ctx,
        &usage,
        &usage_ctx,
        5,
    )
    .await
    .expect("resolver submits a verdict");

    assert_eq!(
        result.path, "organizations/amazon",
        "returns the submitted structured path"
    );
    assert_eq!(mock.calls(), 2, "looped: one tool turn + one submit turn");
}
