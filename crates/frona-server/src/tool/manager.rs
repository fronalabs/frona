use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::models::Agent;
use crate::agent::prompt::PromptLoader;
use crate::agent::task::models::Task;
use crate::agent::task::service::TaskService;
use crate::chat::service::ChatService;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::policy::models::PolicyAction;
use crate::policy::service::PolicyService;
use crate::storage::StorageService;

/// Passed to `ToolManager::build_agent_registry` when the chat is backed by
/// an in-progress task; carries everything `TaskControlTool` and
/// `ReportSignalTool` need.
pub struct TaskToolContext {
    pub task: Task,
    pub storage_service: StorageService,
    pub prompts: PromptLoader,
    pub chat_service: ChatService,
    pub task_service: TaskService,
}

use super::registry::AgentToolRegistry;
use super::{AgentTool, InferenceContext, ToolDefinition, ToolOutput};

struct UserToolRegistry {
    tools: HashMap<String, Arc<dyn AgentTool>>,
}

impl UserToolRegistry {
    fn new(builtins: &[Arc<dyn AgentTool>]) -> Self {
        let mut tools = HashMap::new();
        for tool in builtins {
            tools.insert(tool.name().to_string(), tool.clone());
        }
        Self { tools }
    }

    fn register(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    fn deregister(&mut self, owner_name: &str) {
        self.tools.remove(owner_name);
    }

    fn definitions(&self) -> Vec<(String, ToolDefinition)> {
        let mut result = Vec::new();
        for (owner_name, tool) in &self.tools {
            for def in tool.definitions() {
                result.push((owner_name.clone(), def));
            }
        }
        result
    }

    fn tool_groups(&self) -> Vec<String> {
        let mut groups: HashSet<String> = HashSet::new();
        for tool in self.tools.values() {
            for def in tool.definitions() {
                if !def.provider_id.is_empty() {
                    groups.insert(def.provider_id);
                }
            }
        }
        let mut sorted: Vec<String> = groups.into_iter().collect();
        sorted.sort();
        sorted
    }
}

struct AuthorizedTool {
    inner: Arc<dyn AgentTool>,
    policy_service: PolicyService,
}

#[async_trait]
impl AgentTool for AuthorizedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let tool_group = self
            .definitions()
            .iter()
            .find(|d| d.id == tool_name)
            .map(|d| d.provider_id.clone())
            .unwrap_or_default();

        let decision = self
            .policy_service
            .authorize(
                &ctx.user.id,
                &ctx.agent,
                PolicyAction::InvokeTool {
                    tool_name: tool_name.to_string(),
                    tool_group,
                },
            )
            .await?;

        if decision.is_denied() {
            return Ok(ToolOutput::error(format!(
                "Authorization denied: agent '{}' is not permitted to use tool '{}'.",
                ctx.agent.name, tool_name
            )));
        }

        self.inner.execute(tool_name, arguments, ctx).await
    }
}

pub struct ToolManager {
    builtins: OnceLock<Vec<Arc<dyn AgentTool>>>,
    user_registries: RwLock<HashMap<String, UserToolRegistry>>,
    mcp_bridge_mode: bool,
}

impl ToolManager {
    pub fn new(mcp_bridge_mode: bool) -> Self {
        Self {
            builtins: OnceLock::new(),
            user_registries: RwLock::new(HashMap::new()),
            mcp_bridge_mode,
        }
    }

    pub(crate) fn initialize(&self, state: &AppState) {
        let tools = create_builtin_tools(state);
        assert!(
            self.builtins.set(tools).is_ok(),
            "built-in tools already initialized"
        );
    }

    fn builtins(&self) -> &[Arc<dyn AgentTool>] {
        self.builtins
            .get()
            .expect("AppState must initialize built-in tools before use")
    }

    pub async fn register_user_tool(&self, user_id: &str, tool: Arc<dyn AgentTool>) {
        let mut registries = self.user_registries.write().await;
        let registry = registries
            .entry(user_id.to_string())
            .or_insert_with(|| UserToolRegistry::new(self.builtins()));
        registry.register(tool);
    }

    pub async fn deregister_user_tool(&self, user_id: &str, owner_name: &str) {
        let mut registries = self.user_registries.write().await;
        if let Some(registry) = registries.get_mut(user_id) {
            registry.deregister(owner_name);
        }
    }

    pub async fn tool_groups(&self, user_id: &str) -> Vec<String> {
        let registries = self.user_registries.read().await;
        if let Some(registry) = registries.get(user_id) {
            registry.tool_groups()
        } else {
            let temp = UserToolRegistry::new(self.builtins());
            temp.tool_groups()
        }
    }

