use crate::agent::config::parse_frontmatter;
use crate::agent::workspace::{AgentPromptLoader, AgentWorkspaceManager};
use crate::api::repo::agents::SurrealAgentRepo;
use crate::api::repo::chats::SurrealChatRepo;
use crate::api::repo::messages::SurrealMessageRepo;
use crate::core::error::AppError;
use crate::core::metrics::InferenceMetricsContext;
use crate::inference::ModelProviderRegistry;
use crate::inference::convert::to_rig_messages;
use crate::inference::text_inference;
use crate::inference::provider::ModelRef;
use crate::memory::service::MemoryService;
use crate::agent::prompt::PromptLoader;
use crate::core::repository::Repository;
use rig::completion::Message as RigMessage;

pub struct AgentConfig {
    pub system_prompt: String,
    pub model_group: String,
    pub tools: Vec<String>,
    pub sandbox_config: Option<crate::agent::models::SandboxSettings>,
    pub identity: std::collections::BTreeMap<String, String>,
}

use super::models::{ChatResponse, CreateChatRequest, UpdateChatRequest};
use super::message::models::{MessageResponse, SendMessageRequest};
use super::message::models::{Message, MessageRole, MessageTool, ToolStatus};
use super::message::repository::MessageRepository;
use super::models::Chat;
use super::repository::ChatRepository;

#[derive(Clone)]
pub struct ChatService {
    chat_repo: SurrealChatRepo,
    message_repo: SurrealMessageRepo,
    agent_repo: SurrealAgentRepo,
    provider_registry: ModelProviderRegistry,
    workspaces: AgentWorkspaceManager,
    memory_service: MemoryService,
    prompts: PromptLoader,
}

