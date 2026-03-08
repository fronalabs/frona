use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::chat::message::models::{MessageTool, ToolStatus};
use crate::core::error::AppError;
use crate::credential::vault::service::VaultService;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct RequestCredentialsTool {
    vault_service: VaultService,
    prompts: PromptLoader,
}

impl RequestCredentialsTool {
    pub fn new(vault_service: VaultService, prompts: PromptLoader) -> Self {
        Self {
            vault_service,
            prompts,
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

        let env_var_prefix = arguments
            .get("env_var_prefix")
            .and_then(|v| v.as_str())
            .map(String::from);

        let force = arguments
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !force
            && let Some(grant) = self
                .vault_service
                .find_matching_grant(
                    &ctx.user.id,
                    &ctx.agent.id,
                    &query,
                    env_var_prefix.as_deref(),
                )
                .await?
        {
            let secret = self
                .vault_service
                .get_secret(&ctx.user.id, &grant.connection_id, &grant.vault_item_id)
                .await?;

            self.vault_service
                .log_access(
                    &ctx.user.id,
                    &ctx.agent.id,
                    &ctx.chat.id,
                    &grant.connection_id,
                    &grant.vault_item_id,
                    env_var_prefix.as_deref(),
                    &query,
                    &reason,
                )
                .await?;

            if let Some(ref prefix) = env_var_prefix {
                let env_vars = secret.to_env_vars(prefix);
                let var_names: Vec<String> =
                    env_vars.iter().map(|(k, _)| k.clone()).collect();

                let mut vault_vars = ctx.vault_env_vars.write().await;
                vault_vars.extend(env_vars);

                return Ok(ToolOutput::text(format!(
                    "Credentials loaded into environment variables: {}. Use these in CLI commands.",
                    var_names.join(", ")
                )));
            }

            let mut parts = Vec::new();
            parts.push(format!("Credentials for: {}", secret.name));
            if let Some(ref u) = secret.username {
                parts.push(format!("Username: {u}"));
            }
            if let Some(ref p) = secret.password {
                parts.push(format!("Password: {p}"));
            }
            for (k, v) in &secret.fields {
                parts.push(format!("{k}: {v}"));
            }
            return Ok(ToolOutput::text(parts.join("\n")));
        }

        if !force
            && let Some(access) = self
                .vault_service
                .find_existing_access(&ctx.chat.id, &query, env_var_prefix.as_deref())
                .await?
        {
            let secret = self
                .vault_service
                .get_secret(&ctx.user.id, &access.connection_id, &access.vault_item_id)
                .await?;

            if let Some(ref prefix) = env_var_prefix {
                let env_vars = secret.to_env_vars(prefix);
                let var_names: Vec<String> =
                    env_vars.iter().map(|(k, _)| k.clone()).collect();

                let mut vault_vars = ctx.vault_env_vars.write().await;
                vault_vars.extend(env_vars);

                return Ok(ToolOutput::text(format!(
                    "Credentials already loaded into environment variables: {}. Use these in CLI commands.",
                    var_names.join(", ")
                )));
            }

            let mut parts = Vec::new();
            parts.push(format!("Credentials for: {}", secret.name));
            if let Some(ref u) = secret.username {
                parts.push(format!("Username: {u}"));
            }
            if let Some(ref p) = secret.password {
                parts.push(format!("Password: {p}"));
            }
            for (k, v) in &secret.fields {
                parts.push(format!("{k}: {v}"));
            }
            return Ok(ToolOutput::text(parts.join("\n")));
        }

        let json = serde_json::json!({
            "tool_type": "VaultApproval",
            "query": query,
            "reason": reason,
            "env_var_prefix": env_var_prefix,
        });

        Ok(ToolOutput::text(json.to_string())
            .with_tool_data(MessageTool::VaultApproval {
                query,
                reason,
                env_var_prefix,
                status: ToolStatus::Pending,
                response: None,
            })
            .as_pending_external())
    }
}
