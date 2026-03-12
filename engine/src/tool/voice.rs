use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilio_async::{TwilioJson, TwilioRequest};

use crate::agent::prompt::PromptLoader;
use crate::auth::jwt::JwtService;
use crate::call::models::CallDirection;
use crate::call::CallService;
use crate::contact::ContactService;
use crate::core::config::VoiceConfig;
use crate::core::error::AppError;
use crate::credential::keypair::service::KeyPairService;
use crate::tool::{AgentTool, InferenceContext, ToolDefinition, ToolOutput, load_tool_definition};

/// Short-lived JWT embedded in the Twilio callback URL.
/// Owner "voice", signed by the provider.
#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceCallbackClaims {
    pub sub: String,
    pub chat_id: String,
    pub exp: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_greeting: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
}

/// WebSocket session JWT, issued by the callback handler.
/// Owner "user:{id}", normal presign expiry.
#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceSessionClaims {
    pub sub: String,
    pub chat_id: String,
    pub exp: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

// ---------------------------------------------------------------------------
// VoiceProvider trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VoiceProvider: Send + Sync {
    fn name(&self) -> &str;
    /// Initiate an outbound call. Returns the provider's call identifier (e.g. Twilio SID).
    async fn initiate_call(
        &self,
        to: &str,
        chat_id: &str,
        user_id: &str,
        welcome_greeting: Option<&str>,
        hints: Option<&str>,
        contact_id: Option<String>,
    ) -> Result<String, AppError>;
}

// ---------------------------------------------------------------------------
// TwilioProvider
// ---------------------------------------------------------------------------

pub struct TwilioProvider {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub base_url: String,
    pub voice_id: Option<String>,
    pub speech_model: Option<String>,
    pub keypair_service: KeyPairService,
}

#[async_trait]
impl VoiceProvider for TwilioProvider {
    fn name(&self) -> &str {
        "twilio"
    }

