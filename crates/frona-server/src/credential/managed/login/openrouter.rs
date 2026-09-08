use super::provider::*;
use crate::core::error::AppError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

const API: &str = "https://openrouter.ai/api/v1";
const MAX_BODY: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct OpenRouterLogin {
    http: reqwest::Client,
    // Fixed in production. Tests substitute a local server without widening acceptance.
    api: String,
}

impl Default for OpenRouterLogin {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .expect("OpenRouter HTTP client"),
            api: API.into(),
        }
    }
}

fn failed() -> AppError {
    AppError::Validation("OpenRouter connection failed; start a new login".into())
}

async fn read_json(mut response: reqwest::Response) -> Result<Value, AppError> {
    if !response.status().is_success() {
        return Err(failed());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(failed());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| failed())
}

#[async_trait::async_trait]
impl LoginProvider for OpenRouterLogin {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    async fn start(&self) -> Result<StartedLogin, AppError> {
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse("https://openrouter.ai/auth").expect("fixed URL");
        url.query_pairs_mut()
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("key_label", "Frona");
        Ok(StartedLogin {
            challenge: LoginChallenge::Redirect {
                url: url.into(),
                message: Some("Authorize Frona in OpenRouter, then paste the displayed authorization code here. This creates an API key billed to your OpenRouter account, not subscription access.".into()),
            },
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            session: Box::new(Session { driver: self.clone(), verifier: Some(verifier) }),
        })
    }
}

struct Session {
    driver: OpenRouterLogin,
    verifier: Option<String>,
}

#[async_trait::async_trait]
impl LoginSession for Session {
    async fn advance(&mut self, completion: Option<&str>) -> Result<LoginProgress, AppError> {
        let Some(code) = completion else {
            return Ok(LoginProgress::Pending);
        };
        let verifier = self.verifier.take().ok_or_else(failed)?;
        let code = code.trim();
        if code.is_empty() || code.len() > 4096 || !code.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(failed());
        }
        let response = self
            .driver
            .http
            .post(format!("{}/auth/keys", self.driver.api))
            .json(&json!({"code":code,"code_verifier":verifier,"code_challenge_method":"S256"}))
            .send()
            .await
            .map_err(|_| failed())?;
        let data = read_json(response).await?;
        let key = data
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| {
                !key.is_empty() && key.len() <= 4096 && key.bytes().all(|b| b.is_ascii_graphic())
            })
            .ok_or_else(failed)?;
        Ok(LoginProgress::Authenticated(LoginDocument {
            integration: crate::credential::managed::integration::static_secret::ID,
            secret: json!({"API_KEY": key}),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn driver(server: &MockServer) -> OpenRouterLogin {
        OpenRouterLogin {
            api: server.uri(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pkce_is_private_unique_and_exchanged_once_for_an_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/keys"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"key":"fixture-key"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{}})))
            .mount(&server)
            .await;
        let driver = driver(&server);
        let mut started = driver.start().await.unwrap();
        let LoginChallenge::Redirect { url, .. } = &started.challenge else {
            panic!("redirect required")
        };
        let url = reqwest::Url::parse(url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("openrouter.ai"));
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query.contains_key("callback_url"));
        assert!(!query.contains_key("code_verifier"));
        let second = driver.start().await.unwrap();
        assert_ne!(
            serde_json::to_value(&started.challenge).unwrap(),
            serde_json::to_value(second.challenge).unwrap()
        );
        assert!(matches!(
            started.session.advance(None).await.unwrap(),
            LoginProgress::Pending
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(
            matches!(started.session.advance(Some("single-use-code")).await.unwrap(), LoginProgress::Authenticated(LoginDocument { secret, .. }) if secret["API_KEY"] == "fixture-key")
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "successful exchange needs no authentication probe"
        );
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let verifier = body["code_verifier"].as_str().unwrap();
        assert_eq!(verifier.len(), 43);
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        assert_eq!(body["code"], "single-use-code");
        assert_eq!(body["code_challenge_method"], "S256");
        assert!(
            started
                .session
                .advance(Some("single-use-code"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn exchange_rejects_errors_redirects_invalid_and_oversized_keys_without_leaking() {
        for (status, body) in [
            (400, json!({"error":"private-token"})),
            (302, json!({"key":"private-token"})),
            (200, json!({"key":""})),
            (200, json!({"key":"bad\nkey"})),
            (200, json!({"key":"x".repeat(MAX_BODY)})),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(body)
                        .insert_header("Location", format!("{}/redirect", server.uri())),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut started = driver(&server).start().await.unwrap();
            let error = started.session.advance(Some("code")).await.err().unwrap();
            assert!(!error.to_string().contains("private-token"));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}
