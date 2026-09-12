use std::collections::HashMap;
use std::path::Path as StdPath;

use crate::agent::config::parse_frontmatter;
use crate::agent::models::{Agent, AgentResponse, CreateAgentRequest, Model, UpdateAgentRequest};
use crate::chat::broadcast::{BroadcastEvent, BroadcastEventKind};
use crate::inference::tool_loop::InferenceEventKind;
use axum::extract::{Multipart, Path, State};
use axum::routing::{get, put};
use axum::{Json, Router};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::error::AppError;
use crate::core::state::AppState;

/// Resolve `model_group` → live `ModelEntry`. Returns `None` when the group
/// name doesn't match a configured group (config drift, agent referencing a
/// removed group), in which case `AgentResponse.model` stays `None`.
fn resolve_model(state: &AppState, model_group_name: &str) -> Option<Model> {
    let group = state
        .chat_service
        .model_providers()
        .resolve(&crate::inference::ModelRef(
            model_group_name.to_owned().into(),
        ))
        .ok()?;
    let entry = state
        .catalog_sources
        .models
        .current()
        .lookup_for_provider(group.main.provider_name(), &group.main.model_id)
        .cloned();
    Some(Model {
        provider: group.main.provider_name().to_string(),
        model_id: group.main.model_id.clone(),
        context_window: group.context_window,
        entry,
    })
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(
            "/api/agents/{id}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/api/agents/{id}/skills", get(list_agent_skills))
        .route("/api/agents/{id}/avatar", put(upload_avatar))
}

async fn validate_request_sandbox_paths(
    state: &AppState,
    auth: &AuthUser,
    policy: Option<&crate::policy::sandbox::SandboxPolicy>,
) -> Result<(), AppError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    let owned_agents: std::collections::HashSet<String> = state
        .agent_service
        .list(&auth.user_id)
        .await?
        .into_iter()
        .map(|a| a.id)
        .collect();
    policy.validate_paths(&auth.handle, |id| owned_agents.contains(id))
}

async fn sync_agent_tools(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    selected_tools: &[String],
) -> Result<(), crate::core::error::AppError> {
    state
        .policy_service
        .reconcile_agent_tools(user_id, agent_id, selected_tools)
        .await
        .map(|_| ())
        .map_err(crate::core::error::AppError::from)
}

fn resolve_default_prompt(
    state: &AppState,
    user_handle: &crate::core::Handle,
    agent_handle: &crate::core::Handle,
) -> String {
    state
        .storage_service
        .agent_workspace(user_handle, agent_handle)
        .read("AGENT.md")
        .map(|c| parse_frontmatter(&c).template)
        .unwrap_or_default()
}

async fn to_response(
    state: &AppState,
    user_id: &str,
    user_handle: &crate::core::Handle,
    agent: Agent,
) -> Result<AgentResponse, AppError> {
    let registry = state
        .tool_manager
        .build_agent_registry(user_id, &agent, &state.policy_service, None)
        .await;
    let tools: Vec<String> = registry
        .definitions()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    let sandbox_policy = state
        .policy_service
        .evaluate_sandbox_policy(
            crate::policy::service::SandboxPrincipalRef::agent(user_id, user_handle, &agent.handle),
            false,
        )
        .await?
        .as_ref()
        .clone();
    let agent_id = agent.id.clone();
    let mut response = AgentResponse::from_agent(agent, tools, sandbox_policy);
    response.model = resolve_model(state, &response.model_group);
    if let Some(value) = response.identity.get("avatar")
        && !value.is_empty()
    {
        response.avatar_url = if value.starts_with("http://") || value.starts_with("https://") {
            Some(value.clone())
        } else if !value.contains('/') {
            state
                .presign_service
                .sign_with_expiry_by_user_id(
                    &format!("agent:{agent_id}"),
                    value,
                    user_id,
                    crate::credential::presign::PresignService::LONG_TERM_EXPIRY_SECS,
                )
                .await
                .ok()
        } else {
            None
        };
    }
    Ok(response)
}

