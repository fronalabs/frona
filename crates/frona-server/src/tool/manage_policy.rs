use serde_json::Value;

use crate::core::error::AppError;
use crate::policy::schema::prepend_annotations;
use crate::policy::service::PolicyService;
use frona_text::prompt::PromptLoader;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct ManagePolicyTool {
    policy_service: PolicyService,
    prompts: PromptLoader,
}

impl ManagePolicyTool {
    pub fn new(policy_service: PolicyService, prompts: PromptLoader) -> Self {
        Self {
            policy_service,
            prompts,
        }
    }
}

#[agent_tool]
impl ManagePolicyTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: action".into()))?;

        match action {
            "schema" => Ok(Self::handle_schema()),
            "create" => self.handle_create(&arguments, ctx).await,
            "update" => self.handle_update(&arguments, ctx).await,
            "delete" => self.handle_delete(&arguments, ctx).await,
            "list" => self.handle_list(ctx).await,
            "validate" => self.handle_validate(&arguments),
            _ => Err(AppError::Validation(format!("Unknown action: {action}"))),
        }
    }
}

impl ManagePolicyTool {
    async fn handle_create(
        &self,
        arguments: &Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: id".into()))?;

        let description = arguments
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let policy_text = arguments
            .get("policy_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Validation("Missing required parameter: policy_text".into())
            })?;

        let annotated = prepend_annotations(id, description, policy_text);

        let warnings = self
            .policy_service
            .validate_policy_entities(&ctx.user.id, &annotated)
            .await;
        match warnings {
            Ok(w) if !w.is_empty() => {
                return Ok(ToolOutput::error(format!(
                    "Policy validation warnings:\n{}",
                    w.join("\n")
                )));
            }
            Err(e) => return Ok(ToolOutput::error(format!("Validation failed: {e}"))),
            _ => {}
        }

        match self
            .policy_service
            .create_policy(&ctx.user.id, &annotated)
            .await
        {
            Ok(policy) => Ok(ToolOutput::text(format!(
                "Policy '{}' created successfully.",
                policy.name
            ))),
            Err(e) => Ok(ToolOutput::error(format!("Failed to create policy: {e}"))),
        }
    }

    fn check_readonly(&self, id: &str) -> Option<ToolOutput> {
        for policy in self.policy_service.managed_policies() {
            let policy_id: &str = policy.id().as_ref();
            if policy_id == id && policy.annotation("readonly") == Some("true") {
                let config = policy.annotation("config").unwrap_or("unknown");
                return Some(ToolOutput::error(format!(
                    "Policy '{id}' is read-only (managed by server config: {config}). Change the server configuration to modify it."
                )));
            }
        }
        None
    }

    async fn handle_update(
        &self,
        arguments: &Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: id".into()))?;

        if let Some(err) = self.check_readonly(id) {
            return Ok(err);
        }

        let existing = self.policy_service.find_by_name(&ctx.user.id, id).await?;

        let Some(existing) = existing else {
            return Ok(ToolOutput::error(format!(
                "Policy with id '{id}' not found."
            )));
        };

        let description = arguments
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or(&existing.description);

        let policy_text = arguments.get("policy_text").and_then(|v| v.as_str());

        let new_text = if let Some(text) = policy_text {
            prepend_annotations(id, description, text)
        } else {
            prepend_annotations(id, description, &existing.policy_text)
        };

        match self
            .policy_service
            .update_policy(&ctx.user.id, &existing.id, &new_text)
            .await
        {
            Ok(_) => Ok(ToolOutput::text(format!(
                "Policy '{id}' updated successfully."
            ))),
            Err(e) => Ok(ToolOutput::error(format!("Failed to update policy: {e}"))),
        }
    }

    async fn handle_delete(
        &self,
        arguments: &Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: id".into()))?;

        if let Some(err) = self.check_readonly(id) {
            return Ok(err);
        }

        match self
            .policy_service
            .delete_policy_by_name(&ctx.user.id, id)
            .await
        {
            Ok(()) => Ok(ToolOutput::text(format!(
                "Policy '{id}' deleted successfully."
            ))),
            Err(e) => Ok(ToolOutput::error(format!("Failed to delete policy: {e}"))),
        }
    }

    async fn handle_list(&self, ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let managed_policies = self.policy_service.managed_policies();
        let system_policies = self.policy_service.list_system_policies().await?;
        let user_policies = self.policy_service.list_policies(&ctx.user.id).await?;

        if managed_policies.is_empty() && system_policies.is_empty() && user_policies.is_empty() {
            return Ok(ToolOutput::text("No policies found."));
        }

        let mut output = String::new();
        let mut idx = 1;

        if !managed_policies.is_empty() {
            output.push_str(
                "## Managed policies (server-managed by config — cannot be modified)\n\n",
            );
            for policy in &managed_policies {
                let id = policy.id();
                let description = policy.annotation("description").unwrap_or("");
                let config = policy.annotation("config").unwrap_or("");
                output.push_str(&format!(
                    "{idx}. {id} — {description}\n   Config: {config}\n```\n{policy}\n```\n\n",
                ));
                idx += 1;
            }
        }

        if !system_policies.is_empty() {
            output.push_str("## System policies (base defaults)\n\n");
            for policy in &system_policies {
                output.push_str(&format!(
                    "{idx}. {} — {}\n```\n{}\n```\n\n",
                    policy.name, policy.description, policy.policy_text,
                ));
                idx += 1;
            }
        }

        if !user_policies.is_empty() {
            output.push_str("## User policies\n\n");
            for policy in &user_policies {
                output.push_str(&format!(
                    "{idx}. {} — {}\n```\n{}\n```\n\n",
                    policy.name, policy.description, policy.policy_text,
                ));
                idx += 1;
            }
        }

        Ok(ToolOutput::text(output.trim()))
    }

    fn handle_schema() -> ToolOutput {
        ToolOutput::text(include_str!(
            "../../../../resources/policy/frona.cedarschema"
        ))
    }

    fn handle_validate(&self, arguments: &Value) -> Result<ToolOutput, AppError> {
        let policy_text = arguments
            .get("policy_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Validation("Missing required parameter: policy_text".into())
            })?;

        match self.policy_service.validate_policy_text(policy_text) {
            Ok(()) => Ok(ToolOutput::text("Policy syntax is valid.")),
            Err(e) => Ok(ToolOutput::error(format!("Validation failed: {e}"))),
        }
    }
}
