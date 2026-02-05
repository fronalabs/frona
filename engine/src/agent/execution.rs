use tokio_util::sync::CancellationToken;

use crate::api::state::AppState;
use crate::error::AppError;
use crate::llm::convert::to_rig_messages;
use crate::llm::tool_loop::{self, ToolLoopEvent, ToolLoopEventKind, ToolLoopOutcome};
use crate::repository::Repository;

pub struct AgentLoopOutcome {
    pub tool_loop_outcome: ToolLoopOutcome,
    pub accumulated_text: String,
    pub last_segment: String,
}

pub async fn run_agent_loop(
    state: &AppState,
    agent_id: &str,
    user_id: &str,
    chat_id: &str,
    space_id: Option<&str>,
    cancel_token: CancellationToken,
) -> Result<AgentLoopOutcome, AppError> {
    let agent_config = state.chat_service.resolve_agent_config(agent_id).await?;

    let skill_summaries: Vec<(String, String)> = state
        .skill_resolver
        .list(agent_id)
        .await
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();

    let agent_summaries = crate::api::routes::messages::build_agent_summaries_from_state(
        state,
        user_id,
        agent_id,
        &agent_config.tools,
    )
    .await;

    let system_prompt = state
        .memory_service
        .build_augmented_system_prompt(
            &agent_config.system_prompt,
            agent_id,
            user_id,
            space_id,
            &skill_summaries,
            &agent_summaries,
            &agent_config.identity,
        )
        .await
        .unwrap_or(agent_config.system_prompt.clone());

    let model_group = state
        .chat_service
        .provider_registry()
        .resolve_model_group(&agent_config.model_group)
        .map_err(|e| AppError::Llm(e.to_string()))?;

    let registry = state.chat_service.provider_registry().clone();

    let stored_messages = state.chat_service.get_stored_messages(chat_id).await;
    let rig_history = to_rig_messages(&stored_messages, agent_id);

    let (tool_event_tx, mut tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);

    let tool_registry = crate::api::routes::messages::build_tool_registry(
        state,
        agent_id,
        user_id,
        chat_id,
        &agent_config.tools,
        agent_config.sandbox_config.as_ref(),
    )
    .await;

    let user = state
        .user_repo
        .find_by_id(user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    let agent = state
        .agent_service
        .find_by_id(agent_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;
    let chat = state
        .chat_service
        .find_chat(chat_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
    let tool_ctx = crate::tool::ToolContext {
        user,
        agent,
        chat,
        event_tx: tool_event_tx.clone(),
    };

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
