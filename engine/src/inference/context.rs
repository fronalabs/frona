use rig::completion::Message as RigMessage;

const KNOWN_CONTEXT_WINDOWS: &[(&str, usize)] = &[
    ("claude", 200_000),
    ("gpt-4o", 128_000),
    ("gpt-4.1", 128_000),
    ("gpt-4.5", 128_000),
    ("o1", 200_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("gemini-2", 1_000_000),
    ("gemini-1.5-pro", 1_000_000),
    ("gemini", 128_000),
    ("deepseek", 64_000),
    ("llama", 128_000),
    ("grok", 131_072),
    ("mistral-large", 128_000),
    ("command-r", 128_000),
    ("qwen", 128_000),
];

pub fn known_context_window(model_id: &str) -> Option<usize> {
    let id = model_id.to_lowercase();
    KNOWN_CONTEXT_WINDOWS
        .iter()
        .find(|(pattern, _)| id.contains(pattern))
        .map(|(_, window)| *window)
}

const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

pub fn resolve_context_window(model_id: &str, config_override: Option<usize>) -> usize {
    config_override
        .or_else(|| known_context_window(model_id))
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 4
}

pub fn estimate_message_tokens(msg: &RigMessage) -> usize {
    let content_len: usize = match msg {
        RigMessage::User { content } => {
            content.iter().map(|c| -> usize {
                match c {
                    rig::completion::message::UserContent::Text(t) => t.text.len(),
                    rig::completion::message::UserContent::ToolResult(tr) => {
                        tr.content.iter().map(|c| -> usize {
                            match c {
                                rig::completion::message::ToolResultContent::Text(t) => t.text.len(),
                                _ => 100,
                            }
                        }).sum::<usize>()
                    }
                    _ => 100,
                }
            }).sum::<usize>()
        }
        RigMessage::Assistant { content, .. } => {
            content.iter().map(|c| -> usize {
                match c {
                    rig::completion::AssistantContent::Text(t) => t.text.len(),
                    rig::completion::AssistantContent::ToolCall(tc) => {
                        tc.function.name.len() + tc.function.arguments.to_string().len()
                    }
                    _ => 100,
                }
            }).sum::<usize>()
        }
    };

    content_len / 4 + 4
}

pub fn estimate_messages_tokens(messages: &[RigMessage], system_prompt: &str) -> usize {
    let system_tokens = estimate_tokens(system_prompt);
    let message_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    system_tokens + message_tokens
}

pub fn needs_compaction(
    messages: &[RigMessage],
    system_prompt: &str,
    model_id: &str,
    context_window: Option<usize>,
    max_output_tokens: usize,
    compaction_trigger_pct: usize,
) -> bool {
    let window = resolve_context_window(model_id, context_window);
    let used = estimate_messages_tokens(messages, system_prompt);
    let available = window.saturating_sub(max_output_tokens);
    used > available * compaction_trigger_pct / 100
}

pub fn truncate_history(
    history: Vec<RigMessage>,
    system_prompt: &str,
    model_id: &str,
    context_window: Option<usize>,
    max_output_tokens: usize,
    history_truncation_pct: usize,
) -> Vec<RigMessage> {
    let window = resolve_context_window(model_id, context_window);
    let system_tokens = estimate_tokens(system_prompt);
    let budget = window
        .saturating_sub(max_output_tokens)
        .saturating_sub(system_tokens);
    let budget = budget * history_truncation_pct / 100;

    let total: usize = history.iter().map(estimate_message_tokens).sum();
    if total <= budget {
        return history;
    }

    let mut result: Vec<RigMessage> = Vec::new();
    let mut used = 0usize;

    for msg in history.into_iter().rev() {
        let cost = estimate_message_tokens(&msg);
        if used + cost > budget {
            break;
        }
        used += cost;
        result.push(msg);
    }

    result.reverse();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_context_window() {
        assert_eq!(known_context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(known_context_window("gpt-4o"), Some(128_000));
        assert_eq!(known_context_window("gemini-2.0-flash"), Some(1_000_000));
        assert_eq!(known_context_window("deepseek-chat"), Some(64_000));
        assert_eq!(known_context_window("unknown-model"), None);
    }

    #[test]
    fn test_resolve_context_window() {
        assert_eq!(resolve_context_window("claude-sonnet-4-5", None), 200_000);
        assert_eq!(resolve_context_window("claude-sonnet-4-5", Some(100_000)), 100_000);
        assert_eq!(resolve_context_window("unknown-model", None), 128_000);
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 4);
        assert_eq!(estimate_tokens("hello world"), 6); // 11/4 + 4 = 6
    }

    #[test]
    fn test_needs_compaction() {
        let short_msg = vec![RigMessage::user("hello")];
        assert!(!needs_compaction(&short_msg, "system", "claude-sonnet-4-5", None, 8192, 80));
    }

    #[test]
    fn test_truncate_history_within_budget() {
        let msgs = vec![RigMessage::user("hello"), RigMessage::user("world")];
        let result = truncate_history(msgs.clone(), "system", "claude-sonnet-4-5", None, 8192, 90);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_truncate_history_exceeds_budget() {
        let long = "x".repeat(500_000);
        let msgs = vec![
            RigMessage::user(&long),
            RigMessage::user("keep this"),
        ];
        let result = truncate_history(msgs, "system", "claude-sonnet-4-5", None, 8192, 90);
        assert!(result.len() <= 2);
    }

}
