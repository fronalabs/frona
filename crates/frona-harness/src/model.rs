use std::sync::Arc;

use async_trait::async_trait;
use rig_core::completion::{AssistantContent, Message, request::ToolDefinition};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use super::{DefaultBudget, ExecutionControl};

use crate::{
    Handle,
    config::{ApiSurface, CommonModelFields, ModelGroupConfig, ModelProviderConfig, ProviderModel},
    inference::{
        Usage,
        error::InferenceError,
        observation::{LatencyMetrics, Observations},
        provider::{
            InferenceCounter, ModelConfig, ModelProvider, ModelRequestSettings, ResolvedModel,
            RigModelAdapter, StreamToken,
            platform::{ProviderPlatform, build_provider},
        },
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Text,
    Tools,
    Streaming,
    Structured,
    StructuredStreaming,
    StructuredTools,
}

/// Adapter capabilities, not a claim that a remote account/model allows them.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub text: bool,
    pub tools: bool,
    pub streaming: bool,
    pub structured: bool,
    pub structured_streaming: bool,
    pub structured_tools: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            text: true,
            tools: false,
            streaming: false,
            structured: false,
            structured_streaming: false,
            structured_tools: false,
        }
    }
}

impl Capabilities {
    fn supports(self, capability: Capability) -> bool {
        match capability {
            Capability::Text => self.text,
            Capability::Tools => self.tools,
            Capability::Streaming => self.streaming,
            Capability::Structured => self.structured,
            Capability::StructuredStreaming => self.structured_streaming,
            Capability::StructuredTools => self.structured_tools,
        }
    }
}

/// Usage provenance is explicit. LegacyNormalized is neither a verified report
/// nor an estimate: old adapters may substitute zero for missing wire fields.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UsageReport {
    Reported(Usage),
    Unavailable,
    LegacyNormalized(Usage),
}

impl UsageReport {
    pub fn observed(&self) -> Option<&Usage> {
        match self {
            Self::Reported(usage) | Self::LegacyNormalized(usage) => Some(usage),
            Self::Unavailable => None,
        }
    }
}

/// Owned invocation inputs. Provider-specific history/content is not flattened.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub schema: Option<serde_json::Value>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
}

impl ModelRequest {
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            system: String::new(),
            messages: vec![Message::user(input)],
            tools: Vec::new(),
            schema: None,
            max_tokens: None,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelOutput {
    /// Provider provenance required to replay native reasoning and tool items.
    #[serde(default)]
    pub origin: Option<rig_core::message::Origin>,
    #[serde(default)]
    pub stop: Option<rig_core::message::StopReason>,
    pub content: Vec<AssistantContent>,
    /// Native structured output, with the original content retained separately.
    #[serde(deserialize_with = "crate::history::required")]
    pub structured: Option<serde_json::Value>,
    pub usage: UsageReport,
    /// Original metadata from the adapter; null explicitly means unavailable.
    #[serde(deserialize_with = "crate::history::required")]
    pub metadata: serde_json::Value,
    #[serde(deserialize_with = "crate::history::required")]
    pub ttft_ms: Option<u64>,
}

impl ModelOutput {
    pub(crate) fn from_response(response: rig_core::completion::CompletionResponse) -> Self {
        let mut metadata = serde_json::to_value(&response).unwrap_or_default();
        metadata["response_id"] = serde_json::json!(response.origin.response_id);
        let stop = response.stop();
        Self {
            origin: Some(response.origin),
            stop: Some(stop),
            content: response.choice,
            structured: None,
            usage: UsageReport::Reported(response.usage),
            metadata,
            ttft_ms: None,
        }
    }

    /// Validate only after admission/accounting has recorded the returned usage.
    /// Failure retains the original content, provenance, usage and metadata.
    pub fn ensure_complete(&self) -> Result<(), InferenceError> {
        use rig_core::message::StopReason;
        let message = match &self.stop {
            Some(StopReason::Length) => {
                Some("provider output-token limit interrupted the response")
            }
            Some(StopReason::Error(reason) | StopReason::Aborted(reason)) => Some(reason.as_str()),
            _ => None,
        };
        match message {
            Some(message) => Err(self.invalid(format!("Incomplete provider response: {message}"))),
            None => Ok(()),
        }
    }

    pub fn validate(&self, schema: Option<&serde_json::Value>) -> Result<(), InferenceError> {
        self.ensure_complete()?;
        if let Some(schema) = schema {
            let value = self.structured.as_ref().ok_or_else(|| {
                self.invalid("model did not return a valid structured submission")
            })?;
            let validator = jsonschema::validator_for(schema)
                .map_err(|error| self.invalid(error.to_string()))?;
            if let Err(error) = validator.validate(value) {
                return Err(self.invalid(error.to_string()));
            }
        }
        Ok(())
    }

