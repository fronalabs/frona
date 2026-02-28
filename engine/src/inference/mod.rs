pub mod config;
pub mod context;
pub mod convert;
pub mod error;
pub mod provider;
pub mod registry;
pub mod request;
pub mod retry;
pub mod tool_loop;

pub use error::InferenceError;
pub use provider::ModelRef;
pub use registry::ModelProviderRegistry;
pub use request::{InferenceRequest, InferenceResponse};
pub use rig::completion::request::Usage;
pub use tool_loop::{InferenceEvent, InferenceEventKind};

use rig::completion::Message as RigMessage;

use crate::core::error::AppError;
use crate::core::metrics::InferenceMetricsContext;
use crate::tool::ToolContext;

use self::config::ModelGroup;

pub async fn inference(request: InferenceRequest<'_>) -> Result<InferenceResponse, AppError> {
    let metrics_ctx = InferenceMetricsContext {
        user_id: request.user.id.clone(),
        agent_id: request.agent.id.clone(),
        model_group: request.model_group.name.clone(),
    };

    if request.tool_registry.is_empty() {
        let max_output = request
            .model_group
            .max_tokens
            .unwrap_or(request.model_group.inference.default_max_tokens)
            as usize;
        let history = context::truncate_history(
            request.history,
            request.system_prompt,
            &request.model_group.main.model_id,
            request.model_group.context_window,
            max_output,
            request.model_group.inference.history_truncation_pct,
        );

        let mut accumulated_text = String::new();
        match retry::stream_with_retry_and_fallback(
            request.registry,
            request.model_group,
            request.system_prompt,
            &history,
            &[],
            &request.event_tx,
            &request.cancel_token,
            &mut accumulated_text,
            &metrics_ctx,
        )
        .await?
        {
            retry::StreamResult::Contents(_) => {
                let _ = request
                    .event_tx
                    .send(InferenceEvent {
                        kind: InferenceEventKind::Done(accumulated_text.clone()),
                    })
                    .await;
                Ok(InferenceResponse::Completed {
                    text: accumulated_text,
                    attachments: vec![],
                })
            }
            retry::StreamResult::Cancelled => {
                let _ = request
                    .event_tx
                    .send(InferenceEvent {
                        kind: InferenceEventKind::Cancelled(accumulated_text.clone()),
                    })
                    .await;
                Ok(InferenceResponse::Cancelled(accumulated_text))
            }
        }
    } else {
        let tool_ctx = ToolContext {
            user: request.user.clone(),
            agent: request.agent.clone(),
            chat: request.chat.clone(),
            event_tx: request.event_tx.clone(),
        };

        let outcome = tool_loop::run_tool_loop(
            request.registry,
            request.model_group,
            request.system_prompt,
            request.history,
            request.tool_registry,
            request.event_tx,
            request.cancel_token,
            &tool_ctx,
            &metrics_ctx,
        )
        .await?;

        Ok(match outcome {
            tool_loop::ToolLoopOutcome::Completed { text, attachments } => {
                InferenceResponse::Completed { text, attachments }
            }
            tool_loop::ToolLoopOutcome::Cancelled(text) => InferenceResponse::Cancelled(text),
            tool_loop::ToolLoopOutcome::ExternalToolPending {
                accumulated_text,
                tool_calls_json,
                tool_results,
                external_tool,
                system_prompt,
            } => InferenceResponse::ExternalToolPending {
                accumulated_text,
                tool_calls_json,
                tool_results,
                external_tool,
                system_prompt,
            },
        })
    }
}

pub async fn text_inference(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    metrics_ctx: &InferenceMetricsContext,
) -> Result<String, InferenceError> {
    let (contents, _usage) = retry::inference_with_retry_and_fallback(
        registry,
        model_group,
        system_prompt,
        history,
        vec![],
        None,
        metrics_ctx,
    )
    .await?;
    provider::extract_text_from_choice(&contents)
}
