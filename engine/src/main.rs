use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
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
    let log_filter = std::env::var("FRONA_LOG_CONFIG").unwrap_or_else(|_| {
        let level = std::env::var("FRONA_LOG_LEVEL").unwrap_or_else(|_| "info".into());
        format!("frona={level},tower_http={level}")
    });
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&log_filter))
        .init();

    let loaded = Config::load();
    let config = loaded.config;

    const DEFAULT_SECRET: &str = "dev-secret-change-in-production";
    if config.auth.encryption_secret == DEFAULT_SECRET {
        tracing::warn!(
            "SECURITY WARNING: Using default encryption_secret. \
             Set FRONA_AUTH_ENCRYPTION_SECRET to a strong random value before deploying."
        );
    }

    verify_sandbox(&config.storage.workspaces_path, config.server.sandbox_disabled)
        .expect("Sandbox verification failed — filesystem may not support sandboxing. Set FRONA_SERVER_SANDBOX_DISABLED=true to bypass.");

    let shared_config_dir = PathBuf::from(&config.storage.shared_config_dir);
    let workspaces = AgentWorkspaceManager::new(&config.storage.workspaces_path, shared_config_dir.join("agents"));

    let metrics_handle = setup_metrics_recorder();

    let surreal = db::init(&config.database.path).await?;
    db::seed_config_agents(&surreal, &workspaces).await?;
    let state = AppState::new(surreal.clone(), &config, loaded.models, workspaces, metrics_handle);
    state.vault_service.sync_config_connections().await?;
    state.browser_session_manager.kill_all_sessions().await;

    state.init_task_executor();
    if let Some(executor) = state.task_executor() {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor.resume_all().await;
        });
        info!("Task executor initialized, resuming pending tasks");
    }

    {
        let app_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = frona::app::supervisor::restore_and_supervise_apps(app_state).await {
                tracing::error!(error = %e, "App restoration failed");
            }
        });
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
        .merge(routes::apps::router())
        .merge(routes::spaces::router())
        .merge(routes::chats::router())
        .merge(routes::contacts::router())
        .merge(routes::messages::router())
        .merge(routes::tasks::router())
        .merge(routes::browser::router())
        .merge(routes::navigation::router())
        .merge(routes::tools::router())
        .merge(routes::files::router())
        .merge(routes::metrics::router())
        .merge(routes::vaults::router())
        .merge(routes::voice::router())
        .layer(DefaultBodyLimit::max(config.server.max_body_size_bytes))
        .layer(axum::middleware::from_fn(track_http_metrics));
    if let Some(cors) = cors {
        api = api.layer(cors);
    }
    let api = api
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-xss-protection"),
            HeaderValue::from_static("1; mode=block"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
        .fallback_service(ServeDir::new(&config.server.static_dir).fallback(
            axum::routing::get({
                let static_dir = PathBuf::from(&config.server.static_dir);
                move |uri: Uri| {
                    let static_dir = static_dir.clone();
                    async move { html_fallback(static_dir, uri.path().to_owned()).await }
                }
            }),
        ));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.server.port));
    info!("Server starting on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

async fn html_fallback(static_dir: PathBuf, path: String) -> impl IntoResponse {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let html_path = static_dir.join(format!("{path}.html"));
    if let Ok(contents) = tokio::fs::read_to_string(&html_path).await {
        return Html(contents).into_response();
    }
    let not_found = static_dir.join("404.html");
    match tokio::fs::read_to_string(&not_found).await {
        Ok(contents) => (StatusCode::NOT_FOUND, Html(contents)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
