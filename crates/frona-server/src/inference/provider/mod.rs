pub(crate) mod adapter;
pub mod group;
pub mod platform;
pub(crate) mod registry;
pub mod service;
pub mod validation;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rig_core::client::ModelListingClient;
use rig_core::completion::request::{ToolDefinition as RigToolDefinition, Usage};
use rig_core::completion::{
    AssistantContent, CompletionModel, CompletionRequest, CompletionResponse,
    Message as RigMessage,
    message::{ToolCall, ToolChoice, ToolFunction},
};
use rig_core::model::ModelList;
use tokio::sync::mpsc;

use crate::chat::broadcast::BroadcastService;
use crate::core::Handle;
use crate::core::config::{OpenAiApi, ProviderModel};
use crate::core::metrics;
use crate::inference::error::InferenceError;

pub enum StreamToken {
    Text(String),
    Reasoning(String),
}

/// Result of a single provider call. Returned by both streaming and non-streaming
/// paths so the retry layer + tool loop can treat them uniformly. Usage defaults
/// to zeros if the provider omitted it.
///
/// `ttft_ms` is the wall time from the start of `consume_tool_stream` to the
/// first text/reasoning chunk arriving on the wire. `None` on the non-streaming
/// path (no first-token concept - the whole response arrives at once).
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    pub content: Vec<AssistantContent>,
    pub usage: Usage,
    pub ttft_ms: Option<u64>,
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

