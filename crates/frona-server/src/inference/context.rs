use rig_core::completion::Message as RigMessage;

/// Provider-calibrated input size for one exact, in-memory request. This is an
/// estimate, not a token limit: old usage cannot identify individual row costs,
/// and system prompts/tool definitions may have changed since that call.
#[derive(Clone, Copy, Debug)]
pub struct InputTokenEstimate {
    tokens: usize,
    fingerprint: u64,
}

impl InputTokenEstimate {
    pub(crate) fn new(
        tokens: usize,
        model: &crate::inference::ModelConfig,
        system_prompt: &str,
        history: &[RigMessage],
        tools: &[rig_core::completion::request::ToolDefinition],
    ) -> Self {
        Self {
            tokens,
            fingerprint: Self::fingerprint(model, system_prompt, history, tools),
        }
    }

    pub(crate) fn for_request(
        self,
        model: &crate::inference::ModelConfig,
        system_prompt: &str,
        history: &[RigMessage],
        tools: &[rig_core::completion::request::ToolDefinition],
    ) -> Option<usize> {
        (self.fingerprint == Self::fingerprint(model, system_prompt, history, tools))
            .then_some(self.tokens)
    }

    fn fingerprint(
        model: &crate::inference::ModelConfig,
        system_prompt: &str,
        history: &[RigMessage],
        tools: &[rig_core::completion::request::ToolDefinition],
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        model.as_str().hash(&mut hash);
        serde_json::to_vec(&(&model.provider, &model.request_settings.extra_params))
            .expect("provider configuration is serializable")
            .hash(&mut hash);
        system_prompt.hash(&mut hash);
        serde_json::to_vec(&(history, tools))
            .expect("model request is serializable")
            .hash(&mut hash);
        hash.finish()
    }
}

/// Last-resort context window when the model isn't in the catalog AND no
/// config override is set. 128K is the floor most modern chat models meet.
/// Used when compiling named model groups without catalog limits
/// into `ModelGroup.context_window`.
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 4
}

pub fn estimate_message_tokens(msg: &RigMessage) -> usize {
    let content_len: usize = match msg {
        RigMessage::User { content } => content
            .iter()
            .map(|c| -> usize {
                match c {
                    rig_core::completion::message::UserContent::Text(t) => t.text.len(),
                    rig_core::completion::message::UserContent::ToolResult(tr) => tr
                        .content
                        .iter()
                        .map(|c| -> usize {
                            match c {
                                rig_core::completion::message::ToolResultContent::Text(t) => {
                                    t.text.len()
                                }
                                _ => 100,
                            }
                        })
                        .sum::<usize>(),
                    _ => 100,
                }
            })
            .sum::<usize>(),
        RigMessage::Assistant { content, .. } => content
            .iter()
            .map(|c| -> usize {
                match c {
                    rig_core::completion::AssistantContent::Text(t) => t.text.len(),
                    rig_core::completion::AssistantContent::ToolCall(tc) => {
                        tc.function.name.len() + tc.function.arguments.to_string().len()
                    }
                    _ => 100,
                }
            })
            .sum::<usize>(),
        RigMessage::System { content } => content.len(),
    };

    content_len / 4 + 4
}

pub fn estimate_messages_tokens(messages: &[RigMessage], system_prompt: &str) -> usize {
    let system_tokens = estimate_tokens(system_prompt);
    let message_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    system_tokens + message_tokens
}

/// Operates on a resolved window budget (typically from
/// `resolve_context_window`). Doesn't know about model identity.
pub fn needs_compaction(
    messages: &[RigMessage],
    system_prompt: &str,
    context_window: usize,
    max_output_tokens: usize,
    compaction_trigger_pct: usize,
) -> bool {
    let used = estimate_messages_tokens(messages, system_prompt);
    let available = context_window.saturating_sub(max_output_tokens);
    used > available * compaction_trigger_pct / 100
}

/// Trims `history` to fit `history_truncation_pct` of the budget left over
/// after `max_output_tokens` and the system prompt. Newest messages are kept;
/// older ones are dropped first. Operates on a resolved window budget
/// (typically from `resolve_context_window`) - doesn't know about model identity.
pub fn truncate_history(
    history: Vec<RigMessage>,
    system_prompt: &str,
    context_window: usize,
    max_output_tokens: usize,
    history_truncation_pct: usize,
) -> Vec<RigMessage> {
    let window = context_window;
    let system_tokens = estimate_tokens(system_prompt);
    let budget = window
        .saturating_sub(max_output_tokens)
        .saturating_sub(system_tokens);
    let budget = budget * history_truncation_pct / 100;

    let total: usize = history.iter().map(estimate_message_tokens).sum();
    if total <= budget {
        return history;
    }

    let original_len = history.len();
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
    // We only get here when the history is over budget and messages are being
    // dropped oldest-first - a silent context loss the caller can't see. Surface
    // it for ordinary inference. Chat compaction rejects oversized input instead
    // of entering this truncation path. No-op truncations returned above.
    tracing::warn!(
        estimated_tokens = total,
        budget,
        dropped_messages = original_len - result.len(),
        kept_messages = result.len(),
        "history over budget: dropped oldest messages to fit (silent context loss)"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrated_input_is_bound_to_model_provider_system_history_and_tools() {
        let mut model = crate::inference::ModelConfig {
            catalog_provider: String::new(),
            provider_handle: crate::handle!("fixture"),
            model_id: "model".into(),
            provider: crate::core::config::ProviderModel::Generic,
            request_settings: Default::default(),
        };
        let history = vec![RigMessage::user("exact history")];
        let tools = vec![rig_core::completion::request::ToolDefinition {
            name: "read".into(),
            description: "exact tool".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let estimate = InputTokenEstimate::new(123, &model, "exact system", &history, &tools);
        assert_eq!(
            estimate.for_request(&model, "exact system", &history, &tools),
            Some(123)
        );
        assert_eq!(
            estimate.for_request(&model, "changed system", &history, &tools),
            None
        );
        assert_eq!(
            estimate.for_request(
                &model,
                "exact system",
                &[RigMessage::user("changed")],
                &tools
            ),
            None
        );
        assert_eq!(
            estimate.for_request(&model, "exact system", &history, &[]),
            None
        );
        model
            .request_settings
            .extra_params
            .insert("changed".into(), serde_json::json!(true));
        assert_eq!(
            estimate.for_request(&model, "exact system", &history, &tools),
            None
        );
        model.request_settings.extra_params.clear();
        model.model_id = "other-model".into();
        assert_eq!(
            estimate.for_request(&model, "exact system", &history, &tools),
            None
        );
        model.model_id = "model".into();
        model.provider = crate::core::config::ProviderModel::Anthropic {
            params: Default::default(),
        };
        assert_eq!(
            estimate.for_request(&model, "exact system", &history, &tools),
            None
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 4);
        assert_eq!(estimate_tokens("hello world"), 6); // 11/4 + 4 = 6
    }

    #[test]
    fn test_needs_compaction() {
        let short_msg = vec![RigMessage::user("hello")];
        assert!(!needs_compaction(&short_msg, "system", 200_000, 8192, 80));
    }

    #[test]
    fn test_truncate_history_within_budget() {
        let msgs = vec![RigMessage::user("hello"), RigMessage::user("world")];
        let result = truncate_history(msgs.clone(), "system", 200_000, 8192, 90);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_truncate_history_exceeds_budget() {
        let long = "x".repeat(500_000);
        let msgs = vec![RigMessage::user(&long), RigMessage::user("keep this")];
        let result = truncate_history(msgs, "system", 200_000, 8192, 90);
        assert!(result.len() <= 2);
    }
}