    pub async fn build_agent_registry(
        &self,
        user_id: &str,
        agent: &Agent,
        policy_service: &PolicyService,
        task_ctx: Option<TaskToolContext>,
    ) -> AgentToolRegistry {
        let all_defs = {
            let mut registries = self.user_registries.write().await;
            let registry = registries
                .entry(user_id.to_string())
                .or_insert_with(|| UserToolRegistry::new(self.builtins()));
            registry.definitions()
        };

        let mut tools: HashMap<String, Arc<dyn AgentTool>> = HashMap::new();
        let mut tool_name_to_owner: HashMap<String, String> = HashMap::new();
        let mut definitions: Vec<ToolDefinition> = Vec::new();

        for (owner_name, mut def) in all_defs {
            let decision = policy_service
                .authorize(
                    user_id,
                    agent,
                    PolicyAction::InvokeTool {
                        tool_name: def.id.clone(),
                        tool_group: def.provider_id.clone(),
                    },
                )
                .await;

            if let Err(ref e) = decision {
                tracing::warn!(tool = %def.id, error = %e, "Policy authorization error, skipping tool");
                continue;
            }
            if !decision.is_ok_and(|d| d.allowed) {
                continue;
            }

            if let Some(props) = def
                .parameters
                .as_object_mut()
                .and_then(|obj| obj.get_mut("properties"))
                .and_then(|p| p.as_object_mut())
            {
                props.insert(
                    "description".to_string(),
                    serde_json::json!({
                        "type": "string",
                        "description": "A short, specific description of what this tool call will accomplish (e.g. 'Checking the API status', 'Searching for weather data'). Shown to the user as a status indicator."
                    }),
                );
            }

            if !tools.contains_key(&owner_name) {
                let registries = self.user_registries.read().await;
                if let Some(registry) = registries.get(user_id)
                    && let Some(tool) = registry.tools.get(&owner_name)
                {
                    let wrapped: Arc<dyn AgentTool> = Arc::new(AuthorizedTool {
                        inner: tool.clone(),
                        policy_service: policy_service.clone(),
                    });
                    tools.insert(owner_name.clone(), wrapped);
                }
            }

            tool_name_to_owner.insert(def.id.clone(), owner_name);
            definitions.push(def);
        }

        let mut registry =
            AgentToolRegistry::new(tools, tool_name_to_owner, definitions, self.mcp_bridge_mode);

        if let Some(ctx) = task_ctx {
            let result_schema = ctx.task.effective_result_schema().and_then(|v| {
                match crate::agent::task::schema::ResultSpec::new(v) {
                    Ok(spec) => Some(Arc::new(spec)),
                    Err(e) => {
                        tracing::warn!("failed to compile task.result_schema: {e}");
                        None
                    }
                }
            });

            registry.register(Arc::new(crate::tool::task_control::TaskControlTool::new(
                ctx.storage_service.clone(),
                ctx.prompts.clone(),
                result_schema.clone(),
            )));

            let is_continuous_signal = matches!(
                &ctx.task.kind,
                crate::agent::task::models::TaskKind::Signal {
                    mode: crate::agent::task::models::SignalMode::Continuous,
                    ..
                }
            );
            if is_continuous_signal {
                registry.register(Arc::new(crate::tool::report_signal::ReportSignalTool::new(
                    ctx.chat_service,
                    ctx.prompts,
                    result_schema,
                )));
            }
        }

        registry
    }

    pub async fn definitions(&self, user_id: &str) -> Vec<ToolDefinition> {
        let registries = self.user_registries.read().await;
        if let Some(registry) = registries.get(user_id) {
            registry
                .definitions()
                .into_iter()
                .map(|(_, def)| def)
                .collect()
        } else {
            let temp = UserToolRegistry::new(self.builtins());
            temp.definitions().into_iter().map(|(_, def)| def).collect()
        }
    }

