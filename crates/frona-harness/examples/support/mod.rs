//! A deterministic application-owned client, used only by these examples.
//! Replace `model(...)` with `Model::builder(...)` to use a built-in provider.
#![allow(dead_code)] // Each example uses a different subset of this small fixture.

use async_trait::async_trait;
use frona_harness::{
    Capabilities, Model, ModelAdapter, ModelError, ModelOutput, ModelRequest, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::Value;
use std::{collections::VecDeque, sync::Mutex};
use tokio::sync::mpsc;

struct LocalClient(Mutex<VecDeque<ModelOutput>>);

#[async_trait]
impl ModelAdapter for LocalClient {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: true,
            ..Default::default()
        }
    }

    async fn generate(
        &self,
        _request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        let output = self.0.lock().unwrap().pop_front().ok_or_else(|| {
            InferenceError::InferenceFailed("example exhausted its scripted responses".into())
        })?;
        if let Some(tokens) = tokens {
            for content in &output.content {
                if let AssistantContent::Text(text) = content {
                    // Observation is optional: a disconnected observer is not a failure.
                    let _ = tokens.send(StreamToken::Text(text.text.clone())).await;
                }
            }
        }
        Ok(output)
    }
}

pub fn model(outputs: Vec<ModelOutput>) -> Result<Model<'static>, ModelError> {
    Model::from_adapter("example", "local", LocalClient(Mutex::new(outputs.into())))
}

pub fn output(content: Vec<AssistantContent>) -> ModelOutput {
    // The fixture knows its accounting value. Real adapters must report provenance honestly.
    ModelOutput::new(
        content,
        UsageReport::Reported(Usage {
            total_tokens: 3,
            ..Usage::new()
        }),
    )
}

pub fn text(value: &str) -> ModelOutput {
    output(vec![AssistantContent::text(value)])
}

pub fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), arguments),
    ))
}

pub fn submission(arguments: Value) -> ModelOutput {
    output(vec![call("answer", "submit", arguments)])
}
