pub mod adapter;
mod admin;
pub use admin::{ProviderAdmin, ProviderConnection};
pub mod group;
pub mod platform;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{
    AssistantContent, CompletionRequest, Message as RigMessage,
    message::{ToolCall, ToolChoice, ToolFunction},
};
use rig_core::model::ModelList;
use tokio::sync::mpsc;

use crate::Handle;
use crate::config::ProviderModel;
use crate::inference::error::InferenceError;
use crate::inference::metrics;
use crate::inference::observation::ActivityObserver;

pub enum StreamToken {
    Text(String),
    Reasoning(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderModelListSource {
    Account,
    Recipe,
}

#[derive(Debug, Clone)]
pub enum ProviderModelList {
    CatalogFallback,
    Listed {
        source: ProviderModelListSource,
        models: ModelList,
    },
}

#[derive(Debug, Clone)]
pub struct CredentialValidation {
    pub models: Option<ProviderModelList>,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialValidationError {
    #[error("provider rejected authentication")]
    AuthenticationRejected,
    #[error("this adapter does not implement live credential validation")]
    Unsupported,
    #[error("credential validation failed: {0}")]
    Failed(String),
}

impl From<rig_core::error::ProviderError> for CredentialValidationError {
    fn from(error: rig_core::error::ProviderError) -> Self {
        let report = error.report();
        match report.http_status {
            Some(401 | 403) => Self::AuthenticationRejected,
            Some(status) => Self::Failed(format!("provider returned HTTP {status}")),
            None if matches!(
                error,
                rig_core::error::ProviderError::InvalidAuthentication(_)
            ) =>
            {
                Self::AuthenticationRejected
            }
            None => Self::Failed("provider request or response failed".into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("model listing failed: {0}")]
pub struct ModelListError(pub String);

/// Exact name of a configured model group, never an upstream model identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub std::borrow::Cow<'static, str>);

impl ModelRef {
    pub const PRIMARY: Self = Self(std::borrow::Cow::Borrowed("primary"));
    pub const TITLE: Self = Self(std::borrow::Cow::Borrowed("title"));
    pub const MEMORY: Self = Self(std::borrow::Cow::Borrowed("memory"));

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// Canonical catalog brand, populated from the prepared connection.
    pub catalog_provider: String,
    pub provider_handle: Handle,
    pub model_id: String,
    pub provider: ProviderModel,
    pub request_settings: ModelRequestSettings,
}

#[derive(Debug, Clone, Default)]
pub struct ModelRequestSettings {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub extra_params: serde_json::Map<String, serde_json::Value>,
}

impl ModelConfig {
    pub fn as_str(&self) -> String {
        format!("{}/{}", self.provider_handle, self.model_id)
    }

    pub fn provider_name(&self) -> &str {
        self.provider_handle.as_str()
    }
}

#[derive(Clone, Default)]
pub struct InferenceCounter {
    count: Arc<AtomicUsize>,
    observer: Option<Arc<dyn ActivityObserver>>,
}

impl InferenceCounter {
    pub fn new(observer: impl ActivityObserver + 'static) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            observer: Some(Arc::new(observer)),
        }
    }

    fn increment(&self) {
        let val = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(observer) = &self.observer {
            observer.active_requests(val);
        }
        metrics::set_active_inference_requests(val);
    }

    fn decrement(&self) {
        let val = self
            .count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .unwrap_or(0)
            .saturating_sub(1);
        if let Some(observer) = &self.observer {
            observer.active_requests(val);
        }
        metrics::set_active_inference_requests(val);
    }

    pub fn guard(&self) -> InferenceGuard {
        self.increment();
        InferenceGuard {
            counter: self.clone(),
        }
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
    async fn ensure_usable(&self, _model: &ModelConfig) -> Result<(), InferenceError> {
        Ok(())
    }

    /// Resolve the current credential, route and transport for one admitted attempt.
    async fn resolve(&self, model: &ModelConfig) -> Result<ResolvedModel, InferenceError>;
}

/// A callable Rig model and the transport policy selected with its credentials.
/// The options travel together when managed credentials change the selected client.
#[derive(Clone)]
pub struct ResolvedModel {
    pub model: rig_core::DynModel<rig_core::operation::Completion>,
    wire_transport: bool,
    counter: InferenceCounter,
}

impl ResolvedModel {
    pub fn new(model: impl Into<rig_core::DynModel<rig_core::operation::Completion>>) -> Self {
        Self {
            model: model.into(),
            wire_transport: false,
            counter: InferenceCounter::default(),
        }
    }

    pub fn with_wire_transport(mut self) -> Self {
        self.wire_transport = true;
        self
    }

    pub fn with_counter(mut self, counter: InferenceCounter) -> Self {
        self.counter = counter;
        self
    }
}

pub const SUBMIT_TOOL_NAME: &str = "submit";

type CompletionModelFactory = dyn Fn(&ModelConfig) -> ResolvedModel + Send + Sync;

pub struct RigProvider {
    model: Arc<CompletionModelFactory>,
    counter: InferenceCounter,
    wire_transport: bool,
}

impl RigProvider {
    pub fn new(
        model: impl Fn(&str) -> rig_core::DynModel<rig_core::operation::Completion>
        + Send
        + Sync
        + 'static,
        counter: InferenceCounter,
    ) -> Self {
        Self::from_model_factory(
            move |config| ResolvedModel::new(model(&config.model_id)),
            counter,
        )
    }

    pub fn from_model_factory(
        model: impl Fn(&ModelConfig) -> ResolvedModel + Send + Sync + 'static,
        counter: InferenceCounter,
    ) -> Self {
        Self {
            model: Arc::new(model),
            counter,
            wire_transport: false,
        }
    }

    pub fn with_wire_transport(mut self) -> Self {
        self.wire_transport = true;
        self
    }
}

#[async_trait]
impl ModelProvider for RigProvider {
    async fn resolve(&self, model: &ModelConfig) -> Result<ResolvedModel, InferenceError> {
        let mut resolved = (self.model)(model);
        resolved.wire_transport |= self.wire_transport;
        resolved.counter = self.counter.clone();
        Ok(resolved)
    }
}

/// One Rig invocation shared by SDK clients, prepared groups and server streaming.
/// Admission, retries and output validation belong to the calling execution layer.
pub struct RigModelAdapter {
    provider: Arc<dyn ModelProvider>,
    config: ModelConfig,
}

impl RigModelAdapter {
    pub fn new(provider: Arc<dyn ModelProvider>, config: ModelConfig) -> Self {
        Self { provider, config }
    }
}

#[async_trait]
impl crate::ModelAdapter for RigModelAdapter {
    fn capabilities(&self) -> crate::Capabilities {
        crate::Capabilities {
            text: true,
            tools: true,
            streaming: true,
            structured: true,
            structured_streaming: false,
            structured_tools: false,
        }
    }

    async fn generate(
        &self,
        request: crate::ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<crate::ModelOutput, InferenceError> {
        // Called inside the admitted attempt: a denied permit never resolves or
        // refreshes a credential. Preflight remains the caller's separate check.
        let resolved = self.provider.resolve(&self.config).await?;
        let _guard = resolved.counter.guard();
        let structured = request.schema.is_some();
        let mut wire = crate::inference::protocol::parameters::WireParameters::prepare(
            &self.config,
            request.max_tokens,
            request.temperature,
            structured
                || request
                    .tools
                    .iter()
                    .any(|tool| tool.name == SUBMIT_TOOL_NAME),
        )?;
        if !resolved.wire_transport && !self.config.request_settings.extra_params.is_empty() {
            return Err(InferenceError::ConfigError(
                "extra_params requires the provider platform's native HTTP transport".into(),
            ));
        }
        for override_ in &wire.overrides {
            tracing::warn!(config_path = %override_.config_path, wire_path = ?override_.wire_path, "Custom parameter overrides a typed setting");
        }
        let crate::ModelRequest {
            system,
            mut messages,
            mut tools,
            schema,
            ..
        } = request;
        crate::inference::history_compat::prepare_legacy_history(
            &mut messages,
            &self.config,
            &resolved.model,
        )?;
        if let Some(schema) = schema {
            tools.push(RigToolDefinition {
                name: rig_core::message::ToolName::new(SUBMIT_TOOL_NAME).expect("submit is nonempty"),
                annotations: Default::default(),
                description: "Submit the structured output. You MUST call this tool exactly once with the required fields filled in.".into(),
                parameters: schema,
            });
        }
        let model_id = self.config.model_id.as_str();
        tracing::debug!(model = %model_id, messages = ?messages, tool_count = tools.len(), "LLM request");
        let tool_names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        let traced = crate::inference::trace::enabled().then(|| (messages.clone(), tools.clone()));
        let mut native = completion_request(wire.request.clone(), &system, messages, tools);
        if structured {
            native.tool_choice = Some(ToolChoice::Required);
        }
        wire.configure_native(&mut native, &resolved.model, resolved.wire_transport);
        if !resolved.wire_transport && wire.requires_wire_transport() {
            return Err(InferenceError::ConfigError(
                "Configured final-wire settings require the provider platform's native HTTP transport".into(),
            ));
        }
        let mut output = crate::inference::protocol::http::scope(wire, async {
            if let Some(tokens) = tokens {
                let stream = resolved.model.stream(native)?;
                let StreamConsumed {
                    mut accumulated_text,
                    mut output,
                    still_buffering,
                } = consume_tool_stream(stream, &tokens, &tool_names).await?;
                let has_calls = output
                    .content
                    .iter()
                    .any(|item| matches!(item, AssistantContent::ToolCall(_)));
                if !has_calls && !accumulated_text.is_empty() && still_buffering {
                    let original = accumulated_text.clone();
                    recover_tool_calls_from_text(
                        &mut accumulated_text,
                        &mut output.content,
                        &tool_names,
                        model_id,
                    );
                    if accumulated_text != original {
                        output
                            .content
                            .retain(|item| !matches!(item, AssistantContent::Text(_)));
                        if !accumulated_text.is_empty() {
                            output
                                .content
                                .insert(0, AssistantContent::text(&accumulated_text));
                        }
                    }
                }
                if !accumulated_text.is_empty() && still_buffering {
                    let _ = tokens.send(StreamToken::Text(accumulated_text)).await;
                }
                Ok::<_, InferenceError>(output)
            } else {
                Ok(crate::ModelOutput::from_response(
                    resolved.model.call(native).await?,
                ))
            }
        })
        .await?;
        if structured {
            // Keep the entire response even when submission parsing fails. The
            // caller charges usage before validating or returning feedback.
            output.structured = output.content.iter().find_map(|item| match item {
                AssistantContent::ToolCall(call)
                    if call.function.name == SUBMIT_TOOL_NAME
                        && call.function.invalid_arguments.is_none() =>
                {
                    Some(serde_json::Value::Object(call.function.arguments.clone()))
                }
                _ => None,
            });
        }
        tracing::debug!(model = %model_id, response = ?output.content, usage = ?output.usage, "LLM response");
        if let Some((history, tools)) = traced {
            crate::inference::trace::record(
                crate::inference::trace::Exchange {
                    model: model_id,
                    system: &system,
                    history: &history,
                    tools: &tools,
                },
                &output.content,
            );
        }
        Ok(output)
    }
}

fn completion_request(
    mut request: CompletionRequest,
    system: &str,
    mut history: Vec<RigMessage>,
    tools: Vec<RigToolDefinition>,
) -> CompletionRequest {
    if history.is_empty() {
        history.push(RigMessage::user(""));
    }
    // SurrealDB reorders object keys. Normalize live and replayed arguments so
    // reconstructing history never changes an already-sent prompt prefix.
    for message in &mut history {
        if let RigMessage::Assistant(message) = message {
            for item in &mut message.content {
                if let AssistantContent::ToolCall(call) = item {
                    let mut arguments =
                        serde_json::Value::Object(std::mem::take(&mut call.function.arguments));
                    arguments.sort_all_objects();
                    let serde_json::Value::Object(arguments) = arguments else {
                        unreachable!()
                    };
                    call.function.arguments = arguments;
                }
            }
        }
    }
    request.chat_history = std::iter::once(Arc::new(RigMessage::system(system)))
        .chain(history.into_iter().map(Arc::new))
        .collect();
    request.tools = tools.into_iter().map(Arc::new).collect();
    request
}

struct StreamConsumed {
    accumulated_text: String,
    output: crate::ModelOutput,
    still_buffering: bool,
}

async fn consume_tool_stream(
    mut stream: rig_core::streaming::CompletionStream,
    token_tx: &mpsc::Sender<StreamToken>,
    tool_names: &[String],
) -> Result<StreamConsumed, InferenceError> {
    use futures::StreamExt;
    use rig_core::streaming::{Item, StreamEvent};
    use std::time::Instant;

    let stream_start = Instant::now();
    let mut ttft_ms = None;
    let mut accumulated_text = String::new();
    let mut buffering = true;
    while let Some(chunk) = stream.next().await {
        match chunk? {
            Item::Event(StreamEvent::Text { text, .. }) => {
                ttft_ms.get_or_insert_with(|| stream_start.elapsed().as_millis() as u64);
                accumulated_text.push_str(&text);
                if buffering {
                    if accumulated_text.len() >= 64
                        && !tool_names
                            .iter()
                            .any(|name| accumulated_text.contains(name.as_str()))
                    {
                        let _ = token_tx
                            .send(StreamToken::Text(accumulated_text.clone()))
                            .await;
                        buffering = false;
                    }
                } else {
                    let _ = token_tx.send(StreamToken::Text(text)).await;
                }
            }
            Item::Event(StreamEvent::Reasoning { text, .. }) => {
                ttft_ms.get_or_insert_with(|| stream_start.elapsed().as_millis() as u64);
                let _ = token_tx.send(StreamToken::Reasoning(text)).await;
            }
            _ => {}
        }
    }
    // Finish verifies the provider end and retains native blocks and their order;
    // reconstructing them from deltas would lose replay signatures and metadata.
    let response = stream.finish().await?;
    Ok(StreamConsumed {
        accumulated_text,
        output: {
            let mut output = crate::ModelOutput::from_response(response);
            output.ttft_ms = ttft_ms;
            output
        },
        still_buffering: buffering,
    })
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
            rig_core::completion::message::CallId::from_wire(
                uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::NoContext)).to_string(),
            ),
            ToolFunction::new(
                rig_core::message::ToolName::new(tc.tool_name)
                    .expect("offered tool name is nonempty"),
                tc.arguments,
            ),
        )));
    }
}

#[derive(Debug)]
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

/// Parse model-authored JSON, tolerating the ways a model writes it rather than the way a
/// serializer does: markdown fences, trailing commas, single quotes, unquoted keys, and an
/// object cut off mid-value.
///
/// `serde_json` first, because it is strict and by far the common case; `json_partial` only
/// when that fails, so a well-formed payload can never be reinterpreted by a repair pass.
/// `None` means unusable even after repair.
fn parse_lenient_json(raw: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return Some(v);
    }
    let parsed = json_partial::jsonish::parse(raw, json_partial::jsonish::ParseOptions::default())
        .ok()
        .map(|v| json_partial::jsonish::jsonish_to_serde(&v))?;

