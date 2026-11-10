//! Scripted native Rig transport shared by harness and server execution tests.
#![allow(dead_code)]

use frona_harness::{ModelOutput, UsageReport, inference::InferenceError};
use rig_core::{
    completion::{CompletionRequest, CompletionResponse, FinishReason},
    driver::{Exchange, Opened, Opening, Transport},
    error::ProviderError,
    operation::Completion,
    test_utils::{MockFrame, MockScript},
    wire::Mode,
};
use std::sync::Arc;

type SendReply = dyn Fn(CompletionRequest, Mode) -> Opening<MockFrame> + Send + Sync;
#[derive(Clone)]
struct ScriptTransport(Arc<SendReply>);
impl Transport<MockScript> for ScriptTransport {
    fn send(&self, request: CompletionRequest, exchange: Exchange) -> Opening<MockFrame> {
        (self.0)(request, exchange.mode)
    }
}

pub fn model(
    id: impl Into<String>,
    send: impl Fn(CompletionRequest, Mode) -> Opening<MockFrame> + Send + Sync + 'static,
) -> rig_core::DynModel<Completion> {
    rig_core::Model::new(
        MockScript::new("fixture").with_id(id),
        ScriptTransport(Arc::new(send)),
    )
    .erase()
}

pub fn response(output: ModelOutput) -> CompletionResponse {
    let usage = match output.usage {
        UsageReport::Reported(usage) | UsageReport::LegacyNormalized(usage) => usage,
        UsageReport::Unavailable => Default::default(),
    };
    let origin = output.origin.unwrap_or_else(|| {
        rig_core::message::Origin::new(rig_core::test_utils::MOCK_API, "fixture", "model")
    });
    let mut response = CompletionResponse::new(
        output.content,
        usage,
        origin,
        output
            .metadata
            .get("raw")
            .cloned()
            .unwrap_or(output.metadata),
    );
    use rig_core::message::StopReason;
    match output.stop {
        Some(StopReason::Length) => response = response.with_finish_reason(FinishReason::Length),
        Some(StopReason::Error(error)) => response.error = Some(error),
        Some(StopReason::Aborted(error)) => response.aborted = Some(error),
        _ => response = response.with_finish_reason(FinishReason::Stop),
    }
    response
}

pub fn reply(output: ModelOutput) -> Opening<MockFrame> {
    Opening::ready(Opened::new(futures::stream::iter([Ok(
        MockFrame::Response(Box::new(response(output))),
    )])))
}

/// Script transport failures in the upstream error vocabulary. Typed Frona
/// controls should be returned by the resolver before constructing a transport.
pub fn error(error: InferenceError) -> ProviderError {
    match error {
        InferenceError::CompletionFailed(error) => *error,
        InferenceError::RateLimited { retry_after_secs } => {
            ProviderError::ProviderResponse(rig_core::ProviderResponseError::new(
                429_u16.try_into().unwrap(),
                serde_json::json!({"error":{"message":"Rate limited","type":"rate_limit_error","retry_after":retry_after_secs}}).to_string(),
            ))
        }
        error if error.is_retryable() => ProviderError::Provider(error.to_string()),
        error => ProviderError::Response(error.to_string()),
    }
}
