use super::provider::*;
use crate::core::error::AppError;
use crate::credential::managed::integration::copilot::{
    CLIENT_ID, CopilotIntegration, Document, ID,
};
use chrono::Utc;
use serde::Deserialize;

#[derive(Default)]
pub struct CopilotLogin {
    pub(crate) integration: CopilotIntegration,
}

#[derive(Deserialize)]
struct DeviceChallenge {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}

struct DeviceSession {
    driver: CopilotIntegration,
    code: String,
    interval: std::time::Duration,
    next_poll: tokio::time::Instant,
}

#[async_trait::async_trait]
impl LoginProvider for CopilotLogin {
    fn id(&self) -> &'static str {
        ID
    }

    async fn start(&self) -> Result<StartedLogin, AppError> {
        let response = self
            .integration
            .http
            .post(&self.integration.device_url)
            .header("accept", "application/json")
            .form(&[("client_id", CLIENT_ID), ("scope", "read:user")])
            .send()
            .await
            .map_err(|_| AppError::Validation("Copilot device login failed".into()))?;
        if !response.status().is_success() {
            return Err(AppError::Validation(
                "Copilot device login was rejected".into(),
            ));
        }
        let challenge: DeviceChallenge = response
            .json()
            .await
            .map_err(|_| AppError::Validation("Invalid Copilot device challenge".into()))?;
        if challenge.device_code.is_empty()
            || challenge.user_code.is_empty()
            || challenge.verification_uri != "https://github.com/login/device"
            || challenge.expires_in == 0
            || challenge.expires_in > 900
        {
            return Err(AppError::Validation(
                "Invalid Copilot device challenge".into(),
            ));
        }
        let interval =
            std::time::Duration::from_secs(challenge.interval.unwrap_or(5).clamp(5, 900));
        Ok(StartedLogin {challenge:LoginChallenge::DeviceCode {url:challenge.verification_uri,user_code:challenge.user_code,message:Some("Authorize GitHub Copilot, then check login status. Polling respects GitHub's interval.".into())},expires_at:Utc::now()+chrono::Duration::seconds(challenge.expires_in as i64),session:Box::new(DeviceSession {driver:self.integration.clone(),code:challenge.device_code,interval,next_poll:tokio::time::Instant::now()+interval})})
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
        let response = self
            .driver
            .http
            .post(&self.driver.token_url)
            .header("accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", &self.code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|_| AppError::Validation("Copilot device poll failed".into()))?;
        if !response.status().is_success() {
            return Ok(LoginProgress::Denied);
        }
        let result: serde_json::Value = response
            .json()
            .await
            .map_err(|_| AppError::Validation("Invalid Copilot device response".into()))?;
        match result.get("error").and_then(serde_json::Value::as_str) {
            Some("authorization_pending") => Ok(LoginProgress::Pending),
            Some("slow_down") => {
                self.interval = (self.interval + std::time::Duration::from_secs(5))
                    .min(std::time::Duration::from_secs(900));
                self.next_poll = tokio::time::Instant::now() + self.interval;
                Ok(LoginProgress::Pending)
            }
            Some(_) => Ok(LoginProgress::Denied),
            None => {
                let token = result
                    .get("access_token")
                    .and_then(serde_json::Value::as_str)
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| AppError::Validation("Missing GitHub access token".into()))?;
                self.driver.exchange(token).await?;
                Ok(LoginProgress::Authenticated(LoginDocument {
                    integration: ID,
                    secret: serde_json::to_value(Document {
                        github_token: token.into(),
                    })
                    .map_err(|_| AppError::Internal("cannot encode Copilot credential".into()))?,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[tokio::test]
    async fn device_polling_pending_slowdown_denial_and_token_exchange_follow_github_contract() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/device")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"device_code":"private-device-code","user_code":"PUBLIC-CODE","verification_uri":"https://github.com/login/device","interval":5,"expires_in":900}))).mount(&server).await;
        let driver = CopilotIntegration {
            exchange_url: format!("{}/exchange", server.uri()),
            device_url: format!("{}/device", server.uri()),
            token_url: format!("{}/token", server.uri()),
            ..Default::default()
        };
        let started = CopilotLogin {
            integration: driver.clone(),
        }
        .start()
        .await
        .unwrap();
        assert!(matches!(
            started.challenge,
            LoginChallenge::DeviceCode { .. }
        ));
        let mut session = DeviceSession {
            driver: driver.clone(),
            code: "private-device-code".into(),
            interval: std::time::Duration::from_secs(5),
            next_poll: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        assert!(matches!(
            session.advance(None).await.unwrap(),
            LoginProgress::Pending
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        for (body, denied) in [
            (json!({"error":"authorization_pending"}), false),
            (json!({"error":"slow_down"}), false),
            (json!({"error":"access_denied"}), true),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            session.next_poll = tokio::time::Instant::now();
            assert_eq!(
                matches!(session.advance(None).await.unwrap(), LoginProgress::Denied),
                denied
            );
        }
        assert_eq!(session.interval, std::time::Duration::from_secs(10));
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"fixture-github-token"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/exchange")).and(header("authorization","token fixture-github-token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":"fixture-chat-token","expires_at":(Utc::now()+chrono::Duration::hours(1)).timestamp()}))).mount(&server).await;
        session.next_poll = tokio::time::Instant::now();
        assert!(matches!(
            session.advance(None).await.unwrap(),
            LoginProgress::Authenticated(LoginDocument {
                integration: ID,
                ..
            })
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let form = String::from_utf8(requests[0].body.to_vec()).unwrap();
        assert!(form.contains("device_code=private-device-code"));
        assert!(form.contains(CLIENT_ID));
    }
}
