use serde_json::Value;

use crate::app::models::{App, AppManifest, AppResponse};
use crate::app::service::AppService;
use crate::chat::broadcast::BroadcastService;
use crate::core::error::AppError;
use crate::inference::hitl::{Hitl, HitlOutcome, HitlRequest, HitlResponse};
use crate::inference::tool_call::ToolStatus;
use crate::notification::models::{NotificationData, NotificationLevel};
use crate::notification::service::NotificationService;
use crate::storage::StorageService;
use frona_text::prompt::PromptLoader;

use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput, active_chat};

pub struct ManageAppTool {
    app_service: AppService,
    prompts: PromptLoader,
    notification_service: NotificationService,
    broadcast_service: BroadcastService,
    storage_service: StorageService,
    public_base_url: String,
}

impl ManageAppTool {
    pub fn new(
        app_service: AppService,
        prompts: PromptLoader,
        notification_service: NotificationService,
        broadcast_service: BroadcastService,
        storage_service: StorageService,
        public_base_url: String,
    ) -> Self {
        Self {
            app_service,
            prompts,
            notification_service,
            broadcast_service,
            storage_service,
            public_base_url,
        }
    }
}

#[agent_tool]
impl ManageAppTool {
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

    async fn on_resume(
        &self,
        _tool_name: &str,
        request: &HitlRequest,
        response: HitlResponse,
        ctx: &InferenceContext,
    ) -> Result<HitlOutcome, AppError> {
        let chat = active_chat(ctx)?;
        let HitlRequest::App {
            action, manifest, ..
        } = request
        else {
            return Err(AppError::Validation(
                "manage_app on_resume: expected App request".into(),
            ));
        };
        match response {
            HitlResponse::Approval(true) => {
                let manifest_parsed: AppManifest = serde_json::from_value(manifest.clone())
                    .map_err(|e| {
                        AppError::Validation(format!("Invalid persisted manifest: {e}"))
                    })?;
                // Always re-deploy with the approved manifest (handles create AND update).
                // Calling restart() on an existing row would use the stale saved command,
                // which fails when kind or command changed since the previous deploy.
                let app = self
                    .app_service
                    .deploy_and_await(
                        &ctx.agent.id,
                        &ctx.user.id,
                        &chat.id,
                        &manifest_parsed,
                        Vec::new(),
                    )
                    .await?;
                Ok(HitlOutcome::Resolved(self.format_running_result(
                    &format!("{action} completed"),
                    &app,
                )))
            }
            HitlResponse::Approval(false) => {
                let handle = manifest
                    .get("handle")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                Ok(HitlOutcome::Denied(format!(
                    "User denied {action} of '{handle}'."
                )))
            }
            _ => Err(AppError::Validation(
                "manage_app on_resume: expected Approval response".into(),
            )),
        }
    }
}