    // The repair pass practically never fails - it coerces. `I cannot classify this` comes
    // back as a JSON *string*, and `{invalid json here}` as `{}`. Neither is a tool call,
    // so two things are checked rather than trusting that a parse succeeded:
    //
    //   * it must be an object, because tool arguments always are;
    //   * it must not be an *empty* object salvaged from non-empty input, which is the
    //     `{invalid json here}` case - accepting it would invent an argument-less call the
    //     model never made, and silently, which is worse than dropping it.
    let obj = parsed.as_object()?;
    let braces_were_empty = raw
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim()
        .is_empty();
    if obj.is_empty() && !braces_were_empty {
        tracing::debug!(
            raw_len = raw.len(),
            "model JSON salvaged nothing; not a tool call"
        );
        return None;
    }
    tracing::debug!(
        raw_len = raw.len(),
        "repaired model JSON that serde_json refused"
    );
    Some(parsed)
}

fn try_extract_tool_calls_from_text(text: &str, tool_names: &[&str]) -> Vec<ExtractedToolCall> {
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

            // Brace depth locates the *end* of the object; `json_partial` decides whether
            // what is inside it is usable.
            //
            // Depth counting alone is not enough and used to be all there was: it has no
            // notion of string literals, so a brace inside a value - `{"note": "use }
            // carefully"}` - closed the object early, `from_str` failed on the fragment,
            // and the call was dropped without a trace. Tracking strings and escapes here
            // fixes that one case; it does not fix fences, trailing commas or a response
            // truncated mid-object, which are the same class of problem and all things a
            // model emitting tool calls as prose actually does.
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escaped = false;
            let mut json_end = None;
            for (i, ch) in text[json_start..].char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' if in_string => escaped = true,
                    '"' => in_string = !in_string,
                    '{' if !in_string => depth += 1,
                    '}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            json_end = Some(json_start + i + ch.len_utf8());
                            break;
                        }
                    }
                    _ => {}
                }
            }
            // Unterminated: hand the rest of the text over anyway, since a truncated
            // object is exactly what the repair pass is for.
            let json_end = json_end.unwrap_or(text.len());

            let json_str = &text[json_start..json_end];
            match parse_lenient_json(json_str) {
                Some(args) => {
                    results.push(ExtractedToolCall {
                        tool_name: name.to_string(),
                        arguments: args,
                        start: abs_pos,
                        end: json_end,
                    });
                    search_from = json_end;
                }
                None => continue,
            }
        }
    }

    results.sort_by_key(|r| r.start);
    results
}