    /// Find a builtin tool by its sub-tool name (e.g. "ask_user_question",
    /// "manage_app") for the resolve dispatcher. Bypasses per-user and
    /// per-agent filtering - the agent already had policy permission to emit
    /// the HITL at execute time, so resolution should always succeed.
    pub fn find_tool_for_resume(&self, tool_name: &str) -> Option<Arc<dyn AgentTool>> {
        for tool in self.builtins() {
            for def in tool.definitions() {
                if def.id == tool_name {
                    return Some(tool.clone());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod initialization_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "AppState must initialize built-in tools before use")]
    fn incomplete_assembly_does_not_silently_hide_tools() {
        ToolManager::new(false).builtins();
    }
}

fn create_builtin_tools(state: &AppState) -> Vec<Arc<dyn AgentTool>> {
    use super::browser::tool::BrowserTool;
    use super::cli::CliTool;
    use super::files::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
    use super::heartbeat::HeartbeatTool;
    use super::notify_human::NotifyHumanTool;
    use super::produce_file::ProduceFileTool;
    use super::request_credentials::RequestCredentialsTool;
    use super::task::TaskTool;
    use super::update_identity::UpdateIdentityTool;
    use super::web_fetch::WebFetchTool;
    use super::web_search::WebSearchTool;

    let prompts = state.prompts.clone();

    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(NotifyHumanTool::new(
            state.vault_service.clone(),
            prompts.clone(),
            state.config.server.external_or_local_base_url(),
        )),
        Arc::new(super::send_message::SendMessageTool::new(
            state.chat_service.clone(),
            state.notification_service.clone(),
            state.broadcast_service.clone(),
            state.agent_service.clone(),
            state.task_service.clone(),
            prompts.clone(),
        )),
        Arc::new(ProduceFileTool::new(
            state.storage_service.clone(),
            prompts.clone(),
        )),
        Arc::new(ReadTool::new(
            state.storage_service.clone(),
            state.sandbox_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(WriteTool::new(
            state.storage_service.clone(),
            state.sandbox_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(EditTool::new(
            state.storage_service.clone(),
            state.sandbox_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(GlobTool::new(
            state.storage_service.clone(),
            state.sandbox_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(GrepTool::new(
            state.storage_service.clone(),
            state.sandbox_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(UpdateIdentityTool::new(
            state.agent_service.clone(),
            prompts.clone(),
        )),
        Arc::new(BrowserTool::new(
            state.browser_session_manager.clone(),
            state.vault_service.clone(),
        )),
        Arc::new(WebFetchTool::new(
            state.browser_session_manager.clone(),
            prompts.clone(),
        )),
        Arc::new(WebSearchTool::new(
            state.search_provider.clone(),
            prompts.clone(),
        )),
        Arc::new(HeartbeatTool::new(
            state.agent_service.clone(),
            state.storage_service.clone(),
            prompts.clone(),
            state.config.server.timezone.clone(),
        )),
        Arc::new(RequestCredentialsTool::new(
            state.vault_service.clone(),
            prompts.clone(),
            state.config.server.external_or_local_base_url(),
        )),
        Arc::new(super::manage_app::ManageAppTool::new(
            state.app_service.clone(),
            prompts.clone(),
            state.notification_service.clone(),
            state.broadcast_service.clone(),
            state.storage_service.clone(),
            state.config.server.external_or_local_base_url(),
        )),
        Arc::new(super::create_agent::CreateAgentTool::new(
            state.agent_service.clone(),
            state.storage_service.clone(),
            state.broadcast_service.clone(),
            prompts.clone(),
        )),
        Arc::new(super::manage_policy::ManagePolicyTool::new(
            state.policy_service.clone(),
            prompts.clone(),
        )),
    ];

    // The active memory service contributes its own tools (e.g. store-memory).
    tools.extend(state.harness.memory_service.tools());

    tools.push(Arc::new(TaskTool::new(
        state.task_service.clone(),
        state.agent_service.clone(),
        state.task_executor.clone(),
        state.policy_service.clone(),
        prompts.clone(),
        state.config.server.timezone.clone(),
    )));

    {
        let signal_service = state.signal_service();
        tools.push(Arc::new(super::await_signal::AwaitSignalTool::new(
            state.task_service.clone(),
            signal_service.clone(),
            state.broadcast_service.clone(),
            prompts.clone(),
            state.config.signal.default_max_evaluations,
            state.config.signal.default_max_continuous_evaluations,
            state.config.server.timezone.clone(),
        )));
        tools.push(Arc::new(super::annotate::AnnotateTool::new(
            signal_service,
            state.chat_service.clone(),
            state.space_service.clone(),
            state.contact_service.clone(),
            state.channel_service.clone(),
            prompts.clone(),
        )));
    }

    if state.voice_provider.is_some() {
        tools.push(Arc::new(super::voice::VoiceCallTool {
            provider: state.voice_provider.clone(),
            prompts: prompts.clone(),
            contact_service: state.contact_service.clone(),
            call_service: state.call_service.clone(),
        }));
        tools.push(Arc::new(super::voice::SendDtmfTool {
            prompts: prompts.clone(),
        }));
        tools.push(Arc::new(super::voice::HangupCallTool {
            prompts: prompts.clone(),
        }));
    }

    for tool_config in state.cli_tools_config.iter() {
        tools.push(Arc::new(CliTool::new(
            tool_config.clone(),
            state.sandbox_manager.clone(),
        )));
    }

    tools
}
