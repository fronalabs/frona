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
            InferenceCounter, ModelConfig, ModelProvider, ModelRequestSettings, StreamToken,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageReport {
    Reported(Usage),
    Unavailable,
    LegacyNormalized(Usage),
}

impl UsageReport {
    pub(super) fn observed(&self) -> Option<&Usage> {
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

#[derive(Debug, Clone)]
pub struct ModelOutput {
    pub content: Vec<AssistantContent>,
    /// Native structured output, with the original content retained separately.
    pub structured: Option<serde_json::Value>,
    pub usage: UsageReport,
    /// Original metadata from the adapter; null explicitly means unavailable.
    pub metadata: serde_json::Value,
    pub ttft_ms: Option<u64>,
}

impl ModelOutput {
    pub fn new(content: Vec<AssistantContent>, usage: UsageReport) -> Self {
        Self {
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
    adapter: Arc<dyn ModelAdapter + 'a>,
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
            adapter: Arc::new(adapter),
        })
    }

    pub fn capabilities(&self) -> Capabilities {
        self.adapter.capabilities()
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

    /// One provider invocation with optional HSDK-03 telemetry. Shared retry and
    /// retry orchestration uses `invoke_controlled` to retain its shared budget.
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

    /// One invocation using the caller's shared allowance and cancellation.
    /// Retry drivers must call this entry for every actual attempt.
    pub async fn invoke_controlled(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ModelOutput, ModelError> {
        let capabilities = self.capabilities();
        let mut required = vec![if request.schema.is_some() {
            Capability::Structured
        } else {
            Capability::Text
        }];
        if !request.tools.is_empty() {
            required.push(Capability::Tools);
        }
        if tokens.is_some() {
            required.push(Capability::Streaming);
        }
        if !request.tools.is_empty() && request.schema.is_some() {
            required.push(Capability::StructuredTools);
        }
        if tokens.is_some() && request.schema.is_some() {
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
        let (execution, report) = execution.begin_model_call(&self.config);
        let start = std::time::Instant::now();
        let result = execution
            .model_attempt(
                &self.config,
                observations,
                || self.adapter.generate(request, tokens),
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
        let result = execution.check_cancellation().and(result);
        if let Some(report) = report {
            report.finish(super::report::inference_outcome(&result));
        }
        result.map_err(ModelError::Provider)
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
            adapter: Arc::new(BuiltinAdapter { client, config }),
        })
    }
}

struct BuiltinAdapter {
    client: Arc<dyn ModelProvider>,
    config: ModelConfig,
}

#[async_trait]
impl ModelAdapter for BuiltinAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
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
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.client.ensure_usable(&self.config).await?;
        let ModelRequest {
            system,
            messages,
            tools,
            schema,
            max_tokens,
            temperature,
        } = request;
        if let Some(schema) = schema {
            let result = self
                .client
                .structured_output(
                    &self.config,
                    &system,
                    messages,
                    schema,
                    max_tokens,
                    temperature,
                )
                .await?;
            let mut output = match result.response {
                Some(response) => from_builtin(response),
                None => ModelOutput::new(Vec::new(), UsageReport::Unavailable),
            };
            output.structured = Some(result.value);
            return Ok(output);
        }
        let output = if let Some(tokens) = tokens {
            self.client
                .stream_inference(
                    &self.config,
                    &system,
                    messages,
                    tools,
                    tokens,
                    max_tokens,
                    temperature,
                )
                .await?
        } else {
            self.client
                .inference(
                    &self.config,
                    &system,
                    messages,
                    tools,
                    max_tokens,
                    temperature,
                )
                .await?
        };
        Ok(from_builtin(output))
    }
}

fn from_builtin(output: crate::inference::provider::InferenceOutput) -> ModelOutput {
    ModelOutput {
        content: output.content,
        structured: None,
        usage: UsageReport::LegacyNormalized(output.usage),
        metadata: output.metadata,
        ttft_ms: output.ttft_ms,
    }
}
