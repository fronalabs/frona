use crate::agent::config::parse_frontmatter;
use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::workspace::AgentPromptLoader;
use crate::auth::UserService;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::core::template::render_template;
use crate::db::repo::chats::SurrealChatRepo;
use crate::db::repo::messages::SurrealMessageRepo;
use crate::inference::conversation::{
    ConversationBuilder, ConversationContext, DefaultConversationBuilder,
};
use crate::inference::provider::service::ModelProviderService;
use crate::inference::text_inference;
use crate::storage::StorageService;
use rig_core::completion::Message as RigMessage;

// Reasoning models may consume output tokens before emitting the short text
// response. A 100-token ceiling can therefore yield a successful completion
// containing reasoning only, which leaves the chat untitled.
const TITLE_MAX_TOKENS: u64 = 1024;

pub struct AgentConfig {
    pub system_prompt: String,
    pub model_group: String,
    pub skills: Option<Vec<String>>,
    pub sandbox_limits: Option<crate::core::config::SandboxLimits>,
    pub identity: std::collections::BTreeMap<String, String>,
}

use super::message::models::{Message, MessageRole, Reasoning};
use super::message::models::{
    MessageEvent, MessageResponse, MessageStatus, PaginatedMessagesResponse, SendMessageRequest,
};
use super::message::repository::MessageRepository;
use super::models::Chat;
use super::models::{ChatResponse, CreateChatRequest, UpdateChatRequest};
use super::repository::ChatRepository;
use crate::db::repo::tool_calls::ToolCallRepository;
use crate::inference::tool_call::{ToolCall, ToolCallResponse, ToolStatus};
pub enum ToolResolveResult {
    Changed(MessageResponse),
    AlreadyResolved(MessageResponse),
}

impl ToolResolveResult {
    pub fn into_message(self) -> MessageResponse {
        match self {
            Self::Changed(msg) | Self::AlreadyResolved(msg) => msg,
        }
    }
}

