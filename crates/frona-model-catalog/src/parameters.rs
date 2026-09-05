use crate::CatalogError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterModel {
    pub provider: String,
    pub auth_type: String,
    pub api_surface: String,
    pub model: String,
    pub wire_id: Option<String>,
    pub params: Vec<Value>,
    #[serde(flatten)]
    pub metadata: HashMap<String, Value>,
}

impl ParameterModel {
    pub fn request_model(&self) -> &str {
        self.wire_id.as_deref().unwrap_or(&self.model)
    }
}

#[derive(Debug, Clone)]
pub struct ParameterCatalogSnapshot {
    pub version: String,
    pub generated_at: DateTime<Utc>,
    pub models: Vec<ParameterModel>,
}

impl ParameterCatalogSnapshot {
    pub fn empty() -> Self {
        Self {
            version: "unavailable".into(),
            generated_at: DateTime::UNIX_EPOCH,
            models: Vec::new(),
        }
    }

    pub fn exact(
        &self,
        provider: &str,
        authentication: &str,
        protocol: &str,
        model: &str,
    ) -> Option<&ParameterModel> {
        self.models.iter().find(|entry| {
            entry.provider == provider
                && entry.auth_type == authentication
                && entry.api_surface == protocol
                && entry.request_model() == model
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    generated_at: DateTime<Utc>,
    count: usize,
    models: Vec<Value>,
}

pub fn parse(raw: &str) -> Result<ParameterCatalogSnapshot, CatalogError> {
    static SCHEMA: OnceLock<jsonschema::Validator> = OnceLock::new();
    let schema = SCHEMA.get_or_init(|| {
        let source: Value =
            serde_json::from_str(include_str!("../resources/modelparams-schema.json"))
                .expect("bundled parameter schema JSON");
        // The bundled schema contains local references only. Network resolution
        // is disabled in the jsonschema dependency.
        jsonschema::validator_for(&source).expect("bundled parameter schema validates")
    });
    let catalog: Catalog = serde_json::from_str(raw).map_err(invalid)?;
    if catalog.count != catalog.models.len() || catalog.models.is_empty() {
        return Err(invalid(
            "parameter catalog count does not match nonempty models",
        ));
    }
    let mut models = Vec::new();
    let mut identities = HashSet::new();
    for (index, value) in catalog.models.into_iter().enumerate() {
        schema.validate(&value).map_err(|error| {
            invalid(format!(
                "models[{index}]{}: {}",
                error.instance_path(),
                error
            ))
        })?;
        let model: ParameterModel = serde_json::from_value(value).map_err(invalid)?;
        let identity = (
            model.provider.clone(),
            model.auth_type.clone(),
            model.api_surface.clone(),
            model.request_model().to_string(),
        );
        if !identities.insert(identity) {
            return Err(invalid(format!(
                "models[{index}]: duplicate exact model identity"
            )));
        }
        models.push(model);
    }
    Ok(ParameterCatalogSnapshot {
        version: super::sources::digest(raw.as_bytes()),
        generated_at: catalog.generated_at,
        models,
    })
}

fn invalid(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Validation(format!("parameter catalog: {error}"))
}