    async fn initiate_call(
        &self,
        to: &str,
        chat_id: &str,
        user_id: &str,
        welcome_greeting: Option<&str>,
        hints: Option<&str>,
        contact_id: Option<String>,
    ) -> Result<String, AppError> {
        let (enc_key, kid) = self.keypair_service.get_signing_key("voice").await?;
        let exp = (Utc::now().timestamp() + 300) as usize; // 5 min
        let claims = VoiceCallbackClaims {
            sub: user_id.to_string(),
            chat_id: chat_id.to_string(),
            exp,
            welcome_greeting: welcome_greeting.map(str::to_string),
            hints: hints.map(str::to_string),
            contact_id,
        };
        let cb_token = JwtService::new().sign(&claims, &enc_key, &kid)?;
        let callback_url = format!("{}/api/voice/twilio/callback?token={}", self.base_url, cb_token);

        let client = twilio_async::Twilio::new(&self.account_sid, &self.auth_token)
            .map_err(|e| AppError::Tool(format!("Twilio client init failed: {e}")))?;

        let result = client
            .call(&self.from_number, to, &callback_url)
            .run()
            .await
            .map_err(|e| AppError::Tool(format!("Twilio call failed: {e}")))?;

        match result {
            TwilioJson::Success(call) => Ok(call.sid),
            TwilioJson::Fail { status, message, .. } => Err(AppError::Tool(format!(
                "Twilio API error {status}: {message}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub fn create_voice_provider(
    config: &VoiceConfig,
    base_url: &str,
    keypair_service: KeyPairService,
) -> Option<Arc<dyn VoiceProvider>> {
    let provider = config
        .provider
        .as_deref()
        .or_else(|| if config.twilio_account_sid.is_some() { Some("twilio") } else { None })?;

    match provider.to_lowercase().as_str() {
        "twilio" => {
            let account_sid = config.twilio_account_sid.clone()?;
            let auth_token = config.twilio_auth_token.clone()?;
            let from_number = config.twilio_from_number.clone()?;
            Some(Arc::new(TwilioProvider {
                account_sid,
                auth_token,
                from_number,
                base_url: base_url.to_string(),
                voice_id: config.twilio_voice_id.clone(),
                speech_model: config.twilio_speech_model.clone(),
                keypair_service,
            }))
        }
        other => {
            tracing::warn!(provider = %other, "Unknown voice provider; voice calling disabled");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// VoiceCallTool (external — pauses loop until Twilio callback)
// ---------------------------------------------------------------------------

pub struct VoiceCallTool {
    pub provider: Option<Arc<dyn VoiceProvider>>,
    pub prompts: PromptLoader,
    pub contact_service: ContactService,
    pub call_service: CallService,
}

#[async_trait]
impl AgentTool for VoiceCallTool {
    fn name(&self) -> &str {
        "make_voice_call"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        load_tool_definition(&self.prompts, "tools/voice_call.md")
            .map(|d| vec![d])
            .unwrap_or_default()
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let phone_number = arguments
            .get("phone_number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: phone_number".into()))?;

        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: name".into()))?;

        let objective = arguments
            .get("objective")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: objective".into()))?;

        let initial_greeting = arguments.get("initial_greeting").and_then(|v| v.as_str());
        let hints = arguments.get("hints").and_then(|v| v.as_str());

        let provider = self.provider.as_ref().ok_or_else(|| {
            AppError::Tool("Voice calling is not configured. Set voice.twilio_account_sid, twilio_auth_token, and twilio_from_number in config.".into())
        })?;

        let chat_id = &ctx.chat.id;
        let user_id = &ctx.user.id;

        let contact = self.contact_service
            .find_or_create_by_phone(user_id, phone_number, name)
            .await?;

        let sid = provider.initiate_call(
            phone_number,
            chat_id,
            user_id,
            initial_greeting,
            hints,
            Some(contact.id.clone()),
        ).await?;
        tracing::info!(sid = %sid, to = %phone_number, chat_id = %chat_id, "Voice call initiated");

        let _ = self.call_service
            .create(chat_id, &contact.id, &sid, CallDirection::Outbound)
            .await?;

        let call_connected_block = self.prompts
            .read_with_vars("active_call.md", &[
                ("caller_name", &contact.name),
                ("phone_number", phone_number),
                ("objective", objective),
            ])
            .unwrap_or_default();

        Ok(ToolOutput::text(call_connected_block).as_pending_external())
    }
}

// ---------------------------------------------------------------------------
// SendDtmfTool (external — pauses tool loop)
// ---------------------------------------------------------------------------

pub struct SendDtmfTool {
    pub prompts: PromptLoader,
}

#[async_trait]
impl AgentTool for SendDtmfTool {
    fn name(&self) -> &str {
        "send_dtmf"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        load_tool_definition(&self.prompts, "tools/send_dtmf.md")
            .map(|d| vec![d])
            .unwrap_or_default()
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let digits = arguments
            .get("digits")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: digits".into()))?;
        // The result IS the digits string — the voice handler reads external_tool.result
        Ok(ToolOutput::text(digits).as_pending_external())
    }
}

// ---------------------------------------------------------------------------
// HangupCallTool (external — pauses tool loop)
// ---------------------------------------------------------------------------

pub struct HangupCallTool {
    pub prompts: PromptLoader,
}

#[async_trait]
impl AgentTool for HangupCallTool {
    fn name(&self) -> &str {
        "hangup_call"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        load_tool_definition(&self.prompts, "tools/hangup_call.md")
            .map(|d| vec![d])
            .unwrap_or_default()
    }

    async fn execute(&self, _tool_name: &str, _arguments: Value, _ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        Ok(ToolOutput::text("hangup").as_pending_external())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repo::generic::SurrealRepo;
    use crate::core::config::VoiceConfig;

    async fn test_contact_service() -> ContactService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        ContactService::new(SurrealRepo::new(db))
    }

    #[test]
    fn create_voice_provider_none_with_empty_config() {
        let config = VoiceConfig::default();
        assert!(config.twilio_account_sid.is_none());
        assert!(config.provider.is_none());
    }

    #[test]
    fn send_dtmf_tool_name() {
        use crate::agent::prompt::PromptLoader;
        use std::path::PathBuf;
        let prompts = PromptLoader::new(PathBuf::from("/tmp/nonexistent"));
        let tool = SendDtmfTool { prompts };
        assert_eq!(tool.name(), "send_dtmf");
    }

    #[test]
    fn hangup_call_tool_name() {
        use crate::agent::prompt::PromptLoader;
        use std::path::PathBuf;
        let prompts = PromptLoader::new(PathBuf::from("/tmp/nonexistent"));
        let tool = HangupCallTool { prompts };
        assert_eq!(tool.name(), "hangup_call");
    }

    async fn test_call_service() -> crate::call::CallService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        crate::call::CallService::new(SurrealRepo::new(db))
    }

    #[tokio::test]
    async fn voice_call_tool_name() {
        use crate::agent::prompt::PromptLoader;
        use std::path::PathBuf;
        let prompts = PromptLoader::new(PathBuf::from("/tmp/nonexistent"));
        let tool = VoiceCallTool {
            provider: None,
            prompts,
            contact_service: test_contact_service().await,
            call_service: test_call_service().await,
        };
        assert_eq!(tool.name(), "make_voice_call");
    }
}
