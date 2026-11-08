pub(super) mod context;
use crate::agent::config::parse_frontmatter;
use crate::agent::context::AgentContextScope;
use crate::agent::prompt::{AgentPromptBuilder, PromptLoader};
use crate::agent::service::AgentService;
use crate::agent::workspace::AgentPromptLoader;
use crate::auth::UserService;
use crate::chat::message::models::MessageCommand;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::core::template::render_template;
use crate::db::repo::chats::SurrealChatRepo;
use crate::db::repo::messages::SurrealMessageRepo;
use crate::inference::conversation::{ConversationContext, DefaultConversationBuilder};
use crate::inference::provider::service::ModelProviderService;
use crate::storage::StorageService;
use context::effective_signature;
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
use super::repository::{ChatRepository, ChatSummaryRepository};
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
    agent_prompt_builder: AgentPromptBuilder,
    agent_context_manager: crate::agent::context::AgentContextManager,
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
        agent_context_manager: crate::agent::context::AgentContextManager,
        agent_prompt_builder: AgentPromptBuilder,
    ) -> Self {
        let compactor = super::compactor::ChatCompactor::new(
            crate::db::repo::chat_summaries::SurrealChatSummaryRepo::new(message_repo.db().clone()),
            message_repo.clone(),
            std::sync::Arc::new(super::compactor::TextInferenceSummarizer::new(
                usage_service.clone(),
            )),
            prompts.clone(),
            agent_context_manager.clone(),
        );
        Self {
            agent_prompt_builder,
            agent_context_manager,
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

    pub async fn build_system_prompt(
        &self,
        config: &AgentConfig,
        user: &crate::auth::models::User,
        agent: &crate::agent::models::Agent,
        additional_instructions: &[&str],
    ) -> Result<String, AppError> {
        self.agent_prompt_builder
            .build(
                &config.system_prompt,
                &config.identity,
                &agent.handle,
                user,
                additional_instructions,
            )
            .await
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
            agent_context_state: Default::default(),
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
            agent_context_state: Default::default(),
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
        let chat = self
            .find_chat(&msg.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let saved = self
            .persist_prepared_message(&chat, msg.clone(), None, None)
            .await?;
        self.broadcast_message_persisted(
            &saved,
            &chat,
            crate::chat::broadcast::EntityAction::Created,
        );
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
        self.message_repo.clear_with_summary(chat_id).await?;
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
        self.send_message_controlled(
            user_id,
            chat_id,
            req,
            Default::default(),
            Default::default(),
        )
        .await
    }

    pub async fn send_message_controlled(
        &self,
        user_id: &str,
        chat_id: &str,
        req: SendMessageRequest,
        controls: crate::inference::RuntimeControls,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<MessageResponse>, AppError> {
        let report = controls.report();
        report
            .record_run(async {
                let result = self
                    .send_message_inner(user_id, chat_id, req, &controls, &cancellation)
                    .await;
                let state = crate::inference::controls::outcome(
                    result
                        .as_ref()
                        .map(|_| &crate::inference::InferenceResponse::Handled),
                );
                (result, state)
            })
            .await
    }

    async fn send_message_inner(
        &self,
        user_id: &str,
        chat_id: &str,
        req: SendMessageRequest,
        controls: &crate::inference::RuntimeControls,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<MessageResponse>, AppError> {
        let chat = self.get_chat(user_id, chat_id).await?;
        let agent = self
            .agent_service
            .find_by_id(&chat.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent".into()))?;
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User".into()))?;
        let task = self.load_chat_task(&chat).await?;
        let scope = crate::agent::context::AgentContextScope {
            user: &user,
            agent: &agent,
            space_id: chat.space_id.as_deref(),
            task: task.as_ref(),
        };

        let mut user_message_builder =
            Message::builder(chat_id, MessageRole::User, req.content.clone())
                .attachments(req.attachments.clone());
        if let Some(ref md) = req.metadata {
            user_message_builder = user_message_builder.metadata(md.clone());
        }
        let user_message = user_message_builder.build();
        let user_message = self
            .save_message_and_broadcast(
                &chat,
                Some(&scope),
                user_message,
                user_id,
                chat.space_id.as_deref(),
                crate::chat::broadcast::EntityAction::Created,
            )
            .await?;

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

        let agent_config = self.resolve_agent_config(&chat.agent_id).await?;
        let system_prompt = self
            .build_system_prompt(&agent_config, &user, &agent, &[])
            .await?;

        let model_group_name = agent_config.model_group;

        let model_group = self.model_providers.resolve_with_fallback(
            &frona_harness::inference::ModelRef(model_group_name.clone().into()),
            &frona_harness::inference::ModelRef::PRIMARY,
        )?;
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
        let mut conversation = super::conversation::PersistentConversation::load(
            self,
            chat_id,
            Some(&user_message.id),
            &conv_builder,
            &conv_ctx,
        )
        .await?;
        let request_signature = conversation
            .stored_messages()
            .last()
            .and_then(|message| message.agent_context_signature.clone());
        let request = conversation
            .prepare_controlled(
                &self.compactor,
                super::conversation::PersistentRequest {
                    agent_context: scope,
                    system_prompt: &system_prompt,
                    model_group: &model_group,
                    tools: &[],
                },
                controls.execution(cancellation),
            )
            .await?;
        let response_signature = if conversation.was_compacted() {
            use crate::chat::repository::ChatSummaryRepository;
            crate::db::repo::chat_summaries::SurrealChatSummaryRepo::new(
                self.message_repo.db().clone(),
            )
            .find_by_chat_id(chat_id)
            .await?
            .and_then(|s| s.agent_context_signature)
            .or(request_signature)
        } else {
            request_signature
        };
        let rig_history = request.messages;
        let input_token_estimate = conversation.input_token_estimate();

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
        let response = model_group
            .inference_preserving_history_controlled(
                frona_harness::inference::ModelRequest {
                    input_token_estimate,
                    system_prompt: &system_prompt,
                    history: rig_history,
                    tools: vec![],
                    observations: frona_harness::inference::observation::Observations::new(
                        &self.usage_service.scoped(&usage_ctx),
                    ),
                    overrides: Default::default(),
                },
                controls.execution(cancellation),
            )
            .await?;
        let mut live =
            frona_harness::Conversation::from_execution(controls.execution(cancellation));
        let actions = live
            .record_response(
                frona_harness::ModelOutput::new(
                    response.content.clone(),
                    frona_harness::UsageReport::LegacyNormalized(response.usage),
                ),
                vec![],
            )
            .map_err(|error| AppError::Internal(error.to_string()))?;
        if !actions.actions.is_empty() {
            live.stop()
                .map_err(|error| AppError::Internal(error.to_string()))?;
            return Err(AppError::Inference(Box::new(
                frona_harness::inference::InferenceError::Stop(
                    frona_harness::ExecutionStop::Stopped(
                        "unexpected tool proposal in text reply".into(),
                    ),
                ),
            )));
        }
        controls.execution(cancellation).check_cancellation()?;
        live.finish()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let response_text =
            frona_harness::inference::provider::extract_text_from_choice(&response.content)?;

        let mut assistant_message = Message::builder(chat_id, MessageRole::Agent, response_text)
            .agent_id(chat.agent_id.clone())
            .status(MessageStatus::Completed)
            .build();
        assistant_message.id = assistant_message_id;
        assistant_message.agent_context_signature = response_signature;
        let assistant_message = self
            .save_message_and_broadcast(
                &chat,
                Some(&scope),
                assistant_message,
                user_id,
                chat.space_id.as_deref(),
                crate::chat::broadcast::EntityAction::Created,
            )
            .await
            .map_err(crate::inference::controls::required_write_error)?;

        if let Some(handle) = title_handle {
            let _ = handle.await;
        }

        // Completion evidence may already be durable. Cancellation during the
        // required write/title await still makes this live invocation incomplete.
        controls.execution(cancellation).check_cancellation()?;
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
            .filter(|msg| !msg.is_internal_context())
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
            &chat,
            None,
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
            &chat,
            None,
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
            &chat,
            None,
            builder.build(),
            user_id,
            chat.space_id.as_deref(),
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    /// Persist a message in the task's own chat using the task already held by its executor.
    pub async fn save_task_message(
        &self,
        task: &crate::agent::task::models::Task,
        message: Message,
    ) -> Result<MessageResponse, AppError> {
        let chat = self
            .find_chat(&message.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let is_placeholder =
            message.role == MessageRole::Agent && message.status == Some(MessageStatus::Executing);
        let saved = self
            .persist_prepared_message(&chat, message, None, Some(task))
            .await?;
        if is_placeholder {
            return Ok(saved.into());
        }
        self.presign_and_broadcast_message(
            saved,
            &task.user_id,
            task.space_id.as_deref(),
            crate::chat::broadcast::EntityAction::Created,
        )
        .await
    }

    async fn load_chat_task(
        &self,
        chat: &Chat,
    ) -> Result<Option<crate::agent::task::models::Task>, AppError> {
        match &chat.task_id {
            Some(id) => Ok(Some(
                crate::db::repo::tasks::SurrealTaskRepo::new(self.message_repo.db().clone())
                    .find_by_id(id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("Task".into()))?,
            )),
            None => Ok(None),
        }
    }

    /// Bind context only when this message starts a new, non-deferred turn.
    async fn bind_message_context(
        &self,
        chat: &mut Chat,
        message: &mut Message,
        scope: Option<&AgentContextScope<'_>>,
        task: Option<&crate::agent::task::models::Task>,
    ) -> Result<Option<Message>, AppError> {
        if message.agent_context_signature.is_some() {
            return Ok(None);
        }
        // Agent-handle commands select the source scope before the turn is
        // persisted. Static commands retain the chat's default agent.
        if let Some(MessageCommand::Command { name, .. }) = &message.command {
            let mut commands = crate::chat::command::CommandRegistry::new();
            crate::chat::command::builtin::register_all(&mut commands);
            if commands.get(name).is_none()
                && let Some(agent) = self
                    .agent_service
                    .find_by_handle(&chat.user_id, name)
                    .await?
            {
                chat.agent_id = agent.id.clone();
                message.agent_id = Some(agent.id);
            }
        }
        let position = self.message_repo.context_position(&chat.id).await?;
        let summaries = crate::db::repo::chat_summaries::SurrealChatSummaryRepo::new(
            self.message_repo.db().clone(),
        );
        let summary = summaries.find_by_chat_id(&chat.id).await?;
        let previous = effective_signature(&position, summary.as_ref());
        let deferred_command = matches!(message.command, Some(MessageCommand::Command { .. }));
        let follows_input = matches!(message.role, MessageRole::Agent)
            && message.status == Some(MessageStatus::Executing)
            && position.last.as_ref().is_some_and(|row| {
                matches!(
                    row.role,
                    MessageRole::User
                        | MessageRole::Contact
                        | MessageRole::LiveCall
                        | MessageRole::TaskCompletion
                )
            });
        let starts_turn = matches!(
            message.role,
            MessageRole::User
                | MessageRole::Contact
                | MessageRole::LiveCall
                | MessageRole::TaskCompletion
        ) || (!follows_input
            && (previous.is_none()
                || (matches!(message.role, MessageRole::Agent)
                    && message.status == Some(MessageStatus::Executing))));
        let prepared = if starts_turn && !deferred_command {
            Some(if let Some(scope) = scope {
                self.prepare_message_context(chat, message, scope, previous.as_ref())
                    .await?
            } else {
                let user = self
                    .user_service
                    .find_by_id(&chat.user_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("User".into()))?;
                let agent = self
                    .agent_service
                    .find_by_id(&chat.agent_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("Agent".into()))?;
                let loaded_task;
                let task = match task {
                    Some(task) => Some(task),
                    None => {
                        loaded_task = self.load_chat_task(chat).await?;
                        loaded_task.as_ref()
                    }
                };
                let space_id = chat.space_id.clone();
                let scope = AgentContextScope {
                    user: &user,
                    agent: &agent,
                    space_id: space_id.as_deref(),
                    task,
                };
                self.prepare_message_context(chat, message, &scope, previous.as_ref())
                    .await?
            })
        } else {
            None
        };
        if !starts_turn
            && matches!(message.role, MessageRole::Agent)
            && message.status == Some(MessageStatus::Executing)
        {
            if let Some(agent_id) = position
                .last
                .as_ref()
                .filter(|row| matches!(row.command, Some(MessageCommand::Command { .. })))
                .and_then(|row| row.agent_id.clone())
            {
                message.agent_id = Some(agent_id);
            }
        } else if starts_turn && message.agent_id.is_none() {
            message.agent_id = Some(chat.agent_id.clone());
        }
        let update = prepared
            .as_ref()
            .and_then(|prepared| prepared.update.clone());
        message.agent_context_signature = if deferred_command {
            None
        } else {
            prepared
                .map(|prepared| prepared.snapshot.signature)
                .or(previous)
                .or_else(|| {
                    position
                        .last
                        .as_ref()
                        .and_then(|row| row.agent_context_signature.clone())
                })
        };
        let prefix = update.unwrap_or_default();
        // Keep new messages after the saved summary and the preceding message.
        message.created_at = chrono::Utc::now()
            .max(position.last.as_ref().map_or(message.created_at, |row| {
                row.created_at + chrono::Duration::nanoseconds(1)
            }))
            .max(summary.as_ref().map_or(message.created_at, |summary| {
                summary.updated_at + chrono::Duration::nanoseconds(1)
            }));
        if matches!(message.role, MessageRole::Agent) && starts_turn {
            // Autonomous turns need user-side context before assistant output.
            // Store it as its own immutable system row, atomically with the reply placeholder.
            let mut context =
                Message::builder(&chat.id, MessageRole::System, String::new()).build();
            context.agent_context_signature = message.agent_context_signature.clone();
            context.processed_content = Some(prefix);
            context.created_at = message.created_at;
            message.created_at += chrono::Duration::nanoseconds(1);
            return Ok(Some(context));
        }
        if !deferred_command && starts_turn {
            message.processed_content = Some(if prefix.is_empty() {
                message.inference_content().to_owned()
            } else {
                format!("{prefix}\n\n{}", message.inference_content())
            });
        }
        Ok(None)
    }

    /// Save the prepared message and any autonomous-turn context as one unit.
    pub(super) async fn persist_prepared_message(
        &self,
        chat: &Chat,
        mut message: Message,
        scope: Option<&AgentContextScope<'_>>,
        task: Option<&crate::agent::task::models::Task>,
    ) -> Result<Message, AppError> {
        let mut chat = chat.clone();
        let previous_state = chat.agent_context_state.clone();
        let context = self
            .bind_message_context(&mut chat, &mut message, scope, task)
            .await?;
        let saved = self
            .message_repo
            .create_with_context(&message, context.as_ref())
            .await?;
        if chat.agent_context_state != previous_state {
            self.chat_repo
                .save_agent_context_state(&chat.id, &chat.agent_context_state)
                .await?;
        }
        Ok(saved)
    }

    /// Skips broadcasting - an empty Executing row would render as a phantom
    /// message in the UI while tokens stream.
    async fn save_message(&self, message: Message) -> Result<MessageResponse, AppError> {
        let chat = self
            .find_chat(&message.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let saved = self
            .persist_prepared_message(&chat, message, None, None)
            .await?;
        Ok(saved.into())
    }

    /// Presigns attachments before broadcasting; without it the file widget
    /// renders broken until refresh because live SSE wouldn't match the GET.
    async fn save_message_and_broadcast(
        &self,
        chat: &Chat,
        scope: Option<&AgentContextScope<'_>>,
        message: Message,
        user_id: &str,
        space_id: Option<&str>,
        action: crate::chat::broadcast::EntityAction,
    ) -> Result<MessageResponse, AppError> {
        let saved = self
            .persist_prepared_message(chat, message, scope, None)
            .await?;
        self.presign_and_broadcast_message(saved, user_id, space_id, action)
            .await
    }

    async fn presign_and_broadcast_message(
        &self,
        saved: Message,
        user_id: &str,
        space_id: Option<&str>,
        action: crate::chat::broadcast::EntityAction,
    ) -> Result<MessageResponse, AppError> {
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
        let chat = self
            .find_chat(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let msg = Message::builder(chat_id, MessageRole::System, String::new())
            .event(event)
            .build();
        self.save_message_and_broadcast(
            &chat,
            None,
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
        let chat = self
            .find_chat(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let msg = Message::builder(chat_id, MessageRole::System, content).build();
        self.save_message_and_broadcast(
            &chat,
            None,
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
        let chat = self
            .find_chat(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let mut builder =
            Message::builder(chat_id, MessageRole::Agent, content).agent_id(agent_id.to_string());
        if let Some(d) = delivery {
            builder = builder.delivery(d);
        }
        self.save_message_and_broadcast(
            &chat,
            None,
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
        let chat = self
            .find_chat(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let msg = Message::builder(chat_id, MessageRole::TaskCompletion, content)
            .agent_id(agent_id.to_string())
            .event(event)
            .attachments(attachments)
            .build();
        self.save_message_and_broadcast(
            &chat,
            None,
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

    /// Restore the canonical external-result barrier before recovery or task
    /// continuation. Pending HITLs can survive a crash before Executing became
    /// Paused. A Paused row remains parked until the host's explicit ready CAS:
    /// resolved HITLs alone cannot prove a callback-only sibling has finished.
    pub(crate) async fn restore_external_wait(
        &self,
        message: &mut Message,
    ) -> Result<Option<Vec<crate::inference::tool_call::ToolCallResponse>>, AppError> {
        if !matches!(
            message.status,
            Some(MessageStatus::Executing | MessageStatus::Paused)
        ) {
            return Ok(None);
        }
        let calls = self.get_tool_calls_by_message(&message.id).await?;
        let pending: Vec<_> = calls
            .iter()
            .filter(|call| {
                call.hitl
                    .as_ref()
                    .is_some_and(|hitl| hitl.status == ToolStatus::Pending)
            })
            .cloned()
            .map(Into::into)
            .collect();
        if !pending.is_empty() {
            if message.status == Some(MessageStatus::Executing) {
                self.pause_agent_message(
                    message.clone(),
                    crate::inference::tool_loop::PauseReason::Hitl,
                    pending.clone(),
                )
                .await
                .map_err(crate::inference::controls::required_write_error)?;
                message.status = Some(MessageStatus::Paused);
            }
            return Ok(Some(pending));
        }
        if message.status == Some(MessageStatus::Paused) {
            return Ok(Some(vec![]));
        }
        Ok(None)
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
        provider_item_id: Option<String>,
        signature: Option<String>,
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
            provider_item_id,
            signature,
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
        let frona_harness::inference::ModelResponse {
            content: contents, ..
        } = model_group
            .inference(frona_harness::inference::ModelRequest {
                input_token_estimate: None,
                system_prompt: &parsed.template,
                history: vec![RigMessage::user(user_content)],
                tools: vec![],
                observations: frona_harness::inference::observation::Observations::new(
                    &self.usage_service.scoped(&usage_ctx),
                ),
                overrides: frona_harness::inference::RequestOverrides {
                    max_tokens: Some(TITLE_MAX_TOKENS),
                    temperature: None,
                },
            })
            .await?;
        let result = frona_harness::inference::provider::extract_text_from_choice(&contents)?;

        let title = parse_title_response(&result, user_content);
        self.update_chat_title(chat_id, &title).await?;
        Ok(title)
    }

    fn build_title_model_group(
        &self,
        model_specifier: Option<&str>,
    ) -> Result<frona_harness::inference::ModelGroup, AppError> {
        let primary = frona_harness::inference::ModelRef::PRIMARY;
        Ok(match model_specifier {
            Some(name) if !name.is_empty() => self
                .model_providers
                .resolve(&frona_harness::inference::ModelRef(name.to_owned().into()))?,
            _ => self
                .model_providers
                .resolve_with_fallback(&frona_harness::inference::ModelRef::TITLE, &primary)?,
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
            frona_harness::inference::error::InferenceError::InferenceFailed(error.into()),
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