impl From<rig_core::model::ModelListingError> for CredentialValidationError {
    fn from(error: rig_core::model::ModelListingError) -> Self {
        use rig_core::model::ModelListingError;
        match error {
            ModelListingError::ApiError {
                status_code: 401 | 403,
                ..
            }
            | ModelListingError::AuthError { .. } => Self::AuthenticationRejected,
            ModelListingError::ApiError { status_code, .. } => {
                Self::Failed(format!("provider returned HTTP {status_code}"))
            }
            ModelListingError::RequestError { .. } => {
                Self::Failed("provider request failed".into())
            }
            ModelListingError::ParseError { .. } => {
                Self::Failed("invalid provider response".into())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("model listing failed: {0}")]
pub struct ModelListError(pub String);

impl InferenceOutput {
    pub fn new(content: Vec<AssistantContent>, usage: Usage) -> Self {
        Self {
            content,
            usage,
            ttft_ms: None,
        }
    }

    pub fn with_ttft(mut self, ttft_ms: Option<u64>) -> Self {
        self.ttft_ms = ttft_ms;
        self
    }
}

struct CompletionRequestBuilder<'a> {
    system_prompt: &'a str,
    chat_history: Vec<RigMessage>,
    tools: Vec<RigToolDefinition>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    additional_params: Option<serde_json::Value>,
    tool_choice: Option<ToolChoice>,
}

impl<'a> CompletionRequestBuilder<'a> {
    fn new(system_prompt: &'a str, chat_history: Vec<RigMessage>) -> Self {
        Self {
            system_prompt,
            chat_history,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            additional_params: None,
            tool_choice: None,
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

    fn additional_params(mut self, v: Option<serde_json::Value>) -> Self {
        self.additional_params = v;
        self
    }

    fn tool_choice(mut self, v: ToolChoice) -> Self {
        self.tool_choice = Some(v);
        self
    }

    fn build(self) -> CompletionRequest {
        let chat_history = if self.chat_history.is_empty() {
            vec![RigMessage::user("")]
        } else {
            self.chat_history
        };

        CompletionRequest {
            model: None,
            preamble: Some(self.system_prompt.to_string()),
            chat_history,
            documents: vec![],
            tools: self.tools,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            tool_choice: self.tool_choice,
            additional_params: self.additional_params,
            // A tool's own `parameters` already carry `T`'s schema; this field is the
            // provider's `response_format`, which constrains *message content* instead -
            // a different channel, and one that pulls against `ToolChoice::Required`.
            output_schema: None,
            record_telemetry_content: false,
        }
    }
}

/// Exact name of a configured model group, never an upstream model identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub std::borrow::Cow<'static, str>);

impl ModelRef {
    pub const PRIMARY: Self = Self(std::borrow::Cow::Borrowed("primary"));
    pub const TITLE: Self = Self(std::borrow::Cow::Borrowed("title"));
    pub const COMPACTION: Self = Self(std::borrow::Cow::Borrowed("compaction"));
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
        let val = self
            .count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .unwrap_or(0)
            .saturating_sub(1);
        self.broadcast.broadcast_inference_count(val);
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

    async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        Err(CredentialValidationError::Unsupported)
    }

    async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        Ok(ProviderModelList::CatalogFallback)
    }

    async fn inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError>;

    async fn stream_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<StreamToken>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError>;

    /// For typed extraction use `inference::structured_inference<T>`.
    async fn structured_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError>;
}

pub const SUBMIT_TOOL_NAME: &str = "submit";

pub struct RigProvider<C> {
    client: C,
    counter: InferenceCounter,
    hook: Option<crate::inference::protocol::hooks::RequestHook>,
    live_check: Option<LiveCheck>,
    validation_check: Option<ValidationCheck>,
    wire_transport: bool,
}

type LiveCheckFuture =
    Pin<Box<dyn Future<Output = Result<ModelList, CredentialValidationError>> + Send + 'static>>;
type LiveCheck = Arc<dyn Fn() -> LiveCheckFuture + Send + Sync>;
type ValidationCheck = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), CredentialValidationError>> + Send>>
        + Send
        + Sync,
>;

pub struct OpenAiProvider<H = reqwest::Client> {
    chat_completions: RigProvider<rig_core::providers::openai::CompletionsClient<H>>,
    responses: RigProvider<rig_core::providers::openai::Client<H>>,
}

impl<
    H: rig_core::http_client::HttpClientExt
        + Clone
        + std::fmt::Debug
        + Default
        + Send
        + Sync
        + 'static,
> OpenAiProvider<H>
{
    pub fn new(
        chat_completions: rig_core::providers::openai::CompletionsClient<H>,
        responses: rig_core::providers::openai::Client<H>,
        counter: InferenceCounter,
    ) -> Self {
        let validation_client = chat_completions.clone();
        Self {
            chat_completions: RigProvider::new(chat_completions, counter.clone())
                .with_live_check(move || {
                    let client = validation_client.clone();
                    async move {
                        client
                            .list_models()
                            .await
                            .map_err(CredentialValidationError::from)
                    }
                })
                .with_hook(crate::inference::protocol::hooks::openai),
            responses: RigProvider::new(responses, counter),
        }
    }

    fn api(model: &ModelConfig) -> OpenAiApi {
        match &model.provider {
            ProviderModel::OpenAI { api, .. } => api.unwrap_or_default(),
            _ => OpenAiApi::ChatCompletions,
        }
    }

    pub(crate) fn with_wire_transport(mut self) -> Self {
        self.chat_completions = self.chat_completions.with_wire_transport();
        self.responses = self.responses.with_wire_transport();
        self
    }
}

#[async_trait]
impl<
    H: rig_core::http_client::HttpClientExt
        + Clone
        + std::fmt::Debug
        + Default
        + Send
        + Sync
        + 'static,
> ModelProvider for OpenAiProvider<H>
{
    async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        self.chat_completions.validate_credentials().await
    }

