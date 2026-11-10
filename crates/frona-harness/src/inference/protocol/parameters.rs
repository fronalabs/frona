use crate::config::{ApiSurface, OpenAiApi, ProviderModel};
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
    use crate::config::{
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
) -> Result<rig_core::completion::CompletionRequest, InferenceError> {
    let mut max_tokens = max_tokens.or(model.request_settings.max_tokens);
    let temperature = temperature.or(model.request_settings.temperature);
    let additional = match &model.provider {
        ProviderModel::Gemini { params } => {
            let mut config = Map::new();
            if let Some(value) = params.top_p {
                config.insert("topP".into(), json!(value));
            }
            if let Some(value) = params.top_k {
                config.insert(
                    "topK".into(),
                    json!(checked_integer::<u32>(value, "top_k")?),
                );
            }
            if let Some(value) = params.candidate_count {
                config.insert(
                    "candidateCount".into(),
                    json!(checked_integer::<u32>(value, "candidate_count")?),
                );
            }
            if let Some(value) = &params.stop_sequences {
                config.insert("stopSequences".into(), json!(value));
            }
            if let Some(thinking) = &params.thinking_config {
                let mut value = json!({"thinkingBudget": checked_integer::<i32>(thinking.thinking_budget, "thinking_config.thinking_budget")?});
                if let Some(include) = thinking.include_thoughts {
                    value["includeThoughts"] = json!(include);
                }
                config.insert("thinkingConfig".into(), value);
            }
            if config.is_empty() {
                json!({})
            } else {
                json!({"generationConfig": config})
            }
        }
        ProviderModel::OpenAI {
            api: Some(crate::config::OpenAiApi::Responses),
            params,
        } => {
            // Keep the legacy cap alias and forward-compatible reasoning strings.
            max_tokens = params.max_completion_tokens.or(max_tokens);
            let mut additional = json!({});
            if let Some(top_p) = params.top_p {
                additional["top_p"] = json!(top_p);
            }
            if let Some(effort) = &params.reasoning_effort {
                additional["reasoning"] = json!({"effort": effort});
            }
            if let Some(value) = params.top_logprobs {
                additional["top_logprobs"] = json!(value);
            }
            additional
        }
        ProviderModel::Bedrock { .. } => {
            // Rig narrows this common value to i32 in the AWS SDK builder.
            if let Some(value) = max_tokens {
                let _: i32 = checked_integer(value, "max_tokens")?;
            }
            json!({})
        }
        // These settings already have Rig's native additional-parameter shape.
        // Ollama itself nests the options and separates the top-level `think` flag.
        ProviderModel::Anthropic { params } => {
            let mut additional = serialize(params)?;
            // Automatic prefix caching: Rig preserves this native top-level
            // control for both streaming and non-streaming Messages requests.
            // https://platform.claude.com/docs/en/build-with-claude/prompt-caching
            additional["cache_control"] = json!({"type": "ephemeral"});
            additional
        }
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
    let mut request = rig_core::completion::CompletionRequest::new("");
    request.max_tokens = max_tokens;
    request.temperature = temperature;
    request.additional_params = additional
        .as_object()
        .filter(|map| !map.is_empty())
        .map(|_| additional.clone());
    Ok(request)
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
    pub request: rig_core::completion::CompletionRequest,
    /// Only settings that Rig cannot express, never a second serialized request.
    compatibility: Value,
    fallback: Value,
    fallback_wire: Value,
    extension: Option<&'static str>,
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
        let mut request =
            request_parameters(model, max_tokens, temperature).map_err(|error| match error {
                InferenceError::ConfigError(message) => {
                    InferenceError::ConfigError(format!("{path}.{message}"))
                }
                error => error,
            })?;
        let mut compatibility = json!({});
        let (fallback, fallback_wire, extension) =
            native_options(&model.provider, &mut request, &mut compatibility);
        Ok(Self {
            fallback,
            fallback_wire,
            extension,
            dialect: WireDialect::for_model(&model.provider),
            request,
            compatibility,
            extra,
            overrides,
        })
    }

    /// Rig's native checks choose a representation, never a new Frona model
    /// allowlist. A catalog or dialect refusal restores only migrated fields.
    pub fn configure_native(
        &mut self,
        request: &mut rig_core::completion::CompletionRequest,
        model: &rig_core::DynModel<rig_core::operation::Completion>,
        wire_transport: bool,
    ) {
        // The platform promises this cap spelling even for manual Chat models.
        // Supplied native models retain their own dialect's common cap mapping.
        if wire_transport
            && self.dialect == WireDialect::OpenAiChat
            && let Some(value) = request.max_tokens.take()
        {
            request
                .additional_params
                .get_or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("settings object")
                .entry("max_completion_tokens")
                .or_insert(json!(value));
        }
        if request.options.is_default() && request.provider_options.is_empty() {
            return;
        }
        let foreign_extension = self
            .extension
            .is_some_and(|provider| provider != model.name());
        // Only OpenAI effort strings are promoted below. Compatible dialects
        // can accept that option with different semantics (Codex adds a summary).
        let foreign_reasoning = request.options.reasoning.is_some()
            && model.name() != rig_core::providers::openai::PROVIDER_NAME;
        if foreign_extension
            || foreign_reasoning
            || matches!(
                model.check(request),
                Err(rig_core::completion::CheckError::Unsupported(_))
            )
        {
            request.options = Default::default();
            request.provider_options = Default::default();
            let mut raw = self.fallback.clone();
            merge_json(
                &mut raw,
                request
                    .additional_params
                    .take()
                    .unwrap_or_else(|| json!({})),
            );
            request.additional_params = raw
                .as_object()
                .filter(|map| !map.is_empty())
                .map(|_| raw.clone());
            merge_json(&mut self.compatibility, self.fallback_wire.clone());
        }
    }

    pub fn requires_wire_transport(&self) -> bool {
        [&self.compatibility, &self.extra]
            .iter()
            .any(|value| value.as_object().is_some_and(|map| !map.is_empty()))
    }

    pub fn apply(&self, body: &mut Value) {
        merge_json(body, self.compatibility.clone());
        merge_json(body, self.extra.clone());
    }
}