impl ManageAppTool {
    async fn handle_status(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let agent_id = &ctx.agent.id;
        let apps = self.app_service.list(agent_id).await?;

        if let Some(ref mv) = manifest_value
            && let Some(handle_str) = mv.get("handle").and_then(|v| v.as_str())
        {
            if let Some(app) = apps.iter().find(|a| a.handle.as_str() == handle_str) {
                return Ok(ToolOutput::text(
                    serde_json::to_string_pretty(app).unwrap_or_default(),
                ));
            }
            return Ok(ToolOutput::text(format!(
                "No app found with handle '{handle_str}'"
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
        let chat = active_chat(ctx)?;
        let manifest_value = manifest_value
            .ok_or_else(|| AppError::Validation("manifest is required for deploy".into()))?;

        let manifest: AppManifest = serde_json::from_value(manifest_value.clone())
            .map_err(|e| AppError::Validation(format!("Invalid manifest: {e}. Tip: `handle` is required — a short URL-safe identifier (e.g. \"notes\", \"my-dashboard\"). Apps are served at /apps/{{handle}}/.")))?;

        // Pre-flight validation for static apps - catches manifest mistakes
        // (bad `static_dir`, missing files) BEFORE the human-approval HITL,
        // so the LLM gets immediate feedback to fix and retry.
        if manifest.effective_kind() == "static" {
            validate_static_dir(
                &self.storage_service,
                &ctx.user.handle,
                &ctx.agent.handle,
                &manifest,
            )?;
        }

        let existing = self
            .app_service
            .find_by_user_handle(&ctx.user.id, &manifest.handle)
            .await?;

        let needs_approval = check_needs_approval(&existing, &manifest_value);

        if needs_approval {
            let previous = existing.map(|a| a.manifest);
            let prompt = format!("Deploy `{}`?", manifest.handle);

            return Ok(ToolOutput::text("").with_hitl(Hitl {
                prompt,
                url: format!("{}/chat?id={}", self.public_base_url, chat.id),
                request: HitlRequest::App {
                    action: "deploy".to_string(),
                    manifest: manifest_value,
                    previous_manifest: previous,
                },
                status: ToolStatus::Pending,
                response: None,
                delivery: None,
            }));
        }

        let app = if let Some(ref existing) = existing {
            self.app_service
                .restart(&ctx.agent.id, &existing.id, &chat.id)
                .await?
        } else {
            self.app_service
                .deploy_and_await(&ctx.agent.id, &ctx.user.id, &chat.id, &manifest, Vec::new())
                .await?
        };

        Ok(ToolOutput::text(
            self.format_running_result("deployed successfully", &app),
        ))
    }

    fn format_running_result(&self, action: &str, app: &AppResponse) -> String {
        let mut out = format_app_result(action, app);

        if app.kind == "service"
            && let Some(port) = app.port
        {
            out.push_str(&format!("\nInternal URL: http://localhost:{port}"));
        }

        if let Some(rel) = app.url.as_deref() {
            out.push_str(&format!("\nPublic URL: {}{rel}", self.public_base_url));
        }
        out
    }

    async fn handle_stop(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self
            .app_service
            .stop(&ctx.agent.id, &app_id, &chat.id)
            .await?;
        self.emit_notification(
            ctx,
            &app.handle,
            "stop",
            NotificationLevel::Info,
            &format!("App '{}' stopped", app.name),
        )
        .await;
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
        let chat = active_chat(ctx)?;
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self
            .app_service
            .start(&ctx.agent.id, &app_id, &chat.id, Vec::new())
            .await?;

        self.emit_notification(
            ctx,
            &app.handle,
            "start",
            NotificationLevel::Success,
            &format!("App '{}' started", app.name),
        )
        .await;
        Ok(ToolOutput::text(
            self.format_running_result("started", &app),
        ))
    }

    async fn handle_restart(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<Value>,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let app_id = self.resolve_app_id(ctx, manifest_value.as_ref()).await?;

        let app = self
            .app_service
            .restart(&ctx.agent.id, &app_id, &chat.id)
            .await?;

        self.emit_notification(
            ctx,
            &app.handle,
            "restart",
            NotificationLevel::Info,
            &format!("App '{}' restarted", app.name),
        )
        .await;
        Ok(ToolOutput::text(
            self.format_running_result("restarted", &app),
        ))
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

    async fn emit_notification(
        &self,
        ctx: &InferenceContext,
        app_handle: &str,
        action: &str,
        level: NotificationLevel,
        title: &str,
    ) {
        if let Ok(notification) = self
            .notification_service
            .create(
                &ctx.user.id,
                NotificationData::App {
                    app_handle: app_handle.to_string(),
                    action: action.to_string(),
                },
                level,
                title.to_string(),
                String::new(),
            )
            .await
        {
            self.broadcast_service
                .send_notification(&ctx.user.id, notification);
        }
    }

    async fn resolve_app_id(
        &self,
        ctx: &InferenceContext,
        manifest_value: Option<&Value>,
    ) -> Result<String, AppError> {
        let handle_str = manifest_value
            .and_then(|v| v.get("handle"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Validation("manifest.handle is required to identify the app".into())
            })?;
        let handle = crate::core::Handle::try_new(handle_str)?;

        let app = self
            .app_service
            .find_by_user_handle(&ctx.user.id, &handle)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("No app found with handle '{handle_str}'"))
            })?;

        if app.agent_id != ctx.agent.id {
            return Err(AppError::Forbidden(
                "App is owned by a different agent".into(),
            ));
        }
        Ok(app.id)
    }
}

pub fn format_app_result(action: &str, app: &AppResponse) -> String {
    format!("App '{}' {action}. Status: {}", app.name, app.status)
}

fn check_needs_approval(existing: &Option<App>, manifest_value: &Value) -> bool {
    let Some(app) = existing else {
        return true;
    };

    let (Ok(old), Ok(new)) = (
        serde_json::from_value::<AppManifest>(app.manifest.clone()),
        serde_json::from_value::<AppManifest>(manifest_value.clone()),
    ) else {
        return true;
    };

    old.command != new.command
        || old.effective_kind() != new.effective_kind()
        || old.static_dir != new.static_dir
        || old.effective_expose() != new.effective_expose()
        || old.sandbox_policy != new.sandbox_policy
        || old.credentials != new.credentials
}

/// Validates a static-kind app's `static_dir` against the agent's workspace.
///
/// Catches the common LLM mistakes that surface as 404s at request time:
/// - `static_dir` empty, `.`, `..`, absolute, or containing `..` traversal
/// - `static_dir` resolves outside the agent's workspace
/// - resolved directory doesn't exist
/// - resolved directory contains no `.html` file (proxy serves `index.html`)
///
/// Returns a `Validation` error with a message aimed at the LLM so it can
/// fix the manifest and retry without going through human approval.
fn validate_static_dir(
    storage: &crate::storage::StorageService,
    user_handle: &crate::core::Handle,
    agent_handle: &crate::core::Handle,
    manifest: &AppManifest,
) -> Result<(), AppError> {
    let static_dir = manifest.static_dir.as_deref().ok_or_else(|| {
        AppError::Validation(format!(
            "static_dir is required for static apps. Convention: write your files to `apps/{0}/` and set \"static_dir\": \"apps/{0}\".",
            manifest.handle.as_str(),
        ))
    })?;

    let trimmed = static_dir.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(format!(
            "static_dir is empty. Set it to the subdirectory containing your built files (e.g. \"apps/{}\").",
            manifest.handle.as_str(),
        )));
    }