impl ChatService {
    pub fn new(
        chat_repo: SurrealChatRepo,
        message_repo: SurrealMessageRepo,
        agent_repo: SurrealAgentRepo,
        provider_registry: ModelProviderRegistry,
        workspaces: AgentWorkspaceManager,
        memory_service: MemoryService,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            chat_repo,
            message_repo,
            agent_repo,
            provider_registry,
            workspaces,
            memory_service,
            prompts,
        }
    }


    pub fn provider_registry(&self) -> &ModelProviderRegistry {
        &self.provider_registry
    }

    pub fn memory_service(&self) -> &MemoryService {
        &self.memory_service
    }

    pub async fn create_chat(
        &self,
        user_id: &str,
        req: CreateChatRequest,
    ) -> Result<ChatResponse, AppError> {
        let now = chrono::Utc::now();
        let chat = Chat {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            space_id: req.space_id,
            task_id: req.task_id,
            agent_id: req.agent_id,
            title: req.title,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };

        let chat = self.chat_repo.create(&chat).await?;
        Ok(chat.into())
    }

    pub async fn get_chat(&self, user_id: &str, chat_id: &str) -> Result<Chat, AppError> {
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

        Ok(chat)
    }

    pub async fn list_chats(&self, user_id: &str) -> Result<Vec<ChatResponse>, AppError> {
        let chats = self.chat_repo.find_by_user_id(user_id).await?;
        Ok(chats.into_iter().map(Into::into).collect())
    }

    pub async fn update_chat(
        &self,
        user_id: &str,
        chat_id: &str,
        req: UpdateChatRequest,
    ) -> Result<ChatResponse, AppError> {
        let mut chat = self.get_chat(user_id, chat_id).await?;

        if let Some(title) = req.title {
            chat.title = Some(title);
        }
        if let Some(space_id) = req.space_id {
            chat.space_id = Some(space_id);
        }
        chat.updated_at = chrono::Utc::now();

        let chat = self.chat_repo.update(&chat).await?;
        Ok(chat.into())
    }

    pub async fn delete_chat(&self, user_id: &str, chat_id: &str) -> Result<(), AppError> {
        self.get_chat(user_id, chat_id).await?;
        self.chat_repo.delete(chat_id).await
    }

    pub async fn archive_chat(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<ChatResponse, AppError> {
        let mut chat = self.get_chat(user_id, chat_id).await?;

        chat.archived_at = Some(chrono::Utc::now());
        chat.updated_at = chrono::Utc::now();
        let chat = self.chat_repo.update(&chat).await?;
        Ok(chat.into())
    }

    pub async fn unarchive_chat(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<ChatResponse, AppError> {
        let mut chat = self.get_chat(user_id, chat_id).await?;

        chat.archived_at = None;
        chat.updated_at = chrono::Utc::now();
        let chat = self.chat_repo.update(&chat).await?;
        Ok(chat.into())
    }

    pub async fn list_archived_chats(
        &self,
        user_id: &str,
    ) -> Result<Vec<ChatResponse>, AppError> {
        let chats = self.chat_repo.find_archived_by_user_id(user_id).await?;
        Ok(chats.into_iter().map(Into::into).collect())
    }

    pub async fn send_message(
        &self,
        user_id: &str,
        chat_id: &str,
        req: SendMessageRequest,
    ) -> Result<Vec<MessageResponse>, AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;

        let title_handle = if chat.title.is_none() {
            let svc = self.clone();
            let agent_id = chat.agent_id.clone();
            let user_content = req.content.clone();
            let cid = chat_id.to_string();
            Some(tokio::spawn(async move {
                if let Err(e) = svc
                    .generate_title(&cid, &agent_id, &user_content)
                    .await
                {
                    tracing::warn!(error = %e, "Title generation failed");
                }
            }))
        } else {
            None
        };

        let user_message = Message::builder(chat_id, MessageRole::User, req.content.clone()).build();
        let user_message = self.message_repo.create(&user_message).await?;

        let agent_config = self.resolve_agent_config(&chat.agent_id).await?;
        let system_prompt = agent_config.system_prompt;
        let model_group_name = agent_config.model_group;

        let stored_messages = self.message_repo.find_by_chat_id(chat_id).await?;
        let mut rig_history = to_rig_messages(&stored_messages, &chat.agent_id);

        let model_group = self.provider_registry.get_model_group(&model_group_name)?;

        rig_history.push(RigMessage::user(&req.content));
        let response_text = text_inference(
            &self.provider_registry,
            model_group,
            &system_prompt,
            rig_history,
            &InferenceMetricsContext::default(),
        )
        .await?;

        let assistant_message = Message::builder(chat_id, MessageRole::Agent, response_text)
            .agent_id(chat.agent_id.clone())
            .build();
        let assistant_message = self.message_repo.create(&assistant_message).await?;

        if let Some(handle) = title_handle {
            let _ = handle.await;
        }

        Ok(vec![user_message.into(), assistant_message.into()])
    }

    pub async fn list_messages(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<Vec<MessageResponse>, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let messages = self.message_repo.find_by_chat_id(chat_id).await?;
        Ok(messages.into_iter().map(Into::into).collect())
    }

    pub async fn create_stream_user_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        attachments: Vec<crate::api::files::Attachment>,
    ) -> Result<MessageResponse, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let msg = Message::builder(chat_id, MessageRole::User, content.to_string())
            .attachments(attachments)
            .build();
        self.save_message(msg).await
    }

    pub async fn create_contact_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        contact_id: Option<&str>,
    ) -> Result<MessageResponse, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let mut builder = Message::builder(chat_id, MessageRole::Contact, content.to_string());
        if let Some(cid) = contact_id {
            builder = builder.contact_id(cid);
        }
        self.save_message(builder.build()).await
    }

    pub async fn save_live_call_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        contact_id: Option<&str>,
    ) -> Result<MessageResponse, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let mut builder = Message::builder(chat_id, MessageRole::LiveCall, content.to_string());
        if let Some(cid) = contact_id {
            builder = builder.contact_id(cid);
        }
        self.save_message(builder.build()).await
    }

    async fn save_message(&self, message: Message) -> Result<MessageResponse, AppError> {
        let saved = self.message_repo.create(&message).await?;
        Ok(saved.into())
    }

    pub async fn save_assistant_message(
        &self,
        chat_id: &str,
        content: String,
    ) -> Result<MessageResponse, AppError> {
        self.save_assistant_message_with_tool_calls(chat_id, content, None, vec![]).await
    }

    pub async fn save_assistant_message_with_tool_calls(
        &self,
        chat_id: &str,
        content: String,
        tool_calls: Option<serde_json::Value>,
        attachments: Vec<crate::api::files::Attachment>,
    ) -> Result<MessageResponse, AppError> {
        let chat = self.chat_repo.find_by_id(chat_id).await?.ok_or_else(|| {
            AppError::NotFound("Chat not found".into())
        })?;
        let mut builder = Message::builder(chat_id, MessageRole::Agent, content)
            .agent_id(chat.agent_id)
            .attachments(attachments);
        if let Some(tc) = tool_calls {
            builder = builder.tool_calls(tc);
        }
        self.save_message(builder.build()).await
    }

    pub async fn save_agent_message(
        &self,
        chat_id: &str,
        agent_id: &str,
        content: String,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::Agent, content)
            .agent_id(agent_id.to_string())
            .build();
        self.save_message(msg).await
    }

    pub async fn save_tool_result_message_with_tool(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        content: String,
        tool: Option<MessageTool>,
        system_prompt: Option<String>,
    ) -> Result<MessageResponse, AppError> {
        let mut builder = Message::builder(chat_id, MessageRole::ToolResult, content)
            .tool_call_id(tool_call_id.to_string());
        if let Some(t) = tool {
            builder = builder.tool(t);
        }
        if let Some(sp) = system_prompt {
            builder = builder.system_prompt(sp);
        }
        self.save_message(builder.build()).await
    }

    pub async fn resolve_tool_message(
        &self,
        message_id: &str,
        response: Option<String>,
    ) -> Result<MessageResponse, AppError> {
        let mut message = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;

        let response_text = response.clone()
            .unwrap_or_else(|| "Human resolved the request.".to_string());

        match &mut message.tool {
            Some(MessageTool::HumanInTheLoop { status, response: resp, .. }) => {
                *status = ToolStatus::Resolved;
                *resp = Some(response_text.clone());
            }
            Some(MessageTool::Question { status, response: resp, .. }) => {
                *status = ToolStatus::Resolved;
                *resp = Some(response_text.clone());
            }
            Some(MessageTool::VaultApproval { status, response: resp, .. }) => {
                *status = ToolStatus::Resolved;
                *resp = Some(response_text.clone());
            }
            _ => return Err(AppError::Validation("Message has no resolvable tool".into())),
        }

        message.content = response_text;

        let updated = self.message_repo.update(&message).await?;
        Ok(updated.into())
    }

    pub async fn deny_tool_message(
        &self,
        message_id: &str,
        response: Option<String>,
    ) -> Result<MessageResponse, AppError> {
        let mut message = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;

        let response_text = response
            .unwrap_or_else(|| "User denied the request.".to_string());

        match &mut message.tool {
            Some(MessageTool::VaultApproval { status, response: resp, .. }) => {
                *status = ToolStatus::Denied;
                *resp = Some(response_text.clone());
            }
            _ => return Err(AppError::Validation("Message has no deniable tool".into())),
        }

        message.content = response_text;

        let updated = self.message_repo.update(&message).await?;
        Ok(updated.into())
    }

    pub async fn save_task_completion_message(
        &self,
        chat_id: &str,
        agent_id: &str,
        content: String,
        tool: MessageTool,
        attachments: Vec<crate::api::files::Attachment>,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::TaskCompletion, content)
            .agent_id(agent_id.to_string())
            .tool(tool)
            .attachments(attachments)
            .build();
        self.save_message(msg).await
    }

    pub async fn save_external_tool_pending(
        &self,
        chat_id: &str,
        accumulated_text: String,
        tool_calls_json: serde_json::Value,
        tool_results: &[crate::inference::tool_loop::ToolCallResult],
        external_tool: Box<crate::inference::tool_loop::ToolCallResult>,
    ) -> Result<MessageResponse, AppError> {
        let _ = self
            .save_assistant_message_with_tool_calls(
                chat_id,
                accumulated_text,
                Some(tool_calls_json),
                vec![],
            )
            .await;

        for tr in tool_results {
            let _ = self
                .save_tool_result_message_with_tool(
                    chat_id,
                    &tr.tool_call_id,
                    tr.result.clone(),
                    tr.tool_data.clone(),
                    None,
                )
                .await;
        }

        self.save_tool_result_message_with_tool(
            chat_id,
            &external_tool.tool_call_id,
            external_tool.result,
            external_tool.tool_data,
            external_tool.system_prompt,
        )
        .await
    }

    pub async fn find_chat(&self, chat_id: &str) -> Result<Option<Chat>, AppError> {
        self.chat_repo.find_by_id(chat_id).await
    }

    pub async fn resolve_agent_config(&self, agent_id: &str) -> Result<AgentConfig, AppError> {
        let ws = self.workspaces.get(agent_id);

        if let Ok(Some(agent)) = self.agent_repo.find_by_id(agent_id).await {
            tracing::info!(agent_id, ?agent.tools, user_id = ?agent.user_id, "Resolved agent from DB");
            let tools = if agent.tools.is_empty() {
                crate::tool::configurable_tools().to_vec()
            } else {
                agent.tools
            };

            let raw_prompt = ws.read("AGENT.md")
                .map(|c| parse_frontmatter(&c).template)
                .ok_or_else(|| AppError::Internal(format!("No AGENT.md found for agent {agent_id}")))?;

            let system_prompt = raw_prompt.replace("{{agent_name}}", &agent.name);

            return Ok(AgentConfig {
                system_prompt,
                model_group: agent.model_group,
                tools,
                sandbox_config: agent.sandbox_config,
                identity: agent.identity,
            });
        }

        let raw_content = ws.read("AGENT.md")
            .ok_or_else(|| AppError::Internal(format!("No AGENT.md found for agent {agent_id}")))?;
        let parsed = parse_frontmatter(&raw_content);

        let model_group = parsed.metadata.get("model_group")
            .cloned()
            .unwrap_or_else(|| "primary".to_string());

        let tools = parsed.metadata.get("tools")
            .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_else(|| crate::tool::configurable_tools().to_vec());

        Ok(AgentConfig {
            system_prompt: parsed.template,
            model_group,
            tools,
            sandbox_config: None,
            identity: std::collections::BTreeMap::new(),
        })
    }

    pub async fn generate_title(
        &self,
        chat_id: &str,
        agent_id: &str,
        user_content: &str,
    ) -> Result<String, AppError> {
        let ws = self.workspaces.get(agent_id);
        let prompts = AgentPromptLoader::new(&ws, &self.prompts);
        let content = prompts.read("TITLE.md")
            .ok_or_else(|| AppError::Internal("No title generation prompt found".into()))?;
        let parsed = parse_frontmatter(&content);

        let model_group = self.build_title_model_group(parsed.metadata.get("model").map(|s| s.as_str()))?;

        let result = text_inference(
            &self.provider_registry,
            &model_group,
            &parsed.template,
            vec![RigMessage::user(user_content)],
            &InferenceMetricsContext::default(),
        )
        .await?;

        let title = parse_title_response(&result, user_content);
        self.update_chat_title(chat_id, &title).await?;
        Ok(title)
    }

    fn build_title_model_group(&self, model_specifier: Option<&str>) -> Result<crate::inference::config::ModelGroup, AppError> {
        let base = match model_specifier {
            Some(m) if m.contains('/') => {
                let model_ref = ModelRef::parse(m)
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                return Ok(crate::inference::config::ModelGroup {
                    name: "title".to_string(),
                    main: model_ref,
                    fallbacks: vec![],
                    max_tokens: Some(100),
                    temperature: Some(0.7),
                    context_window: None,
                    retry: Default::default(),
                    inference: Default::default(),
                });
            }
            Some(group) if !group.is_empty() => {
                self.provider_registry.get_model_group(group)?
            }
            _ => self.provider_registry.get_model_group("primary")?,
        };
        Ok(crate::inference::config::ModelGroup {
            name: "title".to_string(),
            main: base.main.clone(),
            fallbacks: base.fallbacks.clone(),
            max_tokens: Some(100),
            temperature: Some(0.7),
            context_window: None,
            retry: base.retry.clone(),
            inference: base.inference.clone(),
        })
    }

    async fn update_chat_title(&self, chat_id: &str, title: &str) -> Result<(), AppError> {
        if let Some(mut chat) = self.chat_repo.find_by_id(chat_id).await? {
            chat.title = Some(title.to_string());
            chat.updated_at = chrono::Utc::now();
            self.chat_repo.update(&chat).await?;
        }
        Ok(())
    }

    pub async fn get_stored_messages(&self, chat_id: &str) -> Vec<Message> {
        self.message_repo
            .find_by_chat_id(chat_id)
            .await
            .unwrap_or_default()
    }

    pub async fn find_chats_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        self.chat_repo.find_by_space_id(space_id).await
    }

    pub async fn find_standalone_chats_by_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<Chat>, AppError> {
        self.chat_repo.find_standalone_by_user_id(user_id).await
    }

    pub async fn find_attachments_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Result<Vec<crate::api::files::Attachment>, AppError> {
        self.message_repo.find_attachments_by_chat_id(chat_id).await
    }
}

