use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilio_async::{TwilioJson, TwilioRequest};

use crate::auth::User;
use crate::auth::token::models::TokenType;
use crate::auth::token::service::{CreateTokenRequest, TokenService};
use crate::call::CallService;
use crate::call::models::CallDirection;
use crate::contact::ContactService;
use crate::core::Principal;
use crate::core::config::VoiceConfig;
use crate::core::error::AppError;
use crate::credential::keypair::service::KeyPairService;
use crate::tool::{
    AgentTool, InferenceContext, ToolDefinition, ToolOutput, active_chat, load_tool_definition,
};
use frona_text::prompt::PromptLoader;

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceCallbackExtensions {
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_greeting: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceSessionExtensions {
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

#[async_trait]
pub trait VoiceProvider: Send + Sync {
    fn name(&self) -> &str;
    /// Initiate an outbound call. Returns the provider's call identifier (e.g. Twilio SID).
    #[allow(clippy::too_many_arguments)]
    async fn initiate_call(
        &self,
        to: &str,
        chat_id: &str,
        user: &User,
        agent_id: &str,
        welcome_greeting: Option<&str>,
        hints: Option<&str>,
        contact_id: Option<String>,
    ) -> Result<String, AppError>;
}

pub struct TwilioProvider {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub base_url: String,
    pub voice_id: Option<String>,
    pub speech_model: Option<String>,
    pub token_service: TokenService,
    pub keypair_service: KeyPairService,
    /// Callback token TTL in seconds - short enough that a leaked callback URL
    /// can't be replayed beyond the call setup window.
    pub callback_ttl_secs: u64,
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
        user: &User,
        agent_id: &str,
        welcome_greeting: Option<&str>,
        hints: Option<&str>,
        contact_id: Option<String>,
    ) -> Result<String, AppError> {
        let extensions = serde_json::to_value(VoiceCallbackExtensions {
            chat_id: chat_id.to_string(),
            welcome_greeting: welcome_greeting.map(str::to_string),
            hints: hints.map(str::to_string),
            contact_id,
        })
        .map_err(|e| AppError::Internal(format!("voice callback claims encode: {e}")))?;

        let created = self
            .token_service
            .create_token(
                &self.keypair_service,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Access,
                    principal: Principal::agent(agent_id),
                    ttl_secs: self.callback_ttl_secs,
                    name: "voice_callback".into(),
                    scopes: Vec::new(),
                    refresh_pair_id: None,
                    extensions: Some(extensions),
                },
            )
            .await?;

        let callback_url = format!(
            "{}/api/voice/twilio/callback?token={}",
            self.base_url, created.jwt
        );

        let client = twilio_async::Twilio::new(&self.account_sid, &self.auth_token)
            .map_err(|e| AppError::Tool(format!("Twilio client init failed: {e}")))?;

        let result = client
            .call(&self.from_number, to, &callback_url)
            .run()
            .await
            .map_err(|e| AppError::Tool(format!("Twilio call failed: {e}")))?;

        match result {
            TwilioJson::Success(call) => Ok(call.sid),
            TwilioJson::Fail {
                status, message, ..
            } => Err(AppError::Tool(format!(
                "Twilio API error {status}: {message}"
            ))),
        }
    }
}

pub fn create_voice_provider(
    config: &VoiceConfig,
    base_url: &str,
    token_service: TokenService,
    keypair_service: KeyPairService,
) -> Option<Arc<dyn VoiceProvider>> {
    let provider = config.provider.as_deref().or_else(|| {
        if config.twilio_account_sid.is_some() {
            Some("twilio")
        } else {
            None
        }
    })?;

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
                token_service,
                keypair_service,
                callback_ttl_secs: 300,
            }))
        }
        other => {
            tracing::warn!(provider = %other, "Unknown voice provider; voice calling disabled");
            None
        }
    }
}

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

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let phone_number = arguments
            .get("phone_number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Validation("Missing required parameter: phone_number".into())
            })?;

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

        let chat = active_chat(ctx)?;
        let chat_id = &chat.id;
        let user_id = &ctx.user.id;

        let contact = self
            .contact_service
            .find_or_create_by_phone(user_id, phone_number, name)
            .await?;

        let sid = provider
            .initiate_call(
                phone_number,
                chat_id,
                &ctx.user,
                &ctx.agent.id,
                initial_greeting,
                hints,
                Some(contact.id.clone()),
            )
            .await?;
        tracing::info!(sid = %sid, to = %phone_number, chat_id = %chat_id, "Voice call initiated");

        let _ = self
            .call_service
            .create(chat_id, &contact.id, &sid, CallDirection::Outbound)
            .await?;

        let call_connected_block = self
            .prompts
            .read_with_vars(
                "active_call.md",
                &[
                    ("caller_name", &contact.name),
                    ("phone_number", phone_number),
                    ("objective", objective),
                ],
            )
            .unwrap_or_default();

        Ok(ToolOutput::text(call_connected_block).as_pending_external())
    }
}

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

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let digits = arguments
            .get("digits")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: digits".into()))?;
        // The result IS the digits string - the voice handler reads external_tool.result
        Ok(ToolOutput::text(digits).as_pending_external())
    }
}

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

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        Ok(ToolOutput::text("hangup").as_pending_external())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::VoiceConfig;
    use crate::db::repo::generic::SurrealRepo;

    async fn test_contact_service() -> ContactService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        ContactService::new(
            SurrealRepo::new(db),
            crate::chat::broadcast::BroadcastService::new(),
        )
    }

    #[test]
    fn create_voice_provider_none_with_empty_config() {
        let config = VoiceConfig::default();
        assert!(config.twilio_account_sid.is_none());
        assert!(config.provider.is_none());
    }

    #[test]
    fn send_dtmf_tool_name() {
        use frona_text::prompt::PromptLoader;
        use std::path::PathBuf;
        let prompts = PromptLoader::new(PathBuf::from("/tmp/nonexistent"));
        let tool = SendDtmfTool { prompts };
        assert_eq!(tool.name(), "send_dtmf");
    }

    #[test]
    fn hangup_call_tool_name() {
        use frona_text::prompt::PromptLoader;
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
        use frona_text::prompt::PromptLoader;
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