    let rel = std::path::Path::new(trimmed);

    if rel.is_absolute() {
        return Err(AppError::Validation(format!(
            "static_dir must be a relative path inside the workspace — got absolute path '{static_dir}'."
        )));
    }

    for comp in rel.components() {
        match comp {
            std::path::Component::ParentDir => {
                return Err(AppError::Validation(format!(
                    "static_dir cannot contain '..' (path traversal) — got '{static_dir}'."
                )));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(AppError::Validation(format!(
                    "static_dir must be a relative path inside the workspace — got '{static_dir}'."
                )));
            }
            _ => {}
        }
    }

    // Reject pure "." (resolves to workspace root, which is almost never what
    // the LLM intended for a static app).
    let normalized: std::path::PathBuf = rel
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if normalized.as_os_str().is_empty() {
        return Err(AppError::Validation(format!(
            "static_dir resolves to the workspace root — that's almost never what you want. Did you mean \"apps/{}\"?",
            manifest.handle.as_str(),
        )));
    }

    let workspace = storage.agent_workspace_path(user_handle, agent_handle);
    let resolved = workspace.join(&normalized);

    if !resolved.exists() {
        return Err(AppError::Validation(format!(
            "static_dir '{static_dir}' resolves to {} but no such directory exists. Create the directory and write your files (especially an `index.html`) there first.",
            resolved.display(),
        )));
    }
    if !resolved.is_dir() {
        return Err(AppError::Validation(format!(
            "static_dir '{static_dir}' resolves to {} but that's a file, not a directory.",
            resolved.display(),
        )));
    }

