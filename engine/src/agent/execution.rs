use rig::completion::Message as RigMessage;
use tokio_util::sync::CancellationToken;

use crate::chat::session::ChatSessionContext;
use crate::core::state::AppState;
use crate::core::error::AppError;
use crate::inference::config::ModelGroup;
use crate::inference::request::{InferenceRequest, InferenceResponse};
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};
use crate::inference::ModelProviderRegistry;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::ToolContext;

pub struct AgentLoopOutcome {
    pub response: InferenceResponse,
    pub accumulated_text: String,
    pub last_segment: String,
}

pub async fn run_agent_loop(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
    cancel_token: CancellationToken,
) -> Result<AgentLoopOutcome, AppError> {
    let chat = state
        .chat_service
        .find_chat(chat_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

    let (tool_event_tx, tool_event_rx) = tokio::sync::mpsc::channel::<InferenceEvent>(32);
    let ChatSessionContext {
        system_prompt, model_group, rig_history, registry,
        tool_registry, tool_ctx, tool_event_tx,
        mut tool_event_rx, ..
    } = ChatSessionContext::build(state, user_id, chat, cancel_token.clone(), tool_event_tx, tool_event_rx).await?;

    let inference_handle = spawn_inference(
        registry, model_group, system_prompt,
        rig_history, tool_registry, tool_ctx, tool_event_tx, cancel_token,
    );

    let mut accumulated = String::new();
    let mut last_segment = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        match event.kind {
            InferenceEventKind::Text(text) => {
                accumulated.push_str(&text);
                last_segment.push_str(&text);
            }
            InferenceEventKind::ToolCall { .. } | InferenceEventKind::ToolResult { .. } => {
                last_segment.clear();
            }
            InferenceEventKind::Done(_) | InferenceEventKind::Cancelled(_) => {}

            _ => {}
        }
    }

    let response = inference_handle.await.map_err(|e| {
        AppError::Internal(format!("Inference task panicked: {e}"))
    })??;

    Ok(AgentLoopOutcome {
        response,
        accumulated_text: accumulated,
        last_segment,
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_inference(
    registry: ModelProviderRegistry,
    model_group: ModelGroup,
    system_prompt: String,
    history: Vec<RigMessage>,
    tool_registry: AgentToolRegistry,
    tool_ctx: ToolContext,
    event_tx: tokio::sync::mpsc::Sender<InferenceEvent>,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<Result<InferenceResponse, AppError>> {
    tokio::spawn(async move {
        crate::inference::inference(InferenceRequest {
            registry: &registry,
            model_group: &model_group,
            system_prompt: &system_prompt,
            history,
            tool_registry: &tool_registry,
            user: &tool_ctx.user,
            agent: &tool_ctx.agent,
            chat: &tool_ctx.chat,
            event_tx,
            cancel_token,
        })
        .await
    })
}
