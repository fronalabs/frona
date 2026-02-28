use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rig::completion::{
    AssistantContent, CompletionModel, CompletionRequest, CompletionResponse,
    Message as RigMessage,
    message::{ToolCall, ToolFunction},
};
use rig::completion::request::{ToolDefinition as RigToolDefinition, Usage};
use tokio::sync::mpsc;

use crate::chat::broadcast::BroadcastService;
use crate::core::metrics;
use super::error::InferenceError;

struct CompletionRequestBuilder<'a> {
    system_prompt: &'a str,
    chat_history: Vec<RigMessage>,
    tools: Vec<RigToolDefinition>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
}

impl<'a> CompletionRequestBuilder<'a> {
    fn new(system_prompt: &'a str, chat_history: Vec<RigMessage>) -> Self {
        Self {
            system_prompt,
            chat_history,
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    fn tools(mut self, tools: Vec<RigToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    fn max_tokens(mut self, v: Option<u64>) -> Self {
        self.max_tokens = v;
        self
    }

    fn temperature(mut self, v: Option<f64>) -> Self {
        self.temperature = v;
        self
    }

    fn build(self) -> CompletionRequest {
        CompletionRequest {
            preamble: Some(self.system_prompt.to_string()),
            chat_history: rig::OneOrMany::many(self.chat_history)
                .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::user(""))),
            documents: vec![],
            tools: self.tools,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            tool_choice: None,
            additional_params: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn parse(s: &str) -> Result<Self, InferenceError> {
        let (provider, model_id) = s
            .split_once('/')
            .ok_or_else(|| InferenceError::InvalidModelRef(format!(
                "expected 'provider/model' format, got '{s}'"
            )))?;

        if provider.is_empty() || model_id.is_empty() {
            return Err(InferenceError::InvalidModelRef(format!(
                "provider and model must be non-empty, got '{s}'"
            )));
        }

        Ok(Self {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn as_str(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }
}

#[derive(Clone)]
pub struct InferenceCounter {
    count: Arc<AtomicUsize>,
    broadcast: BroadcastService,
}

impl InferenceCounter {
    pub fn new(broadcast: BroadcastService) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            broadcast,
        }
    }

    fn increment(&self) {
        let val = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        self.broadcast.broadcast_inference_count(val);
        metrics::set_active_inference_requests(val);
    }

    fn decrement(&self) {
        let val = self.count.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(1))
        }).unwrap_or(0).saturating_sub(1);
        self.broadcast.broadcast_inference_count(val);
        metrics::set_active_inference_requests(val);
    }

    pub fn guard(&self) -> InferenceGuard {
        self.increment();
        InferenceGuard { counter: self.clone() }
    }
}

pub struct InferenceGuard {
    counter: InferenceCounter,
}

impl Drop for InferenceGuard {
    fn drop(&mut self) {
        self.counter.decrement();
    }
}

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(Vec<AssistantContent>, Usage), InferenceError>;

    async fn stream_inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<String>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<Vec<AssistantContent>, InferenceError>;
}

pub struct RigProvider<C> {
    client: C,
    counter: InferenceCounter,
}

impl<C> RigProvider<C> {
    pub fn new(client: C, counter: InferenceCounter) -> Self {
        Self { client, counter }
    }
}

#[async_trait]
impl<C> ModelProvider for RigProvider<C>
where
    C: rig::client::CompletionClient + Send + Sync,
    C::CompletionModel: CompletionModel + Send + Sync + 'static,
    <C::CompletionModel as CompletionModel>::Response: Send + Sync,
    <C::CompletionModel as CompletionModel>::StreamingResponse:
        Clone + Unpin + Send + Sync + 'static,
{
    async fn inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(Vec<AssistantContent>, Usage), InferenceError> {
        use rig::completion::CompletionModel as _;

        let _guard = self.counter.guard();
        let model = self.client.completion_model(model_id);

        tracing::debug!(
            model = %model_id,
            messages = ?chat_history,
            tool_count = tools.len(),
            "LLM request"
        );

        let request = CompletionRequestBuilder::new(system_prompt, chat_history)
            .tools(tools)
            .max_tokens(max_tokens)
            .temperature(temperature)
            .build();

        let response: CompletionResponse<_> = model
            .completion(request)
            .await
            .map_err(InferenceError::CompletionFailed)?;

        let contents: Vec<AssistantContent> = response.choice.into_iter().collect();
        let usage = response.usage;

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            "LLM response"
        );

        Ok((contents, usage))
    }

    async fn stream_inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<String>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<Vec<AssistantContent>, InferenceError> {
        use rig::completion::CompletionModel as _;

        let _guard = self.counter.guard();
        let model = self.client.completion_model(model_id);

        tracing::debug!(
            model = %model_id,
            tool_count = tools.len(),
            "LLM streaming request"
        );
        tracing::debug!(system_prompt = %system_prompt, "LLM system prompt");
        tracing::debug!(chat_history = ?chat_history, "LLM chat history");

        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

        let request = CompletionRequestBuilder::new(system_prompt, chat_history)
            .tools(tools)
            .max_tokens(max_tokens)
            .temperature(temperature)
            .build();

        let stream = model
            .stream(request)
            .await
            .map_err(InferenceError::CompletionFailed)?;

        let (mut accumulated_text, mut contents, still_buffering) =
            consume_tool_stream(stream, &token_tx, &tool_names).await?;

        let has_tool_calls = contents.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)));
        if !has_tool_calls && !accumulated_text.is_empty() && still_buffering {
            recover_tool_calls_from_text(
                &mut accumulated_text,
                &mut contents,
                &tool_names,
                model_id,
            );
        }

        if !accumulated_text.is_empty() {
            if still_buffering {
                let _ = token_tx.send(accumulated_text.clone()).await;
            }
            contents.insert(0, AssistantContent::text(&accumulated_text));
        }

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            "LLM streaming response"
        );

        Ok(contents)
    }
}

