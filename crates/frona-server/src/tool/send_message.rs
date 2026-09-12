use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::task::service::TaskService;
use crate::chat::broadcast::BroadcastService;
use crate::chat::models::{Chat, CreateChatRequest};
use crate::chat::service::ChatService;
use crate::core::error::AppError;
use crate::inference;
use crate::notification::models::{NotificationData, NotificationLevel};
use crate::notification::service::NotificationService;
use frona_derive::agent_tool;
use rig_core::completion::Message as RigMessage;

use super::{InferenceContext, ToolOutput};

pub struct SendMessageTool {
    chat_service: ChatService,
    notification_service: NotificationService,
    broadcast_service: BroadcastService,
    agent_service: AgentService,
    task_service: TaskService,
    prompts: PromptLoader,
}

impl SendMessageTool {
    pub fn new(
        chat_service: ChatService,
        notification_service: NotificationService,
        broadcast_service: BroadcastService,
        agent_service: AgentService,
        task_service: TaskService,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            chat_service,
            notification_service,
            broadcast_service,
            agent_service,
            task_service,
            prompts,
        }
    }
}

#[agent_tool(files("send_message"))]
impl SendMessageTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("content is required".into()))?
            .to_string();

        let attachments: Vec<String> = arguments
            .get("attachments")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let _ = attachments;

        let (resolved_chat, is_new_chat) = self.resolve_target_chat(ctx, &content).await?;

        // Broadcast space_id must match the resolved chat - task chats are
        // None-spaced, but the resolved channel chat lives in the channel's
        // space, and the outbound dispatcher filters on it.
        let message = self
            .chat_service
            .save_agent_message(
                &ctx.user.id,
                resolved_chat.space_id.as_deref(),
                &resolved_chat.id,
                &ctx.agent.id,
                content.clone(),
                None,
            )
            .await?;

        if is_new_chat {
            let svc = self.chat_service.clone();
            let cid = resolved_chat.id.clone();
            let aid = ctx.agent.id.clone();
            let c = content.clone();
            tokio::spawn(async move {
                if let Err(e) = svc.generate_title(&cid, &aid, &c).await {
                    tracing::warn!(error = %e, "Title generation failed for send_message chat");
                }
            });
        }

        let truncated_body = if content.len() > 200 {
            format!("{}…", &content[..200])
        } else {
            content
        };

        if let Ok(notification) = self
            .notification_service
            .create(
                &ctx.user.id,
                NotificationData::Agent {
                    agent_id: ctx.agent.id.clone(),
                    chat_id: resolved_chat.id.clone(),
                },
                NotificationLevel::Info,
                ctx.agent.name.clone(),
                truncated_body,
            )
            .await
        {
            self.broadcast_service
                .send_notification(&ctx.user.id, notification);
        }

        Ok(ToolOutput::text(
            serde_json::json!({
                "status": "sent",
                "chat_id": resolved_chat.id,
                "message_id": message.id,
            })
            .to_string(),
        ))
    }
}

impl SendMessageTool {
    async fn resolve_target_chat(
        &self,
        ctx: &InferenceContext,
        message_content: &str,
    ) -> Result<(Chat, bool), AppError> {
        // Detached (chatless) send: no chat to anchor to → let the model pick/create
        // a target via `llm_resolve_chat`. When a chat IS present, keep the existing
        // anchor-to-current / walk-up-the-task-chain behavior.
        if let Some(chat) = &ctx.chat {
            if chat.task_id.is_none() && ctx.agent.heartbeat_chat_id.as_deref() != Some(&chat.id) {
                return Ok((chat.clone(), false));
            }

            if chat.task_id.is_some()
                && let Some(anchor) = self.walk_task_chain(ctx, &chat.id).await
            {
                return Ok((anchor, false));
            }
        }

        self.llm_resolve_chat(ctx, message_content).await
    }

    async fn walk_task_chain(&self, ctx: &InferenceContext, start_chat_id: &str) -> Option<Chat> {
        let mut current_chat_id = start_chat_id.to_string();
        let mut depth = 0;
        const MAX_DEPTH: usize = 10;

        loop {
            if depth >= MAX_DEPTH {
                tracing::warn!("Task chain walk exceeded max depth");
                return None;
            }

            let chat = self
                .chat_service
                .get_chat(&ctx.user.id, &current_chat_id)
                .await
                .ok()?;

            match &chat.task_id {
                None => return Some(chat),
                Some(task_id) => {
                    let task = self.task_service.find_by_id(task_id).await.ok()??;
                    match task.kind.source_chat_id() {
                        Some(source_id) => {
                            current_chat_id = source_id.to_string();
                            depth += 1;
                        }
                        None => return None,
                    }
                }
            }
        }
    }

    async fn llm_resolve_chat(
        &self,
        ctx: &InferenceContext,
        message_content: &str,
    ) -> Result<(Chat, bool), AppError> {
        let heartbeat_ids = self.agent_service.heartbeat_chat_ids(&ctx.user.id).await;

        let standalone_chats = self
            .chat_service
            .find_standalone_chats_by_user(&ctx.user.id)
            .await?;

        let candidates: Vec<_> = standalone_chats
            .into_iter()
            .filter(|c| !heartbeat_ids.contains(&c.id))
            .take(10)
            .collect();

        if candidates.is_empty() {
            return self.create_new_chat(ctx).await;
        }

        let chats_text = candidates
            .iter()
            .map(|c| {
                let title = c.title.as_deref().unwrap_or("Untitled");
                format!("- {} ({})", title, c.id)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system_prompt = self
            .prompts
            .read_with_vars(
                "send_message_resolve.md",
                &[("message", message_content), ("chats", &chats_text)],
            )
            .ok_or_else(|| AppError::Internal("send_message_resolve.md prompt not found".into()))?;
        let registry = self.chat_service.model_providers();
        let model_group = registry.resolve_with_fallback(
            &crate::inference::ModelRef::COMPACTION,
            &crate::inference::ModelRef::PRIMARY,
        )?;

        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Router {
                agent_id: ctx.agent.id.clone(),
                chat_id: None, // routing decision happens BEFORE a chat is chosen
            },
            ctx.user.id.clone(),
            model_group.name.clone(),
        );
        let response = inference::text_inference(
            &model_group,
            &system_prompt,
            vec![RigMessage::user("Which chat should this message go to?")],
            self.chat_service.usage_service(),
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("LLM chat resolution failed: {e}")))?;

        let chosen = response.trim();

        if chosen.eq_ignore_ascii_case("new") {
            return self.create_new_chat(ctx).await;
        }

        if let Some(c) = candidates.iter().find(|c| c.id == chosen) {
            return Ok((c.clone(), false));
        }

        Ok((candidates.into_iter().next().unwrap(), false))
    }

    async fn create_new_chat(&self, ctx: &InferenceContext) -> Result<(Chat, bool), AppError> {
        let created = self
            .chat_service
            .create_chat(
                &ctx.user.id,
                CreateChatRequest {
                    agent_id: ctx.agent.id.clone(),
                    space_id: None,
                    task_id: None,
                    title: None,
                    metadata: None,
                },
            )
            .await?;
        let chat = self
            .chat_service
            .get_chat(&ctx.user.id, &created.id)
            .await?;
        Ok((chat, true))
    }
}
