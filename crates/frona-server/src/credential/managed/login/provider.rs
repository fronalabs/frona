use crate::core::error::AppError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginChallenge {
    Redirect {
        url: String,
        message: Option<String>,
    },
    DeviceCode {
        url: String,
        user_code: String,
        message: Option<String>,
    },
}

pub struct LoginDocument {
    pub integration: &'static str,
    pub secret: Value,
}

pub enum LoginProgress {
    Pending,
    Denied,
    Authenticated(LoginDocument),
}

#[async_trait::async_trait]
pub trait LoginSession: Send + Sync {
    async fn advance(&mut self, completion: Option<&str>) -> Result<LoginProgress, AppError>;
}

pub struct StartedLogin {
    pub challenge: LoginChallenge,
    pub expires_at: DateTime<Utc>,
    pub session: Box<dyn LoginSession>,
}

#[async_trait::async_trait]
pub trait LoginProvider: Send + Sync {
    fn id(&self) -> &'static str;

    async fn start(&self) -> Result<StartedLogin, AppError>;
}
