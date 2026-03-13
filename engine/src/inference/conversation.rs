use async_trait::async_trait;
use base64::Engine;
use rig::completion::message::{DocumentSourceKind, ImageMediaType, MimeType, UserContent};
use rig::completion::{AssistantContent, Message as RigMessage};

use crate::auth::UserService;
use crate::chat::message::models::{Message, MessageRole};
use crate::storage::{Attachment, StorageService, VirtualPath, is_image_content_type};

use super::ModelRef;

pub struct ConversationContext {
    pub agent_id: String,
    pub model_ref: ModelRef,
    pub user_id: String,
}

#[async_trait]
pub trait ConversationBuilder: Send + Sync {
    async fn build(&self, messages: &[Message], ctx: &ConversationContext) -> Vec<RigMessage>;
}

pub struct DefaultConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
}

#[async_trait]
impl ConversationBuilder for DefaultConversationBuilder {
    // NOTE: fallback models reuse this history — messages are not rebuilt per model.
    async fn build(&self, messages: &[Message], ctx: &ConversationContext) -> Vec<RigMessage> {
        let mut result = Vec::with_capacity(messages.len());
        for msg in messages {
            let converted = match msg.role {
                MessageRole::User | MessageRole::TaskCompletion | MessageRole::Contact => {
                    Some(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    )
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    Some(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    )
                }
                MessageRole::Agent => convert_agent_message(msg, &ctx.agent_id),
                MessageRole::ToolResult => convert_tool_result(msg),
            };
            if let Some(m) = converted {
                result.push(m);
            }
        }
        result
    }
}

// --- Pure functions (no service dependencies) ---

pub fn format_files_block_simple(content: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let paths: Vec<&str> = attachments.iter().map(|a| a.path.as_str()).collect();
    format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
}

pub fn is_embeddable_image(attachment: &Attachment) -> bool {
    is_image_content_type(&attachment.content_type)
        && !attachment.content_type.contains("svg")
}

pub fn convert_agent_message(msg: &Message, agent_id: &str) -> Option<RigMessage> {
    let is_self = msg.agent_id.as_deref() == Some(agent_id);
    if is_self {
        if let Some(tool_calls_val) = &msg.tool_calls
            && let Some(calls) = tool_calls_val.as_array()
        {
            let mut items: Vec<AssistantContent> = Vec::new();
            if !msg.content.is_empty() {
                items.push(AssistantContent::text(&msg.content));
            }
            for call in calls {
                let id = call["id"].as_str().unwrap_or_default();
                let name = call["name"].as_str().unwrap_or_default();
                let arguments = call.get("arguments").cloned().unwrap_or_default();
                items.push(AssistantContent::tool_call(id, name, arguments));
            }
            if items.is_empty() {
                return None;
            }
            if let Ok(content) = rig::OneOrMany::many(items) {
                return Some(RigMessage::Assistant { id: None, content });
            }
        }
        Some(RigMessage::assistant(&msg.content))
    } else {
        Some(RigMessage::user(&msg.content))
    }
}

pub fn convert_tool_result(msg: &Message) -> Option<RigMessage> {
    let tool_call_id = msg.tool_call_id.as_deref().unwrap_or_default();
    Some(RigMessage::tool_result(tool_call_id, &msg.content))
}

// --- Service-dependent functions ---

pub async fn resolve_attachment_path(
    attachment: &Attachment,
    user_service: &UserService,
    storage_service: &StorageService,
) -> String {
    let vpath = if let Some(user_id) = attachment.owner.strip_prefix("user:") {
        match user_service.find_by_id(user_id).await {
            Ok(Some(user)) => VirtualPath::user(&user.username, &attachment.path),
            _ => return attachment.path.clone(),
        }
    } else if let Some(agent_id) = attachment.owner.strip_prefix("agent:") {
        VirtualPath::agent(agent_id, &attachment.path)
    } else {
        return attachment.path.clone();
    };

    storage_service
        .resolve(&vpath)
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

    // Build text content with non-image file paths
    let text = if non_images.is_empty() {
        content.to_string()
    } else {
        let mut paths = Vec::with_capacity(non_images.len());
        for att in &non_images {
            paths.push(resolve_attachment_path(att, user_service, storage_service).await);
        }
        format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
    };

    // If no images to embed, return simple text message
    if images.is_empty() {
        return RigMessage::user(&text);
    }

    // Build multi-content message with text + embedded images
    let mut contents: Vec<UserContent> = vec![UserContent::text(&text)];

    for (resolved_path, att) in &images {
        if let Ok(bytes) = tokio::fs::read(resolved_path).await {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            contents.push(UserContent::Image(rig::completion::message::Image {
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

    RigMessage::User {
        content: rig::OneOrMany::many(contents).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::agent::task::models::TaskStatus;
    use crate::chat::message::models::{MessageRole, MessageTool};

    fn make_message(role: MessageRole, content: &str) -> Message {
        Message {
            id: "m1".to_string(),
            chat_id: "c1".to_string(),
            role,
            content: content.to_string(),
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
            contact_id: None,
            system_prompt: None,
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
        let result = convert_agent_message(&msg, "agent-1");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::Assistant { .. }));
    }

    #[test]
    fn agent_different_id_converts_to_user() {
        let msg = make_agent_message("task instruction", "agent-2");
        let result = convert_agent_message(&msg, "agent-1");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::User { .. }));
    }

    #[test]
    fn agent_with_tool_calls_converts_to_assistant_with_tools() {
        let tool_calls = serde_json::json!([{
            "id": "tc-1",
            "name": "web_search",
            "arguments": {"query": "test"}
        }]);
        let msg = Message {
            agent_id: Some("agent-1".to_string()),
            tool_calls: Some(tool_calls),
            ..make_message(MessageRole::Agent, "searching...")
        };
        let result = convert_agent_message(&msg, "agent-1");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::Assistant { .. }));
    }

    #[test]
    fn tool_result_converts() {
        let msg = Message {
            tool_call_id: Some("tc-1".to_string()),
            ..make_message(MessageRole::ToolResult, "result text")
        };
        let result = convert_tool_result(&msg);
        assert!(result.is_some());
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
            tool: Some(MessageTool::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: Some("c2".to_string()),
                status: TaskStatus::Completed,
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
            tool: Some(MessageTool::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: None,
                status: TaskStatus::Completed,
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
}
