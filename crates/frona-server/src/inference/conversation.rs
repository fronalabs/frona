use async_trait::async_trait;
use base64::Engine;
use rig_core::completion::message::{
    DocumentSourceKind, ImageMediaType, MimeType, ProviderCallId, Reasoning, ToolCallId,
    ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, Message as RigMessage};

use std::collections::HashMap;

use crate::agent::service::AgentService;
use crate::auth::UserService;
use crate::chat::message::models::{Message, MessageEvent, MessageRole, MessageStatus};
use crate::inference::tool_call::ToolCall;
use crate::storage::{Attachment, StorageService, VirtualPath, is_image_content_type};

use crate::inference::ModelConfig;

pub struct ConversationContext {
    pub agent_id: String,
    pub model_config: ModelConfig,
    pub user_id: String,
}

#[async_trait]
pub trait ConversationBuilder: Send + Sync {
    /// `summary`: rolling summary of compacted-away earlier messages, prepended
    /// as a leading message by chat builders; `None` when there is no summary.
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> Vec<RigMessage>;

    /// Turn-scoped system prompt appended to the agent's main system prompt.
    fn system_prompt(&self) -> Option<String> {
        None
    }
}

pub struct DefaultConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
}

#[async_trait]
impl ConversationBuilder for DefaultConversationBuilder {
    // NOTE: fallback models reuse this history - messages are not rebuilt per model.
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> Vec<RigMessage> {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut result = Vec::with_capacity(messages.len() + 1);
        // Compacted earlier conversation, re-injected as a leading message so
        // the model sees the gist of what was summarized away.
        if let Some(s) = summary
            && !s.trim().is_empty()
        {
            result.push(RigMessage::user(format!(
                "<conversation_summary>\n{s}\n</conversation_summary>"
            )));
        }
        for msg in messages {
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    let other_handle =
                        resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service).await;
                    if let Some(tes) = te_map.get(&msg.id) {
                        convert_agent_with_tool_calls(
                            msg,
                            tes,
                            &ctx.agent_id,
                            other_handle.as_deref(),
                            &mut result,
                        );
                    } else if let Some(m) =
                        convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                    {
                        result.push(m);
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
        }
        result
    }
}

pub struct TaskConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub continuation_prompt: Option<String>,
}

#[async_trait]
impl ConversationBuilder for TaskConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        _summary: Option<&str>,
    ) -> Vec<RigMessage> {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut result = Vec::with_capacity(messages.len());
        let mut instruction_wrapped = false;
        for msg in messages {
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    if !instruction_wrapped {
                        instruction_wrapped = true;
                        let content = format!("<task>\n{}\n</task>", msg.content);
                        result.push(RigMessage::user(&content));
                    } else {
                        let other_handle =
                            resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service)
                                .await;
                        if let Some(tes) = te_map.get(&msg.id) {
                            convert_agent_with_tool_calls(
                                msg,
                                tes,
                                &ctx.agent_id,
                                other_handle.as_deref(),
                                &mut result,
                            );
                        } else if let Some(m) =
                            convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                        {
                            result.push(m);
                        }
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
        }
        if let Some(prompt) = &self.continuation_prompt
            && !prompt.is_empty()
        {
            result.push(RigMessage::user(prompt));
        }
        result
    }
}

/// The injected heartbeat turn is never written to the message table -
/// it exists only inside the inference request.
pub struct HeartbeatConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub continuation_prompt: Option<String>,
    pub heartbeat_content: String,
}

#[async_trait]
impl ConversationBuilder for HeartbeatConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> Vec<RigMessage> {
        let default = DefaultConversationBuilder {
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            agent_service: self.agent_service.clone(),
        };
        let mut result = default.build(messages, tool_calls, ctx, summary).await;

