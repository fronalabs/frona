//! Exercise the same single-attempt adapter as production provider calls.
#![allow(dead_code)]
use frona_harness::{
    ModelAdapter, ModelOutput, ModelRequest,
    inference::{
        InferenceError,
        provider::{ModelConfig, ModelProvider, RigModelAdapter, StreamToken},
    },
};
use std::sync::Arc;

pub async fn invoke(
    provider: Arc<dyn ModelProvider>,
    config: &ModelConfig,
    request: ModelRequest,
    tokens: Option<tokio::sync::mpsc::Sender<StreamToken>>,
) -> Result<ModelOutput, InferenceError> {
    let schema = request.schema.clone();
    let output = RigModelAdapter::new(provider, config.clone())
        .generate(request, tokens)
        .await?;
    output.validate(schema.as_ref())?;
    Ok(output)
}
