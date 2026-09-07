use super::{CachePolicy, ManagedIntegration, ResolvedSecret, SecretContext};
use crate::core::error::AppError;
use serde_json::Value;
use std::collections::HashMap;

pub const ID: &str = "static";

/// The stored document is the complete exportable map.
pub type Document = HashMap<String, String>;
pub struct StaticSecretIntegration;

#[async_trait::async_trait]
impl ManagedIntegration for StaticSecretIntegration {
    type Credentials = Document;

    async fn get_secret(
        &self,
        doc: Value,
        _: &mut SecretContext,
    ) -> Result<ResolvedSecret<Document>, AppError> {
        let fields: Document = serde_json::from_value(doc)
            .map_err(|_| AppError::Validation("invalid static credential document".into()))?;
        if fields.is_empty() || fields.keys().any(|key| key.is_empty()) {
            return Err(AppError::Validation(
                "static credential requires named fields".into(),
            ));
        }
        Ok(ResolvedSecret {
            credentials: fields,
            expires_at: None,
            cache: CachePolicy::UntilChanged,
        })
    }
}

impl super::SecretEnv for Document {
    fn to_env(&self) -> Result<HashMap<String, String>, AppError> {
        Ok(self.clone())
    }
}

pub fn api_key(credentials: &Document) -> Result<&str, AppError> {
    credentials
        .get("API_KEY")
        .filter(|key| !key.is_empty())
        .map(String::as_str)
        .ok_or_else(|| AppError::Validation("static credential has no API_KEY".into()))
}

pub fn document(api_key: String) -> Value {
    serde_json::to_value(Document::from([("API_KEY".into(), api_key)]))
        .expect("string fields serialize")
}
