//! App repair and health verification using a chat run.
use super::{
    manager::ProcessStatus,
    models::{App, AppManifest, AppStatus, HealthCheck},
    service::AppService,
};
use crate::{
    agent::harness::ChatRunOptions,
    core::{error::AppError, execution::ExecutionKind, state::AppState},
};
use frona_harness::BudgetLimits;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepairResult {
    pub summary: String,
}

/// A submitted claim is accepted only after fresh process and HTTP checks.
/// Failure text is model feedback, while inability to read authoritative state
/// is an inspector error and must stop the run.
pub async fn verify_running(
    service: &AppService,
    app_id: &str,
) -> Result<Result<(), String>, AppError> {
    let app = service
        .get(app_id)
        .await?
        .ok_or_else(|| AppError::NotFound("App disappeared during repair".into()))?;
    if app.status != AppStatus::Running {
        return Ok(Err(format!(
            "App '{}' is still {}. Inspect its current log and restart it.",
            app.handle, app.status
        )));
    }
    if !matches!(
        service.manager().check_process(app_id).await,
        ProcessStatus::Alive
    ) {
        return Ok(Err(
            "The app has no live managed process; a stored running status is not sufficient."
                .into(),
        ));
    }
    let Some(port) = app.port else {
        return Ok(Err("The app has no assigned port.".into()));
    };
    let manifest: AppManifest = serde_json::from_value(app.manifest)
        .map_err(|e| AppError::Validation(format!("Invalid app manifest: {e}")))?;
    let check = manifest.health_check.unwrap_or(HealthCheck {
        path: "/".into(),
        interval_secs: None,
        timeout_secs: Some(5),
        initial_delay_secs: None,
        failure_threshold: None,
    });
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(check.effective_timeout());
    loop {
        if service.manager().health_check(port, &check).await {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Err(format!(
                "The app's HTTP health check on port {port} path '{}' failed. Check the current logs and repair it before submitting again.",
                check.path
            )));
        }
        if !matches!(
            service.manager().check_process(app_id).await,
            ProcessStatus::Alive
        ) {
            return Ok(Err("The managed app process exited during startup.".into()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // The process can exit during the HTTP probe (or a stale process can answer).
    if !matches!(
        service.manager().check_process(app_id).await,
        ProcessStatus::Alive
    ) {
        return Ok(Err(
            "The managed app process exited during verification.".into()
        ));
    }
    Ok(Ok(()))
}

pub async fn repair_app(state: &AppState, app: &App) -> Result<(), AppError> {
    let prompt = state
        .prompts
        .read_with_vars(
            "APP_CRASH.md",
            &[("app_name", &app.name), ("app_handle", app.handle.as_str())],
        )
        .ok_or_else(|| AppError::Internal("Missing APP_CRASH.md".into()))?;
    let mut chat_id = None;
    let result = async {
        let harness = state
            .harness
            .prepare_with_new_chat_options(
                &app.user_id,
                &app.agent_id,
                &format!("Fixing {}", app.name),
                ChatRunOptions {
                    tools: &["shell", "manage_app"],
                    kind: ExecutionKind::App,
                    action: "Repairing crashed app",
                    related_chat_ids: &[&app.chat_id],
                },
            )
            .await?;
        chat_id = harness.chat_id().map(str::to_owned);
        let service = state.app_service.clone();
        let app_id = app.id.clone();
        harness
            .run(prompt)
            .budget_limits(BudgetLimits {
                model_attempts: Some(30),
                tool_calls: Some(60),
                ..Default::default()
            })
            .structured::<RepairResult>()
            .submission_attempts(3)
            .verify(async move |_| verify_running(&service, &app_id).await)
            .await
    }
    .await;
    let outcome = match &result {
        Ok(result) => format!(
            "App '{}' repaired and verified healthy. {}",
            app.name, result.summary
        ),
        Err(error) => format!("App '{}' repair did not complete: {error}", app.name),
    };
    let outcome = match chat_id {
        Some(id) if result.is_ok() || state.chat_service.find_chat(&id).await?.is_some() => {
            format!("{outcome}\n[Repair chat](/chat?id={id})")
        }
        _ => outcome,
    };
    state
        .chat_service
        .save_system_message(&app.user_id, None, &app.chat_id, outcome)
        .await?;
    result.map(|_| ())
}