        let mut injected = String::new();
        if let Some(prompt) = &self.continuation_prompt
            && !prompt.is_empty()
        {
            injected.push_str("<heartbeat_instructions>\n");
            injected.push_str(prompt);
            injected.push_str("\n</heartbeat_instructions>\n\n");
        }
        injected.push_str("<heartbeat_checklist file=\"HEARTBEAT.md\">\n");
        injected.push_str(&self.heartbeat_content);
        injected.push_str("\n</heartbeat_checklist>");
        result.push(RigMessage::user(&injected));
        result
    }
}

pub struct ChannelConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub channel: String,
    pub sender: Option<String>,
    pub inbound_prompt: Option<String>,
}

#[async_trait]
impl ConversationBuilder for ChannelConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        _summary: Option<&str>,
    ) -> Vec<RigMessage> {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut result = Vec::with_capacity(messages.len());
        for msg in messages {
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    let other_handle =
                        resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service).await;
                    if let Some(tes) = te_map.get(&msg.id) {
                        convert_agent_with_tool_calls(
                            msg,
                            tes,
                            &ctx.agent_id,
                            other_handle.as_deref(),
                            &mut result,
                        );
                    } else if let Some(m) =
                        convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                    {
                        result.push(m);
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
        }
        result
    }

    fn system_prompt(&self) -> Option<String> {
        self.inbound_prompt.clone()
    }
}

fn group_tool_calls_by_message(tool_calls: &[ToolCall]) -> HashMap<String, Vec<&ToolCall>> {
    let mut map: HashMap<String, Vec<&ToolCall>> = HashMap::new();
    for te in tool_calls {
        map.entry(te.message_id.clone()).or_default().push(te);
    }
    map
}

fn reasoning_for_replay(r: &crate::chat::message::models::Reasoning) -> Reasoning {
    r.raw
        .as_ref()
        .and_then(|raw| serde_json::from_value(raw.clone()).ok())
        .unwrap_or_else(|| Reasoning {
            id: r.id.clone(),
            ..Reasoning::new_with_signature(&r.content, r.signature.clone())
        })
}

fn convert_agent_with_tool_calls(
    msg: &Message,
    tool_calls: &[&ToolCall],
    agent_id: &str,
    other_agent_handle: Option<&str>,
    result: &mut Vec<RigMessage>,
) {
    let is_self = msg.agent_id.as_deref() == Some(agent_id);
    if !is_self {
        if !msg.content.is_empty() {
            result.push(RigMessage::user(attribute_cross_agent(
                &msg.content,
                other_agent_handle,
            )));
        }
        return;
    }

    let mut turns: std::collections::BTreeMap<u32, Vec<&ToolCall>> =
        std::collections::BTreeMap::new();
    for te in tool_calls {
        turns.entry(te.turn).or_default().push(te);
    }

    for tes in turns.values() {
        let mut assistant_items: Vec<AssistantContent> = Vec::new();
        // Per-turn reasoning - stamped on the first tool_call of the turn at
        // begin time. Replayed here so thinking-mode providers (DeepSeek,
        // Anthropic extended thinking) see the `reasoning_content` they
        // originally emitted; without it they reject the request with
        // `invalid_request_error` on resume after a HITL pause.
        if let Some(r) = tes.iter().find_map(|te| te.turn_reasoning.as_ref()) {
            assistant_items.push(AssistantContent::Reasoning(reasoning_for_replay(r)));
        }
        if let Some(text) = tes.iter().find_map(|te| te.turn_text.as_deref())
            && !text.is_empty()
        {
            assistant_items.push(AssistantContent::text(text));
        }
        for te in tes {
            assistant_items.push(AssistantContent::tool_call(
                &te.provider_call_id,
                &te.name,
                te.arguments.clone(),
            ));
        }
        if !assistant_items.is_empty() {
            result.push(RigMessage::Assistant {
                id: None,
                content: assistant_items,
            });
        }

        let tool_results: Vec<UserContent> = tes
            .iter()
            .map(|te| {
                let provider = ProviderCallId::new(te.provider_call_id.clone());
                UserContent::ToolResult(ToolResult {
                    call: ToolCallId::for_provider(provider.as_ref()),
                    provider,
                    name: te.name.clone(),
                    content: vec![ToolResultContent::text(&te.result)],
                })
            })
            .collect();
        if !tool_results.is_empty() {
            result.push(RigMessage::User {
                content: tool_results,
            });
        }
    }

    let is_completed = msg.status.as_ref() == Some(&MessageStatus::Completed);
    if is_completed && !msg.content.is_empty() {
        let mut items: Vec<AssistantContent> = Vec::new();
        if let Some(r) = &msg.reasoning {
            items.push(AssistantContent::Reasoning(reasoning_for_replay(r)));
        }
        items.push(AssistantContent::text(&msg.content));
        result.push(RigMessage::Assistant {
            id: None,
            content: items,
        });
    }
}

