//! Authoring-only setting schemas. Runtime serialization remains catalog-free.

use crate::inference::{
    protocol::{parameters as wire_params, parameters::WireDialect},
    provider::platform::ResolvedConnection,
};
use crate::{
    core::config::{ApiSurface, ModelGroupConfig, OpenAiApi, ProviderModel},
    inference::credential::store::CredentialMethod,
};
use frona_model_catalog::{
    ModelCatalogSnapshot,
    parameters::{ParameterCatalogSnapshot, ParameterModel},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingStorage {
    Typed { config_path: String },
    ExtraParams { config_path: String },
}

impl SettingStorage {
    pub fn path(&self) -> &str {
        match self {
            Self::Typed { config_path } | Self::ExtraParams { config_path } => config_path,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Applicability {
    All {
        expressions: Vec<Applicability>,
    },
    Any {
        expressions: Vec<Applicability>,
    },
    Not {
        expression: Box<Applicability>,
    },
    In {
        config_path: String,
        values: Vec<Value>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelSettingInfo {
    pub id: String,
    pub storage: Option<SettingStorage>,
    pub request_path: Option<String>,
    pub catalog_path: Option<String>,
    pub label: String,
    pub description: Option<String>,
    pub group: String,
    pub scope: &'static str,
    pub schema: Value,
    pub applicability: Option<Applicability>,
    pub support: &'static str,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SettingsDescriptor {
    pub settings: Vec<ModelSettingInfo>,
    pub warnings: Vec<String>,
    pub exact_catalog_match: bool,
}

#[derive(Clone, Default)]
pub struct SettingsDirectory {
    cache: Arc<Mutex<HashMap<String, Arc<SettingsDescriptor>>>>,
}

impl SettingsDirectory {
    pub fn describe(
        &self,
        connection: &ResolvedConnection,
        method: Option<CredentialMethod>,
        model: &str,
        api: ApiSurface,
        models: &ModelCatalogSnapshot,
        parameters: &ParameterCatalogSnapshot,
    ) -> Arc<SettingsDescriptor> {
        let key = serde_json::to_string(&(
            2,
            &connection.brand,
            connection.adapter,
            connection.request_adapter_name(),
            &connection.effective_base_url,
            method,
            connection.catalog_identity(method),
            model,
            api,
            &models.version,
            &parameters.version,
        ))
        .unwrap();
        let mut cache = self.cache.lock().expect("descriptor cache lock");
        if let Some(value) = cache.get(&key) {
            return value.clone();
        }
        let value = Arc::new(build(connection, method, model, api, models, parameters));
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(key, value.clone());
        value
    }
}

pub fn pointer(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect()
}

fn catalog_parts(path: &str, dialect: WireDialect) -> Vec<String> {
    if let Some(pointer) = path.strip_prefix('/') {
        return pointer
            .split('/')
            .map(|part| part.replace("~1", "/").replace("~0", "~"))
            .collect();
    }
    let path = if matches!(
        dialect,
        WireDialect::OpenAiChat | WireDialect::LegacyChat | WireDialect::Responses
    ) {
        path.strip_prefix("extra_body.").unwrap_or(path)
    } else {
        path
    };
    path.split('.').map(String::from).collect()
}

fn request_model(connection: &ResolvedConnection, api: ApiSurface) -> Option<ProviderModel> {
    if !connection.protocols.contains(&api) {
        return None;
    }
    let mut model = ProviderModel::from_name(connection.request_adapter_name());
    if let ProviderModel::OpenAI { api: selected, .. } = &mut model {
        *selected = Some(if api == ApiSurface::Responses {
            OpenAiApi::Responses
        } else {
            OpenAiApi::ChatCompletions
        });
    }
    Some(model)
}

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::to_value(schemars::schema_for!(ModelGroupConfig)).unwrap())
}

fn expanded(value: &Value, root: &Value) -> Value {
    if let Some(reference) = value
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix('#'))
        && let Some(target) = root.pointer(reference)
    {
        return expanded(target, root);
    }
    if let Some(options) = value.get("anyOf").and_then(Value::as_array)
        && let Some(target) = options
            .iter()
            .find(|item| item.get("type") != Some(&json!("null")))
    {
        return expanded(target, root);
    }
    value.clone()
}

fn typed_schema(path: &str) -> Option<Value> {
    static FIELDS: OnceLock<Mutex<HashMap<String, Option<Value>>>> = OnceLock::new();
    let mut fields = FIELDS
        .get_or_init(Default::default)
        .lock()
        .expect("typed schema cache");
    if let Some(value) = fields.get(path) {
        return value.clone();
    }
    let value = extract_typed_schema(path);
    fields.insert(path.into(), value.clone());
    value
}

fn extract_typed_schema(path: &str) -> Option<Value> {
    let root = schema();
    let mut current = root.clone();
    for part in path.split('.') {
        current = expanded(&current, root)
            .get("properties")?
            .get(part)?
            .clone();
    }
    let mut current = expanded(&current, root);
    if let Some(object) = current.as_object_mut() {
        object.remove("default");
        // None means omitted in typed configuration, not an explicit wire null.
        if let Some(types) = object.get_mut("type").and_then(Value::as_array_mut) {
            types.retain(|value| value != "null");
        }
    }
    Some(current)
}

fn control(config: &str, request: Option<String>, schema: Value) -> ModelSettingInfo {
    let storage_path = pointer(&config.split('.').map(String::from).collect::<Vec<_>>());
    ModelSettingInfo {
        id: request.clone().unwrap_or_else(|| storage_path.clone()),
        storage: Some(SettingStorage::Typed {
            config_path: storage_path,
        }),
        request_path: request.clone(),
        catalog_path: None,
        label: config.replace('_', " "),
        description: schema
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        group: if config.contains("thinking") || config.contains("reasoning") {
            "reasoning"
        } else {
            "general"
        }
        .into(),
        scope: if request.is_some() {
            "provider_request"
        } else {
            "frona_runtime"
        },
        schema,
        applicability: None,
        support: "typed",
        sources: vec!["frona".into(), "adapter".into()],
    }
}

fn raw_allowed(parts: &[String], dialect: WireDialect) -> bool {
    !parts.is_empty()
        && !parts.iter().any(String::is_empty)
        && !wire_params::reserved_paths(dialect, false)
            .iter()
            .any(|reserved| parts.starts_with(reserved) || reserved.starts_with(parts))
}

fn catalog_schema(parameter: &Value) -> Value {
    let mut schema = json!({});
    match parameter["type"].as_str() {
        Some("enum") => {
            schema["enum"] = parameter["values"].clone();
        }
        Some(kind) => {
            schema["type"] = json!(kind);
        }
        None => {}
    }
    if let Some(default) = parameter.get("default") {
        schema["default"] = default.clone();
    }
    for (source, target) in [
        ("min", "minimum"),
        ("max", "maximum"),
        ("step", "multipleOf"),
    ] {
        if let Some(value) = parameter.get("range").and_then(|range| range.get(source)) {
            schema[target] = value.clone();
        }
    }
    schema
}

fn refine(typed: &Value, catalog: &Value) -> Option<Value> {
    let validator = jsonschema::validator_for(typed).ok()?;
    if let Some(values) = catalog.get("enum").and_then(Value::as_array)
        && values.iter().any(|value| !validator.is_valid(value))
    {
        return None;
    }
    if let Some(default) = catalog.get("default")
        && !validator.is_valid(default)
    {
        return None;
    }
    if let Some(kind) = catalog.get("type").and_then(Value::as_str) {
        let types = typed.get("type")?;
        if types != kind
            && !types
                .as_array()
                .is_some_and(|types| types.iter().any(|item| item == kind))
        {
            return None;
        }
    }
    let mut result = typed.clone();
    // allOf keeps Rust bounds authoritative rather than replacing them.
    let mut constraints = catalog.clone();
    constraints.as_object_mut()?.remove("default");
    result["allOf"] = json!([constraints]);
    if let Some(default) = catalog.get("default") {
        result["default"] = default.clone();
    }
    Some(result)
}

fn conditions(value: &Value, paths: &HashMap<String, String>) -> Option<Applicability> {
    if let Some(alternatives) = value.as_array() {
        return Some(Applicability::Any {
            expressions: alternatives
                .iter()
                .map(|value| conditions(value, paths))
                .collect::<Option<_>>()?,
        });
    }
    let mut expressions = Vec::new();
    for (key, value) in value.as_object()? {
        let path = paths.get(key)?.clone();
        let (value, negate) = match value.get("not") {
            Some(value) => (value, true),
            None => (value, false),
        };
        let expression = Applicability::In {
            config_path: path,
            values: value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![value.clone()]),
        };
        expressions.push(if negate {
            Applicability::Not {
                expression: Box::new(expression),
            }
        } else {
            expression
        });
    }
    Some(Applicability::All { expressions })
}

fn applicability(parameter: &Value, paths: &HashMap<String, String>) -> Option<Applicability> {
    let rules = parameter.get("applicability")?;
    let mut expressions = Vec::new();
    if let Some(only) = rules.get("only") {
        expressions.push(conditions(only, paths)?);
    }
    if let Some(except) = rules.get("except") {
        expressions.push(Applicability::Not {
            expression: Box::new(conditions(except, paths)?),
        });
    }
    Some(Applicability::All { expressions })
}

fn enrich(result: &mut SettingsDescriptor, record: &ParameterModel, dialect: WireDialect) {
    for parameter in &record.params {
        let Some(path) = parameter["path"].as_str() else {
            continue;
        };
        let parts = catalog_parts(path, dialect);
        let request_path = pointer(&parts);
        let constraint = catalog_schema(parameter);
        let index = result
            .settings
            .iter()
            .position(|setting| setting.request_path.as_ref() == Some(&request_path));
        let mut setting = if let Some(index) = index {
            let mut existing = result.settings.remove(index);
            if let Some(schema) = refine(&existing.schema, &constraint) {
                existing.schema = schema;
            } else {
                result.warnings.push(format!(
                    "{path}: catalog constraint conflicts with the Rust type; typed schema retained"
                ));
            }
            existing
        } else {
            let allowed = raw_allowed(&parts, dialect);
            ModelSettingInfo {
                id: request_path.clone(),
                storage: allowed.then(|| SettingStorage::ExtraParams {
                    config_path: format!("/extra_params{request_path}"),
                }),
                request_path: Some(request_path),
                catalog_path: None,
                label: path.into(),
                description: None,
                group: "general".into(),
                scope: "provider_request",
                schema: constraint,
                applicability: None,
                support: if allowed {
                    "extra_params"
                } else {
                    "unsupported_by_adapter"
                },
                sources: vec!["adapter".into()],
            }
        };
        setting.catalog_path = Some(path.into());
        setting.label = parameter["label"].as_str().unwrap_or(path).into();
        setting.description = parameter["description"].as_str().map(String::from);
        setting.group = parameter["group"].as_str().unwrap_or("general").into();
        setting.sources.push("modelparams.dev".into());
        result.settings.push(setting);
    }
    let paths: HashMap<_, _> = result
        .settings
        .iter()
        .filter_map(|setting| {
            Some((
                setting.catalog_path.clone()?,
                setting.storage.as_ref()?.path().into(),
            ))
        })
        .collect();
    for parameter in &record.params {
        let Some(path) = parameter["path"].as_str() else {
            continue;
        };
        if let Some(setting) = result
            .settings
            .iter_mut()
            .find(|setting| setting.catalog_path.as_deref() == Some(path))
        {
            setting.applicability = applicability(parameter, &paths);
            if parameter.get("applicability").is_some() && setting.applicability.is_none() {
                setting.storage = None;
                setting.support = "unsupported_by_adapter";
                result.warnings.push(format!(
                    "{path}: applicability references an unavailable setting"
                ));
            }
        }
    }
}

fn build(
    connection: &ResolvedConnection,
    method: Option<CredentialMethod>,
    model: &str,
    api: ApiSurface,
    models: &ModelCatalogSnapshot,
    parameters: &ParameterCatalogSnapshot,
) -> SettingsDescriptor {
    let mut result = SettingsDescriptor::default();
    if connection.brand == "openai" && method == Some(CredentialMethod::Oauth) {
        result.warnings.push("ChatGPT subscription transport omits typed temperature and output-token limits because the backend does not support them. Custom JSON is sent unchanged and may be rejected by the backend. Model access and quota are not proven by login.".into());
    }
    let Some(request) = request_model(connection, api) else {
        result
            .warnings
            .push("unsupported_protocol_for_adapter".into());
        return result;
    };
    let dialect = WireDialect::for_model(&request);
    for mapping in wire_params::parameter_bindings(&request) {
        if let Some(schema) = typed_schema(&mapping.config_path) {
            let path = pointer(&mapping.wire_path);
            // Common max_tokens owns the output limit; its legacy alias is still
            // accepted in YAML, but does not create a duplicate UI setting.
            if !result
                .settings
                .iter()
                .any(|setting| setting.request_path.as_ref() == Some(&path))
            {
                result
                    .settings
                    .push(control(&mapping.config_path, Some(path), schema));
            }
        }
    }
    if let Some(schema) = typed_schema("context_window") {
        result
            .settings
            .push(control("context_window", None, schema));
    }
    let mut raw = control(
        "extra_params",
        None,
        json!({"type":"object","additionalProperties":true,
            "x-frona-reserved-paths":wire_params::reserved_paths(dialect, false).iter().map(|path| pointer(path)).collect::<Vec<_>>() }),
    );
    raw.scope = "provider_request";
    raw.support = "configured_unverified";
    raw.description = Some("Native request values. Unknown keys are unverified; reserved request fields cannot be replaced.".into());
    result.settings.push(raw);
    let (brand, access) = connection.catalog_identity(method);
    if let Some(record) = crate::inference::directory::protocols::exact_parameters(
        parameters, brand, access, api, model,
    ) {
        result.exact_catalog_match = true;
        enrich(&mut result, record, dialect);
    } else if let Some(metadata) = models.entries.get(&format!("{}/{model}", connection.brand)) {
        // Catalog entries describe controls, not values. Only effort strings
        // belong in this typed field; null remains omitted ("Use default").
        let reasoning_efforts: Vec<_> = metadata
            .reasoning_options
            .iter()
            .filter(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
            .filter_map(|option| option.get("values").and_then(Value::as_array))
            .flatten()
            .filter(|value| value.is_string())
            .cloned()
            .collect();
        for setting in &mut result.settings {
            if setting
                .storage
                .as_ref()
                .is_some_and(|storage| storage.path() == "/max_tokens")
                && let Some(limit) = metadata.max_output_tokens()
            {
                setting.schema["maximum"] = json!(limit);
                setting.sources.push("models.dev".into());
            }
            if setting
                .storage
                .as_ref()
                .is_some_and(|storage| storage.path() == "/reasoning_effort")
                && !reasoning_efforts.is_empty()
            {
                if let Some(refined) = refine(&setting.schema, &json!({"enum":reasoning_efforts})) {
                    setting.schema = refined;
                    setting.sources.push("models.dev".into());
                } else {
                    result
                        .warnings
                        .push("reasoning_options conflict with the typed reasoning field".into());
                }
            }
        }
    }
    result.settings.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

/// Saved-value diagnostics are assembled after the shared cache. Only inferred
/// types enter the descriptor; the actual values remain in their model groups.
pub fn configured_settings(
    protocol: &mut crate::inference::directory::models::ModelProtocolInfo,
    group: &ModelGroupConfig,
) {
    let configuration = serde_json::to_value(group).expect("model group serializes");
    for setting in &protocol.settings {
        if let Some(storage) = &setting.storage
            && let Some(value) = configuration.pointer(storage.path())
            && setting.catalog_path.is_some()
            && jsonschema::validator_for(&setting.schema)
                .is_ok_and(|validator| !validator.is_valid(value))
        {
            protocol.warnings.push(format!(
                "{}: saved value conflicts with catalog constraints; value preserved",
                storage.path()
            ));
        }
    }
    let dialect = match protocol.api {
        ApiSurface::Completions => WireDialect::OpenAiChat,
        ApiSurface::Responses => WireDialect::Responses,
        ApiSurface::AnthropicMessages => WireDialect::Anthropic,
        ApiSurface::GoogleGenerateContent => WireDialect::Gemini,
        ApiSurface::Ollama => WireDialect::Ollama,
        ApiSurface::CohereChat => WireDialect::Cohere,
        ApiSurface::HuggingFace => WireDialect::HuggingFace,
        ApiSurface::AmazonBedrockConverse => WireDialect::Bedrock,
    };

    fn visit(
        protocol: &mut crate::inference::directory::models::ModelProtocolInfo,
        parts: Vec<String>,
        value: &Value,
        dialect: WireDialect,
    ) {
        let request = pointer(&parts);
        let storage = format!("/extra_params{request}");
        if protocol.settings.iter().any(|setting| {
            setting.catalog_path.is_some()
                && setting
                    .storage
                    .as_ref()
                    .is_some_and(|target| target.path() == storage)
        }) {
            return;
        }
        if let Some(object) = value.as_object().filter(|object| !object.is_empty()) {
            for (key, value) in object {
                let mut path = parts.clone();
                path.push(key.clone());
                visit(protocol, path, value, dialect);
            }
            return;
        }
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if let Some(existing) = protocol.settings.iter_mut().find(|setting| {
            setting
                .storage
                .as_ref()
                .is_some_and(|target| target.path() == storage)
        }) {
            let types = existing.schema["type"]
                .as_array_mut()
                .expect("inferred type union");
            if !types.iter().any(|value| value == kind) {
                types.push(json!(kind));
                types.sort_by_key(Value::to_string);
            }
            return;
        }
        if protocol
            .settings
            .iter()
            .any(|setting| setting.request_path.as_deref() == Some(&request))
        {
            protocol.warnings.push(format!(
                "{storage}: raw value overrides the typed request setting"
            ));
        }
        let allowed = raw_allowed(&parts, dialect)
            && protocol
                .settings
                .iter()
                .any(|setting| setting.id == "/extra_params");
        if let Some(mut described) = protocol
            .settings
            .iter()
            .find(|setting| {
                setting.request_path.as_deref() == Some(&request) && setting.catalog_path.is_some()
            })
            .cloned()
        {
            if jsonschema::validator_for(&described.schema)
                .is_ok_and(|validator| !validator.is_valid(value))
            {
                protocol.warnings.push(format!(
                    "{storage}: saved raw value conflicts with catalog constraints; value preserved"
                ));
            }
            described.id = format!("raw:{request}");
            described.storage = allowed.then_some(SettingStorage::ExtraParams {
                config_path: storage,
            });
            described.support = if allowed {
                "extra_params"
            } else {
                "unsupported_by_adapter"
            };
            described.sources.push("configured".into());
            protocol.settings.push(described);
            return;
        }
        protocol.settings.push(ModelSettingInfo {
            id: format!("raw:{request}"),
            storage: allowed.then_some(SettingStorage::ExtraParams {
                config_path: storage,
            }),
            request_path: Some(request),
            catalog_path: None,
            label: parts.last().cloned().unwrap_or_default(),
            description: Some("Saved native parameter; provider support is unverified.".into()),
            group: "custom".into(),
            scope: "provider_request",
            schema: json!({"type":[kind]}),
            applicability: None,
            support: if allowed {
                "configured_unverified"
            } else {
                "unsupported_by_adapter"
            },
            sources: vec!["configured".into()],
        });
    }
    for (key, value) in &group.common.extra_params {
        visit(protocol, vec![key.clone()], value, dialect);
    }
    protocol.settings.sort_by(|a, b| a.id.cmp(&b.id));
    protocol.warnings.sort();
    protocol.warnings.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Handle, config::ModelProviderConfig};
    use crate::inference::provider::platform::ProviderPlatform;

    fn connection(brand: &str) -> ResolvedConnection {
        ProviderPlatform::resolve(
            &Handle::const_validated("account"),
            &ModelProviderConfig {
                provider: Some(brand.into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn parameter(path: &str, kind: &str) -> Value {
        json!({"path":path,"type":kind,"label":path,"description":"fixture", "group":"reasoning"})
    }

    fn record(api: ApiSurface, access: &str, params: Vec<Value>) -> ParameterModel {
        ParameterModel {
            provider: "openai".into(),
            auth_type: access.into(),
            api_surface: serde_json::to_value(api).unwrap().as_str().unwrap().into(),
            model: "display-name".into(),
            wire_id: Some("exact-model".into()),
            params,
            metadata: HashMap::new(),
        }
    }

    fn catalog(records: Vec<ParameterModel>) -> ParameterCatalogSnapshot {
        ParameterCatalogSnapshot {
            models: records,
            version: "fixture-v1".into(),
            ..ParameterCatalogSnapshot::empty()
        }
    }

    fn setting<'a>(descriptor: &'a SettingsDescriptor, path: &str) -> &'a ModelSettingInfo {
        descriptor
            .settings
            .iter()
            .find(|setting| setting.request_path.as_deref() == Some(path))
            .unwrap_or_else(|| panic!("missing {path}: {descriptor:?}"))
    }

    #[test]
    fn raw_editor_exposes_the_same_reserved_paths_as_request_validation() {
        let directory = SettingsDirectory::default();
        for brand in [
            "openai",
            "anthropic",
            "google",
            "ollama",
            "cohere",
            "amazon-bedrock",
        ] {
            let connection = connection(brand);
            for api in connection.protocols {
                let descriptor = directory.describe(
                    &connection,
                    Some(CredentialMethod::ApiKey),
                    "manual-unknown",
                    *api,
                    &ModelCatalogSnapshot::empty(),
                    &ParameterCatalogSnapshot::empty(),
                );
                let raw = descriptor
                    .settings
                    .iter()
                    .find(|setting| setting.id == "/extra_params")
                    .unwrap();
                let model = request_model(&connection, *api).unwrap();
                let expected: Vec<_> =
                    wire_params::reserved_paths(WireDialect::for_model(&model), false)
                        .iter()
                        .map(|path| pointer(path))
                        .collect();
                assert_eq!(raw.schema["x-frona-reserved-paths"], json!(expected));
            }
        }
    }

    #[test]
    fn model_catalog_reasoning_controls_refine_only_effort_values() {
        for (options, expected) in [
            (
                json!([
                    {"type":"toggle"},
                    {"type":"effort","values":[null,"default","none","low","high","xhigh","max"]},
                    {"type":"budget_tokens","min":1024,"max":32768}
                ]),
                Some(json!(["default", "none", "low", "high", "xhigh", "max"])),
            ),
            (json!([{"type":"toggle"}]), None),
            (json!([{"type":"budget_tokens","min":1024}]), None),
            (json!([{"type":"effort","values":[null]}]), None),
            (json!([]), None),
        ] {
            let mut models = ModelCatalogSnapshot::empty();
            models.entries.insert(
                "openai/exact-model".into(),
                frona_model_catalog::ModelEntry {
                    reasoning_options: options.as_array().unwrap().clone(),
                    ..Default::default()
                },
            );
            for api in [ApiSurface::Completions, ApiSurface::Responses] {
                let descriptor = build(
                    &connection("openai"),
                    Some(CredentialMethod::ApiKey),
                    "exact-model",
                    api,
                    &models,
                    &ParameterCatalogSnapshot::empty(),
                );
                assert!(descriptor.warnings.is_empty(), "{:?}", descriptor.warnings);
                let effort = descriptor
                    .settings
                    .iter()
                    .find(|setting| {
                        setting
                            .storage
                            .as_ref()
                            .is_some_and(|storage| storage.path() == "/reasoning_effort")
                    })
                    .unwrap();
                assert_eq!(effort.schema.pointer("/allOf/0/enum"), expected.as_ref());
                assert_eq!(
                    effort.sources.iter().any(|source| source == "models.dev"),
                    expected.is_some()
                );
            }
            assert_eq!(
                json!(models.entries["openai/exact-model"].reasoning_options),
                options
            );
        }
    }

    #[test]
    fn protocols_have_independent_mappings_and_exact_access_constraints() {
        let mut chat = parameter("reasoning_effort", "enum");
        chat["values"] = json!(["low", "high"]);
        chat["default"] = json!("low");
        let mut response = parameter("reasoning.effort", "enum");
        response["values"] = json!(["medium", "high"]);
        response["default"] = json!("medium");
        let mut subscription = response.clone();
        subscription["default"] = json!("high");
        let catalog = catalog(vec![
            record(ApiSurface::Completions, "api_key", vec![chat]),
            record(ApiSurface::Responses, "api_key", vec![response]),
            record(ApiSurface::Responses, "subscription", vec![subscription]),
        ]);
        let connection = connection("openai");
        let models = ModelCatalogSnapshot::empty();
        let directory = SettingsDirectory::default();
        let chat = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "exact-model",
            ApiSurface::Completions,
            &models,
            &catalog,
        );
        let responses = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "exact-model",
            ApiSurface::Responses,
            &models,
            &catalog,
        );
        let subscription = directory.describe(
            &connection,
            Some(CredentialMethod::Oauth),
            "exact-model",
            ApiSurface::Responses,
            &models,
            &catalog,
        );
        for (descriptor, path, default) in [
            (&chat, "/reasoning_effort", "low"),
            (&responses, "/reasoning/effort", "medium"),
            (&subscription, "/reasoning/effort", "high"),
        ] {
            let setting = setting(descriptor, path);
            assert_eq!(
                setting.storage.as_ref().unwrap().path(),
                "/reasoning_effort"
            );
            assert_eq!(setting.schema["default"], default);
            assert!(descriptor.exact_catalog_match);
            assert!(
                !serde_json::to_value(&descriptor.settings)
                    .unwrap()
                    .to_string()
                    .contains("current_value")
            );
        }
        assert_eq!(
            setting(&chat, "/max_completion_tokens")
                .storage
                .as_ref()
                .unwrap()
                .path(),
            "/max_tokens"
        );
        assert_eq!(
            setting(&responses, "/max_output_tokens")
                .storage
                .as_ref()
                .unwrap()
                .path(),
            "/max_tokens"
        );
        assert!(
            responses
                .settings
                .iter()
                .all(|setting| setting.request_path.as_deref() != Some("/seed"))
        );
        for unknown in ["exact-model-2026", "display-name", "manual"] {
            let descriptor = directory.describe(
                &connection,
                Some(CredentialMethod::ApiKey),
                unknown,
                ApiSurface::Responses,
                &models,
                &catalog,
            );
            assert!(!descriptor.exact_catalog_match);
            assert!(
                setting(&descriptor, "/reasoning/effort")
                    .schema
                    .get("default")
                    .is_none()
            );
            assert!(
                setting(&descriptor, "/max_output_tokens")
                    .schema
                    .get("maximum")
                    .is_none()
            );
            assert!(
                descriptor
                    .settings
                    .iter()
                    .any(|setting| setting.id == "/extra_params")
            );
        }
    }

    #[test]
    fn raw_nulls_pointers_applicability_and_reserved_fields_are_explicit() {
        let mut nullable = parameter("thinking.keep", "enum");
        nullable["values"] = json!(["all", null]);
        nullable["default"] = Value::Null;
        let mut top_k = parameter("extra_body.top_k", "integer");
        top_k["range"] = json!({"min":0,"max":99});
        top_k["applicability"] = json!({"only":{"thinking.keep":["all", null]},"except":{"thinking.keep":{"not":"all"}}});
        let catalog = catalog(vec![record(
            ApiSurface::Completions,
            "api_key",
            vec![
                nullable,
                top_k,
                parameter("/literal.dot/a~1b", "string"),
                parameter("messages", "string"),
            ],
        )]);
        let result = build(
            &connection("openai"),
            Some(CredentialMethod::ApiKey),
            "exact-model",
            ApiSurface::Completions,
            &ModelCatalogSnapshot::empty(),
            &catalog,
        );
        let nullable = setting(&result, "/thinking/keep");
        assert_eq!(nullable.schema["enum"], json!(["all", null]));
        assert_eq!(nullable.schema.get("default"), Some(&Value::Null));
        assert_eq!(
            nullable.storage.as_ref().unwrap().path(),
            "/extra_params/thinking/keep"
        );
        let top_k = setting(&result, "/top_k");
        assert_eq!(top_k.catalog_path.as_deref(), Some("extra_body.top_k"));
        assert_eq!(
            top_k.storage.as_ref().unwrap().path(),
            "/extra_params/top_k"
        );
        let rule = serde_json::to_value(&top_k.applicability).unwrap();
        assert_eq!(rule["op"], "all");
        assert_eq!(
            rule["expressions"][0]["expressions"][0]["config_path"],
            "/extra_params/thinking/keep"
        );
        assert_eq!(
            setting(&result, "/literal.dot/a~1b")
                .storage
                .as_ref()
                .unwrap()
                .path(),
            "/extra_params/literal.dot/a~1b"
        );
        assert!(setting(&result, "/messages").storage.is_none());
        assert_eq!(
            setting(&result, "/messages").support,
            "unsupported_by_adapter"
        );
    }

    #[test]
    fn typed_conflicts_and_unresolved_conditions_never_invent_writable_contracts() {
        let mut conflict = parameter("temperature", "boolean");
        conflict["default"] = json!(false);
        let mut conditional = parameter("extra_body.top_k", "integer");
        conditional["applicability"] = json!({"only":{"unavailable":true}});
        let catalog = catalog(vec![record(
            ApiSurface::Completions,
            "api_key",
            vec![conflict, conditional],
        )]);
        let result = build(
            &connection("openai"),
            Some(CredentialMethod::ApiKey),
            "exact-model",
            ApiSurface::Completions,
            &ModelCatalogSnapshot::empty(),
            &catalog,
        );
        let temperature = setting(&result, "/temperature");
        let validator = jsonschema::validator_for(&temperature.schema).unwrap();
        assert!(validator.is_valid(&json!(0.5)));
        assert!(!validator.is_valid(&json!(false)));
        assert!(temperature.schema.get("default").is_none());
        assert!(setting(&result, "/top_k").storage.is_none());
        assert_eq!(result.warnings.len(), 2);
    }

    #[test]
    fn nested_typed_paths_match_the_serializers_and_recipe_aliases_are_explicit() {
        let empty = ParameterCatalogSnapshot::empty();
        for (brand, api, request, config) in [
            (
                "anthropic",
                ApiSurface::AnthropicMessages,
                "/thinking/budget_tokens",
                "/thinking/budget_tokens",
            ),
            (
                "google",
                ApiSurface::GoogleGenerateContent,
                "/generationConfig/thinkingConfig/thinkingBudget",
                "/thinking_config/thinking_budget",
            ),
            ("ollama", ApiSurface::Ollama, "/options/num_ctx", "/num_ctx"),
        ] {
            let result = build(
                &connection(brand),
                Some(CredentialMethod::ApiKey),
                "manual",
                api,
                &ModelCatalogSnapshot::empty(),
                &empty,
            );
            assert_eq!(
                setting(&result, request).storage.as_ref().unwrap().path(),
                config
            );
        }
        assert_eq!(
            connection("kimi-for-coding").catalog_identity(Some(CredentialMethod::ApiKey)),
            ("moonshot", "subscription")
        );
        assert_eq!(
            connection("moonshotai").catalog_identity(Some(CredentialMethod::ApiKey)),
            ("moonshot", "api_key")
        );
    }

    #[test]
    fn descriptor_cache_tracks_versions_and_does_not_contain_group_values() {
        let directory = SettingsDirectory::default();
        let connection = connection("openai");
        let models = ModelCatalogSnapshot::empty();
        let mut parameters = ParameterCatalogSnapshot::empty();
        let first = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "manual",
            ApiSurface::Responses,
            &models,
            &parameters,
        );
        let repeated = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "manual",
            ApiSurface::Responses,
            &models,
            &parameters,
        );
        assert!(Arc::ptr_eq(&first, &repeated));
        parameters.version = "new-version".into();
        let refreshed = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "manual",
            ApiSurface::Responses,
            &models,
            &parameters,
        );
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(
            serde_json::to_value(&first.settings).unwrap(),
            serde_json::to_value(&refreshed.settings).unwrap()
        );
        let unsupported = directory.describe(
            &connection,
            Some(CredentialMethod::ApiKey),
            "manual",
            ApiSurface::AnthropicMessages,
            &models,
            &parameters,
        );
        assert!(unsupported.settings.is_empty());
        assert_eq!(unsupported.warnings, ["unsupported_protocol_for_adapter"]);
    }

    #[test]
    fn saved_raw_types_and_conflicts_are_assembled_outside_the_shared_cache() {
        let mut temperature = parameter("temperature", "number");
        temperature["range"] = json!({"min":0,"max":1});
        let catalog = catalog(vec![record(
            ApiSurface::Responses,
            "api_key",
            vec![temperature],
        )]);
        let shared = SettingsDirectory::default().describe(
            &connection("openai"),
            Some(CredentialMethod::ApiKey),
            "exact-model",
            ApiSurface::Responses,
            &ModelCatalogSnapshot::empty(),
            &catalog,
        );
        let mut protocol = crate::inference::directory::models::ModelProtocolInfo {
            api: ApiSurface::Responses,
            available: true,
            settings: shared.settings.clone(),
            warnings: vec![],
        };
        for value in [json!(123), json!("another group")] {
            let group: ModelGroupConfig = serde_json::from_value(json!({"provider":"account","model":"exact-model", "api":"responses",
                "temperature":1.5, "extra_params":{"literal.dot":value,"a/b":null,"temperature":2}})).unwrap();
            let before = serde_json::to_value(&group).unwrap();
            configured_settings(&mut protocol, &group);
            assert_eq!(serde_json::to_value(&group).unwrap(), before);
        }
        let custom = protocol
            .settings
            .iter()
            .find(|setting| setting.id == "raw:/literal.dot")
            .unwrap();
        assert_eq!(custom.schema["type"], json!(["integer", "string"]));
        assert_eq!(custom.support, "configured_unverified");
        assert_eq!(
            custom.storage.as_ref().unwrap().path(),
            "/extra_params/literal.dot"
        );
        assert!(protocol.settings.iter().any(|setting| {
            setting
                .storage
                .as_ref()
                .is_some_and(|storage| storage.path() == "/extra_params/a~1b")
        }));
        assert!(
            protocol
                .warnings
                .iter()
                .any(|warning| warning.contains("/temperature: saved value conflicts"))
        );
        assert!(protocol.warnings.iter().any(|warning| {
            warning.contains("/extra_params/temperature: saved raw value conflicts")
        }));
        assert!(
            !serde_json::to_string(&protocol.settings)
                .unwrap()
                .contains("another group")
        );
        assert!(
            shared
                .settings
                .iter()
                .all(|setting| !setting.id.starts_with("raw:"))
        );
    }

    #[test]
    fn fixture_exact_records_generate_valid_schema_fragments_for_compiled_protocols() {
        let parameters = catalog(vec![
            record(ApiSurface::Completions, "api_key", vec![]),
            record(ApiSurface::Responses, "api_key", vec![]),
        ]);
        let models = ModelCatalogSnapshot::empty();
        let mut described = 0;
        for record in &parameters.models {
            if record.auth_type != "api_key" {
                continue;
            }
            let config = ModelProviderConfig {
                provider: Some(record.provider.clone()),
                ..Default::default()
            };
            let Ok(connection) =
                ProviderPlatform::resolve(&Handle::const_validated("account"), &config)
            else {
                continue;
            };
            let Some(api) = crate::inference::directory::protocols::parameter_protocol(record)
            else {
                continue;
            };
            if !connection.protocols.contains(&api) {
                continue;
            }
            let result = build(
                &connection,
                Some(CredentialMethod::ApiKey),
                record.request_model(),
                api,
                &models,
                &parameters,
            );
            if !result.exact_catalog_match {
                continue;
            }
            described += 1;
            for setting in result.settings {
                let validator = jsonschema::validator_for(&setting.schema).unwrap();
                if let Some(default) = setting.schema.get("default") {
                    assert!(
                        validator.is_valid(default),
                        "{} {} {} invalid default",
                        record.provider,
                        record.request_model(),
                        setting.id
                    );
                }
                if let Some(storage) = setting.storage {
                    assert!(storage.path().starts_with('/'));
                    if let Some(request) = setting.request_path {
                        assert!(request.starts_with('/'));
                    }
                }
            }
        }
        assert_eq!(described, 2);
    }
}