async fn create_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<Json<AgentResponse>, ApiError> {
    let tools = req.tools.clone();
    validate_request_sandbox_paths(&state, &auth, req.sandbox_policy.as_ref()).await?;
    let agent = state.agent_service.create(&auth.user_id, req).await?;

    if let Some(tool_list) = tools {
        sync_agent_tools(&state, &auth.user_id, &agent.id, &tool_list).await?;
    }

    let handle = agent.handle.clone();
    let mut response = to_response(&state, &auth.user_id, &auth.handle, agent).await?;
    response.default_prompt = resolve_default_prompt(&state, &auth.handle, &handle);
    Ok(Json(response))
}

async fn list_agents(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentResponse>>, ApiError> {
    let agents = state.agent_service.list(&auth.user_id).await?;

    let count_map: HashMap<String, u64> = state
        .db
        .query("SELECT agent_id, count() AS count FROM chat WHERE user_id = $user_id GROUP BY agent_id")
        .bind(("user_id", auth.user_id.clone()))
        .await
        .and_then(|mut r| r.take::<Vec<serde_json::Value>>(0))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let agent_id = v.get("agent_id")?.as_str()?.to_string();
            let count = v.get("count")?.as_u64()?;
            Some((agent_id, count))
        })
        .collect();

    let mut responses = Vec::new();
    for agent in agents {
        let (id, handle) = (agent.id.clone(), agent.handle.clone());
        let mut response = to_response(&state, &auth.user_id, &auth.handle, agent).await?;

        if let Some(&count) = count_map.get(id.as_str()) {
            response.chat_count = count;
        }
        response.default_prompt = resolve_default_prompt(&state, &auth.handle, &handle);
        responses.push(response);
    }

    Ok(Json(responses))
}

async fn get_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentResponse>, ApiError> {
    let agent = state.agent_service.get(&auth.user_id, &id).await?;
    let handle = agent.handle.clone();
    let mut response = to_response(&state, &auth.user_id, &auth.handle, agent).await?;
    response.default_prompt = resolve_default_prompt(&state, &auth.handle, &handle);
    Ok(Json(response))
}

async fn update_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentRequest>,
) -> Result<Json<AgentResponse>, ApiError> {
    let tools = req.tools.clone();
    validate_request_sandbox_paths(&state, &auth, req.sandbox_policy.as_ref()).await?;
    let agent = state.agent_service.update(&auth.user_id, &id, req).await?;

    if let Some(tool_list) = tools {
        sync_agent_tools(&state, &auth.user_id, &id, &tool_list).await?;
    }

    let handle = agent.handle.clone();
    let mut response = to_response(&state, &auth.user_id, &auth.handle, agent).await?;
    response.default_prompt = resolve_default_prompt(&state, &auth.handle, &handle);

    state.broadcast_service.send(BroadcastEvent {
        user_id: auth.user_id,
        chat_id: None,
        space_id: None,
        kind: BroadcastEventKind::Inference(InferenceEventKind::EntityUpdated {
            table: "agent".to_string(),
            record_id: id,
            fields: serde_json::to_value(&response).unwrap_or_default(),
        }),
    });

    Ok(Json(response))
}

async fn delete_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    let agent = state.agent_service.get(&auth.user_id, &id).await?;
    state.agent_service.delete(&auth.user_id, &id).await?;
    state
        .policy_service
        .delete_agent_policies(&auth.user_id, &auth.handle, &agent.handle)
        .await?;
    Ok(())
}