async fn consume_tool_stream<S, R>(
    mut stream: S,
    token_tx: &mpsc::Sender<String>,
    tool_names: &[String],
) -> Result<(String, Vec<AssistantContent>, bool), InferenceError>
where
    S: futures::Stream<Item = Result<rig::streaming::StreamedAssistantContent<R>, rig::completion::CompletionError>>
        + Unpin,
    R: Clone + Unpin,
{
    use futures::StreamExt;

    let mut contents: Vec<AssistantContent> = Vec::new();
    let mut accumulated_text = String::new();
    let mut buffering = true;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(rig::streaming::StreamedAssistantContent::Text(text)) => {
                accumulated_text.push_str(&text.text);
                if buffering {
                    if accumulated_text.len() >= 64 {
                        let has_tool_name = tool_names
                            .iter()
                            .any(|name| accumulated_text.contains(name.as_str()));
                        if !has_tool_name {
                            let _ = token_tx.send(accumulated_text.clone()).await;
                            buffering = false;
                        }
                    }
                } else {
                    let _ = token_tx.send(text.text).await;
                }
            }
            Ok(rig::streaming::StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                contents.push(AssistantContent::ToolCall(tool_call));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(InferenceError::CompletionFailed(e));
            }
        }
    }

    Ok((accumulated_text, contents, buffering))
}

fn recover_tool_calls_from_text(
    accumulated_text: &mut String,
    contents: &mut Vec<AssistantContent>,
    tool_names: &[String],
    model_id: &str,
) {
    let names: Vec<&str> = tool_names.iter().map(|s| s.as_str()).collect();
    let extracted = try_extract_tool_calls_from_text(accumulated_text, &names);

    if extracted.is_empty() {
        return;
    }

    tracing::warn!(
        model = %model_id,
        count = extracted.len(),
        "Recovered tool call from text output"
    );

    let mut remaining = accumulated_text.clone();
    for tc in extracted.iter().rev() {
        remaining.replace_range(tc.start..tc.end, "");
    }
    *accumulated_text = remaining.trim().to_string();

    for tc in extracted {
        contents.push(AssistantContent::ToolCall(ToolCall::new(
            uuid::Uuid::new_v4().to_string(),
            ToolFunction::new(tc.tool_name, tc.arguments),
        )));
    }
}

struct ExtractedToolCall {
    tool_name: String,
    arguments: serde_json::Value,
    start: usize,
    end: usize,
}

fn is_word_boundary(text: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    text[..pos]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
}

fn try_extract_tool_calls_from_text(
    text: &str,
    tool_names: &[&str],
) -> Vec<ExtractedToolCall> {
    let mut results = Vec::new();

    for &name in tool_names {
        let mut search_from = 0;
        while let Some(name_pos) = text[search_from..].find(name) {
            let abs_pos = search_from + name_pos;
            search_from = abs_pos + name.len();

            if !is_word_boundary(text, abs_pos) {
                continue;
            }

            let after_name = &text[abs_pos + name.len()..];
            let json_offset = match after_name.find('{') {
                Some(off) => off,
                None => continue,
            };

            if !after_name[..json_offset].chars().all(|c| c.is_whitespace()) {
                continue;
            }

            let json_start = abs_pos + name.len() + json_offset;

            let mut depth = 0i32;
            let mut json_end = None;
            for (i, ch) in text[json_start..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            json_end = Some(json_start + i + ch.len_utf8());
                            break;
                        }
                    }
                    _ => {}
                }
            }

            let json_end = match json_end {
                Some(end) => end,
                None => continue,
            };

            let json_str = &text[json_start..json_end];
            match serde_json::from_str::<serde_json::Value>(json_str) {
                Ok(args) => {
                    results.push(ExtractedToolCall {
                        tool_name: name.to_string(),
                        arguments: args,
                        start: abs_pos,
                        end: json_end,
                    });
                    search_from = json_end;
                }
                Err(_) => continue,
            }
        }
    }

    results.sort_by_key(|r| r.start);
    results
}