    async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        self.chat_completions.list_models().await
    }

    async fn inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        match Self::api(model) {
            OpenAiApi::ChatCompletions => {
                self.chat_completions
                    .inference(
                        model,
                        system_prompt,
                        chat_history,
                        tools,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
            OpenAiApi::Responses => {
                self.responses
                    .inference(
                        model,
                        system_prompt,
                        chat_history,
                        tools,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
        }
    }

    async fn stream_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<StreamToken>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        match Self::api(model) {
            OpenAiApi::ChatCompletions => {
                self.chat_completions
                    .stream_inference(
                        model,
                        system_prompt,
                        chat_history,
                        tools,
                        token_tx,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
            OpenAiApi::Responses => {
                self.responses
                    .stream_inference(
                        model,
                        system_prompt,
                        chat_history,
                        tools,
                        token_tx,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
        }
    }

    async fn structured_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        match Self::api(model) {
            OpenAiApi::ChatCompletions => {
                self.chat_completions
                    .structured_inference(
                        model,
                        system_prompt,
                        chat_history,
                        schema,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
            OpenAiApi::Responses => {
                self.responses
                    .structured_inference(
                        model,
                        system_prompt,
                        chat_history,
                        schema,
                        max_tokens,
                        temperature,
                    )
                    .await
            }
        }
    }
}

impl<C> RigProvider<C> {
    pub fn new(client: C, counter: InferenceCounter) -> Self {
        Self {
            client,
            counter,
            hook: None,
            live_check: None,
            validation_check: None,
            wire_transport: false,
        }
    }

    pub fn with_hook(mut self, hook: crate::inference::protocol::hooks::RequestHook) -> Self {
        self.hook = Some(hook);
        self
    }

    pub(crate) fn with_wire_transport(mut self) -> Self {
        self.wire_transport = true;
        self
    }

    fn prepare_parameters(
        &self,
        model: &ModelConfig,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
        structured: bool,
    ) -> Result<
        (
            crate::inference::protocol::hooks::RequestParams,
            crate::inference::protocol::parameters::WireParameters,
        ),
        InferenceError,
    > {
        // Callers pass only explicit request/model values. Application defaults
        // are used for budgeting in the retry layer, not sent as generation settings.
        let wire = crate::inference::protocol::parameters::WireParameters::prepare(
            model,
            max_tokens,
            temperature,
            structured,
        )?;
        if !self.wire_transport && !model.request_settings.extra_params.is_empty() {
            return Err(InferenceError::ConfigError(
                "extra_params requires the provider platform's native HTTP transport".into(),
            ));
        }
        let params = match (self.wire_transport, self.hook) {
            (false, Some(hook)) => hook(wire.request.clone()),
            _ => wire.request.clone(),
        };
        for override_ in &wire.overrides {
            tracing::warn!(config_path = %override_.config_path, wire_path = ?override_.wire_path, "Custom parameter overrides a typed setting");
        }
        Ok((params, wire))
    }

    pub fn with_live_check<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ModelList, CredentialValidationError>> + Send + 'static,
    {
        self.live_check = Some(Arc::new(move || Box::pin(check())));
        self
    }

    /// A read-only authentication check that does not return model inventory.
    pub fn with_validation_check<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), CredentialValidationError>> + Send + 'static,
    {
        self.validation_check = Some(Arc::new(move || Box::pin(check())));
        self
    }
}

#[async_trait]
impl<C> ModelProvider for RigProvider<C>
where
    C: rig_core::client::CompletionClient + Send + Sync,
    C::CompletionModel: CompletionModel + Send + Sync + 'static,
{
    async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        if let Some(check) = &self.validation_check {
            check().await?;
            return Ok(CredentialValidation { models: None });
        }
        let check = self
            .live_check
            .as_ref()
            .ok_or(CredentialValidationError::Unsupported)?;
        let models = check().await?;
        Ok(CredentialValidation {
            models: Some(ProviderModelList::Listed {
                source: ProviderModelListSource::Account,
                models,
            }),
        })
    }

    async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        let Some(check) = &self.live_check else {
            return Err(ModelListError(
                "Live model discovery is unavailable for this adapter and authentication method"
                    .into(),
            ));
        };
        check()
            .await
            .map(|models| ProviderModelList::Listed {
                source: ProviderModelListSource::Account,
                models,
            })
            .map_err(|error| ModelListError(error.to_string()))
    }

    async fn inference(
        &self,
        model_ref: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        use rig_core::completion::CompletionModel as _;

        let (params, wire) = self.prepare_parameters(
            model_ref,
            max_tokens,
            temperature,
            tools.iter().any(|tool| tool.name == SUBMIT_TOOL_NAME),
        )?;
        let model_id = model_ref.model_id.as_str();

        let _guard = self.counter.guard();
        let model = self.client.completion_model(model_id);

        tracing::debug!(
            model = %model_id,
            messages = ?chat_history,
            tool_count = tools.len(),
            "LLM request"
        );

        // Kept only when tracing is on - the builder consumes both, and cloning a whole
        // history per call to satisfy a disabled debug path is not worth it.
        let traced = crate::inference::trace::enabled().then(|| {
            (
                chat_history.clone(),
                tools.clone(),
                system_prompt.to_string(),
            )
        });

        let request = CompletionRequestBuilder::new(system_prompt, chat_history)
            .tools(tools)
            .max_tokens(params.max_tokens)
            .temperature(params.temperature)
            .additional_params(params.additional_params)
            .build();

        let response: CompletionResponse =
            crate::inference::protocol::http::scope(wire, model.completion(request))
                .await
                .map_err(InferenceError::CompletionFailed)?;

        let contents: Vec<AssistantContent> = response.choice.into_iter().collect();
        let usage = response.usage;

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            "LLM response"
        );
        if let Some((history, tools, system)) = traced {
            crate::inference::trace::record(
                crate::inference::trace::Exchange {
                    model: model_id,
                    system: &system,
                    history: &history,
                    tools: &tools,
                },
                &contents,
            );
        }

        Ok(InferenceOutput::new(contents, usage))
    }

    async fn stream_inference(
        &self,
        model_ref: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<StreamToken>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        use rig_core::completion::CompletionModel as _;

        let (params, wire) = self.prepare_parameters(
            model_ref,
            max_tokens,
            temperature,
            tools.iter().any(|tool| tool.name == SUBMIT_TOOL_NAME),
        )?;
        let model_id = model_ref.model_id.as_str();

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
        let traced = crate::inference::trace::enabled().then(|| {
            (
                chat_history.clone(),
                tools.clone(),
                system_prompt.to_string(),
            )
        });

        let request = CompletionRequestBuilder::new(system_prompt, chat_history)
            .tools(tools)
            .max_tokens(params.max_tokens)
            .temperature(params.temperature)
            .additional_params(params.additional_params)
            .build();

        let StreamConsumed {
            mut accumulated_text,
            mut contents,
            still_buffering,
            usage,
            ttft_ms,
        } = crate::inference::protocol::http::scope(wire, async {
            let stream = model
                .stream(request)
                .await
                .map_err(InferenceError::CompletionFailed)?;
            consume_tool_stream(stream, &token_tx, &tool_names).await
        })
        .await?;

        let has_tool_calls = contents
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
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
                let _ = token_tx
                    .send(StreamToken::Text(accumulated_text.clone()))
                    .await;
            }
            let text_index = contents
                .iter()
                .take_while(|item| matches!(item, AssistantContent::Reasoning(_)))
                .count();
            contents.insert(text_index, AssistantContent::text(&accumulated_text));
        }

        tracing::debug!(
            model = %model_id,
            response = ?contents,
            usage = ?usage,
            ttft_ms = ?ttft_ms,
            "LLM streaming response"
        );
        if let Some((history, tools, system)) = traced {
            crate::inference::trace::record(
                crate::inference::trace::Exchange {
                    model: model_id,
                    system: &system,
                    history: &history,
                    tools: &tools,
                },
                &contents,
            );
        }

        Ok(InferenceOutput::new(contents, usage).with_ttft(ttft_ms))
    }

    async fn structured_inference(
        &self,
        model_ref: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<RigMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        use rig_core::completion::CompletionModel as _;

        let (params, wire) = self.prepare_parameters(model_ref, max_tokens, temperature, true)?;
        let model_id = model_ref.model_id.as_str();

        let _guard = self.counter.guard();
        let model = self.client.completion_model(model_id);

        let submit = RigToolDefinition {
            name: SUBMIT_TOOL_NAME.to_string(),
            description: "Submit the structured output. You MUST call this tool exactly once with the required fields filled in.".to_string(),
            parameters: schema,
        };

        // Logged BEFORE the builder takes ownership of `chat_history`. The response is
        // already logged below; without the request beside it there is no way to tell a
        // model that misread its input from one that ignored it - the difference between
        // a prompt bug and a model returning content that was never sent to it.
        tracing::debug!(
            model = %model_id,
            system = %system_prompt,
            messages = ?chat_history,
            "LLM structured-output request"
        );

        let request = CompletionRequestBuilder::new(system_prompt, chat_history)
            .tools(vec![submit])
            .tool_choice(ToolChoice::Required)
            .max_tokens(params.max_tokens)
            .temperature(params.temperature)
            .additional_params(params.additional_params)
            .build();

        let response: CompletionResponse =
            crate::inference::protocol::http::scope(wire, model.completion(request))
                .await
                .map_err(InferenceError::CompletionFailed)?;

        let arguments = response
            .choice
            .into_iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(ToolCall {
                    function: ToolFunction { name, arguments },
                    ..
                }) if name == SUBMIT_TOOL_NAME => Some(arguments),
                _ => None,
            })
            .ok_or_else(|| {
                InferenceError::InferenceFailed(format!(
                    "model {model_id} did not call the `{SUBMIT_TOOL_NAME}` tool"
                ))
            })?;

        tracing::debug!(model = %model_id, arguments = %arguments, "LLM structured-output response");

        Ok(arguments)
    }
}

