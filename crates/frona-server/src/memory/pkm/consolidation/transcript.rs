use crate::inference::tool_call::ToolCall;

pub(crate) fn push_message(out: &mut String, handle: &str, speaker: &str, text: &str) {
    out.push_str(&format!("[{handle}] {speaker}: {text}\n"));
}

pub(crate) fn push_agent_message(out: &mut String, handle: &str, text: &str, recall: &str) {
    out.push_str(&format!("[{handle}] Agent: {text}\n"));
    out.push_str(recall);
}

pub(crate) fn push_remembered(out: &mut String, text: &str) {
    out.push_str(&format!("[remembered: {}]\n", text.trim()));
}

pub(crate) fn push_task(out: &mut String, handle: &str, text: &str) {
    out.push_str(&format!("[{handle}] {text}\n"));
}

pub(crate) fn external_note(content: &str) -> String {
    format!("[m1] External note: {content}")
}

pub(crate) fn message_text(message_id: &str, final_text: &str, tool_calls: &[ToolCall]) -> String {
    let mut turns = tool_calls
        .iter()
        .filter(|call| call.message_id == message_id)
        .filter_map(|call| call.turn_text.as_deref().map(|text| (call, text.trim())))
        .filter(|(_, text)| !text.is_empty())
        .collect::<Vec<_>>();
    turns.sort_by(|(left, _), (right, _)| {
        (left.turn, left.created_at, &left.id).cmp(&(right.turn, right.created_at, &right.id))
    });
    turns
        .into_iter()
        .map(|(_, text)| rtb_redact::string(text).into_owned())
        .chain((!final_text.trim().is_empty()).then(|| final_text.trim().to_string()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn call(id: &str, message_id: &str, turn: u32, turn_text: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.into(),
            chat_id: "chat-1".into(),
            message_id: message_id.into(),
            turn,
            provider_call_id: format!("provider-{id}"),
            name: "web_search".into(),
            arguments: serde_json::json!({"query":"firmware"}),
            result: "result".into(),
            success: true,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: turn_text.map(str::to_string),
            signature: Some("opaque-provider-signature-not-evidence".into()),
            turn_reasoning: None,
            created_at: Utc.timestamp_opt(turn as i64, 0).unwrap(),
        }
    }

    #[test]
    fn includes_each_non_empty_tool_turn_before_the_final_response() {
        let calls = vec![
            call("later", "message-1", 2, Some("  second research update  ")),
            call("empty", "message-1", 3, Some("  \n ")),
            call("other", "message-2", 0, Some("unrelated text")),
            call("earlier", "message-1", 1, Some("first research update")),
        ];

        let text = message_text("message-1", "  short final response  ", &calls);

        assert_eq!(
            text,
            concat!(
                "first research update\n\n",
                "second research update\n\n",
                "short final response",
            )
        );
    }

    #[test]
    fn keeps_the_existing_text_shape_when_there_is_no_tool_turn_text() {
        let calls = vec![call("empty", "message-1", 0, None)];

        assert_eq!(
            message_text("message-1", "  final response  ", &calls),
            "final response"
        );
    }

    #[test]
    fn keeps_tool_turn_text_when_the_final_response_is_empty() {
        let calls = vec![call(
            "research",
            "message-1",
            0,
            Some("durable research result"),
        )];

        assert_eq!(
            message_text("message-1", "  ", &calls),
            "durable research result"
        );
    }

    #[test]
    fn transcript_keeps_existing_message_shape() {
        let calls = vec![call("research", "message-1", 0, Some("research update"))];
        let text = message_text("message-1", "final response", &calls);
        let mut rendered = String::new();

        push_agent_message(&mut rendered, "m2", &text, "Recall calls for m2:\n");

        assert_eq!(
            rendered,
            concat!(
                "[m2] Agent: research update\n\nfinal response\n",
                "Recall calls for m2:\n",
            )
        );
    }

    #[test]
    fn transcript_renders_other_items() {
        let mut rendered = String::new();

        push_message(&mut rendered, "m1", "User", "hello");
        push_remembered(&mut rendered, "known fact");
        push_task(&mut rendered, "m2", "Task completed: Check firmware");

        assert_eq!(
            rendered,
            concat!(
                "[m1] User: hello\n",
                "[remembered: known fact]\n",
                "[m2] Task completed: Check firmware\n",
            )
        );
    }

    #[test]
    fn external_note_uses_transcript_format() {
        assert_eq!(
            external_note("firmware note"),
            "[m1] External note: firmware note"
        );
    }
}
