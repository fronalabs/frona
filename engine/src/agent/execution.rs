use tokio_util::sync::CancellationToken;

use crate::chat::session::ChatSessionContext;
use crate::core::state::AppState;
use crate::core::error::AppError;
use crate::llm::tool_loop::{self, ToolLoopEvent, ToolLoopEventKind, ToolLoopOutcome};

pub struct AgentLoopOutcome {
    pub tool_loop_outcome: ToolLoopOutcome,
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

    let (tool_event_tx, tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);
    let ChatSessionContext {
        system_prompt, model_group, rig_history, registry,
        tool_registry, tool_ctx, tool_event_tx,
        mut tool_event_rx, ..
    } = ChatSessionContext::build(state, user_id, chat, tool_event_tx, tool_event_rx).await?;

    let tool_handle = {
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            tool_loop::run_tool_loop(
                &registry,
                &model_group,
                &system_prompt,
                rig_history,
                &tool_registry,
                tool_event_tx,
                cancel_token,
                &tool_ctx,
            )
            .await
        })
    };

    let mut accumulated = String::new();
    let mut last_segment = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        match event.kind {
            ToolLoopEventKind::Text(text) => {
                accumulated.push_str(&text);
                last_segment.push_str(&text);
            }
            ToolLoopEventKind::ToolCall { .. } | ToolLoopEventKind::ToolResult { .. } => {
                last_segment.clear();
            }
            _ => {}
        }
    }

    let tool_loop_outcome = tool_handle.await.map_err(|e| {
        AppError::Internal(format!("Tool loop task panicked: {e}"))
    })??;

    Ok(AgentLoopOutcome {
        tool_loop_outcome,
        accumulated_text: accumulated,
        last_segment,
    })
}