pub fn format_files_block_simple(content: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let paths: Vec<&str> = attachments.iter().map(|a| a.path.as_str()).collect();
    format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
}

pub fn is_embeddable_image(attachment: &Attachment) -> bool {
    is_image_content_type(&attachment.content_type) && !attachment.content_type.contains("svg")
}

pub fn convert_agent_message(
    msg: &Message,
    agent_id: &str,
    other_agent_handle: Option<&str>,
) -> Option<RigMessage> {
    if matches!(
        msg.status.as_ref(),
        Some(MessageStatus::Executing) | Some(MessageStatus::Paused)
    ) {
        return None;
    }
    let is_self = msg.agent_id.as_deref() == Some(agent_id);
    if is_self {
        if let Some(r) = &msg.reasoning {
            let mut items: Vec<AssistantContent> =
                vec![AssistantContent::Reasoning(reasoning_for_replay(r))];
            if !msg.content.is_empty() {
                items.push(AssistantContent::text(&msg.content));
            }
            return Some(RigMessage::Assistant {
                id: None,
                content: items,
            });
        }
        // Empty Assistant text blocks make Anthropic reject the request.
        if msg.content.is_empty() {
            return None;
        }
        Some(RigMessage::assistant(&msg.content))
    } else {
        if msg.content.is_empty() {
            return None;
        }
        Some(RigMessage::user(attribute_cross_agent(
            &msg.content,
            other_agent_handle,
        )))
    }
}

async fn resolve_other_agent_handle(
    msg: &Message,
    current_agent_id: &str,
    agent_service: &AgentService,
) -> Option<String> {
    let id = msg.agent_id.as_deref()?;
    if id == current_agent_id {
        return None;
    }
    agent_service
        .find_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|a| a.handle.to_string())
}

/// Mirrors `task_completion_content`'s `<task_result>…</task_result>`
/// wrapper for the sibling task-delivery cross-agent flow.
fn attribute_cross_agent(content: &str, handle: Option<&str>) -> String {
    let label = handle.unwrap_or("unknown");
    format!("<system source=\"agent:{label}\">\n{content}\n</system>")
}

/// Wraps schema-bearing task results in `<task_result>{json}</task_result>`
/// so the parent LLM sees a consistent shape. Other cases pass through.
fn task_completion_content(msg: &Message) -> String {
    let has_schema = matches!(
        &msg.event,
        Some(MessageEvent::TaskCompletion {
            schema: Some(_),
            ..
        })
    );
    if has_schema && !msg.content.is_empty() {
        format!("<task_result>{}</task_result>", msg.content)
    } else {
        msg.content.clone()
    }
}

pub async fn resolve_attachment_path(
    attachment: &Attachment,
    user_service: &UserService,
    storage_service: &StorageService,
) -> String {
    let vpath = if let Some(user_id) = attachment.owner.strip_prefix("user:") {
        match user_service.find_by_id(user_id).await {
            Ok(Some(user)) => VirtualPath::user(&user.handle, &attachment.path),
            _ => return attachment.path.clone(),
        }
    } else if let Some(agent_id) = attachment.owner.strip_prefix("agent:") {
        VirtualPath::agent(agent_id, &attachment.path)
    } else {
        return attachment.path.clone();
    };

    storage_service
        .resolve_virtual_path(&vpath)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| attachment.path.clone())
}