pub fn extract_text_from_choice(contents: &[AssistantContent]) -> Result<String, InferenceError> {
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
    use crate::config::{AnthropicParams, GeminiParams, OpenAICompatParams, OpenAiApi};
    use crate::inference::protocol::parameters::WireParameters;
    use rig_core::completion::message::{ToolCall, ToolFunction};

    #[test]
    fn model_reference_preserves_exact_configured_name() {
        let reference: ModelRef = serde_json::from_str("\" Primary \"").unwrap();
        assert_eq!(reference.as_str(), " Primary ");
        assert_ne!(reference, ModelRef::PRIMARY);
        assert_eq!(serde_json::to_string(&reference).unwrap(), "\" Primary \"");
    }

    #[test]
    fn known_model_references_are_borrowed_and_round_trip() {
        for (reference, name) in [
            (ModelRef::PRIMARY, "primary"),
            (ModelRef::TITLE, "title"),
            (ModelRef::MEMORY, "memory"),
        ] {
            assert!(matches!(reference.0, std::borrow::Cow::Borrowed(_)));
            assert_eq!(reference.as_str(), name);
            let json = serde_json::to_string(&reference).unwrap();
            let restored: ModelRef = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, reference);
            assert_eq!(restored, ModelRef(name.to_owned().into()));
        }
    }

    fn openai_model(api: OpenAiApi, params: OpenAICompatParams) -> ModelConfig {
        ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: Handle::const_validated("openai"),
            model_id: "test-model".to_string(),
            provider: ProviderModel::OpenAI {
                api: Some(api),
                params,
            },
        }
    }

    #[test]
    fn typed_provider_params_do_not_serialize_absent_fields() {
        let mut model = openai_model(OpenAiApi::ChatCompletions, Default::default());
        model.provider = ProviderModel::Anthropic {
            params: AnthropicParams::default(),
        };
        let plan = WireParameters::prepare(&model, None, None, false).unwrap();
        // Automatic caching is a request default, not a configured sampler field.
        assert_eq!(
            serde_json::to_value(AnthropicParams::default()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(plan.request.additional_params, None);
        assert_eq!(
            plan.request.options.cache,
            Some(rig_core::completion::CacheRetention::Short)
        );
        model.provider = ProviderModel::Gemini {
            params: GeminiParams::default(),
        };
        let plan = WireParameters::prepare(&model, None, None, false).unwrap();
        assert_eq!(plan.request.additional_params, None);
        model.provider = ProviderModel::Gemini {
            params: GeminiParams {
                top_p: Some(0.5),
                ..Default::default()
            },
        };
        let plan = WireParameters::prepare(&model, None, None, false).unwrap();
        assert_eq!(plan.request.additional_params, None);
        assert_eq!(plan.request.options.top_p, Some(0.5));
    }

    #[test]
    fn supported_native_options_survive_check_and_foreign_extensions_restore_raw_fields() {
        use rig_core::{completion::GenerationOptions, providers::openai};
        let model = openai_model(
            OpenAiApi::ChatCompletions,
            OpenAICompatParams {
                top_p: Some(0.75),
                seed: Some(7),
                stop: Some(vec!["done".into()]),
                logprobs: Some(false),
                ..Default::default()
            },
        );
        let native = openai::OpenAIConfig::new("fixture-key")
            .client()
            .chat("private-model")
            .erase();
        let mut plan = WireParameters::prepare(&model, None, None, false).unwrap();
        let mut request = plan.request.clone();
        plan.configure_native(&mut request, &native, false);
        assert_eq!(
            request.options,
            GenerationOptions::default()
                .top_p(0.75)
                .seed(7)
                .stop(["done"])
        );
        assert!(
            request
                .provider_options
                .contains::<openai::extension::OpenAiExt>()
        );
        assert_eq!(request.additional_params, None);

        // A typed OpenAI extension must never silently disappear on another
        // vendor's otherwise compatible wire.
        let foreign = openai::OpenAIConfig::with_key(&openai::wire::GROQ, "fixture-key")
            .client()
            .chat("private-model")
            .erase();
        let mut plan = WireParameters::prepare(&model, None, None, false).unwrap();
        let mut request = plan.request.clone();
        plan.configure_native(&mut request, &foreign, false);
        assert!(request.options.is_default());
        assert!(request.provider_options.is_empty());
        assert_eq!(
            request.additional_params,
            Some(serde_json::json!({
                "top_p": 0.75, "seed": 7, "stop": ["done"], "logprobs": false,
            }))
        );
    }

    #[test]
    fn provider_integer_narrowing_is_checked_before_sdk_conversion() {
        let mut model = openai_model(OpenAiApi::ChatCompletions, Default::default());
        model.provider = ProviderModel::Gemini {
            params: GeminiParams {
                top_k: Some(u64::MAX),
                ..Default::default()
            },
        };
        let error = WireParameters::prepare_named(&model, None, None, false, "models.primary")
            .err()
            .unwrap();
        assert!(error.to_string().contains("models.primary.top_k"));
        model.provider = ProviderModel::Bedrock {
            params: Default::default(),
        };
        let error =
            WireParameters::prepare_named(&model, Some(u64::MAX), None, false, "models.primary")
                .err()
                .unwrap();
        assert!(error.to_string().contains("models.primary.max_tokens"));
    }

    #[test]
    fn openai_chat_request_uses_chat_parameter_shape() {
        let model = openai_model(
            OpenAiApi::ChatCompletions,
            OpenAICompatParams {
                max_completion_tokens: Some(123),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
        );
        let mut plan = WireParameters::prepare(&model, Some(456), None, false).unwrap();
        let native = rig_core::providers::openai::OpenAIConfig::new("fixture-key")
            .client()
            .chat("test-model")
            .erase();
        let mut params = plan.request.clone();
        plan.configure_native(&mut params, &native, true);
        assert_eq!(params.max_tokens, None);
        assert_eq!(
            params.options.reasoning,
            Some(rig_core::completion::Reasoning::Effort(
                rig_core::completion::Effort::High
            ))
        );
        assert_eq!(
            params.additional_params,
            Some(serde_json::json!({"max_completion_tokens": 123}))
        );
    }

    #[test]
    fn openai_responses_request_uses_responses_parameter_shape() {
        let model = openai_model(
            OpenAiApi::Responses,
            OpenAICompatParams {
                max_completion_tokens: Some(123),
                reasoning_effort: Some("high".to_string()),
                top_logprobs: Some(5),
                ..Default::default()
            },
        );
        let plan = WireParameters::prepare(&model, Some(456), None, false).unwrap();
        let params = &plan.request;
        assert_eq!(params.max_tokens, Some(123));
        assert_eq!(params.additional_params, None);
        assert_eq!(
            params.options.reasoning,
            Some(rig_core::completion::Reasoning::Effort(
                rig_core::completion::Effort::High
            ))
        );
        assert_eq!(
            serde_json::to_value(
                params
                    .provider_options
                    .get::<rig_core::providers::openai::extension::OpenAiExt>()
                    .unwrap()
            )
            .unwrap(),
            serde_json::json!({"openai.responses": {"top_logprobs": 5}}),
        );
    }

    #[test]
    fn openai_responses_rejects_unsupported_parameters() {
        let model = openai_model(
            OpenAiApi::Responses,
            OpenAICompatParams {
                stop: Some(vec!["done".to_string()]),
                ..Default::default()
            },
        );
        let error = match WireParameters::prepare(&model, None, None, false) {
            Ok(_) => panic!("unsupported parameter was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("stop"));
    }

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

    /// A brace inside a string value is not a structural brace. The depth counter does not
    /// know that, so it closes early, `from_str` fails on the fragment, and `Err(_) =>
    /// continue` drops the whole tool call - silently, which reads downstream as the model
    /// having ignored the tool.
    #[test]
    fn extract_tolerates_a_brace_inside_a_string_value() {
        let text = r#"mytool {"note": "use } carefully", "key": "value"}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert_eq!(
            results.len(),
            1,
            "the call must not be dropped: {results:?}"
        );
        assert_eq!(
            results[0].arguments,
            serde_json::json!({"note": "use } carefully", "key": "value"})
        );
    }

    /// The same for an escaped quote, which can hide a brace from any scanner that does not
    /// track escapes either.
    #[test]
    fn extract_tolerates_an_escaped_quote_before_a_brace() {
        let text = r#"mytool {"q": "a \" then }", "k": 1}"#;
        let results = try_extract_tool_calls_from_text(text, &["mytool"]);
        assert_eq!(
            results.len(),
            1,
            "the call must not be dropped: {results:?}"
        );
    }

    /// Repair must not become invention. The pass coerces rather than fails, so these are
    /// the cases where a successful parse still has to be rejected.
    #[test]
    fn repair_does_not_invent_a_call_from_unusable_text() {
        // Salvages nothing but an empty object - accepting it would fabricate a call.
        assert!(parse_lenient_json("{invalid json here}").is_none());
        // Coerced to a JSON string; tool arguments are always an object.
        assert!(parse_lenient_json("I cannot classify this entity.").is_none());
        // A genuinely argument-less call is still fine.
        assert_eq!(parse_lenient_json("{}"), Some(serde_json::json!({})));
    }

    /// The repair only runs when strict parsing fails, so a well-formed payload can never
    /// be reinterpreted by it.
    #[test]
    fn valid_json_is_parsed_strictly_and_unchanged() {
        let raw = r#"{"a":1,"b":[{"c":"x"}]}"#;
        assert_eq!(
            parse_lenient_json(raw),
            Some(serde_json::json!({"a":1,"b":[{"c":"x"}]}))
        );
    }

    /// The failure modes a model emitting prose tool calls actually produces.
    #[test]
    fn repair_recovers_the_shapes_models_write() {
        for raw in [
            r#"{"key": "value",}"#, // trailing comma
            r#"{key: "value"}"#,    // unquoted key
            r#"{'key': 'value'}"#,  // single quotes
            r#"{"key": "value""#,   // truncated
        ] {
            let got = parse_lenient_json(raw);
            assert_eq!(got, Some(serde_json::json!({"key": "value"})), "raw: {raw}");
        }
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
                assert_eq!(
                    tc.function.arguments_value(),
                    serde_json::json!({"query": "rust"})
                );
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
                rig_core::completion::message::CallId::from_wire("id1"),
                ToolFunction::new("tool".try_into().unwrap(), serde_json::json!({})),
            )),
            AssistantContent::text("part2"),
        ];
        let result = extract_text_from_choice(&contents).unwrap();
        assert_eq!(result, "part1part2");
    }

    #[test]
    fn test_extract_text_from_choice_no_text() {
        let contents = vec![AssistantContent::ToolCall(ToolCall::new(
            rig_core::completion::message::CallId::from_wire("id1"),
            ToolFunction::new("tool".try_into().unwrap(), serde_json::json!({})),
        ))];
        let result = extract_text_from_choice(&contents);
        assert!(result.is_err());
    }
}
