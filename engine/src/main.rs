use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderName, Method};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use frona::api::config::Config;
use frona::api::db;
use frona::api::repo::generic::SurrealRepo;
use frona::api::routes;
use frona::api::state::AppState;
use frona::scheduler::Scheduler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env();

    let cors_origin = std::env::var("CORS_ORIGIN")
        .unwrap_or_else(|_| "http://localhost:3000".into());

    let surreal = db::init(&config.surreal_path).await?;
    db::seed_config_agents(&surreal).await?;
    let state = AppState::new(surreal.clone(), &config);
    state.browser_session_manager.kill_all_sessions().await;

    if let Ok(compaction_group) = state.chat_service.provider_registry()
        .get_model_group("compaction")
        .or_else(|_| state.chat_service.provider_registry().get_model_group("primary"))
    {
        let scheduler = Arc::new(Scheduler::new(
            state.memory_service.clone(),
            SurrealRepo::new(surreal.clone()),
            SurrealRepo::new(surreal.clone()),
            SurrealRepo::new(surreal.clone()),
            compaction_group.clone(),
            std::time::Duration::from_secs(3600),
        ));
        scheduler.start();
        info!("Scheduler started (space compaction: 1h, fact compaction: 2h)");
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
