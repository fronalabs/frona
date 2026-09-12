use serde_json::Value;

use crate::agent::models::CreateAgentRequest;
use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::chat::broadcast::BroadcastService;
use crate::core::error::AppError;
use crate::inference::InferenceEventKind;
use crate::storage::service::StorageService;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct CreateAgentTool {
    agent_service: AgentService,
    storage_service: StorageService,
    broadcast_service: BroadcastService,
    prompts: PromptLoader,
}

impl CreateAgentTool {
    pub fn new(
        agent_service: AgentService,
        storage_service: StorageService,
        broadcast_service: BroadcastService,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            agent_service,
            storage_service,
            broadcast_service,
            prompts,
        }
    }
}

#[agent_tool]
impl CreateAgentTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: id".into()))?
            .to_string();

        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: name".into()))?
            .to_string();

        let summary = arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: summary".into()))?
            .to_string();

        let instructions = arguments
            .get("instructions")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: instructions".into()))?
            .to_string();

        let model_group = arguments
            .get("model_group")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: model_group".into()))?
            .to_string();

        let tools: Option<Vec<String>> =
            arguments
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });

        let req = CreateAgentRequest {
            id: Some(id.clone()),
            handle: Some(id),
            name: name.clone(),
            description: summary,
            model_group,
            tools,
            skills: None,
            sandbox_policy: None,
            sandbox_limits: None,
        };

        let agent = self.agent_service.create(&ctx.user.id, req).await?;

        let workspace = self
            .storage_service
            .agent_workspace(&ctx.user.handle, &agent.handle);
        workspace
            .write("AGENT.md", &instructions)
            .map_err(|e| AppError::Internal(format!("Failed to write AGENT.md: {e}")))?;

        ctx.event_tx
            .send(crate::inference::tool_loop::InferenceEvent {
                kind: InferenceEventKind::EntityUpdated {
                    table: "agent".to_string(),
                    record_id: agent.id.clone(),
                    fields: serde_json::json!({
                        "name": agent.name,
                        "id": agent.id,
                    }),
                },
            });

        self.broadcast_service
            .send(crate::chat::broadcast::BroadcastEvent {
                user_id: ctx.user.id.clone(),
                chat_id: None,
                space_id: None,
                kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                    InferenceEventKind::EntityUpdated {
                        table: "agent".to_string(),
                        record_id: agent.id.clone(),
                        fields: serde_json::json!({
                            "name": agent.name,
                            "id": agent.id,
                        }),
                    },
                ),
            });

        Ok(ToolOutput::text(format!(
            "Agent '{}' created successfully (id: {}). The user can now start chatting with it.",
            name, agent.id
        )))
    }
}