    let has_html = std::fs::read_dir(&resolved)
        .ok()
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
            })
        })
        .unwrap_or(false);
    if !has_html {
        return Err(AppError::Validation(format!(
            "static_dir '{static_dir}' resolves to {} but contains no .html files. The proxy serves `index.html` — write your entry HTML file there first.",
            resolved.display(),
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_app(manifest: serde_json::Value) -> App {
        let now = Utc::now();
        App {
            id: "app-1".to_string(),
            agent_id: "agent-1".to_string(),
            user_id: "user-1".to_string(),
            handle: crate::handle!("test"),
            name: "Test".to_string(),
            description: None,
            kind: "service".to_string(),
            command: Some("python app.py".to_string()),
            static_dir: None,
            port: Some(4000),
            status: crate::app::models::AppStatus::Running,
            pid: Some(1234),
            manifest,
            chat_id: "test-chat".to_string(),
            crash_fix_attempts: 0,
            last_accessed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn approval_required_for_new_app() {
        let manifest = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        assert!(check_needs_approval(&None, &manifest));
    }

    #[test]
    fn no_approval_when_manifest_identical() {
        let manifest = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let app = make_app(manifest.clone());
        assert!(!check_needs_approval(&Some(app), &manifest));
    }

    #[test]
    fn no_approval_when_only_name_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Old Name", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "New Name", "command": "python app.py"});
        let app = make_app(old);
        assert!(!check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn no_approval_when_only_description_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "description": "new desc"});
        let app = make_app(old);
        assert!(!check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn no_approval_when_only_health_check_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "health_check": {"path": "/healthz"}});
        let app = make_app(old);
        assert!(!check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_command_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "node server.js"});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_sandbox_policy_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "sandbox_policy": {"network_destinations": ["evil.com:443"]}});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_credentials_change() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "credentials": [{"query": "api-key", "reason": "need it", "env_var_prefix": "API"}]});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_expose_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "expose": false});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py", "expose": true});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_kind_changes() {
        let old = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "kind": "static", "static_dir": "dist/"});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    #[test]
    fn approval_required_when_stored_manifest_unparseable() {
        let old = serde_json::json!("not a valid manifest");
        let new = serde_json::json!({"id": "test", "handle": "test", "name": "Test", "command": "python app.py"});
        let app = make_app(old);
        assert!(check_needs_approval(&Some(app), &new));
    }

    fn make_static_manifest(static_dir: Option<&str>) -> AppManifest {
        AppManifest {
            handle: crate::handle!("countdown"),
            name: "Countdown".to_string(),
            description: None,
            icon: None,
            kind: Some("static".to_string()),
            command: None,
            restart_policy: None,
            health_check: None,
            resources: None,
            static_dir: static_dir.map(String::from),
            expose: None,
            sandbox_policy: None,
            credentials: None,
            hibernate: None,
        }
    }

    fn test_storage(base: &std::path::Path) -> crate::storage::StorageService {
        let config = crate::core::config::Config {
            storage: crate::core::config::StorageConfig {
                data_dir: base.to_string_lossy().into_owned(),
                shared_config_dir: format!("{}/config", base.display()),
                ..Default::default()
            },
            ..Default::default()
        };
        crate::storage::StorageService::new(&config)
    }

    #[test]
    fn rejects_missing_static_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(None),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)));
    }

    #[test]
    fn rejects_dot_static_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some(".")),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("workspace root"), "unexpected: {msg}");
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some("../escape")),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(".."), "unexpected: {msg}");
    }

    #[test]
    fn rejects_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some("/etc/passwd")),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("absolute") || msg.contains("relative"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn rejects_nonexistent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some("apps/countdown")),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no such directory"), "unexpected: {msg}");
    }

    #[test]
    fn rejects_dir_with_no_html() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("users/mina/agents/system/apps/countdown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "hi").unwrap();

        let storage = test_storage(tmp.path());
        let err = validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some("apps/countdown")),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(".html"), "unexpected: {msg}");
    }

    #[test]
    fn accepts_valid_directory_with_html() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("users/mina/agents/system/apps/countdown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();

        let storage = test_storage(tmp.path());
        validate_static_dir(
            &storage,
            &crate::handle!("mina"),
            &crate::handle!("system"),
            &make_static_manifest(Some("apps/countdown")),
        )
        .expect("should pass");
    }
}
