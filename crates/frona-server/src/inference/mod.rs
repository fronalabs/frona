use self::streaming::ModelGroupStreaming;
use frona_harness::inference::{InferenceError, ModelGroup};
pub mod config;
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
    // For the no-tool path we record one Chat row. For the tool-loop path,
    // tool_loop builds a fresh ToolTurn UsageContext per iteration and a
    // final Chat row when it emits its last text turn.
    let chat_usage_ctx = UsageContext::new(
        InferenceKind::Text {
            agent_id: request.ctx.agent.id.clone(),
            chat_id: active_chat(&request.ctx)?.id.clone(),
            message_id: request.message_id.clone(),
        },
        request.ctx.user.id.clone(),
        request.model_group.name.clone(),
    );

    // Single source of truth: every inference turn (initial, resume, task
    // executor's inner runs) flows through this function, so emitting
    // `Start` here covers all entry points exactly once per turn.
    request.ctx.event_tx.send(tool_loop::InferenceEvent {
        kind: tool_loop::InferenceEventKind::Start,
    });

    if request.tool_registry.is_empty() {
        use crate::inference::tool_loop::extract_reasoning;
        // History is compaction-aware at load time; an
        // over-budget request is rejected by the provider, not silently trimmed.
        let history = request.history;

        let mut response_text = String::new();
        let event_tx = &request.ctx.event_tx;
        match (request.model_group)
            .stream_inference(
                frona_harness::inference::ModelRequest {
                    input_token_estimate: request.input_token_estimate,
                    system_prompt: &request.system_prompt,
                    history: history.to_vec(),
                    tools: vec![],
                    observations: frona_harness::inference::observation::Observations::new(
                        &request.usage_service.scoped(&chat_usage_ctx),
                    ),
                    overrides: Default::default(),
                },
                event_tx,
                &request.cancel_token,
                &mut response_text,
            )
            .await?
        {
            streaming::StreamResult::Contents {
                content: contents,
                usage: _,
            } => {
                let reasoning = extract_reasoning(&contents);
                Ok(InferenceResponse::Completed {
                    text: response_text,
                    attachments: vec![],
                    lifecycle_event: None,
                    reasoning,
                })
            }
            streaming::StreamResult::Cancelled => Ok(InferenceResponse::Cancelled(response_text)),
        }
    } else {
        let event_tx = request.ctx.event_tx.clone();
        let outcome = tool_loop::run_tool_loop(
            &request.model_group,
            &request.system_prompt,
            request.history,
            &request.tool_registry,
            event_tx,
            request.cancel_token,
            &request.ctx,
            &request.usage_service,
            &request.chat_service,
            &request.message_id,
            request.input_token_estimate,
        )
        .await?;

        Ok(match outcome {
            tool_loop::ToolLoopOutcome::Completed {
                text,
                attachments,
                lifecycle_event,
                reasoning,
            } => InferenceResponse::Completed {
                text,
                attachments,
                lifecycle_event,
                reasoning,
            },
            tool_loop::ToolLoopOutcome::Cancelled(text) => InferenceResponse::Cancelled(text),
            tool_loop::ToolLoopOutcome::ExternalToolPending {
                turn_text,
                tool_calls,
                system_prompt,
            } => InferenceResponse::ExternalToolPending {
                turn_text,
                tool_calls,
                system_prompt,
            },
        })
    }
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