fn parse_title_response(response: &str, fallback_content: &str) -> String {
    if let Some(title) = try_extract_title(response) {
        return title;
    }

    tracing::debug!(response = %response, "Failed to parse title from LLM response, using fallback");
    fallback_content.chars().take(60).collect()
}

fn try_extract_title(response: &str) -> Option<String> {
    let trimmed = response.trim();

    if let Some(title) = try_parse_title_json(trimmed) {
        return Some(title);
    }

    let open = trimmed.find('{')?;
    let close = trimmed.rfind('}')?;
    if open < close
        && let Some(title) = try_parse_title_json(&trimmed[open..=close])
    {
        return Some(title);
    }

    None
}

fn try_parse_title_json(s: &str) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(s).ok()?;
    let title = v.get("title")?.as_str()?;
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_title_response_valid_json() {
        let response = r#"{ "title": "🍝 Pasta Carbonara Recipe" }"#;
        assert_eq!(
            parse_title_response(response, "fallback"),
            "🍝 Pasta Carbonara Recipe"
        );
    }

    #[test]
    fn test_parse_title_response_markdown_fenced() {
        let response = "```json\n{ \"title\": \"🍝 Pasta\" }\n```";
        assert_eq!(parse_title_response(response, "fallback"), "🍝 Pasta");
    }

    #[test]
    fn test_parse_title_response_markdown_fenced_multiline() {
        let response = "```json\n{\n  \"title\": \"⚽ World Cup Organizer\"\n}\n```";
        assert_eq!(
            parse_title_response(response, "fallback"),
            "⚽ World Cup Organizer"
        );
    }

    #[test]
    fn test_parse_title_response_extra_text_around_json() {
        let response = "Here is the title:\n{ \"title\": \"🎯 Test\" }\nDone.";
        assert_eq!(parse_title_response(response, "fallback"), "🎯 Test");
    }

    #[test]
    fn test_parse_title_response_invalid_json() {
        let response = "not json at all";
        assert_eq!(
            parse_title_response(response, "How do I make pasta carbonara?"),
            "How do I make pasta carbonara?"
        );
    }

    #[test]
    fn test_parse_title_response_empty_title() {
        let response = r#"{ "title": "" }"#;
        assert_eq!(parse_title_response(response, "fallback text"), "fallback text");
    }
}
