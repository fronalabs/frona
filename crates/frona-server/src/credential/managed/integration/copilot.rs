use super::{CachePolicy, ManagedIntegration, ResolvedSecret, SecretContext};
use crate::core::error::AppError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ID: &str = "copilot";
pub(crate) const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

#[derive(Clone, Serialize, Deserialize)]
pub struct Document {
    pub github_token: String,
}

#[derive(Clone)]
pub struct CopilotIntegration {
    pub(crate) http: reqwest::Client,
    pub(crate) exchange_url: String,
    pub(crate) device_url: String,
    pub(crate) token_url: String,
}

impl Default for CopilotIntegration {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("HTTP client"),
            exchange_url: "https://api.github.com/copilot_internal/v2/token".into(),
            device_url: "https://github.com/login/device/code".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
        }
    }
}

#[derive(Deserialize)]
struct ChatToken {
    token: String,
    expires_at: i64,
}

impl CopilotIntegration {
    pub(crate) async fn exchange(
        &self,
        github_token: &str,
    ) -> Result<ResolvedSecret<rig_core::providers::copilot::CopilotAuth>, AppError> {
        let response = self
            .http
            .get(&self.exchange_url)
            .header("authorization", format!("token {github_token}"))
            .header("accept", "application/json")
            .header("user-agent", "GitHubCopilotChat/0.35.0")
            .header("editor-version", "vscode/1.107.0")
            .header("editor-plugin-version", "copilot-chat/0.35.0")
            .send()
            .await
            .map_err(|_| AppError::Validation("Copilot token exchange failed".into()))?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => {
                return Err(AppError::Validation(
                    "Copilot authentication rejected".into(),
                ));
            }
            status => {
                return Err(AppError::Validation(format!(
                    "Copilot token exchange returned HTTP {status}"
                )));
            }
        }
        let token: ChatToken = response
            .json()
            .await
            .map_err(|_| AppError::Validation("Invalid Copilot token response".into()))?;
        let expires = DateTime::from_timestamp(token.expires_at, 0)
            .filter(|expires| *expires > Utc::now() + chrono::Duration::seconds(60))
            .ok_or_else(|| AppError::Validation("Copilot returned an expired token".into()))?;
        if token.token.is_empty() {
            return Err(AppError::Validation(
                "Copilot authentication rejected".into(),
            ));
        }
        Ok(ResolvedSecret {
            credentials: rig_core::providers::copilot::CopilotAuth::ApiKey(token.token),
            expires_at: Some(expires),
            cache: CachePolicy::Until(expires - chrono::Duration::seconds(30)),
        })
    }
}

#[async_trait::async_trait]
impl ManagedIntegration for CopilotIntegration {
    type Credentials = rig_core::providers::copilot::CopilotAuth;

    async fn get_secret(
        &self,
        doc: Value,
        _: &mut SecretContext,
    ) -> Result<ResolvedSecret<rig_core::providers::copilot::CopilotAuth>, AppError> {
        let doc: Document = serde_json::from_value(doc)
            .map_err(|_| AppError::Validation("invalid Copilot credential".into()))?;
        if doc.github_token.is_empty() {
            return Err(AppError::Validation("missing GitHub token".into()));
        }
        self.exchange(&doc.github_token).await
    }
}

impl super::SecretEnv for rig_core::providers::copilot::CopilotAuth {
    fn to_env(&self) -> Result<std::collections::HashMap<String, String>, AppError> {
        match self {
            Self::ApiKey(token) if !token.is_empty() => {
                Ok([("ACCESS_TOKEN".into(), token.clone())].into())
            }
            _ => Err(AppError::Validation(
                "unresolved Copilot authentication".into(),
            )),
        }
    }
}