    fn invalid(&self, message: impl Into<String>) -> InferenceError {
        InferenceError::InvalidOutput {
            message: message.into(),
            output: Box::new(self.clone()),
        }
    }

    pub fn new(content: Vec<AssistantContent>, usage: UsageReport) -> Self {
        Self {
            origin: None,
            stop: None,
            content,
            structured: None,
            usage,
            metadata: serde_json::Value::Null,
            ttft_ms: None,
        }
    }
}

#[derive(Debug)]
pub struct TypedOutput<T> {
    pub value: T,
    pub response: ModelOutput,
}

/// Implement this around an existing client/gateway. Credentials remain owned
/// by that client. One invocation is made; this trait does not implement retries.
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn generate(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{model} does not support {capability:?}")]
    Unsupported {
        model: String,
        capability: Capability,
    },
    #[error(transparent)]
    Provider(#[from] InferenceError),
    #[error("{model} returned invalid structured output: {message}")]
    Structured {
        model: String,
        message: String,
        response: Box<ModelOutput>,
    },
}

/// A prepared model shared by callers without identity or persistence services.
#[derive(Clone)]
pub struct Model<'a> {
    config: ModelConfig,
    backend: ModelBackend<'a>,
}

#[derive(Clone)]
enum ModelBackend<'a> {
    Adapter(Arc<dyn ModelAdapter + 'a>),
    Group(Arc<crate::inference::ModelGroup>),
}

/// Internal provenance survives fallback without changing the durable output format.
pub(super) struct InvocationOutput {
    pub output: ModelOutput,
    pub origin: String,
}

impl<'a> Model<'a> {
    /// Build a reusable built-in client without registry/catalog administration.
    pub fn builder(provider: impl Into<String>, model: impl Into<String>) -> ModelBuilder {
        ModelBuilder {
            provider: provider.into(),
            model: model.into(),
            connection: ModelProviderConfig::default(),
            api: None,
        }
    }

    /// Embed a configured Rig model without rebuilding its transport or credentials.
    /// Frona final-wire extras require a Frona-owned transport and are not applied
    /// to externally supplied models.
    pub fn from_rig(
        model: impl Into<rig_core::DynModel<rig_core::operation::Completion>>,
    ) -> Result<Model<'static>, ModelError> {
        let native = model.into();
        let provider = native.name().replace('.', "-");
        let id = native
            .id()
            .ok_or_else(|| {
                InferenceError::ConfigError("Rig completion model must name a model ID".into())
            })?
            .to_owned();
        let handle = Handle::try_new(&provider)
            .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
        validate_model(&id)?;
        let config = ModelConfig {
            catalog_provider: native.name().into(),
            provider_handle: handle,
            model_id: id,
            provider: ProviderModel::Custom {
                name: native.name().into(),
            },
            request_settings: ModelRequestSettings::default(),
        };
        let client: Arc<dyn ModelProvider> = Arc::new(SuppliedRigModel(native));
        Ok(Model {
            config: config.clone(),
            backend: ModelBackend::Adapter(Arc::new(RigModelAdapter::new(client, config))),
        })
    }

    pub fn from_adapter(
        provider: &str,
        model: impl Into<String>,
        adapter: impl ModelAdapter + 'a,
    ) -> Result<Self, ModelError> {
        let handle = Handle::try_new(provider)
            .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
        let model = model.into();
        validate_model(&model)?;
        Ok(Self {
            config: ModelConfig {
                catalog_provider: provider.into(),
                provider_handle: handle,
                model_id: model,
                provider: ProviderModel::Custom {
                    name: provider.into(),
                },
                request_settings: ModelRequestSettings::default(),
            },
            backend: ModelBackend::Adapter(Arc::new(adapter)),
        })
    }

    /// Use prepared retry/fallback configuration without rebuilding clients.
    /// This path supports non-streaming text and tool requests. SDK structured
    /// runs use submission tools; native structured/streaming APIs are unsupported.
    /// Every actual candidate attempt shares the run's admission and reporting.
    pub fn from_group(group: crate::inference::ModelGroup) -> Self {
        Self {
            config: group.main.clone(),
            backend: ModelBackend::Group(Arc::new(group)),
        }
    }

    pub fn capabilities(&self) -> Capabilities {
        match &self.backend {
            ModelBackend::Adapter(adapter) => adapter.capabilities(),
            ModelBackend::Group(_) => Capabilities {
                tools: true,
                ..Default::default()
            },
        }
    }