struct StreamConsumed {
    accumulated_text: String,
    contents: Vec<AssistantContent>,
    still_buffering: bool,
    usage: Usage,
    ttft_ms: Option<u64>,
}

async fn consume_tool_stream<S>(
    mut stream: S,
    token_tx: &mpsc::Sender<StreamToken>,
    tool_names: &[String],
) -> Result<StreamConsumed, InferenceError>
where
    S: futures::Stream<
            Item = Result<
                rig_core::streaming::StreamedAssistantContent,
                rig_core::completion::CompletionError,
            >,
        > + Unpin,
{
    use futures::StreamExt;
    use std::time::Instant;

    let stream_start = Instant::now();
    let mut ttft_ms: Option<u64> = None;
    let mut contents: Vec<AssistantContent> = Vec::new();
    let mut accumulated_text = String::new();
    let mut buffering = true;
    let mut accumulated_reasoning = String::new();
    let mut reasoning_id: Option<String> = None;
    let mut reasoning_signature: Option<String> = None;
    let mut complete_reasoning: Option<rig_core::completion::message::Reasoning> = None;
    let mut final_usage: Option<Usage> = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(rig_core::streaming::StreamedAssistantContent::Text(text)) => {
                if ttft_ms.is_none() {
                    ttft_ms = Some(stream_start.elapsed().as_millis() as u64);
                }
                accumulated_text.push_str(&text.text);
                if buffering {
                    if accumulated_text.len() >= 64 {
                        let has_tool_name = tool_names
                            .iter()
                            .any(|name| accumulated_text.contains(name.as_str()));
                        if !has_tool_name {
                            let _ = token_tx
                                .send(StreamToken::Text(accumulated_text.clone()))
                                .await;
                            buffering = false;
                        }
                    }
                } else {
                    let _ = token_tx.send(StreamToken::Text(text.text)).await;
                }
            }
            Ok(rig_core::streaming::StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                contents.push(AssistantContent::ToolCall(tool_call));
            }
            Ok(rig_core::streaming::StreamedAssistantContent::Reasoning {
                reasoning: r, ..
            }) => {
                if ttft_ms.is_none() {
                    ttft_ms = Some(stream_start.elapsed().as_millis() as u64);
                }
                let text = r.display_text();
                reasoning_id = r.id.clone();
                reasoning_signature = r.first_signature().map(|s| s.to_string());
                complete_reasoning = Some(r);
                let _ = token_tx.send(StreamToken::Reasoning(text)).await;
            }
            Ok(rig_core::streaming::StreamedAssistantContent::ReasoningDelta {
                provider_id,
                reasoning,
                ..
            }) => {
                if ttft_ms.is_none() {
                    ttft_ms = Some(stream_start.elapsed().as_millis() as u64);
                }
                accumulated_reasoning.push_str(&reasoning);
                if provider_id.is_some() {
                    reasoning_id = provider_id;
                }
                let _ = token_tx.send(StreamToken::Reasoning(reasoning)).await;
            }
            Ok(rig_core::streaming::StreamedAssistantContent::Final(r)) => {
                final_usage = Some(r.usage);
            }
            Ok(_) => {}
            Err(e) => {
                return Err(InferenceError::CompletionFailed(e));
            }
        }
    }

    if let Some(reasoning) = complete_reasoning {
        contents.insert(0, AssistantContent::Reasoning(reasoning));
    } else if !accumulated_reasoning.is_empty() {
        let thinking_chars = accumulated_reasoning.len();
        tracing::debug!(thinking_chars, "Thinking tokens received");
        contents.insert(
            0,
            AssistantContent::Reasoning(rig_core::completion::message::Reasoning {
                id: reasoning_id,
                ..rig_core::completion::message::Reasoning::new_with_signature(
                    &accumulated_reasoning,
                    reasoning_signature,
                )
            }),
        );
    }

    Ok(StreamConsumed {
        accumulated_text,
        contents,
        still_buffering: buffering,
        usage: final_usage.unwrap_or_default(),
        ttft_ms,
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
            rig_core::completion::message::ToolCallId::new_or_mint(
                crate::core::repository::new_id(),
            ),
            ToolFunction::new(tc.tool_name, tc.arguments),
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
    use crate::core::config::{AnthropicParams, GeminiParams, OpenAICompatParams};
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
            (ModelRef::COMPACTION, "compaction"),
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
        assert_eq!(plan.request.additional_params, None);
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
        assert_eq!(
            plan.request.additional_params,
            Some(serde_json::json!({"generationConfig": {"topP": 0.5}}))
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
        let plan = WireParameters::prepare(&model, Some(456), None, false).unwrap();
        let params = crate::inference::protocol::hooks::openai(plan.request);
        assert_eq!(params.max_tokens, None);
        assert_eq!(
            params.additional_params,
            Some(serde_json::json!({
                "max_completion_tokens": 123,
                "reasoning_effort": "high"
            }))
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
        assert_eq!(
            params.additional_params,
            Some(serde_json::json!({
                "reasoning": {"effort": "high"}
            }))
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
    fn openai_provider_selects_api_for_each_model_reference() {
        let chat = openai_model(OpenAiApi::ChatCompletions, Default::default());
        let responses = openai_model(OpenAiApi::Responses, Default::default());
        assert_eq!(
            OpenAiProvider::<reqwest::Client>::api(&chat),
            OpenAiApi::ChatCompletions
        );
        assert_eq!(
            OpenAiProvider::<reqwest::Client>::api(&responses),
            OpenAiApi::Responses
        );
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
                rig_core::completion::message::ToolCallId::new_or_mint("id1"),
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
            rig_core::completion::message::ToolCallId::new_or_mint("id1"),
            ToolFunction::new("tool".to_string(), serde_json::json!({})),
        ))];
        let result = extract_text_from_choice(&contents);
        assert!(result.is_err());
    }
}