#[derive(Clone)]
pub struct ChatService {
    chat_repo: SurrealChatRepo,
    message_repo: SurrealMessageRepo,
    tool_call_repo: crate::db::repo::tool_calls::SurrealToolCallRepo,
    agent_service: AgentService,
    model_providers: ModelProviderService,
    storage_service: StorageService,
    user_service: UserService,
    prompts: PromptLoader,
    broadcast: crate::chat::broadcast::BroadcastService,
    presign: crate::credential::presign::PresignService,
    usage_service: crate::inference::usage::UsageService,
    compactor: super::compactor::ChatCompactor,
}

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chat_repo: SurrealChatRepo,
        message_repo: SurrealMessageRepo,
        tool_call_repo: crate::db::repo::tool_calls::SurrealToolCallRepo,
        agent_service: AgentService,
        model_providers: ModelProviderService,
        storage_service: StorageService,
        user_service: UserService,
        prompts: PromptLoader,
        broadcast: crate::chat::broadcast::BroadcastService,
        presign: crate::credential::presign::PresignService,
        usage_service: crate::inference::usage::UsageService,
    ) -> Self {
        let compactor = super::compactor::ChatCompactor::new(
            crate::db::repo::chat_summaries::SurrealChatSummaryRepo::new(message_repo.db().clone()),
            message_repo.clone(),
            std::sync::Arc::new(super::compactor::TextInferenceSummarizer::new(
                model_providers.clone(),
                usage_service.clone(),
            )),
            prompts.clone(),
        );
        Self {
            chat_repo,
            message_repo,
            tool_call_repo,
            agent_service,
            model_providers,
            storage_service,
            user_service,
            prompts,
            broadcast,
            presign,
            usage_service,
            compactor,
        }
    }

    fn broadcast_chat_entity(&self, chat: &Chat, action: crate::chat::broadcast::EntityAction) {
        self.broadcast.broadcast_entity_updated(
            &chat.user_id,
            "chat",
            &chat.id,
            action,
            chat.space_id.clone(),
            None,
        );
    }

    fn broadcast_message_persisted(
        &self,
        msg: &Message,
        chat: &Chat,
        action: crate::chat::broadcast::EntityAction,
    ) {
        self.broadcast.broadcast_entity_updated(
            &chat.user_id,
            "message",
            &msg.id,
            action,
            chat.space_id.clone(),
            None,
        );
        self.broadcast.broadcast_chat_message(
            &chat.user_id,
            &chat.id,
            chat.space_id.clone(),
            msg.clone().into(),
        );
    }

    pub fn model_providers(&self) -> &ModelProviderService {
        &self.model_providers
    }

    pub fn usage_service(&self) -> &crate::inference::usage::UsageService {
        &self.usage_service
    }

    pub fn compactor(&self) -> &super::compactor::ChatCompactor {
        &self.compactor
    }

    pub async fn create_chat(
        &self,
        user_id: &str,
        req: CreateChatRequest,
    ) -> Result<ChatResponse, AppError> {
        // Fail eagerly on bad `agent_id` - first /messages/stream would silently 404.
        let agent = self
            .agent_service
            .find_by_id(&req.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent '{}' not found", req.agent_id)))?;
        if agent.user_id != user_id {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        let now = chrono::Utc::now();
        let chat = Chat {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            space_id: req.space_id,
            task_id: req.task_id,
            agent_id: req.agent_id,
            title: req.title,
            archived_at: None,
            channel_id: None,
            channel_external_id: None,
            metadata: req.metadata.unwrap_or_default(),
            created_at: now,
            updated_at: now,
        };

        let chat = self.chat_repo.create(&chat).await?;
        self.broadcast_chat_entity(&chat, crate::chat::broadcast::EntityAction::Created);
        Ok(chat.into())
    }

    pub async fn get_message(&self, user_id: &str, message_id: &str) -> Result<Message, AppError> {
        let msg = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        let chat = self
            .chat_repo
            .find_by_id(&msg.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your message".into()));
        }
        Ok(msg)
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
        if let Some(patch) = req.metadata {
            crate::core::metadata::apply_metadata_patch(&mut chat.metadata, patch);
        }
        chat.updated_at = chrono::Utc::now();

        let chat = self.chat_repo.update(&chat).await?;
        self.broadcast_chat_entity(&chat, crate::chat::broadcast::EntityAction::Updated);
        Ok(chat.into())
    }

    pub async fn upsert_channel_chat(
        &self,
        user_id: &str,
        space_id: &str,
        agent_id: &str,
        channel_id: &str,
        channel_external_id: &str,
        title: Option<&str>,
    ) -> Result<Chat, AppError> {
        if let Some(c) = self
            .chat_repo
            .find_by_channel_thread(channel_id, channel_external_id)
            .await?
        {
            return Ok(c);
        }
        let now = chrono::Utc::now();
        let chat = Chat {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            space_id: Some(space_id.to_string()),
            task_id: None,
            agent_id: agent_id.to_string(),
            title: title.map(|s| s.to_string()),
            archived_at: None,
            channel_id: Some(channel_id.to_string()),
            channel_external_id: Some(channel_external_id.to_string()),
            metadata: std::collections::BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        let saved = self.chat_repo.create(&chat).await?;
        self.broadcast_chat_entity(&saved, crate::chat::broadcast::EntityAction::Created);
        Ok(saved)
    }

    pub async fn patch_chat_metadata(
        &self,
        chat_id: &str,
        patch: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<Chat, AppError> {
        let mut chat = self
            .chat_repo
            .find_by_id(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
        crate::core::metadata::apply_metadata_patch(&mut chat.metadata, patch);
        chat.updated_at = chrono::Utc::now();
        let saved = self.chat_repo.update(&chat).await?;
        self.broadcast_chat_entity(&saved, crate::chat::broadcast::EntityAction::Updated);
        Ok(saved)
    }

    pub async fn update_message_metadata(
        &self,
        user_id: &str,
        message_id: &str,
        req: crate::chat::message::models::UpdateMessageRequest,
    ) -> Result<crate::chat::message::models::MessageResponse, AppError> {
        let mut msg = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        let chat = self
            .chat_repo
            .find_by_id(&msg.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
        if chat.user_id != user_id {
            return Err(AppError::Forbidden("Not your message".into()));
        }
        if let Some(patch) = req.metadata {
            crate::core::metadata::apply_metadata_patch(&mut msg.metadata, patch);
        }
        let saved = self.message_repo.update(&msg).await?;
        self.broadcast_message_persisted(
            &saved,
            &chat,
            crate::chat::broadcast::EntityAction::Updated,
        );
        Ok(saved.into())
    }

    pub async fn persist_inbound_message(&self, msg: &Message) -> Result<Message, AppError> {
        let saved = self.message_repo.create(msg).await?;
        if let Ok(Some(chat)) = self.chat_repo.find_by_id(&saved.chat_id).await {
            self.broadcast_message_persisted(
                &saved,
                &chat,
                crate::chat::broadcast::EntityAction::Created,
            );
        }
        Ok(saved)
    }

    /// Update an existing message row from a fully-constructed `Message` and
    /// broadcast the change. Used by the command dispatch path so a handler
    /// can mutate the placeholder agent message (e.g. swap its `agent_id`)
    /// and have the harness persist the change in a single call.
    pub async fn save_updated_message(&self, msg: &Message) -> Result<Message, AppError> {
        let saved = self.message_repo.update(msg).await?;
        if let Ok(Some(chat)) = self.chat_repo.find_by_id(&saved.chat_id).await {
            self.broadcast_message_persisted(
                &saved,
                &chat,
                crate::chat::broadcast::EntityAction::Updated,
            );
        }
        Ok(saved)
    }

    pub async fn patch_message_metadata(
        &self,
        message_id: &str,
        patch: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<Message, AppError> {
        let mut msg = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        crate::core::metadata::apply_metadata_patch(&mut msg.metadata, patch);
        let saved = self.message_repo.update(&msg).await?;
        if let Ok(Some(chat)) = self.chat_repo.find_by_id(&saved.chat_id).await {
            self.broadcast_message_persisted(
                &saved,
                &chat,
                crate::chat::broadcast::EntityAction::Updated,
            );
        }
        Ok(saved)
    }

    pub async fn delete_chat(&self, user_id: &str, chat_id: &str) -> Result<(), AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;
        self.chat_repo.delete(chat_id).await?;
        self.broadcast_chat_entity(&chat, crate::chat::broadcast::EntityAction::Deleted);
        Ok(())
    }

    /// Wipe every message in a chat without deleting the chat itself.
    /// Used by `/clear`. The chat row stays intact (same id, agent, title);
    /// the message history goes away. Broadcasts an `EntityUpdated` event
    /// keyed on the chat so SSE listeners refresh.
    pub async fn delete_messages_for_chat(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<(), AppError> {
        // Authorize by ownership: get_chat returns NotFound otherwise.
        let chat = self.get_chat(user_id, chat_id).await?;
        self.message_repo.delete_by_chat_id(chat_id).await?;
        self.broadcast_chat_entity(&chat, crate::chat::broadcast::EntityAction::Updated);
        Ok(())
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

    pub async fn list_archived_chats(&self, user_id: &str) -> Result<Vec<ChatResponse>, AppError> {
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
                if let Err(e) = svc.generate_title(&cid, &agent_id, &user_content).await {
                    tracing::warn!(error = %e, "Title generation failed");
                }
            }))
        } else {
            None
        };

        let mut user_message_builder =
            Message::builder(chat_id, MessageRole::User, req.content.clone());
        if let Some(ref md) = req.metadata {
            user_message_builder = user_message_builder.metadata(md.clone());
        }
        let user_message = user_message_builder.build();
        let user_message = self
            .save_message_and_broadcast(
                user_message,
                user_id,
                chat.space_id.as_deref(),
                crate::chat::broadcast::EntityAction::Created,
            )
            .await?;

        let agent_config = self.resolve_agent_config(&chat.agent_id).await?;
        let system_prompt = agent_config.system_prompt;
        let model_group_name = agent_config.model_group;

        let model_group = self
            .model_providers
            .resolve(&crate::inference::ModelRef(model_group_name.clone().into()))?;
        let max_output = model_group
            .max_tokens
            .unwrap_or(model_group.inference.default_max_tokens) as usize;
        let loaded = self
            .compactor
            .compact_chat(
                user_id,
                chat_id,
                &chat.agent_id,
                &system_prompt,
                model_group.context_window,
                max_output,
            )
            .await?
            .conversation;
        let conversation_summary = loaded.summary;
        let stored_messages = loaded.messages;
        let conv_builder = DefaultConversationBuilder {
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            agent_service: self.agent_service.clone(),
        };
        let conv_ctx = ConversationContext {
            agent_id: chat.agent_id.clone(),
            model_config: model_group.main.clone(),
            user_id: user_id.to_string(),
        };
        let tool_calls = self.get_tool_calls(chat_id).await?;
        let mut rig_history = conv_builder
            .build(
                &stored_messages,
                &tool_calls,
                &conv_ctx,
                conversation_summary.as_deref(),
            )
            .await;

        rig_history.push(RigMessage::user(&req.content));
        // Pre-allocate the message id so the usage row's message_id matches
        // the assistant Message that's about to be persisted.
        let assistant_message_id = crate::core::repository::new_id();
        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Text {
                agent_id: chat.agent_id.clone(),
                chat_id: chat_id.to_string(),
                message_id: assistant_message_id.clone(),
            },
            user_id,
            model_group.name.clone(),
        );
        let response_text = text_inference(
            &model_group,
            &system_prompt,
            rig_history,
            &self.usage_service,
            &usage_ctx,
        )
        .await?;

        let mut assistant_message = Message::builder(chat_id, MessageRole::Agent, response_text)
            .agent_id(chat.agent_id.clone())
            .status(MessageStatus::Completed)
            .build();
        assistant_message.id = assistant_message_id;
        let assistant_message = self
            .save_message_and_broadcast(
                assistant_message,
                user_id,
                chat.space_id.as_deref(),
                crate::chat::broadcast::EntityAction::Created,
            )
            .await?;

        if let Some(handle) = title_handle {
            let _ = handle.await;
        }

        Ok(vec![user_message, assistant_message])
    }

    pub async fn list_messages(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<Vec<MessageResponse>, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let messages = self.message_repo.find_by_chat_id(chat_id).await?;
        let tool_calls = self.get_tool_calls(chat_id).await.unwrap_or_default();

        let mut te_map: std::collections::HashMap<String, Vec<ToolCallResponse>> =
            std::collections::HashMap::new();
        for te in tool_calls {
            te_map
                .entry(te.message_id.clone())
                .or_default()
                .push(te.into());
        }

        Ok(messages
            .into_iter()
            .map(|msg| {
                let id = msg.id.clone();
                let mut resp: MessageResponse = msg.into();
                if let Some(tes) = te_map.remove(&id) {
                    resp.tool_calls = tes;
                }
                resp
            })
            .collect())
    }

    pub async fn list_messages_paginated(
        &self,
        user_id: &str,
        chat_id: &str,
        before: Option<chrono::DateTime<chrono::Utc>>,
        after: Option<chrono::DateTime<chrono::Utc>>,
        limit: u32,
    ) -> Result<PaginatedMessagesResponse, AppError> {
        self.get_chat(user_id, chat_id).await?;

        let fetch_limit = limit + 1;
        let mut messages = self
            .message_repo
            .find_by_chat_id_page(chat_id, before, after, fetch_limit)
            .await?;

        let has_more = messages.len() > limit as usize;
        if has_more {
            trim_overfetched_page(&mut messages, limit as usize, after.is_some());
        }

        let message_ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
        let tool_calls = self
            .tool_call_repo
            .find_by_message_ids(&message_ids)
            .await
            .unwrap_or_default();

        let mut te_map: std::collections::HashMap<String, Vec<ToolCallResponse>> =
            std::collections::HashMap::new();
        for te in tool_calls {
            te_map
                .entry(te.message_id.clone())
                .or_default()
                .push(te.into());
        }

        let messages = messages
            .into_iter()
            .map(|msg| {
                let id = msg.id.clone();
                let mut resp: MessageResponse = msg.into();
                if let Some(tes) = te_map.remove(&id) {
                    resp.tool_calls = tes;
                }
                resp
            })
            .collect();

        Ok(PaginatedMessagesResponse { messages, has_more })
    }

    pub async fn create_stream_user_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        attachments: Vec<crate::storage::Attachment>,
        command: Option<crate::chat::message::models::MessageCommand>,
    ) -> Result<MessageResponse, AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;

        let mut builder = Message::builder(chat_id, MessageRole::User, content.to_string())
            .attachments(attachments);
        if let Some(c) = command {
            builder = builder.command(c);
        }
        let msg = builder.build();
        self.save_message_and_broadcast(
            msg,
            user_id,
            chat.space_id.as_deref(),
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    pub async fn create_contact_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        contact_id: Option<&str>,
    ) -> Result<MessageResponse, AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;

        let mut builder = Message::builder(chat_id, MessageRole::Contact, content.to_string());
        if let Some(cid) = contact_id {
            builder = builder.contact_id(cid);
        }
        self.save_message_and_broadcast(
            builder.build(),
            user_id,
            chat.space_id.as_deref(),
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    pub async fn save_live_call_message(
        &self,
        user_id: &str,
        chat_id: &str,
        content: &str,
        contact_id: Option<&str>,
    ) -> Result<MessageResponse, AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;

        let mut builder = Message::builder(chat_id, MessageRole::LiveCall, content.to_string());
        if let Some(cid) = contact_id {
            builder = builder.contact_id(cid);
        }
        self.save_message_and_broadcast(
            builder.build(),
            user_id,
            chat.space_id.as_deref(),
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    /// Skips broadcasting - an empty Executing row would render as a phantom
    /// message in the UI while tokens stream.
    async fn save_message(&self, message: Message) -> Result<MessageResponse, AppError> {
        let saved = self.message_repo.create(&message).await?;
        Ok(saved.into())
    }

    /// Presigns attachments before broadcasting; without it the file widget
    /// renders broken until refresh because live SSE wouldn't match the GET.
    async fn save_message_and_broadcast(
        &self,
        message: Message,
        user_id: &str,
        space_id: Option<&str>,
        action: crate::chat::broadcast::EntityAction,
    ) -> Result<MessageResponse, AppError> {
        let saved = self.message_repo.create(&message).await?;

        let mut response: MessageResponse = saved.clone().into();
        crate::credential::presign::presign_response_by_user_id(
            &self.presign,
            &mut response,
            user_id,
        )
        .await;

        let space_id_owned = space_id.map(String::from);
        self.broadcast.broadcast_entity_updated(
            user_id,
            "message",
            &saved.id,
            action,
            space_id_owned.clone(),
            None,
        );
        self.broadcast.broadcast_chat_message(
            user_id,
            &saved.chat_id,
            space_id_owned,
            response.clone(),
        );
        Ok(response)
    }

    pub async fn save_system_event(
        &self,
        user_id: &str,
        space_id: Option<&str>,
        chat_id: &str,
        event: MessageEvent,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::System, String::new())
            .event(event)
            .build();
        self.save_message_and_broadcast(
            msg,
            user_id,
            space_id,
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    pub async fn save_system_message(
        &self,
        user_id: &str,
        space_id: Option<&str>,
        chat_id: &str,
        content: String,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::System, content).build();
        self.save_message_and_broadcast(
            msg,
            user_id,
            space_id,
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    pub async fn save_agent_message(
        &self,
        user_id: &str,
        space_id: Option<&str>,
        chat_id: &str,
        agent_id: &str,
        content: String,
        delivery: Option<crate::chat::message::models::MessageDelivery>,
    ) -> Result<MessageResponse, AppError> {
        let mut builder =
            Message::builder(chat_id, MessageRole::Agent, content).agent_id(agent_id.to_string());
        if let Some(d) = delivery {
            builder = builder.delivery(d);
        }
        self.save_message_and_broadcast(
            builder.build(),
            user_id,
            space_id,
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn save_task_lifecycle_message(
        &self,
        user_id: &str,
        space_id: Option<&str>,
        chat_id: &str,
        agent_id: &str,
        content: String,
        event: MessageEvent,
        attachments: Vec<crate::storage::Attachment>,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::TaskCompletion, content)
            .agent_id(agent_id.to_string())
            .event(event)
            .attachments(attachments)
            .build();
        self.save_message_and_broadcast(
            msg,
            user_id,
            space_id,
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    pub async fn create_executing_agent_message(
        &self,
        chat_id: &str,
        agent_id: &str,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::Agent, String::new())
            .agent_id(agent_id.to_string())
            .status(MessageStatus::Executing)
            .build();
        self.save_message(msg).await
    }

    /// Stamps `dispatch_mode = Signal` so the outbound dispatcher refuses delivery.
    pub async fn create_executing_signal_message(
        &self,
        chat_id: &str,
        agent_id: &str,
    ) -> Result<MessageResponse, AppError> {
        let msg = Message::builder(chat_id, MessageRole::Agent, String::new())
            .agent_id(agent_id.to_string())
            .status(MessageStatus::Executing)
            .dispatch_mode(crate::chat::channel::DispatchMode::Signal)
            .build();
        self.save_message(msg).await
    }

    /// Mark `msg` as completed. The caller is responsible for pre-filling
    /// `content`, `attachments`, `reasoning`, `agent_id`, and any other field
    /// the completed reply should carry. We just set status + write + broadcast.
    pub async fn complete_agent_message(
        &self,
        mut msg: Message,
    ) -> Result<MessageResponse, AppError> {
        msg.status = Some(MessageStatus::Completed);
        msg.error = None;
        let updated = self.message_repo.update(&msg).await?;
        let chat = self.chat_repo.find_by_id(&updated.chat_id).await?;

        let mut response: MessageResponse = updated.clone().into();
        if let Ok(tes) = self.get_tool_calls_by_message(&updated.id).await {
            response.tool_calls = tes.into_iter().map(Into::into).collect();
        }
        if let Some(ref chat) = chat {
            crate::credential::presign::presign_response_by_user_id(
                &self.presign,
                &mut response,
                &chat.user_id,
            )
            .await;
        }

        if let Some(chat) = chat {
            self.broadcast.broadcast_entity_updated(
                &chat.user_id,
                "message",
                &updated.id,
                crate::chat::broadcast::EntityAction::Updated,
                chat.space_id.clone(),
                None,
            );
            self.broadcast.send(crate::chat::broadcast::BroadcastEvent {
                user_id: chat.user_id.clone(),
                chat_id: Some(chat.id.clone()),
                space_id: chat.space_id.clone(),
                kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                    crate::inference::tool_loop::InferenceEventKind::Done {
                        message: response.clone(),
                    },
                ),
            });
        }
        Ok(response)
    }

    /// Mark `msg` as cancelled. Caller pre-fills any fields it wants set.
    pub async fn cancel_agent_message(
        &self,
        mut msg: Message,
    ) -> Result<MessageResponse, AppError> {
        msg.status = Some(MessageStatus::Cancelled);
        msg.error = None;
        let updated = self.message_repo.update(&msg).await?;
        if let Ok(Some(chat)) = self.chat_repo.find_by_id(&updated.chat_id).await {
            self.broadcast.broadcast_entity_updated(
                &chat.user_id,
                "message",
                &updated.id,
                crate::chat::broadcast::EntityAction::Updated,
                chat.space_id.clone(),
                None,
            );
            self.broadcast.send(crate::chat::broadcast::BroadcastEvent {
                user_id: chat.user_id.clone(),
                chat_id: Some(chat.id.clone()),
                space_id: chat.space_id.clone(),
                kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                    crate::inference::tool_loop::InferenceEventKind::Cancelled {
                        reason: "Cancelled".to_string(),
                    },
                ),
            });
        }
        Ok(updated.into())
    }

    /// Persist the failure so live and reloaded chats show the same error.
    pub async fn fail_agent_message(
        &self,
        mut msg: Message,
        error: super::message::error::MessageError,
    ) -> Result<MessageResponse, AppError> {
        msg.status = Some(MessageStatus::Failed);
        msg.error = Some(error.clone());
        let updated = self.message_repo.update(&msg).await?;
        if let Ok(Some(chat)) = self.chat_repo.find_by_id(&updated.chat_id).await {
            self.broadcast.broadcast_entity_updated(
                &chat.user_id,
                "message",
                &updated.id,
                crate::chat::broadcast::EntityAction::Updated,
                chat.space_id.clone(),
                None,
            );
            self.broadcast.send(crate::chat::broadcast::BroadcastEvent {
                user_id: chat.user_id.clone(),
                chat_id: Some(chat.id.clone()),
                space_id: chat.space_id.clone(),
                kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                    crate::inference::tool_loop::InferenceEventKind::Failed {
                        error,
                        message_id: updated.id.clone(),
                    },
                ),
            });
        }
        Ok(updated.into())
    }

    /// Mark `msg` as paused (for external tool resolution). Idempotent on
    /// status. `reason` + `tool_calls` are piggybacked on the broadcast event.
    pub async fn pause_agent_message(
        &self,
        mut msg: Message,
        reason: crate::inference::tool_loop::PauseReason,
        tool_calls: Vec<crate::inference::tool_call::ToolCallResponse>,
    ) -> Result<(), AppError> {
        if !matches!(msg.status, Some(MessageStatus::Paused)) {
            msg.status = Some(MessageStatus::Paused);
            msg.error = None;
            msg = self.message_repo.update(&msg).await?;
        }
        let chat = match self.chat_repo.find_by_id(&msg.chat_id).await? {
            Some(c) => c,
            None => return Ok(()),
        };
        let mut response: MessageResponse = msg.into();
        response.tool_calls = tool_calls;
        self.broadcast.send(crate::chat::broadcast::BroadcastEvent {
            user_id: chat.user_id.clone(),
            chat_id: Some(chat.id.clone()),
            space_id: chat.space_id.clone(),
            kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                crate::inference::tool_loop::InferenceEventKind::Paused {
                    reason,
                    message: response,
                },
            ),
        });
        Ok(())
    }

    /// Generic chat row update + broadcast. Mirror of `save_updated_message`
    /// for the Chat entity. Used by the command dispatch path so handlers can
    /// mutate `ctx.chat.*` and have the harness persist the result.
    pub async fn save_chat(&self, chat: &Chat) -> Result<Chat, AppError> {
        let updated = self.chat_repo.update(chat).await?;
        self.broadcast_chat_entity(&updated, crate::chat::broadcast::EntityAction::Updated);
        Ok(updated)
    }

    /// Returns `true` iff this call performed the flip. Callers racing on the
    /// last HITL of a message must use this as the dedup signal - the loser
    /// sees `false` and must skip the resume spawn.
    pub async fn mark_message_executing(&self, message_id: &str) -> Result<bool, AppError> {
        let query = "UPDATE message
            SET status = $new_status, error = NONE
            WHERE meta::id(id) = $msg_id
              AND status = $old_status
              AND array::len(
                    (SELECT VALUE id FROM tool_call
                     WHERE message_id = $msg_id
                       AND hitl.status = $pending)
                  ) = 0
            RETURN meta::id(id) AS id";
        let mut result = self
            .message_repo
            .db()
            .query(query)
            .bind(("msg_id", message_id.to_string()))
            .bind(("new_status", MessageStatus::Executing))
            .bind(("old_status", MessageStatus::Paused))
            .bind(("pending", crate::inference::tool_call::ToolStatus::Pending))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<serde_json::Value> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(!rows.is_empty())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn begin_tool_call(
        &self,
        id: &str,
        chat_id: &str,
        message_id: &str,
        turn: u32,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
        description: Option<String>,
        turn_text: Option<String>,
        turn_reasoning: Option<Reasoning>,
    ) -> Result<ToolCall, AppError> {
        let te = ToolCall {
            id: id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: message_id.to_string(),
            turn,
            provider_call_id: tool_call_id.to_string(),
            name: tool_name.to_string(),
            arguments: arguments.clone(),
            result: String::new(),
            success: false,
            duration_ms: 0,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description,
            turn_text,
            turn_reasoning,
            created_at: chrono::Utc::now(),
        };
        self.tool_call_repo.create(&te).await?;
        Ok(te)
    }

    pub async fn finish_tool_call(
        &self,
        tool_call_id: &str,
        result: String,
        success: bool,
        duration_ms: u64,
        system_prompt: Option<String>,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        te.result = result;
        te.success = success;
        te.duration_ms = duration_ms;
        te.system_prompt = system_prompt;
        self.tool_call_repo.update(&te).await?;
        Ok(())
    }

    pub async fn set_hitl(
        &self,
        tool_call_id: &str,
        hitl: crate::inference::hitl::Hitl,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        te.hitl = Some(hitl);
        self.tool_call_repo.update(&te).await?;
        Ok(())
    }

    pub async fn set_task_event(
        &self,
        tool_call_id: &str,
        event: crate::inference::tool_call::TaskEvent,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        te.task_event = Some(event);
        self.tool_call_repo.update(&te).await?;
        Ok(())
    }

    pub async fn set_hitl_delivery(
        &self,
        tool_call_id: &str,
        delivery: crate::inference::hitl::HitlDelivery,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        if let Some(ref mut h) = te.hitl {
            h.delivery = Some(delivery);
            self.tool_call_repo.update(&te).await?;
        }
        Ok(())
    }

    pub async fn resolve_tool_call(
        &self,
        tool_call_id: &str,
        response: Option<String>,
    ) -> Result<ToolResolveResult, AppError> {
        self.resolve_tool_call_with_hitl_response(tool_call_id, response, None)
            .await
    }

    pub async fn resolve_tool_call_with_hitl_response(
        &self,
        tool_call_id: &str,
        response: Option<String>,
        hitl_response: Option<crate::inference::hitl::HitlResponse>,
    ) -> Result<ToolResolveResult, AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;

        let current_status = te.hitl.as_ref().map(|h| h.status.clone());

        match current_status {
            Some(ToolStatus::Resolved) => {
                let existing = te.result.as_str();
                let incoming = response.as_deref().unwrap_or("Human resolved the request.");
                if existing == incoming {
                    return Ok(ToolResolveResult::AlreadyResolved(MessageResponse::from(
                        self.message_repo
                            .find_by_id(&te.message_id)
                            .await?
                            .ok_or_else(|| AppError::NotFound("Message not found".into()))?,
                    )));
                }
                return Err(AppError::Http {
                    status: 409,
                    message: "Tool call is already resolved with a different response".into(),
                });
            }
            Some(ToolStatus::Denied) => {
                return Err(AppError::Http {
                    status: 409,
                    message: "Tool call has already been denied".into(),
                });
            }
            Some(ToolStatus::Pending) => {}
            None => {
                return Err(AppError::Validation(
                    "Tool call has no resolvable HITL".into(),
                ));
            }
        }

        let response_text = response.unwrap_or_else(|| "Human resolved the request.".to_string());

        if let Some(ref mut h) = te.hitl {
            h.status = ToolStatus::Resolved;
            h.response = hitl_response.clone();
        }

        te.result = response_text;
        self.tool_call_repo.update(&te).await?;

        let message = self
            .message_repo
            .find_by_id(&te.message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        Ok(ToolResolveResult::Changed(message.into()))
    }

    pub async fn deny_tool_call(
        &self,
        tool_call_id: &str,
        response: Option<String>,
    ) -> Result<ToolResolveResult, AppError> {
        self.deny_tool_call_with_hitl_response(tool_call_id, response, None)
            .await
    }

    pub async fn deny_tool_call_with_hitl_response(
        &self,
        tool_call_id: &str,
        response: Option<String>,
        hitl_response: Option<crate::inference::hitl::HitlResponse>,
    ) -> Result<ToolResolveResult, AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;

        let current_status = te.hitl.as_ref().map(|h| h.status.clone());

        match current_status {
            Some(ToolStatus::Denied) => {
                let message = self
                    .message_repo
                    .find_by_id(&te.message_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
                return Ok(ToolResolveResult::AlreadyResolved(message.into()));
            }
            Some(ToolStatus::Resolved) => {
                return Err(AppError::Http {
                    status: 409,
                    message: "Tool call has already been resolved".into(),
                });
            }
            Some(ToolStatus::Pending) => {}
            None => {
                return Err(AppError::Validation(
                    "Tool call has no deniable HITL".into(),
                ));
            }
        }

        let response_text = response.unwrap_or_else(|| "User denied the request.".to_string());

        if let Some(ref mut h) = te.hitl {
            h.status = ToolStatus::Denied;
            h.response = hitl_response.clone();
        }

        te.result = response_text;
        self.tool_call_repo.update(&te).await?;

        let message = self
            .message_repo
            .find_by_id(&te.message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        Ok(ToolResolveResult::Changed(message.into()))
    }

    pub async fn find_pending_tool_call(
        &self,
        chat_id: &str,
    ) -> Result<Option<ToolCall>, AppError> {
        self.tool_call_repo.find_pending_by_chat_id(chat_id).await
    }

    pub async fn get_tool_call(&self, id: &str) -> Result<Option<ToolCall>, AppError> {
        self.tool_call_repo.find_by_id(id).await
    }

    pub async fn get_tool_calls(&self, chat_id: &str) -> Result<Vec<ToolCall>, AppError> {
        self.tool_call_repo.find_by_chat_id(chat_id).await
    }

    pub async fn get_tool_calls_by_message(
        &self,
        message_id: &str,
    ) -> Result<Vec<ToolCall>, AppError> {
        self.tool_call_repo.find_by_message_id(message_id).await
    }

    pub async fn find_executing_message_for_chat(
        &self,
        chat_id: &str,
    ) -> Result<Option<Message>, AppError> {
        let query = "SELECT *, meta::id(id) as id FROM message WHERE chat_id = $chat_id AND status = $status LIMIT 1";
        let mut result = self
            .message_repo
            .db()
            .query(query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("status", MessageStatus::Executing))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let message: Option<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(message)
    }

    pub async fn find_paused_message_for_chat(
        &self,
        chat_id: &str,
    ) -> Result<Option<Message>, AppError> {
        let query = "SELECT *, meta::id(id) as id FROM message WHERE chat_id = $chat_id AND status = $status LIMIT 1";
        let mut result = self
            .message_repo
            .db()
            .query(query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("status", MessageStatus::Paused))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let message: Option<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(message)
    }

    /// Strictly Executing - Paused messages are waiting on a human and MUST
    /// NOT be auto-resumed (doing so would feed the loop empty HITL answers).
    pub async fn find_executing_chat_messages(&self) -> Vec<Message> {
        let query = "SELECT *, meta::id(id) as id FROM message WHERE status = $status AND chat_id IN (SELECT VALUE meta::id(id) FROM chat WHERE task_id IS NONE)";
        let result = self
            .message_repo
            .db()
            .query(query)
            .bind(("status", MessageStatus::Executing))
            .await;

        match result {
            Ok(mut r) => {
                let messages: Vec<Message> = r.take(0).unwrap_or_default();
                messages
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to find executing chat messages");
                vec![]
            }
        }
    }

    pub async fn find_message(&self, message_id: &str) -> Result<Option<Message>, AppError> {
        self.message_repo.find_by_id(message_id).await
    }

    pub async fn find_chat(&self, chat_id: &str) -> Result<Option<Chat>, AppError> {
        self.chat_repo.find_by_id(chat_id).await
    }

    pub async fn find_chat_by_channel_external_id(
        &self,
        channel_id: &str,
        channel_external_id: &str,
    ) -> Result<Option<Chat>, AppError> {
        self.chat_repo
            .find_by_channel_thread(channel_id, channel_external_id)
            .await
    }

    pub async fn resolve_agent_config(&self, agent_id: &str) -> Result<AgentConfig, AppError> {
        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent {agent_id} not found")))?;
        let user = self
            .user_service
            .find_by_id(&agent.user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User {} not found", agent.user_id)))?;
        let ws = self
            .storage_service
            .agent_workspace(&user.handle, &agent.handle);

        tracing::debug!(agent_id, user_id = %agent.user_id, "Resolved agent from DB");

        let raw_prompt = match agent.prompt.as_deref().filter(|p| !p.is_empty()) {
            Some(prompt) => prompt.to_string(),
            None => ws
                .read("AGENT.md")
                .map(|c| parse_frontmatter(&c).template)
                .ok_or_else(|| {
                    AppError::Internal(format!("No AGENT.md found for agent {agent_id}"))
                })?,
        };

        let system_prompt =
            render_template(&raw_prompt, &[("agent_name", &agent.name)]).unwrap_or(raw_prompt);

        Ok(AgentConfig {
            system_prompt,
            model_group: agent.model_group,
            skills: agent.skills,
            sandbox_limits: agent.sandbox_limits,
            identity: agent.identity,
        })
    }

    pub async fn generate_title(
        &self,
        chat_id: &str,
        agent_id: &str,
        user_content: &str,
    ) -> Result<String, AppError> {
        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent {agent_id} not found")))?;
        let user = self
            .user_service
            .find_by_id(&agent.user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User {} not found", agent.user_id)))?;
        let ws = self
            .storage_service
            .agent_workspace(&user.handle, &agent.handle);
        let prompts = AgentPromptLoader::new(&ws, &self.prompts);
        let content = prompts
            .read("TITLE.md")
            .ok_or_else(|| AppError::Internal("No title generation prompt found".into()))?;
        let parsed = parse_frontmatter(&content);

        let model_group =
            self.build_title_model_group(parsed.metadata.get("model").map(|s| s.as_str()))?;

        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Title {
                agent_id: agent_id.to_string(),
                chat_id: chat_id.to_string(),
            },
            agent.user_id.clone(),
            model_group.name.clone(),
        );
        let crate::inference::ModelResponse {
            content: contents, ..
        } = model_group
            .inference(crate::inference::ModelRequest {
                system_prompt: &parsed.template,
                history: vec![RigMessage::user(user_content)],
                tools: vec![],
                usage_service: &self.usage_service,
                usage_context: &usage_ctx,
                overrides: crate::inference::RequestOverrides {
                    max_tokens: Some(TITLE_MAX_TOKENS),
                    temperature: None,
                },
            })
            .await?;
        let result = crate::inference::provider::extract_text_from_choice(&contents)?;

        let title = parse_title_response(&result, user_content);
        self.update_chat_title(chat_id, &title).await?;
        Ok(title)
    }

    fn build_title_model_group(
        &self,
        model_specifier: Option<&str>,
    ) -> Result<crate::inference::ModelGroup, AppError> {
        let primary = crate::inference::ModelRef::PRIMARY;
        Ok(match model_specifier {
            Some(name) if !name.is_empty() => self
                .model_providers
                .resolve(&crate::inference::ModelRef(name.to_owned().into()))?,
            _ => self
                .model_providers
                .resolve_with_fallback(&crate::inference::ModelRef::TITLE, &primary)?,
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

    pub async fn get_stored_messages(&self, chat_id: &str) -> Result<Vec<Message>, AppError> {
        self.message_repo.find_by_chat_id(chat_id).await
    }

    pub async fn find_chats_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        self.chat_repo.find_by_space_id(space_id).await
    }

    pub async fn find_user_chats_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        self.chat_repo.find_user_chats_by_space_id(space_id).await
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
    ) -> Result<Vec<crate::storage::Attachment>, AppError> {
        self.message_repo.find_attachments_by_chat_id(chat_id).await
    }
}

/// `find_by_chat_id_page` returns messages in oldest-first order. When we
/// over-fetch by one to detect `has_more`, the extra item sits at whichever
/// end was the "edge" of the SQL window:
/// - default DESC + reverse path (`after.is_none()`): extra is the OLDEST item,
///   sitting at the front of the returned vec.
/// - ASC path (`after.is_some()`): extra is the NEWEST item, at the back.
///
/// Dropping from the wrong end silently hides one real message per page; in
/// the default case it hides the newest message in the chat once the chat
/// crosses the limit.
fn trim_overfetched_page<T>(messages: &mut Vec<T>, limit: usize, after_is_some: bool) {
    if messages.len() <= limit {
        return;
    }
    let drop = messages.len() - limit;
    if after_is_some {
        messages.truncate(limit);
    } else {
        messages.drain(..drop);
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

    #[tokio::test]
    async fn failed_message_persists_and_broadcasts_error_details() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let fixture = crate::app_state_fixture::build(&db).await;
        let service = &fixture.state.chat_service;
        let now = chrono::Utc::now();
        let chat: Chat = serde_json::from_value(serde_json::json!({
            "id": "failed-chat", "user_id": "user", "agent_id": "agent",
            "created_at": now, "updated_at": now,
        }))
        .unwrap();
        service.chat_repo.create(&chat).await.unwrap();
        let message = Message::builder(&chat.id, MessageRole::Agent, "Partial reply".into())
            .status(MessageStatus::Executing)
            .build();
        let message = service.message_repo.create(&message).await.unwrap();
        let mut events = service.broadcast.subscribe_raw();
        let error =
            "The 'gpt-5.3-codex' model is not supported when using Codex with a ChatGPT account.";
        let failure = crate::chat::message::error::MessageError::from(&AppError::from(
            crate::inference::error::InferenceError::InferenceFailed(error.into()),
        ));
        let response = service
            .fail_agent_message(message.clone(), failure.clone())
            .await
            .unwrap();
        assert_eq!(response.status, Some(MessageStatus::Failed));
        assert_eq!(response.error.as_ref(), Some(&failure));
        let saved = service.get_message("user", &message.id).await.unwrap();
        assert_eq!(saved.error.as_ref(), Some(&failure));
        assert!(!saved.metadata.contains_key("error"));
        assert_eq!(saved.content, "Partial reply");
        loop {
            let event = events.try_recv().expect("failure event must be broadcast");
            if let crate::chat::broadcast::BroadcastEventKind::Inference(
                crate::inference::tool_loop::InferenceEventKind::Failed {
                    error: detail,
                    message_id,
                },
            ) = event.kind
            {
                assert_eq!(detail, failure);
                assert_eq!(message_id, message.id);
                break;
            }
        }
        let completed = service.complete_agent_message(saved).await.unwrap();
        assert_eq!(completed.status, Some(MessageStatus::Completed));
        assert_eq!(completed.error, None);
        assert_eq!(
            service
                .get_message("user", &message.id)
                .await
                .unwrap()
                .error,
            None
        );
    }

    #[test]
    fn trim_overfetched_drops_oldest_when_loading_latest_page() {
        let mut msgs = vec![1, 2, 3, 4, 5];
        trim_overfetched_page(&mut msgs, 4, /* after_is_some */ false);
        assert_eq!(msgs, vec![2, 3, 4, 5], "newest must be preserved");
    }

    #[test]
    fn trim_overfetched_drops_newest_when_paging_forward_with_after() {
        let mut msgs = vec![1, 2, 3, 4, 5];
        trim_overfetched_page(&mut msgs, 4, /* after_is_some */ true);
        assert_eq!(msgs, vec![1, 2, 3, 4]);
    }

    #[test]
    fn trim_overfetched_noop_when_under_limit() {
        let mut msgs = vec![1, 2, 3];
        trim_overfetched_page(&mut msgs, 4, false);
        assert_eq!(msgs, vec![1, 2, 3]);
    }

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
        assert_eq!(
            parse_title_response(response, "fallback text"),
            "fallback text"
        );
    }
}
