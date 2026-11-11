//! Shared scripted transport for phase tests. Domain inputs remain with each phase.
use async_trait::async_trait;
use frona_harness::{
    Capabilities, Harness, Model, ModelAdapter, ModelOutput, ModelRequest, UsageReport,
    inference::{InferenceError, provider::StreamToken},
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub(crate) type Requests = Arc<Mutex<Vec<ModelRequest>>>;
struct Script {
    outputs: Mutex<VecDeque<Result<ModelOutput, InferenceError>>>,
    requests: Requests,
}
#[async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests.lock().unwrap().push(request);
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request: script exhausted")
    }
}
pub(crate) fn harness(
    outputs: impl IntoIterator<Item = Result<ModelOutput, InferenceError>>,
) -> (Harness<'static>, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Model::from_adapter(
        "test",
        "pkm",
        Script {
            outputs: Mutex::new(outputs.into_iter().collect()),
            requests: requests.clone(),
        },
    )
    .unwrap();
    (Harness::new(model), requests)
}
pub(crate) fn tool_call(name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        name,
        ToolFunction::new(name.try_into().unwrap(), args),
    ))
}
pub(crate) fn tool_output(name: &str, args: Value) -> ModelOutput {
    output(vec![tool_call(name, args)])
}
pub(crate) fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Unavailable)
}
pub(crate) fn messages(requests: &Requests) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| format!("{:?}", request.messages))
        .collect()
}
