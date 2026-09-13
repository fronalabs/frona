use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::Principal;
use crate::core::error::AppError;
use crate::credential::vault::models::{BindingScope, GrantDuration};
use crate::credential::vault::service::VaultService;
use crate::inference::hitl::{Hitl, HitlOutcome, HitlRequest, HitlResponse, VaultGrant};
use crate::inference::tool_call::ToolStatus;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput, active_chat};

pub struct RequestCredentialsTool {
    vault_service: VaultService,
    prompts: PromptLoader,
    public_base_url: String,
}

impl RequestCredentialsTool {
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

    fn scope_for(
        grant_duration: &GrantDuration,
        chat_id: &str,
    ) -> (BindingScope, Option<chrono::DateTime<chrono::Utc>>) {
        match grant_duration {
            GrantDuration::Once => (
                BindingScope::Chat {
                    chat_id: chat_id.to_string(),
                },
                None,
            ),
            GrantDuration::Hours(h) => (
                BindingScope::Durable,
                Some(chrono::Utc::now() + chrono::Duration::hours(*h as i64)),
            ),
            GrantDuration::Days(d) => (
                BindingScope::Durable,
                Some(chrono::Utc::now() + chrono::Duration::days(*d as i64)),
            ),
            GrantDuration::Permanent => (BindingScope::Durable, None),
        }
    }
}

#[agent_tool]
impl RequestCredentialsTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: query".into()))?
            .to_string();

        let reason = arguments
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Credential access requested")
            .to_string();

        let force = arguments
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let principal = Principal::agent(ctx.agent.id.clone());
        if !force
            && let Some(binding) = self
                .vault_service
                .find_binding(&ctx.user.id, &principal, &query, Some(&chat.id))
                .await?
        {
            let secret = self
                .vault_service
                .resolve_binding(&ctx.user.id, &principal, &binding, Some(&chat.id))
                .await?;

            self.vault_service
                .log_access(
                    &ctx.user.id,
                    principal.clone(),
                    &chat.id,
                    &binding.connection_id,
                    &binding.vault_item_id,
                    None,
                    &query,
                    &reason,
                )
                .await?;

            let env_vars =
                crate::credential::vault::service::project_target(&secret, &binding.target)?;
            let var_names: Vec<String> = env_vars.iter().map(|(k, _)| k.clone()).collect();

            return Ok(ToolOutput::text(format!(
                "Credentials approved for command startup: {}. Each command resolves current values.",
                var_names.join(", ")
            )));
        }

        Ok(ToolOutput::text("").with_hitl(Hitl {
            prompt: format!("Allow access to credential matching '{query}'?\n\n{reason}"),
            url: format!("{}/chat?id={}", self.public_base_url, chat.id),
            request: HitlRequest::Credential { query, reason },
            status: ToolStatus::Pending,
            response: None,
            delivery: None,
        }))
    }

    async fn on_resume(
        &self,
        _tool_name: &str,
        request: &HitlRequest,
        response: HitlResponse,
        ctx: &InferenceContext,
    ) -> Result<HitlOutcome, AppError> {
        let chat = active_chat(ctx)?;
        let HitlRequest::Credential { query, reason } = request else {
            return Err(AppError::Validation(
                "request_credentials on_resume: expected Credential request".into(),
            ));
        };

        match response {
            HitlResponse::Vault(VaultGrant::Granted {
                connection_id,
                vault_item_id,
                grant_duration,
                target,
            }) => {
                let principal = Principal::agent(ctx.agent.id.clone());

                let secret = self
                    .vault_service
                    .get_secret(&ctx.user.id, &connection_id, &vault_item_id)
                    .await?;

                if !matches!(grant_duration, GrantDuration::Once) {
                    self.vault_service
                        .create_grant(
                            &ctx.user.id,
                            principal.clone(),
                            &connection_id,
                            &vault_item_id,
                            query,
                            &grant_duration,
                        )
                        .await?;
                }

                let (scope, expires_at) = Self::scope_for(&grant_duration, &chat.id);

                self.vault_service
                    .create_binding(
                        &ctx.user.id,
                        principal.clone(),
                        query,
                        &connection_id,
                        &vault_item_id,
                        target.clone(),
                        scope,
                        expires_at,
                    )
                    .await?;

                self.vault_service
                    .log_access(
                        &ctx.user.id,
                        principal,
                        &chat.id,
                        &connection_id,
                        &vault_item_id,
                        None,
                        query,
                        reason,
                    )
                    .await?;

                let env_vars = crate::credential::vault::service::project_target(&secret, &target)?;
                let var_names: Vec<String> = env_vars.iter().map(|(k, _)| k.clone()).collect();

                Ok(HitlOutcome::Resolved(format!(
                    "Credentials approved for command startup: {}. Each command resolves current values.",
                    var_names.join(", "),
                )))
            }
            HitlResponse::Vault(VaultGrant::Denied) => Ok(HitlOutcome::Denied(format!(
                "User denied access to credentials for: {query}.",
            ))),
            _ => Err(AppError::Validation(
                "request_credentials on_resume: expected Vault response".into(),
            )),
        }
    }
}
