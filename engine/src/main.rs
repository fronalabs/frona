use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderName, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use frona::agent::workspace::AgentWorkspaceManager;
use frona::api::db;
use frona::api::middleware::metrics::track_http_metrics;
use frona::api::routes;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use frona::scheduler::Scheduler;
use frona::tool::workspace::sandbox::verify_sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let loaded = Config::load();
    let config = loaded.config;

    verify_sandbox(&config.storage.workspaces_path, config.server.sandbox_disabled)
        .expect("Sandbox verification failed — filesystem may not support sandboxing. Set FRONA_SERVER_SANDBOX_DISABLED=true to bypass.");

    let workspaces = AgentWorkspaceManager::new(&config.storage.workspaces_path);

    let metrics_handle = setup_metrics_recorder();

    let surreal = db::init(&config.database.path).await?;
    db::seed_config_agents(&surreal, &workspaces).await?;
    let state = AppState::new(surreal.clone(), &config, loaded.models, workspaces, metrics_handle);
    state.browser_session_manager.kill_all_sessions().await;

    state.init_task_executor();
    if let Some(executor) = state.task_executor() {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor.resume_all().await;
        });
        info!("Task executor initialized, resuming pending tasks");
    }

    if let Ok(compaction_group) = state.chat_service.provider_registry()
        .get_model_group("compaction")
        .or_else(|_| state.chat_service.provider_registry().get_model_group("primary"))
    {
        let scheduler = Arc::new(Scheduler::new(state.clone(), compaction_group.clone()));
        scheduler.start();
        info!(
            space_secs = config.scheduler.space_compaction_secs,
            insight_secs = config.scheduler.insight_compaction_secs,
            poll_secs = config.scheduler.poll_secs,
            "Scheduler started"
        );
    }

    let cors = config.server.cors_origins.as_deref().map(|origins_str| {
        let origins: Vec<String> = origins_str
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();
        info!(?origins, "CORS enabled");
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(move |origin, _| {
                let origin = origin.to_str().unwrap_or_default();
                origins.iter().any(|allowed| allowed == origin)
            }))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                HeaderName::from_static("content-type"),
                HeaderName::from_static("authorization"),
            ])
            .allow_credentials(true)
    });

    let mut api = axum::Router::new()
        .merge(routes::auth::router())
        .merge(routes::well_known::router())
        .merge(routes::agents::router())
        .merge(routes::spaces::router())
        .merge(routes::chats::router())
        .merge(routes::messages::router())
        .merge(routes::tasks::router())
        .merge(routes::credentials::router())
        .merge(routes::browser::router())
        .merge(routes::navigation::router())
        .merge(routes::tools::router())
        .merge(routes::files::router())
        .merge(routes::metrics::router())
        .layer(axum::middleware::from_fn(track_http_metrics));
    if let Some(cors) = cors {
        api = api.layer(cors);
    }
    let api = api
        .layer(TraceLayer::new_for_http())
        .with_state(state)
        .fallback_service(ServeDir::new(&config.server.static_dir));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.server.port));
    info!("Server starting on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api).await?;

    Ok(())
}
