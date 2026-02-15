use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderName, Method};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use frona::agent::workspace::AgentWorkspaceManager;
use frona::api::db;
use frona::api::routes;
use frona::core::config::Config;
use frona::core::state::AppState;
use frona::scheduler::Scheduler;
use frona::tool::workspace::sandbox::verify_sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env();

    verify_sandbox(&config.workspaces_base_path, config.sandbox_disabled)
        .expect("Sandbox verification failed — filesystem may not support sandboxing. Set SANDBOX_DISABLED=true to bypass.");

    let cors_origin = std::env::var("CORS_ORIGIN")
        .unwrap_or_else(|_| "http://localhost:3000".into());

    let workspaces = AgentWorkspaceManager::new(&config.workspaces_base_path);

    let surreal = db::init(&config.surreal_path).await?;
    db::seed_config_agents(&surreal, &workspaces).await?;
    let state = AppState::new(surreal.clone(), &config, workspaces);
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
            space_secs = config.scheduler_space_compaction_secs,
            insight_secs = config.scheduler_insight_compaction_secs,
            poll_secs = config.scheduler_poll_secs,
            "Scheduler started"
        );
    }

    let cors = CorsLayer::new()
        .allow_origin(cors_origin.parse::<axum::http::HeaderValue>()?)
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
        .allow_credentials(true);

    let api = axum::Router::new()
        .merge(routes::auth::router())
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
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
        .fallback_service(ServeDir::new(&config.static_dir));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server starting on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api).await?;

    Ok(())
}
