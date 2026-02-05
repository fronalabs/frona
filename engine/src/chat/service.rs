use crate::agent::config::parse_frontmatter;
use crate::agent::workspace::{AgentPromptLoader, AgentWorkspaceManager};
use crate::api::repo::agents::SurrealAgentRepo;
use crate::api::repo::chats::SurrealChatRepo;
use crate::api::repo::messages::SurrealMessageRepo;
use crate::error::AppError;
use crate::llm::ModelProviderRegistry;
use crate::llm::convert::to_rig_messages;
use crate::llm::fallback::inference_with_fallback;
use crate::llm::provider::ModelRef;
use crate::memory::service::MemoryService;
use crate::prompt::PromptLoader;
use crate::repository::Repository;
use rig::completion::Message as RigMessage;

pub struct AgentConfig {
    pub system_prompt: String,
    pub model_group: String,
    pub tools: Vec<String>,
    pub sandbox_config: Option<crate::agent::models::SandboxSettings>,
    pub identity: std::collections::BTreeMap<String, String>,
}

use super::dto::{ChatResponse, CreateChatRequest, UpdateChatRequest};
use super::message::dto::{MessageResponse, SendMessageRequest};
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

    pub async fn get_chat(&self, user_id: &str, chat_id: &str) -> Result<ChatResponse, AppError> {
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

        Ok(chat.into())
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
        let mut chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

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
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

        self.chat_repo.delete(chat_id).await
    }

    pub async fn archive_chat(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<ChatResponse, AppError> {
        let mut chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

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
        let mut chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

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
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

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

        let now = chrono::Utc::now();

        let user_message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::User,
            content: req.content.clone(),
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
            created_at: now,
        };
        let user_message = self.message_repo.create(&user_message).await?;

        let agent_config = self.resolve_agent_config(&chat.agent_id).await?;
        let system_prompt = agent_config.system_prompt;
        let model_group_name = agent_config.model_group;

        let stored_messages = self.message_repo.find_by_chat_id(chat_id).await?;
        let rig_history = to_rig_messages(&stored_messages, &chat.agent_id);

        let model_group = self.provider_registry.get_model_group(&model_group_name)?;

        let user_rig_msg = RigMessage::user(&req.content);
        let response_text = inference_with_fallback(
            &self.provider_registry,
            model_group,
            &system_prompt,
            rig_history,
            user_rig_msg,
        )
        .await?;

        let assistant_message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::Agent,
            content: response_text,
            agent_id: Some(chat.agent_id.clone()),
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
            created_at: chrono::Utc::now(),
        };
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
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

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
        let chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your chat".into()));
        }

        let user_message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments,
            created_at: chrono::Utc::now(),
        };
        let saved = self.message_repo.create(&user_message).await?;
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
        let assistant_message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::Agent,
            content,
            agent_id: Some(chat.agent_id),
            tool_calls,
            tool_call_id: None,
            tool: None,
            attachments,
            created_at: chrono::Utc::now(),
        };
        let saved = self.message_repo.create(&assistant_message).await?;
        Ok(saved.into())
    }

    pub async fn save_agent_message(
        &self,
        chat_id: &str,
        agent_id: &str,
        content: String,
    ) -> Result<MessageResponse, AppError> {
        let message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::Agent,
            content,
            agent_id: Some(agent_id.to_string()),
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
            created_at: chrono::Utc::now(),
        };
        let saved = self.message_repo.create(&message).await?;
        Ok(saved.into())
    }

    pub async fn save_tool_result_message(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        content: String,
    ) -> Result<MessageResponse, AppError> {
        self.save_tool_result_message_with_tool(chat_id, tool_call_id, content, None).await
    }

    pub async fn save_tool_result_message_with_tool(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        content: String,
        tool: Option<MessageTool>,
    ) -> Result<MessageResponse, AppError> {
        let message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::ToolResult,
            content,
            agent_id: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
            tool,
            attachments: vec![],
            created_at: chrono::Utc::now(),
        };
        let saved = self.message_repo.create(&message).await?;
        Ok(saved.into())
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
            _ => return Err(AppError::Validation("Message has no resolvable tool".into())),
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
        let message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            role: MessageRole::TaskCompletion,
            content,
            agent_id: Some(agent_id.to_string()),
            tool_calls: None,
            tool_call_id: None,
            tool: Some(tool),
            attachments,
            created_at: chrono::Utc::now(),
        };
        let saved = self.message_repo.create(&message).await?;
        Ok(saved.into())
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

        let raw_prompt = ws.read("AGENT.md")
            .map(|c| parse_frontmatter(&c).template)
            .ok_or_else(|| AppError::Internal(format!("No AGENT.md found for agent {agent_id}")))?;

        Ok(AgentConfig {
            system_prompt: raw_prompt,
            model_group: "primary".to_string(),
            tools: crate::tool::configurable_tools().to_vec(),
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

        let model_group_name = match parsed.metadata.get("model") {
            Some(m) if m.contains('/') => {
                let model_ref = ModelRef::parse(m)
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                let model_group = crate::llm::config::ModelGroup {
                    main: model_ref,
                    fallbacks: vec![],
                    max_tokens: Some(100),
                    temperature: Some(0.7),
                    context_window: None,
                    retry: Default::default(),
                };
                let user_msg = RigMessage::user(user_content);
                let result = inference_with_fallback(
                    &self.provider_registry,
                    &model_group,
                    &parsed.template,
                    vec![],
                    user_msg,
                )
                .await?;
                let title = parse_title_response(&result, user_content);
                self.update_chat_title(chat_id, &title).await?;
                return Ok(title);
            }
            Some(group) if !group.is_empty() => group.clone(),
            _ => "primary".to_string(),
        };

        let model_group = self.provider_registry.get_model_group(&model_group_name)?;
        let title_group = crate::llm::config::ModelGroup {
            main: model_group.main.clone(),
            fallbacks: model_group.fallbacks.clone(),
            max_tokens: Some(100),
            temperature: Some(0.7),
            context_window: None,
            retry: model_group.retry.clone(),
        };

        let user_msg = RigMessage::user(user_content);
        let result = inference_with_fallback(
            &self.provider_registry,
            &title_group,
            &parsed.template,
            vec![],
            user_msg,
        )
        .await?;

        let title = parse_title_response(&result, user_content);
        self.update_chat_title(chat_id, &title).await?;
        Ok(title)
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
