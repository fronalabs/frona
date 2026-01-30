use rig::completion::{AssistantContent, Message as RigMessage};

use crate::chat::message::models::Message;
use crate::chat::message::models::MessageRole;

pub fn to_rig_messages(messages: &[Message]) -> Vec<RigMessage> {
    messages
        .iter()
        .filter_map(|msg| match msg.role {
            MessageRole::User => Some(RigMessage::user(&msg.content)),
            MessageRole::Assistant => {
                if let Some(tool_calls_val) = &msg.tool_calls {
                    if let Some(calls) = tool_calls_val.as_array() {
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
                }
                Some(RigMessage::assistant(&msg.content))
            }
            MessageRole::ToolResult => {
                let tool_call_id = msg.tool_call_id.as_deref().unwrap_or_default();
                Some(RigMessage::tool_result(tool_call_id, &msg.content))
            }
        })
        .collect()
}