/// Move a proven native field out of the raw layer, retaining its exact spelling
/// only for a wire/catalog refusal. Paths are segments, never dotted-key syntax.
fn migrate(raw: &mut Value, fallback: &mut Value, path: &[&str]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        if let Some(value) = raw.as_object_mut().and_then(|map| map.remove(*first)) {
            fallback[*first] = value;
        }
    } else if let Some(child) = raw.get_mut(*first) {
        let mut saved = json!({});
        migrate(child, &mut saved, rest);
        if saved.as_object().is_some_and(|map| !map.is_empty()) {
            merge_json(fallback, json!({*first: saved}));
        }
        if child.as_object().is_some_and(Map::is_empty) {
            raw.as_object_mut().unwrap().remove(*first);
        }
    }
}

/// Native options are candidates, not new admission rules. Frona's configured
/// ranges/forward values remain valid even when Rig's catalog is more restrictive.
fn native_options(
    model: &ProviderModel,
    request: &mut rig_core::completion::CompletionRequest,
    compatibility: &mut Value,
) -> (Value, Value, Option<&'static str>) {
    use rig_core::completion::{CacheRetention, Effort, Reasoning};
    use rig_core::providers::{anthropic, gemini, ollama, openai};
    let mut raw = request
        .additional_params
        .take()
        .unwrap_or_else(|| json!({}));
    let mut fallback = json!({});
    let mut fallback_wire = json!({});
    let mut extension = None;
    let options = &mut request.options;
    let (top_p, seed, stop, top_path, stop_path) = match model {
        ProviderModel::OpenAI { params, .. }
        | ProviderModel::Groq { params }
        | ProviderModel::OpenRouter { params }
        | ProviderModel::DeepSeek { params }
        | ProviderModel::XAI { params }
        | ProviderModel::Together { params }
        | ProviderModel::Hyperbolic { params } => (
            params.top_p,
            params.seed,
            params.stop.as_deref(),
            &["top_p"][..],
            &["stop"][..],
        ),
        ProviderModel::Anthropic { params } => (
            params.top_p,
            None,
            params.stop_sequences.as_deref(),
            &["top_p"][..],
            &["stop_sequences"][..],
        ),
        ProviderModel::Gemini { params } => (
            params.top_p,
            None,
            params.stop_sequences.as_deref(),
            &["generationConfig", "topP"][..],
            &["generationConfig", "stopSequences"][..],
        ),
        ProviderModel::Ollama { params } => (
            params.top_p,
            params.seed,
            params.stop.as_deref(),
            &["top_p"][..],
            &["stop"][..],
        ),
        _ => (None, None, None, &[][..], &[][..]),
    };
    if let Some(value) = top_p {
        options.top_p = Some(value);
        migrate(&mut raw, &mut fallback, top_path);
    }
    if let Some(value) = seed.and_then(|value| u64::try_from(value).ok()) {
        options.seed = Some(value);
        migrate(&mut raw, &mut fallback, &["seed"]);
    }
    if let Some(value) = stop.filter(|value| !value.is_empty()) {
        options.stop = value.to_vec();
        migrate(&mut raw, &mut fallback, stop_path);
    }
    match model {
        ProviderModel::OpenAI { api, params } => {
            if let Some(effort) = params
                .reasoning_effort
                .as_deref()
                .and_then(|value| serde_json::from_value::<Effort>(json!(value)).ok())
            {
                options.reasoning = Some(Reasoning::Effort(effort));
                let path = if *api == Some(OpenAiApi::Responses) {
                    &["reasoning", "effort"][..]
                } else {
                    &["reasoning_effort"][..]
                };
                migrate(&mut raw, &mut fallback, path);
            }
            let mut typed = openai::extension::OpenAiOptions::default();
            if *api == Some(OpenAiApi::Responses) {
                if let Some(value) = params
                    .top_logprobs
                    .and_then(|value| u8::try_from(value).ok())
                {
                    typed.responses.top_logprobs = Some(value);
                    migrate(&mut raw, &mut fallback, &["top_logprobs"]);
                }
            } else {
                typed.chat.frequency_penalty = params.frequency_penalty;
                typed.chat.presence_penalty = params.presence_penalty;
                typed.chat.logprobs = params.logprobs;
                for key in ["frequency_penalty", "presence_penalty", "logprobs"] {
                    migrate(&mut raw, &mut fallback, &[key]);
                }
                if let Some(value) = params
                    .top_logprobs
                    .and_then(|value| u8::try_from(value).ok())
                {
                    typed.chat.top_logprobs = Some(value);
                    migrate(&mut raw, &mut fallback, &["top_logprobs"]);
                }
            }
            if typed != Default::default() {
                request.provider_options = std::mem::take(&mut request.provider_options).set(typed);
                extension = Some(openai::PROVIDER_NAME);
            }
        }
        ProviderModel::Anthropic { params } => {
            // Short retention is exactly the existing top-level ephemeral cache.
            // Thinking stays raw: native effort/budget mapping may choose adaptive
            // thinking or omit disabled settings on known model families.
            options.cache = Some(CacheRetention::Short);
            migrate(&mut raw, &mut fallback, &["cache_control"]);
            if let Some(value) = params.top_k.and_then(|value| u32::try_from(value).ok()) {
                request.provider_options = std::mem::take(&mut request.provider_options)
                    .set(anthropic::extension::AnthropicOptions::default().top_k(value));
                migrate(&mut raw, &mut fallback, &["top_k"]);
                extension = Some(anthropic::PROVIDER_NAME);
            }
        }
        ProviderModel::Gemini { params } => {
            let mut typed = gemini::extension::GeminiOptions::default();
            if let Some(value) = params.top_k {
                typed = typed.top_k(value as u32); // Checked by the settings compiler.
                migrate(&mut raw, &mut fallback, &["generationConfig", "topK"]);
            }
            if let Some(value) = params
                .thinking_config
                .as_ref()
                .and_then(|value| value.include_thoughts)
            {
                typed = typed.include_thoughts(value);
                migrate(
                    &mut raw,
                    &mut fallback,
                    &["generationConfig", "thinkingConfig", "includeThoughts"],
                );
            }
            if params.candidate_count == Some(1) {
                typed = typed.candidate_count(gemini::extension::CandidateCount::One);
                migrate(
                    &mut raw,
                    &mut fallback,
                    &["generationConfig", "candidateCount"],
                );
            }
            if typed != Default::default() {
                request.provider_options = std::mem::take(&mut request.provider_options).set(typed);
                extension = Some(gemini::PROVIDER_NAME);
            }
        }
        ProviderModel::Ollama { params } => {
            let mut typed = ollama::extension::OllamaOptions::default();
            macro_rules! option {
                ($field:ident, $ty:ty) => {
                    if let Some(value) = params.$field.and_then(|value| <$ty>::try_from(value).ok())
                    {
                        typed.chat.options.$field = Some(value);
                        migrate(&mut raw, &mut fallback, &[stringify!($field)]);
                    }
                };
            }
            option!(num_ctx, u32);
            option!(num_keep, u32);
            option!(top_k, u32);
            option!(repeat_last_n, i32);
            option!(num_gpu, i32);
            option!(num_thread, u32);
            typed.chat.options.min_p = params.min_p;
            typed.chat.options.repeat_penalty = params.repeat_penalty;
            for key in ["min_p", "repeat_penalty"] {
                migrate(&mut raw, &mut fallback, &[key]);
            }
            if typed != Default::default() {
                request.provider_options = std::mem::take(&mut request.provider_options).set(typed);
                extension = Some(ollama::PROVIDER_NAME);
            }
        }
        ProviderModel::Bedrock { params } => {
            // The AWS SDK narrows topP to f32. Keep final-wire compatibility for
            // values that would lose precision through that SDK conversion.
            if let Some(value) = params.top_p {
                if (value as f32) as f64 == value {
                    options.top_p = Some(value);
                    fallback_wire["inferenceConfig"] = json!({"topP": value});
                } else {
                    compatibility["inferenceConfig"] = json!({"topP": value});
                }
            }
            if let Some(value) = &params.stop_sequences {
                if value.is_empty() {
                    merge_json(
                        compatibility,
                        json!({"inferenceConfig": {"stopSequences": value}}),
                    );
                } else {
                    options.stop = value.clone();
                    merge_json(
                        &mut fallback_wire,
                        json!({"inferenceConfig": {"stopSequences": value}}),
                    );
                }
            }
        }
        _ => {}
    }
    request.additional_params = raw
        .as_object()
        .filter(|map| !map.is_empty())
        .map(|_| raw.clone());
    if request.provider_options.is_empty() {
        extension = None;
    }
    (fallback, fallback_wire, extension)
}
