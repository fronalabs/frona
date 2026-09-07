use super::provider::*;
use crate::core::error::AppError;
use crate::credential::managed::integration::openai_codex::{
    CLIENT_ID, ID, OpenAiCodexIntegration,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const DEVICE_URL: &str = "https://auth.openai.com/codex/device";

#[derive(Default)]
pub struct OpenAiCodexLogin {
    pub(crate) integration: OpenAiCodexIntegration,
}

fn auth_error() -> AppError {
    AppError::Validation("ChatGPT authentication failed or expired; start a new login".into())
}

#[derive(Deserialize)]
struct DeviceChallenge {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    interval: Option<Value>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    authorization_code: String,
    code_verifier: String,
}

struct DeviceSession {
    driver: OpenAiCodexIntegration,
    device: DeviceChallenge,
    interval: Duration,
    next_poll: tokio::time::Instant,
}

#[async_trait::async_trait]
impl LoginProvider for OpenAiCodexLogin {
    fn id(&self) -> &'static str {
        ID
    }

    async fn start(&self) -> Result<StartedLogin, AppError> {
        let response = self
            .integration
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.integration.auth_endpoint
            ))
            .json(&json!({"client_id":CLIENT_ID}))
            .send()
            .await
            .map_err(|_| auth_error())?;
        if !response.status().is_success() {
            return Err(auth_error());
        }
        let device: DeviceChallenge = response.json().await.map_err(|_| auth_error())?;
        if device.device_auth_id.is_empty() || device.user_code.is_empty() {
            return Err(auth_error());
        }
        let seconds = device
            .interval
            .as_ref()
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(5)
            .clamp(5, 900);
        let interval = Duration::from_secs(seconds);
        Ok(StartedLogin {
            challenge: LoginChallenge::DeviceCode { url:DEVICE_URL.into(), user_code:device.user_code.clone(),
                message:Some("Enable device login in your ChatGPT account or workspace settings. Successful login authenticates the account; model access and quota are checked during inference.".into()) },
            expires_at:Utc::now()+chrono::Duration::minutes(15),
            session:Box::new(DeviceSession {driver:self.integration.clone(),device,interval,next_poll:tokio::time::Instant::now()+interval}),
        })
    }
}

#[async_trait::async_trait]
impl LoginSession for DeviceSession {
    async fn advance(&mut self, completion: Option<&str>) -> Result<LoginProgress, AppError> {
        if completion.is_some() {
            return Ok(LoginProgress::Denied);
        }
        if tokio::time::Instant::now() < self.next_poll {
            return Ok(LoginProgress::Pending);
        }
        self.next_poll = tokio::time::Instant::now() + self.interval;
        let response = self.driver.http.post(format!("{}/api/accounts/deviceauth/token",self.driver.auth_endpoint))
            .json(&json!({"device_auth_id":self.device.device_auth_id,"user_code":self.device.user_code})).send().await.map_err(|_| auth_error())?;
        // Pinned Rig's device contract uses these statuses for pending approval.
        if matches!(response.status().as_u16(), 403 | 404) {
            return Ok(LoginProgress::Pending);
        }
        if !response.status().is_success() {
            return Ok(LoginProgress::Denied);
        }
        let authorization: DeviceAuthorization = response.json().await.map_err(|_| auth_error())?;
        if authorization.authorization_code.is_empty() || authorization.code_verifier.is_empty() {
            return Err(auth_error());
        }
        let payload = self
            .driver
            .exchange(
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &authorization.authorization_code),
                    (
                        "redirect_uri",
                        "https://auth.openai.com/deviceauth/callback",
                    ),
                    ("client_id", CLIENT_ID),
                    ("code_verifier", &authorization.code_verifier),
                ],
                None,
                None,
            )
            .await?;
        Ok(LoginProgress::Authenticated(LoginDocument {
            integration: ID,
            secret: serde_json::to_value(payload).map_err(|_| auth_error())?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[tokio::test]
    async fn device_pending_denial_and_poll_interval_follow_the_protocol() {
        let server = MockServer::start().await;
        let driver = OpenAiCodexIntegration {
            auth_endpoint: server.uri(),
            ..Default::default()
        };
        for (status, pending) in [(403, true), (404, true), (401, false), (400, false)] {
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let mut session = DeviceSession {
                driver: driver.clone(),
                device: DeviceChallenge {
                    device_auth_id: "d".into(),
                    user_code: "u".into(),
                    interval: None,
                },
                interval: Duration::from_secs(5),
                next_poll: tokio::time::Instant::now(),
            };
            let result = session.advance(None).await.unwrap();
            assert_eq!(matches!(result, LoginProgress::Pending), pending);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert!(matches!(
                session.advance(None).await.unwrap(),
                LoginProgress::Pending
            ));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            server.reset().await;
        }
    }
}
