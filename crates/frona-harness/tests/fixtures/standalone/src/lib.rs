//! Ownership witnesses using the production crate-root model and tool APIs.

use async_trait::async_trait;
use frona_harness::{
    Capabilities, Decision, Harness, Model, ModelAdapter, ModelOutput, ModelRequest, Run, Tool,
    ToolContext, ToolOutput, ToolResult, UsageReport,
    inference::{InferenceError, provider::StreamToken},
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
    request::ToolDefinition,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A model adapter borrows application-owned configuration.
pub struct ApplicationClient<'a>(pub &'a str);

#[async_trait]
impl ModelAdapter for ApplicationClient<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: true,
            ..Capabilities::default()
        }
    }

    async fn generate(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        if let Some(tokens) = tokens {
            tokens.send(StreamToken::Text(self.0.into())).await.unwrap();
        }
        let content = if request.messages.len() == 1
            && request.tools.iter().any(|tool| tool.name == "observe")
        {
            AssistantContent::ToolCall(ToolCall::from_wire(
                "observe-call",
                ToolFunction::new("observe".into(), json!({})),
            ))
        } else if request.tools.iter().any(|tool| tool.name == "submit") {
            AssistantContent::ToolCall(ToolCall::from_wire(
                "answer",
                ToolFunction::new("submit".into(), json!({"value": self.0})),
            ))
        } else if request.messages.len() == 1
            && request.tools.iter().any(|tool| tool.name == "echo")
        {
            AssistantContent::ToolCall(ToolCall::from_wire(
                "echo-call",
                ToolFunction::new("echo".into(), json!({"value": self.0})),
            ))
        } else if request.tools.iter().any(|tool| tool.name == "read")
            && request.messages.len() == 1
        {
            AssistantContent::ToolCall(ToolCall::from_wire(
                "discover",
                ToolFunction::new("glob".into(), json!({"pattern":"*.txt"})),
            ))
        } else if request.tools.iter().any(|tool| tool.name == "read")
            && request.messages.len() == 3
        {
            AssistantContent::ToolCall(ToolCall::from_wire(
                "inspect",
                ToolFunction::new("read".into(), json!({"path":"notes.txt"})),
            ))
        } else {
            AssistantContent::text(self.0)
        };
        Ok(ModelOutput::new(vec![content], UsageReport::Unavailable))
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Answer {
    pub value: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Accepted(pub String);

#[derive(Default)]
pub struct Inspector {
    pub seen: Vec<String>,
}

impl Inspector {
    pub async fn inspect(&mut self, answer: Answer) -> Result<Decision<Accepted>, ()> {
        let seen = &mut self.seen;
        tokio::task::yield_now().await;
        seen.push(answer.value.clone());
        Ok(if seen.len() == 1 {
            Decision::revise("Inspect once more")
        } else {
            Decision::accept(Accepted(answer.value.to_uppercase()))
        })
    }
}

pub fn model(response: &str) -> Model<'_> {
    Model::from_adapter("application", "local", ApplicationClient(response)).unwrap()
}

/// One real run configuration can be consumed by automatic or manual execution.
/// The temporary Harness is dropped before either driver starts.
pub fn configured_run<'a>(model: Model<'a>, observations: &'a mut Vec<String>) -> Run<'a> {
    Harness::new(model)
        .run("record then answer")
        .instructions("Use the offered observation tool before answering.")
        .tool(LendingTool::new(observations))
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
///
/// Manual action ownership prevents dispatching the same capability twice.
///
/// ```compile_fail,E0382
/// use frona_harness::{ManualSession, PendingAction};
/// async fn twice(session: &mut ManualSession<'_>, action: PendingAction) {
///     let _ = session.execute(action).await;
///     let _ = session.execute(action).await;
/// }
/// ```
///
/// An inspector borrowing mutable application state cannot outlive that state.
///
/// ```compile_fail,E0597
/// use frona_harness::Harness;
/// use frona_standalone_consumer::{Answer, Inspector, model};
/// async fn invalid() {
///     let future;
///     {
///         let mut inspector = Inspector::default();
///         future = Harness::new(model("answer")).run("inspect")
///             .structured::<Answer>()
///             .inspect(async |answer| inspector.inspect(answer).await);
///     }
///     let _ = future.await;
/// }
/// ```
///
/// Local non-Send inspector captures cannot escape into a spawned task.
///
/// ```compile_fail
/// use frona_harness::{Decision, Harness};
/// use frona_standalone_consumer::{Answer, model};
/// use std::{cell::Cell, rc::Rc};
/// async fn invalid() {
///     let calls = Rc::new(Cell::new(0));
///     let future = Harness::new(model("answer")).run("inspect")
///         .structured::<Answer>()
///         .inspect(async move |answer| {
///             tokio::task::yield_now().await;
///             calls.set(calls.get() + 1);
///             Ok::<_, ()>(Decision::accept(answer.value))
///         });
///     tokio::spawn(future);
/// }
/// ```
///
/// Non-Send accepted output is valid locally but cannot leave a spawned task.
///
/// ```compile_fail
/// use frona_harness::{Decision, Harness};
/// use frona_standalone_consumer::{Answer, model};
/// use std::rc::Rc;
/// async fn invalid() {
///     let future = Harness::new(model("answer")).run("inspect")
///         .structured::<Answer>()
///         .inspect(async |answer| Ok::<_, ()>(Decision::accept(Rc::new(answer.value))));
///     tokio::spawn(future);
/// }
/// ```
pub mod ownership {}
