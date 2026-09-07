use super::{CachePolicy, ManagedIntegration, ResolvedSecret, SecretContext};
use crate::core::error::AppError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub const ID: &str = "openai_codex";
pub(crate) const AUTH_ENDPOINT: &str = "https://auth.openai.com";
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone, Serialize, Deserialize)]
pub struct Document {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub account_id: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Clone)]
pub struct OpenAiCodexIntegration {
    pub(crate) http: reqwest::Client,
    pub(crate) auth_endpoint: String,
}

impl Default for OpenAiCodexIntegration {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .expect("HTTP client"),
            auth_endpoint: AUTH_ENDPOINT.into(),
        }
    }
}

fn auth_error() -> AppError {
    AppError::Validation("ChatGPT authentication failed or expired; start a new login".into())
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}
// Claims are read only from token responses obtained from the fixed TLS issuer.
// Parsing a JWT is not authentication and is never exposed as a validation API.
fn claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn account(claims: &Value) -> Option<String> {
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

impl OpenAiCodexIntegration {
    pub(crate) async fn exchange(
        &self,
        form: &[(&str, &str)],
        previous_refresh: Option<&str>,
        previous_account: Option<&str>,
    ) -> Result<Document, AppError> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.auth_endpoint))
            .form(form)
            .send()
            .await
            .map_err(|_| auth_error())?;
        if !response.status().is_success() {
            return Err(auth_error());
        }
        let tokens: Tokens = response.json().await.map_err(|_| auth_error())?;
        let access_claims = claims(&tokens.access_token);
        let expires_at = access_claims["exp"]
            .as_i64()
            .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
            .or_else(|| {
                tokens
                    .expires_in
                    .filter(|seconds| *seconds > 0 && *seconds <= 31_536_000)
                    .map(|seconds| Utc::now() + chrono::Duration::seconds(seconds))
            })
            .filter(|expires| *expires > Utc::now() + chrono::Duration::seconds(60))
            .ok_or_else(auth_error)?;
        let account_id = tokens
            .id_token
            .as_deref()
            .and_then(|token| account(&claims(token)))
            .or_else(|| account(&access_claims))
            .or_else(|| previous_account.map(str::to_owned));
        if previous_account.is_some_and(|previous| account_id.as_deref() != Some(previous)) {
            return Err(auth_error());
        }
        let refresh_token = tokens
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_owned))
            .filter(|token| !token.is_empty())
            .ok_or_else(auth_error)?;
        if tokens.access_token.is_empty() || account_id.is_none() {
            return Err(auth_error());
        }
        Ok(Document {
            access_token: tokens.access_token,
            refresh_token: Some(refresh_token),
            expires_at: Some(expires_at),
            account_id,
            scopes: vec![],
        })
    }
}

impl OpenAiCodexIntegration {
    pub(crate) async fn refresh(&self, payload: &Document) -> Result<Option<Document>, AppError> {
        let Document {
            refresh_token: Some(refresh),
            expires_at: Some(expires),
            account_id,
            ..
        } = payload
        else {
            return Err(auth_error());
        };
        if *expires > Utc::now() + chrono::Duration::seconds(60) {
            return Ok(None);
        }
        self.exchange(
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("scope", "openid profile email"),
            ],
            Some(refresh),
            account_id.as_deref(),
        )
        .await
        .map(Some)
    }
}

#[async_trait::async_trait]
impl ManagedIntegration for OpenAiCodexIntegration {
    type Credentials = rig_core::providers::chatgpt::ChatGPTAuth;

    async fn get_secret(
        &self,
        doc: Value,
        ctx: &mut SecretContext,
    ) -> Result<ResolvedSecret<Self::Credentials>, AppError> {
        let doc: Document = serde_json::from_value(doc).map_err(|_| auth_error())?;
        let refreshed = self.refresh(&doc).await?;
        let current = refreshed.as_ref().unwrap_or(&doc);
        if current.refresh_token != doc.refresh_token
            || current.account_id != doc.account_id
            || current.scopes != doc.scopes
        {
            ctx.update_secret(serde_json::to_value(current).map_err(|_| auth_error())?)
                .await?;
        }
        current.resolved()
    }
}

impl super::SecretEnv for rig_core::providers::chatgpt::ChatGPTAuth {
    fn to_env(&self) -> Result<std::collections::HashMap<String, String>, AppError> {
        match self {
            Self::AccessToken {
                access_token,
                account_id: Some(account_id),
            } if !access_token.is_empty() && !account_id.is_empty() => Ok([
                ("ACCESS_TOKEN".into(), access_token.clone()),
                ("ACCOUNT_ID".into(), account_id.clone()),
            ]
            .into()),
            _ => Err(auth_error()),
        }
    }
}

impl Document {
    pub(crate) fn resolved(
        &self,
    ) -> Result<ResolvedSecret<rig_core::providers::chatgpt::ChatGPTAuth>, AppError> {
        let current = self;
        let expiry = current
            .expires_at
            .filter(|expiry| *expiry > Utc::now())
            .ok_or_else(auth_error)?;
        let account = current
            .account_id
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or_else(auth_error)?;
        if current.access_token.is_empty() {
            return Err(auth_error());
        }
        let early = expiry - chrono::Duration::seconds(30);
        Ok(ResolvedSecret {
            credentials: rig_core::providers::chatgpt::ChatGPTAuth::AccessToken {
                access_token: current.access_token.clone(),
                account_id: Some(account.clone()),
            },
            expires_at: Some(expiry),
            cache: CachePolicy::Until(if early > Utc::now() { early } else { expiry }),
        })
    }
}