pub async fn format_files_block(
    content: &str,
    attachments: &[Attachment],
    user_service: &UserService,
    storage_service: &StorageService,
) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let mut paths = Vec::with_capacity(attachments.len());
    for att in attachments {
        paths.push(resolve_attachment_path(att, user_service, storage_service).await);
    }
    format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
}

pub async fn build_user_message(
    content: &str,
    attachments: &[Attachment],
    user_service: &UserService,
    storage_service: &StorageService,
) -> RigMessage {
    if attachments.is_empty() {
        return RigMessage::user(content);
    }

    let mut images: Vec<(String, Attachment)> = Vec::new();
    let mut non_images: Vec<&Attachment> = Vec::new();

    for att in attachments {
        if is_embeddable_image(att) {
            let path = resolve_attachment_path(att, user_service, storage_service).await;
            images.push((path, att.clone()));
        } else {
            non_images.push(att);
        }
    }

    let text = if non_images.is_empty() {
        content.to_string()
    } else {
        let mut paths = Vec::with_capacity(non_images.len());
        for att in &non_images {
            paths.push(resolve_attachment_path(att, user_service, storage_service).await);
        }
        format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
    };

    if images.is_empty() {
        return RigMessage::user(&text);
    }

    let mut contents: Vec<UserContent> = vec![UserContent::text(&text)];

    for (resolved_path, att) in &images {
        if let Ok(bytes) = tokio::fs::read(resolved_path).await {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            contents.push(UserContent::Image(rig_core::completion::message::Image {
                data: DocumentSourceKind::Base64(b64),
                media_type: ImageMediaType::from_mime_type(&att.content_type),
                detail: None,
                additional_params: None,
            }));
        }
    }

    if contents.len() == 1 {
        return RigMessage::user(&text);
    }

    RigMessage::User { content: contents }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::models::TaskStatus;
    use crate::chat::message::models::{MessageEvent, MessageRole};
    use chrono::Utc;

    fn make_message(role: MessageRole, content: &str) -> Message {
        Message {
            id: "m1".to_string(),
            chat_id: "c1".to_string(),
            role,
            content: content.to_string(),
            agent_id: None,
            event: None,
            attachments: vec![],
            contact_id: None,
            status: None,
            reasoning: None,
            from_address: None,
            delivery: None,
            dispatch_mode: None,
            command: None,
            metadata: Default::default(),
            created_at: Utc::now(),
        }
    }

    fn make_agent_message(content: &str, agent_id: &str) -> Message {
        Message {
            agent_id: Some(agent_id.to_string()),
            ..make_message(MessageRole::Agent, content)
        }
    }

    #[test]
    fn agent_same_id_converts_to_assistant() {
        let msg = make_agent_message("hello", "agent-1");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::Assistant { .. }));
    }

    #[test]
    fn agent_different_id_converts_to_user() {
        let msg = make_agent_message("task instruction", "agent-2");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::User { .. }));
    }

    #[test]
    fn agent_self_with_empty_content_is_skipped() {
        let mut msg = make_agent_message("", "agent-1");
        msg.status = Some(MessageStatus::Failed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_self_with_empty_content_completed_is_skipped() {
        let mut msg = make_agent_message("", "agent-1");
        msg.status = Some(MessageStatus::Completed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_other_with_empty_content_is_skipped() {
        let mut msg = make_agent_message("", "agent-2");
        msg.status = Some(MessageStatus::Failed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_self_with_reasoning_and_empty_content_still_emits_reasoning() {
        let mut msg = make_agent_message("", "agent-1");
        msg.reasoning = Some(crate::chat::message::models::Reasoning {
            id: None,
            content: "thinking".into(),
            signature: None,
            raw: None,
        });
        assert!(convert_agent_message(&msg, "agent-1", None).is_some());
    }

    #[test]
    fn format_files_block_simple_no_attachments() {
        assert_eq!(format_files_block_simple("hello world", &[]), "hello world");
        assert!(!format_files_block_simple("hello", &[]).contains("<files>"));
    }

    #[test]
    fn format_files_block_simple_with_attachments() {
        let attachments = vec![Attachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size_bytes: 1024,
            owner: "user:uid".to_string(),
            path: "report.pdf".to_string(),
            url: None,
        }];
        let result = format_files_block_simple("check this file", &attachments);
        assert!(result.contains("check this file"));
        assert!(result.contains("<files>"));
        assert!(result.contains("report.pdf"));
        assert!(result.contains("</files>"));
    }

    #[test]
    fn is_embeddable_image_png() {
        let att = Attachment {
            filename: "logo.png".into(),
            content_type: "image/png".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "logo.png".into(),
            url: None,
        };
        assert!(is_embeddable_image(&att));
    }

    #[test]
    fn is_embeddable_image_svg_excluded() {
        let att = Attachment {
            filename: "icon.svg".into(),
            content_type: "image/svg+xml".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "icon.svg".into(),
            url: None,
        };
        assert!(!is_embeddable_image(&att));
    }

    #[test]
    fn is_embeddable_image_pdf_excluded() {
        let att = Attachment {
            filename: "doc.pdf".into(),
            content_type: "application/pdf".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "doc.pdf".into(),
            url: None,
        };
        assert!(!is_embeddable_image(&att));
    }

    #[test]
    fn task_completion_converts_to_user_message_via_simple_format() {
        let msg = Message {
            event: Some(MessageEvent::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: Some("c2".to_string()),
                status: TaskStatus::Completed,
                summary: None,
                schema: None,
            }),
            ..make_message(MessageRole::TaskCompletion, "Task 'research' completed.")
        };
        assert_eq!(msg.role, MessageRole::TaskCompletion);
        let text = format_files_block_simple(&msg.content, &msg.attachments);
        assert_eq!(text, "Task 'research' completed.");
    }

    #[test]
    fn task_completion_with_attachments() {
        let msg = Message {
            event: Some(MessageEvent::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: None,
                status: TaskStatus::Completed,
                summary: None,
                schema: None,
            }),
            attachments: vec![Attachment {
                filename: "output.csv".to_string(),
                content_type: "text/csv".to_string(),
                size_bytes: 512,
                owner: "agent:dev".to_string(),
                path: "output.csv".to_string(),
                url: None,
            }],
            ..make_message(MessageRole::TaskCompletion, "Task done")
        };
        let text = format_files_block_simple(&msg.content, &msg.attachments);
        assert!(text.contains("Task done"));
        assert!(text.contains("output.csv"));
    }

    #[test]
    fn agent_with_reasoning_includes_reasoning_in_assistant_message() {
        use crate::chat::message::models::Reasoning;

        let msg = Message {
            agent_id: Some("agent-1".to_string()),
            reasoning: Some(Reasoning {
                id: Some("reasoning-1".to_string()),
                content: "Let me think about this...".to_string(),
                signature: Some("sig-abc".to_string()),
                raw: None,
            }),
            ..make_message(MessageRole::Agent, "Here is my answer")
        };
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        let rig_msg = result.unwrap();
        if let RigMessage::Assistant { content, .. } = &rig_msg {
            let has_reasoning = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_)));
            assert!(
                has_reasoning,
                "Expected reasoning content in assistant message"
            );
            let has_text = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_)));
            assert!(has_text, "Expected text content in assistant message");
        } else {
            panic!("Expected Assistant message");
        }
    }

    #[test]
    fn executing_agent_message_is_skipped() {
        let msg = Message {
            agent_id: Some("agent-1".to_string()),
            status: Some(MessageStatus::Executing),
            ..make_message(MessageRole::Agent, "")
        };
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_none(), "Executing messages should be skipped");
    }

    #[test]
    fn agent_without_reasoning_is_unchanged() {
        let msg = make_agent_message("hello", "agent-1");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        if let RigMessage::Assistant { content, .. } = result.unwrap() {
            let has_reasoning = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_)));
            assert!(!has_reasoning);
        }
    }
}
