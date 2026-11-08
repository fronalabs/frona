use frona_harness::inference::{InferenceError, ModelGroup};
pub mod config;
pub(crate) mod controls;
pub use controls::RuntimeControls;
pub mod conversation;
pub mod credential;
pub mod directory;
pub mod hitl;
pub mod provider;
pub mod request;
pub mod streaming;
pub mod structured;
pub mod tool_call;
pub mod tool_loop;
pub mod usage;

pub use self::usage::{CompactionTarget, InferenceKind, UsageContext};

pub use self::hitl::{
    Hitl, HitlDelivery, HitlOutcome, HitlRequest, HitlResponse, ResolveOutcome, VaultGrant,
};
pub use self::request::{InferenceContext, InferenceRequest, InferenceResponse, active_chat};
pub use self::structured::{
    AnswerAttempt, StructuredConversation, structured_inference, structured_inference_with_tools,
};
pub use self::tool_loop::{InferenceEvent, InferenceEventKind};
pub use crate::chat::broadcast::EventSender;

use rig_core::completion::Message as RigMessage;

use crate::core::error::AppError;

use self::usage::UsageService;

pub async fn inference(request: InferenceRequest) -> Result<InferenceResponse, AppError> {
    inference_controlled(request, &RuntimeControls::default()).await
}

pub async fn inference_controlled(
    request: InferenceRequest,
    controls: &RuntimeControls,
) -> Result<InferenceResponse, AppError> {
    let report = controls.report();
    report
        .record_run(async {
            let result = inference_in_scope(request, controls).await;
            let state = crate::inference::controls::outcome(result.as_ref());
            (result, state)
        })
        .await
}

pub(crate) async fn inference_in_scope(
    request: InferenceRequest,
    controls: &RuntimeControls,
) -> Result<InferenceResponse, AppError> {
    request.ctx.event_tx.send(tool_loop::InferenceEvent {
        kind: tool_loop::InferenceEventKind::Start,
    });
    tool_loop::run_tool_loop_controlled(
        &request.model_group,
        &request.system_prompt,
        request.history,
        &request.tool_registry,
        request.ctx.event_tx.clone(),
        &request.ctx,
        &request.usage_service,
        &request.chat_service,
        &request.message_id,
        request.input_token_estimate,
        controls.execution(&request.cancel_token),
    )
    .await
}

pub async fn text_inference(
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<String, InferenceError> {
    let frona_harness::inference::ModelResponse {
        content: contents, ..
    } = (model_group)
        .inference(frona_harness::inference::ModelRequest {
            input_token_estimate: None,
            system_prompt,
            history,
            tools: vec![],
            observations: frona_harness::inference::observation::Observations::new(
                &usage_service.scoped(usage_ctx),
            ),
            overrides: Default::default(),
        })
        .await?;
    frona_harness::inference::provider::extract_text_from_choice(&contents)
}

#[cfg(test)]
mod streaming_tests;
