use serde_json::Value;

use crate::core::error::AppError;
use crate::credential::vault::service::VaultService;
use frona_text::prompt::PromptLoader;

use crate::inference::hitl::{Hitl, HitlOutcome, HitlRequest, HitlResponse};
use crate::inference::tool_call::ToolStatus;
use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput, active_chat};

pub struct NotifyHumanTool {
    vault_service: VaultService,
    prompts: PromptLoader,
    public_base_url: String,
}

impl NotifyHumanTool {
    pub fn new(
        vault_service: VaultService,
        prompts: PromptLoader,
        public_base_url: String,
    ) -> Self {
        Self {
            vault_service,
            prompts,
            public_base_url,
        }
    }

    fn url_for_chat(&self, chat_id: &str) -> String {
        format!("{}/chat?id={}", self.public_base_url, chat_id)
    }
}

#[agent_tool(files("ask_user_question", "request_user_takeover"))]
impl NotifyHumanTool {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        match tool_name {
            "request_user_takeover" => {
                let reason = arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("User intervention needed")
                    .to_string();
                let debugger_url = self
                    .vault_service
                    .list_credentials(&ctx.user.id)
                    .await
                    .ok()
                    .and_then(|creds| creds.into_iter().next())
                    .map(|c| format!("/api/browser/debugger/{}", c.id))
                    .unwrap_or_default();

                Ok(ToolOutput::text("").with_hitl(Hitl {
                    prompt: if debugger_url.is_empty() {
                        reason.clone()
                    } else {
                        format!("{reason}\n\nTake over: {debugger_url}")
                    },
                    url: self.url_for_chat(&chat.id),
                    request: HitlRequest::Takeover {
                        reason,
                        debugger_url,
                    },
                    status: ToolStatus::Pending,
                    response: None,
                    delivery: None,
                }))
            }
            "ask_user_question" => {
                let question = arguments
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let options: Vec<String> = arguments
                    .get("options")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                Ok(ToolOutput::text("").with_hitl(Hitl {
                    prompt: question,
                    url: self.url_for_chat(&chat.id),
                    request: HitlRequest::Question { options },
                    status: ToolStatus::Pending,
                    response: None,
                    delivery: None,
                }))
            }
            _ => Err(AppError::Tool(format!(
                "Unknown notify_human sub-tool: {tool_name}"
            ))),
        }
    }

    async fn on_resume(
        &self,
        tool_name: &str,
        _request: &HitlRequest,
        response: HitlResponse,
        _ctx: &InferenceContext,
    ) -> Result<HitlOutcome, AppError> {
        let HitlResponse::Choice(text) = response else {
            return Err(AppError::Validation(format!(
                "notify_human::{tool_name} expected HitlResponse::Choice"
            )));
        };
        match tool_name {
            "ask_user_question" => Ok(HitlOutcome::Resolved(text)),
            "request_user_takeover" => {
                let resolved = if text.is_empty() {
                    "Human completed the takeover.".to_string()
                } else {
                    text
                };
                Ok(HitlOutcome::Resolved(resolved))
            }
            _ => Err(AppError::Tool(format!(
                "Unknown notify_human sub-tool: {tool_name}"
            ))),
        }
    }
}