pub fn extract_text_from_choice(
    contents: &[AssistantContent],
) -> Result<String, InferenceError> {
    let mut text_parts = Vec::new();

    for item in contents {
        if let AssistantContent::Text(t) = item {
            text_parts.push(t.text.clone());
        }
    }

    if text_parts.is_empty() {
        return Err(InferenceError::InferenceFailed(
            "No text content in response".to_string(),
        ));
    }

    Ok(text_parts.join(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::{ToolCall, ToolFunction};

    #[test]
    fn test_is_word_boundary_start_of_string() {
        assert!(is_word_boundary("hello", 0));
        assert!(is_word_boundary("", 0));
    }

    #[test]
    fn test_is_word_boundary_after_space() {
        assert!(is_word_boundary("a b", 2));
        assert!(is_word_boundary(" x", 1));
        assert!(is_word_boundary("\tx", 1));
    }

    #[test]
    fn test_is_word_boundary_after_alphanumeric() {
        assert!(!is_word_boundary("ab", 1));
        assert!(!is_word_boundary("a1b", 2));
        assert!(!is_word_boundary("9x", 1));
    }

    #[test]
    fn test_is_word_boundary_after_underscore() {
        assert!(!is_word_boundary("a_b", 2));
        assert!(!is_word_boundary("_x", 1));
    }

    #[test]
    fn test_is_word_boundary_after_punctuation() {
        assert!(is_word_boundary("a.b", 2));
        assert!(is_word_boundary("a\nb", 2));
        assert!(is_word_boundary("a,b", 2));
        assert!(is_word_boundary("a:b", 2));
    }

    #[test]
    fn test_extract_tool_calls_simple() {
        let text = r#"mytool {"key": "value"}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "mytool");
        assert_eq!(results[0].arguments, serde_json::json!({"key": "value"}));
        assert_eq!(results[0].start, 0);
        assert_eq!(results[0].end, text.len());
    }

    #[test]
    fn test_extract_tool_calls_multiple() {
        let text = r#"tool_a {"a": 1} some text tool_b {"b": 2}"#;
        let results = try_extract_tool_calls_from_text(text, &["tool_a", "tool_b"]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "tool_a");
        assert_eq!(results[1].tool_name, "tool_b");
    }

    #[test]
    fn test_extract_tool_calls_nested_json() {
        let text = r#"mytool {"outer": {"inner": [1, 2]}}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].arguments,
            serde_json::json!({"outer": {"inner": [1, 2]}})
        );
    }

    #[test]
    fn test_extract_tool_calls_no_match() {
        let text = "just some regular text without any tool calls";
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_invalid_json() {
        let text = r#"mytool {invalid json here}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_no_json_after_name() {
        let text = "mytool has no json";
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_non_word_boundary() {
        let text = r#"notatool_mytool {"key": "value"}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_whitespace_gap() {
        let text = "mytool  \t  {\"key\": \"value\"}";
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "mytool");
    }

    #[test]
    fn test_extract_tool_calls_non_whitespace_gap() {
        let text = r#"mytool::: {"key": "value"}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_recover_tool_calls_modifies_text() {
        let mut text = r#"Here is the result: search_web {"query": "rust"}"#.to_string();
        let mut contents: Vec<AssistantContent> = vec![];
        let tool_names = vec!["search_web".to_string()];

        recover_tool_calls_from_text(&mut text, &mut contents, &tool_names, "test-model");

        assert_eq!(text, "Here is the result:");
        assert_eq!(contents.len(), 1);
        match &contents[0] {
            AssistantContent::ToolCall(tc) => {
                assert_eq!(tc.function.name, "search_web");
                assert_eq!(tc.function.arguments, serde_json::json!({"query": "rust"}));
            }
            _ => panic!("Expected ToolCall content"),
        }
    }

    #[test]
    fn test_extract_text_from_choice_text_only() {
        let contents = vec![AssistantContent::text("hello world")];
        let result = extract_text_from_choice(&contents).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_extract_text_from_choice_mixed() {
        let contents = vec![
            AssistantContent::text("part1"),
            AssistantContent::ToolCall(ToolCall::new(
                "id1".to_string(),
                ToolFunction::new("tool".to_string(), serde_json::json!({})),
            )),
            AssistantContent::text("part2"),
        ];
        let result = extract_text_from_choice(&contents).unwrap();
        assert_eq!(result, "part1part2");
    }

    #[test]
    fn test_extract_text_from_choice_no_text() {
        let contents = vec![AssistantContent::ToolCall(ToolCall::new(
            "id1".to_string(),
            ToolFunction::new("tool".to_string(), serde_json::json!({})),
        ))];
        let result = extract_text_from_choice(&contents);
        assert!(result.is_err());
    }
}
