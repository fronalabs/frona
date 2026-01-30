use async_trait::async_trait;
use rig::completion::{
    AssistantContent, CompletionModel, CompletionRequest, CompletionResponse,
    Message as RigMessage,
    message::{ToolCall, ToolFunction},
};
use rig::completion::request::ToolDefinition as RigToolDefinition;
use tokio::sync::mpsc;

use super::error::LlmError;

#[derive(Debug, Clone)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn parse(s: &str) -> Result<Self, LlmError> {
        let (provider, model_id) = s
            .split_once('/')
            .ok_or_else(|| LlmError::InvalidModelRef(format!(
                "expected 'provider/model' format, got '{s}'"
            )))?;

        if provider.is_empty() || model_id.is_empty() {
            return Err(LlmError::InvalidModelRef(format!(
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

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        user_message: RigMessage,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<String, LlmError>;

    async fn stream_inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        user_message: RigMessage,
        token_tx: mpsc::Sender<Result<String, LlmError>>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(), LlmError>;

    async fn inference_with_tools(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(Vec<AssistantContent>, Vec<RigMessage>), LlmError>;

    async fn stream_inference_with_tools(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<String>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<Vec<AssistantContent>, LlmError>;
}

pub struct RigProvider<C> {
    client: C,
}

impl<C> RigProvider<C> {
    pub fn new(client: C) -> Self {
        Self { client }
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
        mut chat_history: Vec<RigMessage>,
        user_message: RigMessage,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<String, LlmError> {
        use rig::completion::CompletionModel as _;

        let model = self.client.completion_model(model_id);

        chat_history.push(user_message);

        tracing::debug!(
            model = %model_id,
            messages = ?chat_history,
            "LLM request"
        );

        let request = CompletionRequest {
            preamble: Some(system_prompt.to_string()),
            chat_history: rig::OneOrMany::many(chat_history)
                .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::user(""))),
            documents: vec![],
            tools: vec![],
            temperature,
            max_tokens,
            tool_choice: None,
            additional_params: None,
        };

        let response = model
            .completion(request)
            .await
            .map_err(LlmError::CompletionFailed)?;

        let text = extract_text_from_choice(&response.choice)?;

        tracing::debug!(
            model = %model_id,
            response = %text,
            "LLM response"
        );

        Ok(text)
    }

    async fn stream_inference(
        &self,
        model_id: &str,
        system_prompt: &str,
        mut chat_history: Vec<RigMessage>,
        user_message: RigMessage,
        token_tx: mpsc::Sender<Result<String, LlmError>>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(), LlmError> {
        use futures::StreamExt;
        use rig::completion::CompletionModel as _;

        let model = self.client.completion_model(model_id);

        chat_history.push(user_message);

        tracing::debug!(
            model = %model_id,
            messages = ?chat_history,
            "LLM stream request"
        );

        let request = CompletionRequest {
            preamble: Some(system_prompt.to_string()),
            chat_history: rig::OneOrMany::many(chat_history)
                .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::user(""))),
            documents: vec![],
            tools: vec![],
            temperature,
            max_tokens,
            tool_choice: None,
            additional_params: None,
        };

        let mut stream = model
            .stream(request)
            .await
            .map_err(LlmError::CompletionFailed)?;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(rig::streaming::StreamedAssistantContent::Text(text)) => {
                    if token_tx.send(Ok(text.text)).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = token_tx
                        .send(Err(LlmError::StreamingFailed(format!("{e}"))))
                        .await;
                    break;
                }
            }
        }

        Ok(())
    }

    async fn inference_with_tools(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<(Vec<AssistantContent>, Vec<RigMessage>), LlmError> {
        use rig::completion::CompletionModel as _;

        let model = self.client.completion_model(model_id);

        tracing::debug!(
            model = %model_id,
            messages = ?chat_history,
            tool_count = tools.len(),
            "LLM tool request"
        );

        let request = CompletionRequest {
            preamble: Some(system_prompt.to_string()),
            chat_history: rig::OneOrMany::many(chat_history.clone())
                .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::user(""))),
            documents: vec![],
            tools,
            temperature,
            max_tokens,
            tool_choice: None,
            additional_params: None,
        };

        let response: CompletionResponse<_> = model
            .completion(request)
            .await
            .map_err(LlmError::CompletionFailed)?;

        let contents: Vec<AssistantContent> = response.choice.into_iter().collect();

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            "LLM tool response"
        );

        Ok((contents, chat_history))
    }

    async fn stream_inference_with_tools(
        &self,
        model_id: &str,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<String>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<Vec<AssistantContent>, LlmError> {
        use futures::StreamExt;
        use rig::completion::CompletionModel as _;

        let model = self.client.completion_model(model_id);

        tracing::debug!(
            model = %model_id,
            tool_count = tools.len(),
            "LLM streaming tool request"
        );
        tracing::debug!(system_prompt = %system_prompt, "LLM system prompt");
        tracing::debug!(chat_history = ?chat_history, "LLM chat history");

        let tool_name_strings: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

        let request = CompletionRequest {
            preamble: Some(system_prompt.to_string()),
            chat_history: rig::OneOrMany::many(chat_history)
                .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::user(""))),
            documents: vec![],
            tools,
            temperature,
            max_tokens,
            tool_choice: None,
            additional_params: None,
        };

        let mut stream = model
            .stream(request)
            .await
            .map_err(LlmError::CompletionFailed)?;

        let mut contents: Vec<AssistantContent> = Vec::new();
        let mut accumulated_text = String::new();
        let mut buffering = true;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(rig::streaming::StreamedAssistantContent::Text(text)) => {
                    accumulated_text.push_str(&text.text);
                    if buffering {
                        if accumulated_text.len() >= 64 {
                            let has_tool_name = tool_name_strings
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
                Ok(rig::streaming::StreamedAssistantContent::ToolCall {
                    tool_call, ..
                }) => {
                    contents.push(AssistantContent::ToolCall(tool_call));
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(LlmError::StreamingFailed(format!("{e}")));
                }
            }
        }

        let has_tool_calls = contents.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)));

        if !has_tool_calls && !accumulated_text.is_empty() && buffering {
            let tool_names: Vec<&str> = tool_name_strings.iter().map(|s| s.as_str()).collect();
            let extracted = try_extract_tool_calls_from_text(&accumulated_text, &tool_names);

            if !extracted.is_empty() {
                tracing::warn!(
                    model = %model_id,
                    count = extracted.len(),
                    "Recovered tool call from text output"
                );

                let mut remaining = accumulated_text.clone();
                for tc in extracted.iter().rev() {
                    remaining.replace_range(tc.start..tc.end, "");
                }
                accumulated_text = remaining.trim().to_string();

                for tc in extracted {
                    contents.push(AssistantContent::ToolCall(ToolCall::new(
                        uuid::Uuid::new_v4().to_string(),
                        ToolFunction::new(tc.tool_name, tc.arguments),
                    )));
                }
            }
        }

        if !accumulated_text.is_empty() {
            if buffering {
                let _ = token_tx.send(accumulated_text.clone()).await;
            }
            contents.insert(0, AssistantContent::text(&accumulated_text));
        }

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            "LLM streaming tool response"
        );

        Ok(contents)
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
        .map_or(true, |ch| !ch.is_alphanumeric() && ch != '_')
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

fn extract_text_from_choice(
    choice: &rig::OneOrMany<AssistantContent>,
) -> Result<String, LlmError> {
    let mut text_parts = Vec::new();

    for item in choice.iter() {
        if let AssistantContent::Text(t) = item {
            text_parts.push(t.text.clone());
        }
    }

    if text_parts.is_empty() {
        return Err(LlmError::InferenceFailed(
            "No text content in response".to_string(),
        ));
    }

    Ok(text_parts.join(""))
}
