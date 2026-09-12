use crate::core::config::{ApiSurface, OpenAiApi, ProviderModel};
use crate::inference::{error::InferenceError, provider::ModelConfig};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireDialect {
    Bedrock,
    OpenAiChat,
    LegacyChat,
    Responses,
    Anthropic,
    Gemini,
    Ollama,
    Cohere,
    HuggingFace,
}

impl WireDialect {
    pub fn for_model(model: &ProviderModel) -> Self {
        match model {
            ProviderModel::Bedrock { .. } => Self::Bedrock,
            ProviderModel::OpenAI {
                api: Some(OpenAiApi::Responses),
                ..
            } => Self::Responses,
            ProviderModel::OpenAI { .. } | ProviderModel::Generic => Self::OpenAiChat,
            ProviderModel::Anthropic { .. } => Self::Anthropic,
            ProviderModel::Gemini { .. } => Self::Gemini,
            ProviderModel::Ollama { .. } => Self::Ollama,
            ProviderModel::Custom { name } if name == "cohere" => Self::Cohere,
            ProviderModel::Custom { name } if name == "huggingface" => Self::HuggingFace,
            ProviderModel::Custom { name } if name == "galadriel" => Self::OpenAiChat,
            _ => Self::LegacyChat,
        }
    }

    pub fn protocol(self) -> ApiSurface {
        match self {
            Self::Bedrock => ApiSurface::AmazonBedrockConverse,
            Self::OpenAiChat | Self::LegacyChat => ApiSurface::Completions,
            Self::Responses => ApiSurface::Responses,
            Self::Anthropic => ApiSurface::AnthropicMessages,
            Self::Gemini => ApiSurface::GoogleGenerateContent,
            Self::Ollama => ApiSurface::Ollama,
            Self::Cohere => ApiSurface::CohereChat,
            Self::HuggingFace => ApiSurface::HuggingFace,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ParameterBinding {
    pub config_path: String,
    pub wire_path: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParameterOverride {
    pub config_path: String,
    pub wire_path: Vec<String>,
}

/// Generated settings metadata; never used to serialize requests.
pub trait ParameterMetadata {
    fn parameter_bindings(dialect: WireDialect) -> Vec<ParameterBinding>;
}

/// Describes settings for validation, UI descriptors, and override diagnostics.
pub fn parameter_bindings(model: &ProviderModel) -> Vec<ParameterBinding> {
    use crate::core::config::{
        AnthropicParams, BedrockParams, CommonModelFields, GeminiParams, OllamaParams,
        OpenAICompatParams,
    };

    let dialect = WireDialect::for_model(model);
    let mut result = CommonModelFields::parameter_bindings(dialect);
    result.extend(match model {
        ProviderModel::Bedrock { .. } => BedrockParams::parameter_bindings(dialect),
        ProviderModel::OpenAI { .. }
        | ProviderModel::Groq { .. }
        | ProviderModel::OpenRouter { .. }
        | ProviderModel::DeepSeek { .. }
        | ProviderModel::XAI { .. }
        | ProviderModel::Together { .. }
        | ProviderModel::Hyperbolic { .. } => OpenAICompatParams::parameter_bindings(dialect),
        ProviderModel::Anthropic { .. } => AnthropicParams::parameter_bindings(dialect),
        ProviderModel::Gemini { .. } => GeminiParams::parameter_bindings(dialect),
        ProviderModel::Ollama { .. } => OllamaParams::parameter_bindings(dialect),
        _ => Vec::new(),
    });
    result
}

pub fn reserved_paths(dialect: WireDialect, structured: bool) -> Vec<Vec<String>> {
    let mut paths = match dialect {
        WireDialect::Bedrock => vec!["modelId", "messages", "system", "toolConfig", "stream"],
        WireDialect::OpenAiChat | WireDialect::LegacyChat => vec![
            "model",
            "messages",
            "tools",
            "functions",
            "stream",
            "stream_options.include_usage",
        ],
        WireDialect::Responses => vec![
            "model",
            "input",
            "instructions",
            "tools",
            "stream",
            "previous_response_id",
            "conversation",
        ],
        WireDialect::Anthropic => vec!["model", "messages", "system", "tools", "stream"],
        WireDialect::Gemini => vec!["model", "contents", "systemInstruction", "tools", "stream"],
        WireDialect::Ollama => vec!["model", "messages", "tools", "stream"],
        WireDialect::Cohere => vec![
            "model",
            "message",
            "messages",
            "chat_history",
            "tool_results",
            "preamble",
            "tools",
            "stream",
            "conversation_id",
        ],
        WireDialect::HuggingFace => vec![
            "model",
            "messages",
            "tools",
            "functions",
            "stream",
            "stream_options.include_usage",
        ],
    };
    if structured {
        paths.extend(match dialect {
            WireDialect::Bedrock => vec!["outputConfig"],
            WireDialect::Responses => vec!["text.format", "tool_choice"],
            WireDialect::Gemini => vec![
                "generationConfig.responseSchema",
                "generationConfig.responseJsonSchema",
                "generationConfig.responseMimeType",
                "toolConfig.functionCallingConfig",
            ],
            WireDialect::Ollama => vec!["format", "tool_choice"],
            WireDialect::HuggingFace => vec!["response_format", "tool_choice"],
            _ => vec!["response_format", "tool_choice"],
        });
    }
    paths
        .into_iter()
        .map(|path| path.split('.').map(str::to_owned).collect())
        .collect()
}

/// Request JSON merge, intentionally distinct from configuration patching.
pub fn merge_json(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                merge_json(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, source) => *target = source,
    }
}

fn touches(value: &Value, path: &[String]) -> bool {
    let Some((first, remaining)) = path.split_first() else {
        return true;
    };
    match value.get(first) {
        None => false,
        Some(_) if remaining.is_empty() => true,
        Some(value) if value.is_object() => touches(value, remaining),
        Some(_) => true, // Replacing an ancestor also replaces the owned child.
    }
}

pub fn validate_extra(
    model: &ProviderModel,
    extra: &Map<String, Value>,
    path: &str,
    structured: bool,
) -> Result<(), InferenceError> {
    let value = Value::Object(extra.clone());
    for reserved in reserved_paths(WireDialect::for_model(model), structured) {
        if touches(&value, &reserved) {
            return Err(InferenceError::ConfigError(format!(
                "{path}.extra_params.{}: reserved request-envelope path",
                reserved.join(".")
            )));
        }
    }
    Ok(())
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, key| value.get(key))
}

fn model_settings(model: &ProviderModel) -> Value {
    let value = match model {
        ProviderModel::OpenAI { params, .. }
        | ProviderModel::Groq { params }
        | ProviderModel::OpenRouter { params }
        | ProviderModel::DeepSeek { params }
        | ProviderModel::XAI { params }
        | ProviderModel::Together { params }
        | ProviderModel::Hyperbolic { params } => serde_json::to_value(params),
        ProviderModel::Anthropic { params } => serde_json::to_value(params),
        ProviderModel::Gemini { params } => serde_json::to_value(params),
        ProviderModel::Bedrock { params } => serde_json::to_value(params),
        ProviderModel::Ollama { params } => serde_json::to_value(params),
        _ => return json!({}),
    };
    value.expect("typed settings serialize")
}

/// Build Rig inputs directly. The binding table above is descriptive only.
fn request_parameters(
    model: &ModelConfig,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> Result<(super::hooks::RequestParams, Value), InferenceError> {
    use rig_core::providers::{gemini::completion::gemini_api_types, openai::responses_api};

    let mut max_tokens = max_tokens.or(model.request_settings.max_tokens);
    let temperature = temperature.or(model.request_settings.temperature);
    let mut compatibility = json!({});
    let additional = match &model.provider {
        ProviderModel::Gemini { params } => {
            let config = gemini_api_types::GenerationConfig {
                top_p: params.top_p,
                top_k: params
                    .top_k
                    .map(|value| checked_integer(value, "top_k"))
                    .transpose()?,
                candidate_count: params
                    .candidate_count
                    .map(|value| checked_integer(value, "candidate_count"))
                    .transpose()?,
                stop_sequences: params.stop_sequences.clone(),
                thinking_config: params
                    .thinking_config
                    .as_ref()
                    .map(|thinking| {
                        Ok::<_, InferenceError>(gemini_api_types::ThinkingConfig {
                            thinking_budget: Some(checked_integer(
                                thinking.thinking_budget,
                                "thinking_config.thinking_budget",
                            )?),
                            include_thoughts: thinking.include_thoughts,
                            thinking_level: None,
                        })
                    })
                    .transpose()?,
                ..Default::default()
            };
            if serialize(&config)?.as_object().is_some_and(Map::is_empty) {
                json!({})
            } else {
                serialize(&gemini_api_types::AdditionalParameters {
                    generation_config: Some(config),
                    additional_params: None,
                })?
            }
        }
        ProviderModel::OpenAI {
            api: Some(crate::core::config::OpenAiApi::Responses),
            params,
        } => {
            // The explicit legacy alias has always taken precedence over the common cap.
            max_tokens = params.max_completion_tokens.or(max_tokens);
            let reasoning = params.reasoning_effort.as_ref().and_then(|effort| {
                match serde_json::from_value::<responses_api::ReasoningEffort>(json!(effort)) {
                    Ok(effort) => Some(responses_api::Reasoning {
                        effort: Some(effort),
                        ..Default::default()
                    }),
                    Err(_) => {
                        // Preserve forward-compatible strings that Rig's enum cannot express.
                        compatibility["reasoning"] = json!({"effort": effort});
                        None
                    }
                }
            });
            if let Some(value) = params.top_logprobs {
                // Rig's Responses AdditionalParameters currently omits this field.
                compatibility["top_logprobs"] = json!(value);
            }
            serialize(&responses_api::AdditionalParameters {
                top_p: params.top_p,
                reasoning,
                ..Default::default()
            })?
        }
        ProviderModel::Bedrock { params } => {
            // Rig narrows this common value to i32 in the AWS SDK builder.
            if let Some(value) = max_tokens {
                let _: i32 = checked_integer(value, "max_tokens")?;
            }
            compatibility =
                crate::inference::provider::adapter::bedrock::inference_settings(params);
            json!({})
        }
        // These settings already have Rig's native additional-parameter shape.
        // Ollama itself nests the options and separates the top-level `think` flag.
        ProviderModel::Anthropic { params } => serialize(params)?,
        ProviderModel::Ollama { params } => serialize(params)?,
        ProviderModel::OpenAI { params, .. }
        | ProviderModel::Groq { params }
        | ProviderModel::OpenRouter { params }
        | ProviderModel::DeepSeek { params }
        | ProviderModel::XAI { params }
        | ProviderModel::Together { params }
        | ProviderModel::Hyperbolic { params } => serialize(params)?,
        ProviderModel::Generic | ProviderModel::Custom { .. } => json!({}),
    };
    Ok((
        super::hooks::RequestParams {
            max_tokens,
            temperature,
            additional_params: additional
                .as_object()
                .filter(|map| !map.is_empty())
                .map(|_| additional.clone()),
        },
        compatibility,
    ))
}

fn serialize(value: &impl Serialize) -> Result<Value, InferenceError> {
    serde_json::to_value(value).map_err(|error| InferenceError::ConfigError(error.to_string()))
}

fn checked_integer<T: TryFrom<u64>>(value: u64, field: &str) -> Result<T, InferenceError> {
    T::try_from(value).map_err(|_| {
        InferenceError::ConfigError(format!(
            "{field}: value exceeds the provider's integer range"
        ))
    })
}

#[derive(Clone)]
pub struct WireParameters {
    pub dialect: WireDialect,
    pub request: super::hooks::RequestParams,
    /// Only settings that Rig cannot express, never a second serialized request.
    compatibility: Value,
    pub extra: Value,
    pub overrides: Vec<ParameterOverride>,
}

impl WireParameters {
    pub fn prepare(
        model: &ModelConfig,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
        structured: bool,
    ) -> Result<Self, InferenceError> {
        Self::prepare_named(model, max_tokens, temperature, structured, &model.as_str())
    }

    pub fn prepare_named(
        model: &ModelConfig,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
        structured: bool,
        path: &str,
    ) -> Result<Self, InferenceError> {
        validate_extra(
            &model.provider,
            &model.request_settings.extra_params,
            path,
            structured,
        )?;
        let mut settings = model_settings(&model.provider);
        if let Some(value) = max_tokens.or(model.request_settings.max_tokens) {
            settings["max_tokens"] = json!(value);
        }
        if let Some(value) = temperature.or(model.request_settings.temperature) {
            settings["temperature"] = json!(value);
        }
        let mappings = parameter_bindings(&model.provider);
        for field in settings.as_object().expect("settings object").keys() {
            if !mappings
                .iter()
                .any(|mapping| mapping.config_path.split('.').next() == Some(field.as_str()))
            {
                return Err(InferenceError::ConfigError(format!(
                    "{path}.{field}: setting is not supported by this protocol"
                )));
            }
        }
        let extra = Value::Object(model.request_settings.extra_params.clone());
        let mut overrides = Vec::new();
        for mapping in mappings {
            if get_path(&settings, &mapping.config_path).is_some()
                && touches(&extra, &mapping.wire_path)
            {
                overrides.push(ParameterOverride {
                    config_path: format!("{path}.{}", mapping.config_path),
                    wire_path: mapping.wire_path,
                });
            }
        }
        let (request, compatibility) =
            request_parameters(model, max_tokens, temperature).map_err(|error| match error {
                InferenceError::ConfigError(message) => {
                    InferenceError::ConfigError(format!("{path}.{message}"))
                }
                error => error,
            })?;
        Ok(Self {
            dialect: WireDialect::for_model(&model.provider),
            request,
            compatibility,
            extra,
            overrides,
        })
    }

    pub fn apply(&self, body: &mut Value) {
        if self.dialect == WireDialect::OpenAiChat
            && let Some(root) = body.as_object_mut()
            && let Some(value) = root.remove("max_tokens")
        {
            root.entry("max_completion_tokens").or_insert(value);
        }
        merge_json(body, self.compatibility.clone());
        merge_json(body, self.extra.clone());
    }
}
