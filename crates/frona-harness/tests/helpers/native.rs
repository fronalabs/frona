use rig_core::completion::{AssistantContent, message::ToolCall};
use serde_json::Value;

/// Attach the original provider item with Rig's canonical fingerprint.
pub fn call(call: ToolCall, item: Value) -> ToolCall {
    match AssistantContent::ToolCall(call).with_native(item) {
        AssistantContent::ToolCall(call) => call,
        _ => unreachable!(),
    }
}
