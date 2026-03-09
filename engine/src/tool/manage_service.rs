use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::app::models::{App, AppManifest};
use crate::app::service::AppService;
use crate::chat::message::models::{MessageTool, ToolStatus};
use crate::core::error::AppError;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct ManageServiceTool {
    app_service: AppService,
    prompts: PromptLoader,
}

impl ManageServiceTool {
    pub fn new(app_service: AppService, prompts: PromptLoader) -> Self {
        Self {
            app_service,
            prompts,
        }
    }
}

#[agent_tool]
impl ManageServiceTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: action".into()))?
            .to_string();

        let manifest_value = arguments.get("manifest").cloned();

        match action.as_str() {
            "status" => self.handle_status(ctx, manifest_value).await,
            "deploy" => self.handle_deploy(ctx, manifest_value).await,
            "stop" => self.handle_stop(ctx, manifest_value).await,
            "start" => self.handle_start(ctx, manifest_value).await,
            "restart" => self.handle_restart(ctx, manifest_value).await,
            "destroy" => self.handle_destroy(ctx, manifest_value).await,
            _ => Err(AppError::Validation(format!(
                "Unknown action: {action}. Valid actions: deploy, stop, start, restart, destroy, status"
            ))),
        }
    }
}

impl ManageServiceTool {
    async fn handle_status(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let agent_id = &ctx.agent.id;
        let apps = self.app_service.list(agent_id).await?;

        if let Some(ref mv) = manifest_value
            && let Some(manifest_id) = mv.get("id").and_then(|v| v.as_str())
        {
            if let Some(app) = apps.iter().find(|a| {
                a.manifest
                    .get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id == manifest_id)
            }) {
                return Ok(ToolOutput::text(serde_json::to_string_pretty(app).unwrap_or_default()));
            }
            return Ok(ToolOutput::text(format!(
                "No app found with id '{manifest_id}'"
            )));
        }

        if apps.is_empty() {
            return Ok(ToolOutput::text("No apps deployed for this agent."));
        }
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&apps).unwrap_or_default(),
        ))
    }

    async fn handle_deploy(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let manifest_value = manifest_value
            .ok_or_else(|| AppError::Validation("manifest is required for deploy".into()))?;

        let manifest: AppManifest = serde_json::from_value(manifest_value.clone())
            .map_err(|e| AppError::Validation(format!("Invalid manifest: {e}")))?;

        let existing = self.app_service.find_by_manifest_id(&ctx.agent.id, &manifest.id).await?;

        if check_needs_approval(&existing, &manifest_value) {
            let previous = existing.map(|a| a.manifest);

            return Ok(ToolOutput::text(serde_json::json!({
                "tool_type": "ServiceApproval",
                "action": "deploy",
                "manifest": manifest_value,
            }).to_string())
            .with_tool_data(MessageTool::ServiceApproval {
                action: "deploy".to_string(),
                manifest: manifest_value,
                previous_manifest: previous,
                status: ToolStatus::Pending,
                response: None,
            })
            .as_pending_external());
        }

        let app = self
            .app_service
            .deploy(&ctx.agent.id, &ctx.user.id, &manifest, Vec::new())
            .await?;

        let url_info = app
            .url
            .as_ref()
            .map(|u| format!("\nURL: {u}"))
            .unwrap_or_default();

        Ok(ToolOutput::text(format!(
            "App '{}' deployed successfully. Status: {}{url_info}",
            app.name, app.status
        )))
    }

    async fn handle_stop(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self.app_service.stop(&ctx.agent.id, &app_id).await?;
        Ok(ToolOutput::text(format!(
            "App '{}' stopped. Status: {}",
            app.name, app.status
        )))
    }

    async fn handle_start(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self
            .app_service
            .start(&ctx.agent.id, &app_id, Vec::new())
            .await?;

        let url_info = app
            .url
            .as_ref()
            .map(|u| format!("\nURL: {u}"))
            .unwrap_or_default();

        Ok(ToolOutput::text(format!(
            "App '{}' started. Status: {}{url_info}",
            app.name, app.status
        )))
    }

    async fn handle_restart(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self.app_service.restart(&ctx.agent.id, &app_id).await?;

        let url_info = app
            .url
            .as_ref()
            .map(|u| format!("\nURL: {u}"))
            .unwrap_or_default();

        Ok(ToolOutput::text(format!(
            "App '{}' restarted. Status: {}{url_info}",
            app.name, app.status
        )))
    }

    async fn handle_destroy(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app_name = self
            .app_service
            .get(&app_id)
            .await?
            .map(|a| a.name)
            .unwrap_or_default();

        self.app_service.destroy(&ctx.agent.id, &app_id).await?;

        Ok(ToolOutput::text(format!("App '{app_name}' destroyed.")))
    }

    async fn resolve_app_id(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<&Value>,
    ) -> Result<String, AppError> {
        let manifest_id = manifest_value
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Validation("manifest.id is required to identify the app".into())
            })?;

        self.app_service
            .find_by_manifest_id(&ctx.agent.id, manifest_id)
            .await?
            .map(|a| a.id)
            .ok_or_else(|| {
                AppError::NotFound(format!("No app found with manifest id '{manifest_id}'"))
            })
    }
}

fn check_needs_approval(existing: &Option<App>, manifest_value: &Value) -> bool {
    match existing {
        None => true,
        Some(app) => app.manifest != *manifest_value,
    }
}
