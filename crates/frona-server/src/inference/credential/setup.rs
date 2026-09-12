//! Maps provider setup to the independent managed login interface.

use crate::core::error::AppError;
use crate::credential::managed::login::service::LoginAttempt as ManagedAttempt;
pub use crate::credential::managed::login::service::LoginStatus;
use crate::inference::{
    credential::store::CredentialMethod, provider::platform::ResolvedConnection,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) fn provider(
    connection: &ResolvedConnection,
    method: CredentialMethod,
) -> Option<&'static str> {
    match (
        connection.brand.as_str(),
        method,
        connection.effective_base_url.as_deref(),
    ) {
        ("openai", CredentialMethod::Oauth, Some("https://api.openai.com/v1")) => {
            Some("openai_codex")
        }
        ("github-copilot", CredentialMethod::Oauth, Some("https://api.githubcopilot.com")) => {
            Some("copilot")
        }
        ("openrouter", CredentialMethod::ApiKey, Some("https://openrouter.ai/api/v1")) => {
            Some("openrouter")
        }
        _ => None,
    }
}

#[derive(Serialize)]
pub struct LoginAttempt {
    pub id: Uuid,
    pub status: LoginStatus,
    pub challenge: Option<crate::credential::managed::login::provider::LoginChallenge>,
    pub credential_id: Option<Uuid>,
    pub error: Option<&'static str>,
}

impl TryFrom<ManagedAttempt> for LoginAttempt {
    type Error = AppError;

    fn try_from(attempt: ManagedAttempt) -> Result<Self, AppError> {
        Ok(Self {
            id: attempt.id,
            status: attempt.status,
            challenge: attempt.challenge,
            credential_id: attempt.credential_id,
            error: attempt.error,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginCompletion {
    pub code: String,
}
