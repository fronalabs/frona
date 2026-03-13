use std::sync::Arc;

use crate::agent::models::SandboxSettings;
use crate::tool::browser::tool::BrowserTool;
use crate::tool::cli::CliTool;
use crate::tool::delegate::DelegateTaskTool;
use crate::tool::heartbeat::HeartbeatTool;
use crate::tool::notify_human::NotifyHumanTool;
use crate::tool::produce_file::ProduceFileTool;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::remember::{RememberTool, RememberUserFactTool};
use crate::tool::request_credentials::RequestCredentialsTool;
use crate::tool::schedule::ScheduleTaskTool;
use crate::tool::skill::SkillTool;
use crate::tool::time::TimeTool;
use crate::tool::update_entity::UpdateEntityTool;
use crate::tool::update_identity::UpdateIdentityTool;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::web_search::WebSearchTool;

use crate::core::state::AppState;

use super::get_compaction_model_group;

pub async fn build_tool_registry(
    state: &AppState,
    agent_id: &str,
    user_id: &str,
    username: &str,
    chat_id: &str,
    allowed_tools: &[String],
    sandbox_config: Option<&SandboxSettings>,
) -> AgentToolRegistry {
    let mut registry = AgentToolRegistry::new();

    let credential = state
        .vault_service
        .list_credentials(user_id)
        .await
        .ok()
        .and_then(|creds| creds.into_iter().next());

    let credential_id = credential.as_ref().map(|c| c.id.clone());

    let prompts = state.prompts.clone();

    registry.register(Arc::new(TimeTool::new(prompts.clone())));
    registry.register(Arc::new(NotifyHumanTool::new(credential_id, prompts.clone())));

    let workspace_path = std::path::Path::new(&state.config.storage.workspaces_path).join(agent_id);
    registry.register(Arc::new(ProduceFileTool::new(
        agent_id.to_string(),
        workspace_path,
        prompts.clone(),
    )));

    registry.register(Arc::new(UpdateEntityTool::new(
        state.db.clone(),
        "agent",
        agent_id,
        user_id,
        "update_agent",
    )));

    registry.register(Arc::new(UpdateIdentityTool::new(
        state.db.clone(),
        agent_id,
        user_id,
        prompts.clone(),
    )));

    registry.register(Arc::new(RememberTool::new(
        state.memory_service.clone(),
        agent_id.to_string(),
        chat_id.to_string(),
        get_compaction_model_group(state),
        prompts.clone(),
    )));

    registry.register(Arc::new(RememberUserFactTool::new(
        state.memory_service.clone(),
        user_id.to_string(),
        chat_id.to_string(),
        get_compaction_model_group(state),
        prompts.clone(),
    )));

    registry.register(Arc::new(SkillTool::new(
        state.skill_resolver.clone(),
        agent_id.to_string(),
        prompts.clone(),
    )));

    if allowed_tools.iter().any(|t| t == "browser")
        && let Some(credential) = credential
    {
        registry.register(Arc::new(BrowserTool::new(
            state.browser_session_manager.clone(),
            username.to_string(),
            credential.provider,
        )));
    }

    if allowed_tools.iter().any(|t| t == "web_fetch") {
        registry.register(Arc::new(WebFetchTool::new(
            state.browser_session_manager.clone(),
            username.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "web_search") {
        registry.register(Arc::new(WebSearchTool::new(state.search_provider.clone(), prompts.clone())));
    }

    if allowed_tools.iter().any(|t| t == "delegate")
        && let Some(executor) = state.task_executor()
    {
        let chat = state.chat_service.find_chat(chat_id).await.ok().flatten();
        let space_id = chat.and_then(|c| c.space_id);

        registry.register(Arc::new(DelegateTaskTool::new(
            state.task_service.clone(),
            state.agent_service.clone(),
            executor,
            state.broadcast_service.clone(),
            user_id.to_string(),
            agent_id.to_string(),
            chat_id.to_string(),
            space_id,
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "schedule") {
        registry.register(Arc::new(ScheduleTaskTool::new(
            state.task_service.clone(),
            state.agent_service.clone(),
            user_id.to_string(),
            agent_id.to_string(),
            chat_id.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "heartbeat") {
        registry.register(Arc::new(HeartbeatTool::new(
            state.agent_service.clone(),
            state.storage_service.clone(),
            agent_id.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "request_credentials") {
        registry.register(Arc::new(RequestCredentialsTool::new(
            state.vault_service.clone(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "manage_service") {
        registry.register(Arc::new(crate::tool::manage_service::ManageServiceTool::new(
            state.app_service.clone(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "make_voice_call") {
        registry.register(Arc::new(crate::tool::voice::VoiceCallTool {
            provider: state.voice_provider.clone(),
            prompts: prompts.clone(),
            contact_service: state.contact_service.clone(),
            call_service: state.call_service.clone(),
        }));
    }

    if allowed_tools.iter().any(|t| t == "send_dtmf") {
        registry.register(Arc::new(crate::tool::voice::SendDtmfTool {
            prompts: prompts.clone(),
        }));
    }

    if allowed_tools.iter().any(|t| t == "hangup_call") {
        registry.register(Arc::new(crate::tool::voice::HangupCallTool {
            prompts: prompts.clone(),
        }));
    }

    let skill_dirs: Vec<(String, String)> = state
        .skill_resolver
        .list(agent_id)
        .await
        .into_iter()
        .filter_map(|s| {
            state
                .skill_resolver
                .skill_dir_path(agent_id, &s.name)
                .map(|p| {
                    let abs = std::fs::canonicalize(&p)
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                    (format!("skills/{}/", s.name), abs)
                })
        })
        .collect();

    let defaults = sandbox_config.cloned().unwrap_or_default();
    tracing::info!(cli_tools_count = state.cli_tools_config.len(), ?allowed_tools, "Building tool registry");
    for tool_config in state.cli_tools_config.iter() {
        if allowed_tools.iter().any(|t| t == &tool_config.name) {
            tracing::info!(tool = %tool_config.name, "Registering CLI tool");
            registry.register(Arc::new(CliTool::new(
                tool_config.clone(),
                state.sandbox_manager.clone(),
                agent_id.to_string(),
                defaults.network_access,
                defaults.allowed_network_destinations.clone(),
            ).with_skill_dirs(skill_dirs.clone())));
        }
    }

    let tool_names: Vec<&str> = registry.definitions.iter().map(|d| d.name.as_str()).collect();
    tracing::info!(
        ?tool_names,
        cli_configs = state.cli_tools_config.len(),
        ?allowed_tools,
        "Tool registry built"
    );

    registry
}

pub async fn build_agent_summaries_from_state(
    state: &AppState,
    user_id: &str,
    current_agent_id: &str,
    tools: &[String],
) -> Vec<(String, String)> {
    if !tools.iter().any(|t| t == "delegate") {
        return Vec::new();
    }

    let agents = match state.agent_service.list(user_id).await {
        Ok(agents) => agents,
        Err(_) => return Vec::new(),
    };

    agents
        .into_iter()
        .filter(|a| a.id != current_agent_id && a.enabled)
        .map(|a| (a.name, a.description))
        .collect()
}