    pub(super) fn compaction_config(&self) -> super::CompactionConfig {
        match &self.backend {
            ModelBackend::Group(group) => super::CompactionConfig {
                context_window: group.context_window,
                ..Default::default()
            },
            ModelBackend::Adapter(_) => Default::default(),
        }
    }

    pub(super) fn output_reservation(&self, override_tokens: Option<u64>) -> (u64, Option<u64>) {
        if let Some(tokens) = override_tokens {
            return (tokens, Some(tokens));
        }
        match &self.backend {
            ModelBackend::Adapter(_) => {
                let tokens = self.config.request_settings.max_tokens.unwrap_or(8192);
                (tokens, Some(tokens))
            }
            ModelBackend::Group(group) => (
                std::iter::once(&group.main)
                    .chain(&group.fallbacks)
                    .map(|model| {
                        model
                            .request_settings
                            .max_tokens
                            .unwrap_or(group.inference.default_max_tokens)
                    })
                    .max()
                    .unwrap_or(group.inference.default_max_tokens),
                None,
            ),
        }
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    pub async fn complete(&self, request: ModelRequest) -> Result<ModelOutput, ModelError> {
        self.invoke(request, None, Observations::default()).await
    }

    /// The caller owns and drains the token channel concurrently. Dropping its
    /// receiver does not change the final response. Optional progress fan-out
    /// for agent runs is provided by the later execution layer.
    pub async fn stream(
        &self,
        request: ModelRequest,
        tokens: mpsc::Sender<StreamToken>,
    ) -> Result<ModelOutput, ModelError> {
        self.invoke(request, Some(tokens), Observations::default())
            .await
    }

    pub async fn complete_typed<T: DeserializeOwned + JsonSchema>(
        &self,
        mut request: ModelRequest,
    ) -> Result<TypedOutput<T>, ModelError> {
        request.schema = Some(schemars::schema_for!(T).to_value());
        let response = self.complete(request).await?;
        let decoded = response
            .structured
            .as_ref()
            .ok_or_else(|| "adapter omitted structured output".to_string())
            .and_then(|value| {
                serde_json::from_value(value.clone()).map_err(|error| error.to_string())
            });
        match decoded {
            Ok(value) => Ok(TypedOutput { value, response }),
            Err(message) => Err(ModelError::Structured {
                model: self.config.as_str(),
                message,
                response: Box::new(response),
            }),
        }
    }

    /// One logical model invocation with optional telemetry and a fresh default budget.
    /// Prepared groups may perform several provider attempts through retry/fallback.
    /// Use `invoke_controlled` to share admission across invocations.
    pub async fn invoke(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
    ) -> Result<ModelOutput, ModelError> {
        let budget = DefaultBudget::default();
        self.invoke_controlled(
            request,
            tokens,
            observations,
            ExecutionControl::new(&budget),
        )
        .await
    }

    /// One logical invocation using the caller's shared allowance and cancellation.
    /// Adapters perform one attempt; prepared groups apply their configured retry and
    /// fallback policy. Every actual attempt is separately admitted and reported.
    pub async fn invoke_controlled(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ModelOutput, ModelError> {
        self.invoke_with_origin(request, tokens, observations, execution)
            .await
            .map(|value| value.output)
    }

    pub(super) async fn invoke_with_origin(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<InvocationOutput, ModelError> {
        self.validate_request(&request, tokens.is_some())?;
        let (execution, report) = execution.begin_model_call(&self.config);
        if let ModelBackend::Group(group) = &self.backend {
            let result = crate::inference::retry::inference_with_retry_and_fallback(
                group,
                crate::inference::ModelRequest {
                    input_token_estimate: None,
                    system_prompt: &request.system,
                    history: request.messages,
                    tools: request.tools,
                    observations,
                    overrides: crate::inference::RequestOverrides {
                        max_tokens: request.max_tokens,
                        temperature: request.temperature,
                    },
                },
                crate::inference::retry::HistoryOverflow::Reject,
                execution,
                crate::inference::retry::FallbackReplay::PortableOnly,
            )
            .await;
            if let Some(report) = report {
                report.finish_inference(&result);
            }
            return result
                .map(|result| InvocationOutput {
                    origin: result.model,
                    output: result.output,
                })
                .map_err(ModelError::Provider);
        }
        let ModelBackend::Adapter(adapter) = &self.backend else {
            unreachable!()
        };
        let start = std::time::Instant::now();
        let schema = request.schema.clone();
        let result = execution
            .model_attempt(
                &self.config,
                observations,
                || adapter.generate(request, tokens),
                |output| output.usage.clone(),
            )
            .await;
        let duration_ms = start.elapsed().as_millis() as u64;
        if let Ok(output) = &result {
            observations
                .usage(
                    &self.config,
                    output.usage.observed(),
                    0,
                    LatencyMetrics {
                        duration_ms,
                        ttft_ms: output.ttft_ms,
                        ..Default::default()
                    },
                )
                .await;
        }
        let result = execution
            .check_cancellation()
            .and(result)
            .and_then(|output| {
                output.validate(schema.as_ref())?;
                Ok(output)
            });
        if let Some(report) = report {
            report.finish(super::report::inference_outcome(&result));
        }
        result
            .map(|output| InvocationOutput {
                output,
                origin: self.config.as_str(),
            })
            .map_err(|error| match error {
                InferenceError::InvalidOutput { message, output } if schema.is_some() => {
                    ModelError::Structured {
                        model: self.config.as_str(),
                        message,
                        response: output,
                    }
                }
                error => ModelError::Provider(error),
            })
    }

    /// Preflight the request shape without admitting an attempt or recording usage.
    pub(super) fn validate_request(
        &self,
        request: &ModelRequest,
        streaming: bool,
    ) -> Result<(), ModelError> {
        let capabilities = self.capabilities();
        let mut required = vec![if request.schema.is_some() {
            Capability::Structured
        } else {
            Capability::Text
        }];
        if !request.tools.is_empty() {
            required.push(Capability::Tools);
        }
        if streaming {
            required.push(Capability::Streaming);
        }
        if !request.tools.is_empty() && request.schema.is_some() {
            required.push(Capability::StructuredTools);
        }
        if streaming && request.schema.is_some() {
            required.push(Capability::StructuredStreaming);
        }
        for capability in required {
            if !capabilities.supports(capability) {
                return Err(ModelError::Unsupported {
                    model: self.config.as_str(),
                    capability,
                });
            }
        }
        Ok(())
    }
}

fn validate_model(model: &str) -> Result<(), InferenceError> {
    if model.trim().is_empty() {
        return Err(InferenceError::ConfigError(
            "model ID must not be empty".into(),
        ));
    }
    Ok(())
}

pub struct ModelBuilder {
    provider: String,
    model: String,
    connection: ModelProviderConfig,
    api: Option<ApiSurface>,
}

impl ModelBuilder {
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.connection.api_key = Some(key.into());
        self
    }
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.connection.base_url = Some(url.into());
        self
    }
    pub fn api(mut self, api: ApiSurface) -> Self {
        self.api = Some(api);
        self
    }

    pub fn build(self) -> Result<Model<'static>, ModelError> {
        validate_model(&self.model)?;
        let handle = Handle::try_new(&self.provider)
            .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
        let resolved = ProviderPlatform::resolve(&handle, &self.connection)?;
        let recipe = ModelGroupConfig {
            provider: handle.clone(),
            api: self.api,
            common: CommonModelFields {
                model: self.model.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let provider_model =
            crate::inference::config::compile_request("sdk.model", &recipe, &self.connection)?;
        crate::initialize_tls();
        let client = build_provider(&resolved, &self.connection, &InferenceCounter::default())?;
        let config = ModelConfig {
            catalog_provider: resolved.brand,
            provider_handle: handle,
            model_id: self.model,
            provider: provider_model,
            request_settings: ModelRequestSettings::default(),
        };
        Ok(Model {
            config: config.clone(),
            backend: ModelBackend::Adapter(Arc::new(RigModelAdapter::new(client, config))),
        })
    }
}

struct SuppliedRigModel(rig_core::DynModel<rig_core::operation::Completion>);

#[async_trait]
impl ModelProvider for SuppliedRigModel {
    async fn resolve(&self, _model: &ModelConfig) -> Result<ResolvedModel, InferenceError> {
        Ok(ResolvedModel::new(self.0.clone()))
    }
}

/// Conservative native-content compatibility shared with explicit model changes.
pub(crate) fn portable_assistant_content(content: &AssistantContent) -> bool {
    matches!(content, AssistantContent::Text(_))
        || matches!(content, AssistantContent::ToolCall(call)
        if call.native.is_none())
}

pub(crate) fn portable_fallback_history(messages: &[Message]) -> bool {
    use rig_core::completion::message::{ToolResultContent, UserContent};
    messages.iter().all(|message| match message {
        Message::Assistant(message) => message.content.iter().all(portable_assistant_content),
        Message::User { content } => content.iter().all(|item| match item {
            UserContent::Text(_) => true,
            UserContent::ToolResult(result) => result
                .content
                .iter()
                .all(|content| matches!(content, ToolResultContent::Text(_))),
            _ => false,
        }),
        Message::System { .. } => true,
    })
}