async fn list_agent_skills(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::agent::skill::service::SkillListItem>>, ApiError> {
    let agent = state.agent_service.get(&auth.user_id, &id).await?;
    let skills = state
        .skill_service
        .list(&auth.handle, &agent.handle, None)
        .await;
    let items = skills
        .into_iter()
        .map(|s| crate::agent::skill::service::SkillListItem {
            name: s.name,
            description: s.description,
            source: None,
            installed_at: None,
            scope: s.scope,
        })
        .collect();
    Ok(Json(items))
}

const MAX_AVATAR_SIZE: usize = 2 * 1024 * 1024;

async fn upload_avatar(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = state.agent_service.get(&auth.user_id, &id).await?;

    let mut file_data: Option<(String, Vec<u8>)> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(AppError::Validation(e.to_string())))?
    {
        if field.name() == Some("file") {
            let filename = field.file_name().unwrap_or("avatar").to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError(AppError::Validation(e.to_string())))?;
            if bytes.len() > MAX_AVATAR_SIZE {
                return Err(ApiError(AppError::Validation(
                    "Avatar too large (max 2MB)".into(),
                )));
            }
            file_data = Some((filename, bytes.to_vec()));
        }
    }

    let (filename, bytes) =
        file_data.ok_or_else(|| ApiError(AppError::Validation("Missing file field".into())))?;

    let ext = StdPath::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg");
    let avatar_filename = format!("avatar.{ext}");

    let workspace = state
        .storage_service
        .agent_workspace(&auth.handle, &agent.handle);
    workspace
        .write_bytes(&avatar_filename, &bytes)
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    let owner = format!("agent:{id}");
    let presigned_url = state
        .presign_service
        .sign_with_expiry_by_user_id(
            &owner,
            &avatar_filename,
            &auth.user_id,
            crate::credential::presign::PresignService::LONG_TERM_EXPIRY_SECS,
        )
        .await?;

    Ok(Json(serde_json::json!({
        "filename": avatar_filename,
        "url": presigned_url,
    })))
}

#[cfg(test)]
mod context_budget_tests {
    use super::*;
    use crate::{core::config::Config, inference::config::ModelRegistryConfig};
    use std::sync::Arc;

    #[tokio::test]
    async fn agent_usage_reports_the_same_catalog_budget_as_compaction() {
        for override_window in [None, Some(5000)] {
            let directory = tempfile::tempdir().unwrap();
            let mut config: Config = serde_json::from_value(serde_json::json!({
                "providers":{"openai-prod":{"provider":"openai","api_key":"fixture-key"}},
                "models":{"primary":{"provider":"openai-prod","model":"gpt-4o","context_window":override_window}}
            })).unwrap();
            config.storage.cache_dir = directory.path().join("cache").to_string_lossy().into();
            let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
                .await
                .unwrap();
            crate::db::init::setup_schema(&db).await.unwrap();
            let state = AppState::new(
                db.clone(),
                {
                    let mut loaded = crate::core::config::ConfigService::load(
                        tempfile::tempdir().unwrap().path().join("config.yaml"),
                    )
                    .unwrap();
                    loaded.config = config.clone();
                    crate::core::config::ConfigService::new(loaded).unwrap()
                },
                Some(ModelRegistryConfig {
                    providers: config.providers.clone(),
                    models: config.models.clone(),
                    skip_auto_discover: true,
                }),
                crate::storage::StorageService::new(&config),
                crate::core::metrics::setup_metrics_recorder(),
                Arc::new(
                    crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                        80.0, 80.0, 90.0, 90.0,
                    ),
                ),
                crate::app_state_fixture::catalogs(&config),
            );
            let expected = override_window.unwrap_or_else(|| {
                state
                    .catalog_sources
                    .models
                    .current()
                    .lookup_for_provider("openai", "gpt-4o")
                    .unwrap()
                    .max_input_tokens()
                    .unwrap() as usize
            });
            let model = resolve_model(&state, "primary").unwrap();
            assert_eq!(model.context_window, expected);
            let group = state
                .chat_service
                .model_providers()
                .resolve(&crate::inference::ModelRef::PRIMARY)
                .unwrap();
            assert_eq!(model.context_window, group.context_window);
            assert_eq!(
                serde_json::to_value(model).unwrap()["context_window"],
                expected
            );
        }
    }
}
