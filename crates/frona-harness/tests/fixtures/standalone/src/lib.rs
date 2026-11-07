//! Ownership witnesses using the production crate-root model and tool APIs.

use async_trait::async_trait;
use frona_harness::{
    Capabilities, ModelAdapter, ModelOutput, ModelRequest, Tool, ToolContext, ToolOutput,
    ToolResult, UsageReport,
    inference::{InferenceError, provider::StreamToken},
};
use rig_core::completion::{AssistantContent, request::ToolDefinition};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A model adapter borrows application-owned configuration.
pub struct ApplicationClient<'a>(pub &'a str);

#[async_trait]
impl ModelAdapter for ApplicationClient<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        Ok(ModelOutput::new(
            vec![AssistantContent::text(self.0)],
            UsageReport::Unavailable,
        ))
    }
}

/// A tool lends mutable application state across an await.
pub struct LendingTool<'a> {
    definition: ToolDefinition,
    observations: &'a mut Vec<String>,
}

impl<'a> LendingTool<'a> {
    pub fn new(observations: &'a mut Vec<String>) -> Self {
        Self {
            definition: ToolDefinition {
                name: "observe".into(),
                description: "Record an observation".into(),
                parameters: json!({"type":"object"}),
            },
            observations,
        }
    }
}

#[async_trait]
impl Tool for LendingTool<'_> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&mut self, arguments: Value, _: &ToolContext<'_>) -> ToolResult {
        let observations = &mut self.observations;
        std::future::ready(()).await;
        observations.push(arguments.to_string());
        Ok(ToolOutput::text("recorded"))
    }
}

/// Borrowed models cannot outlive the state supplied to their adapter.
///
/// ```compile_fail,E0597
/// use frona_harness::{Model, ModelRequest};
/// use frona_standalone_consumer::ApplicationClient;
/// async fn invalid() {
///     let model;
///     {
///         let state = String::from("borrowed response");
///         model = Model::from_adapter("local", "fixture", ApplicationClient(&state)).unwrap();
///     }
///     let _ = model.complete(ModelRequest::new("hello")).await;
/// }
/// ```
///
/// A registered tool cannot outlive the application state it mutates.
///
/// ```compile_fail,E0597
/// use frona_harness::{ToolContext, ToolRegistry};
/// use frona_standalone_consumer::LendingTool;
/// async fn invalid() {
///     let mut tools = ToolRegistry::default();
///     {
///         let mut state = Vec::new();
///         tools.register(LendingTool::new(&mut state)).unwrap();
///     }
///     let _ = tools.get_mut("observe").unwrap()
///         .call(serde_json::json!({}), &ToolContext::default()).await;
/// }
/// ```
///
/// A model borrowing a caller's state cannot escape into a static task.
///
/// ```compile_fail,E0597
/// use frona_harness::{Model, ModelRequest};
/// use frona_standalone_consumer::ApplicationClient;
/// async fn invalid() {
///     let state = String::from("borrowed response");
///     let model = Model::from_adapter("local", "fixture", ApplicationClient(&state)).unwrap();
///     tokio::spawn(async move { model.complete(ModelRequest::new("hello")).await });
/// }
/// ```
pub mod ownership {}
