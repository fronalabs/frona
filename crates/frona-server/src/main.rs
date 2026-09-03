use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse};
use axum::serve::ListenerExt;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use frona::api::middleware::metrics::track_http_metrics;
use frona::api::middleware::setup_redirect::setup_redirect;
use frona::api::middleware::shutdown::shutdown_gate;
use frona::api::routes;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use frona::credential::key_rotation::KeyRotation;
use frona::db::init as db;
use frona::scheduler::Scheduler;
use frona::storage::StorageService;
use frona::tool::sandbox::driver::resource_monitor::SystemResourceManager;
use frona::tool::sandbox::driver::verify_sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let log_filter = std::env::var("FRONA_LOG_CONFIG").unwrap_or_else(|_| {
        let level = std::env::var("FRONA_LOG_LEVEL").unwrap_or_else(|_| "info".into());
        format!("frona={level},tower_http={level}")
    });
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&log_filter))
        .init();

    info!("Frona v{}", env!("CARGO_PKG_VERSION"));

    let loaded = Config::load();
    let config = loaded.config;

    const DEFAULT_SECRET: &str = "dev-secret-change-in-production";
    if config.auth.encryption_secret == DEFAULT_SECRET {
        tracing::warn!(
            "SECURITY WARNING: Using default encryption_secret. \
             Set FRONA_AUTH_ENCRYPTION_SECRET to a strong random value before deploying."
        );
    }

    let users_root = std::path::PathBuf::from(&config.storage.data_dir).join("users");
    std::fs::create_dir_all(&users_root).ok();
    verify_sandbox(users_root.to_string_lossy().as_ref(), config.sandbox.disabled)
        .expect("Sandbox verification failed — filesystem may not support sandboxing. Set FRONA_SANDBOX_DISABLED=true to bypass.");

    frona::tool::sandbox::driver::resource_monitor::log_system_resources();

    let storage = StorageService::new(&config);
    frona::auth::ephemeral_token::prepare_runtime_dir(&storage.users_root());

    let metrics_handle = setup_metrics_recorder();

    let surreal = db::init(&config.database.path).await?;

    if let Some(rotation) = KeyRotation::check(&surreal, &config.auth.encryption_secret).await? {
        rotation.run().await?;
    }

    let resource_manager = Arc::new(SystemResourceManager::new(
        config.sandbox.default_limits.max_cpu_pct,
        config.sandbox.default_limits.max_memory_pct,
        config.sandbox.max_total_cpu_pct,
        config.sandbox.max_total_memory_pct,
    ));
    resource_manager.start_polling();

    let state = AppState::new(
        surreal.clone(),
        &config,
        loaded.models,
        storage,
        metrics_handle,
        resource_manager,
    );
    state.agent_service.sync_agent_limits().await?;
    state.vault_service.sync_config_connections().await?;
    state.browser_session_manager.kill_all_sessions().await;
    state.skill_service.start_watcher();

    let signal_service = state.init_signal_service();
    state.tool_manager.init(&state);
    state.policy_service.sync_base_policies().await?;
    if let Err(e) = signal_service.start().await {
        tracing::warn!(error = %e, "Failed to start signal service");
    }
    frona::chat::channel::spawn_inference_dispatcher(state.clone());

    if let Err(e) = state.channel_supervisor.clone().boot().await {
        tracing::warn!(error = %e, "ChannelSupervisor failed to start; channel adapters will not run");
    }

    if state.config.sandbox.default_network_access {
        let policy = cedar_policy::Policy::from_json(
            Some(cedar_policy::PolicyId::new("default-network-access")),
            serde_json::json!({
                "effect": "permit",
                "principal": { "op": "All" },
                "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
                "resource": { "op": "All" },
                "annotations": {
                    "description": "Default outbound network access for all agents",
                    "config": "sandbox.default_network_access",
                    "readonly": "true"
                },
                "conditions": []
            }),
        )
        .expect("valid default network policy");
        state.policy_service.register_managed_policy(policy);
    }

    let app_supervisor_handle;
    {
        let executor = state.task_executor.clone();
        tokio::spawn(async move {
            executor.resume_all().await;
        });
        info!("Task executor initialized, resuming pending tasks");
    }

    {
        let harness = state.harness.clone();
        tokio::spawn(async move {
            harness.resume_all().await;
        });
    }

    {
        use frona::core::supervisor::{SupervisorConfig, run};

        let app_supervisor =
            std::sync::Arc::new(frona::app::supervisor::AppSupervisor::new(state.clone()));
        let app_config = SupervisorConfig {
            health_check_interval: std::time::Duration::from_secs(10),
            max_restart_attempts: config.app.max_restart_attempts,
            hibernate_after: if config.app.hibernate_after_secs > 0 {
                Some(std::time::Duration::from_secs(
                    config.app.hibernate_after_secs,
                ))
            } else {
                None
            },
        };
        let shutdown = state.shutdown_token.clone();
        let notif = state.notification_service.clone();
        let broadcast = state.broadcast_service.clone();
        app_supervisor_handle = tokio::spawn(async move {
            run(app_supervisor, shutdown, notif, broadcast, app_config).await;
        });

        let mcp_supervisor = std::sync::Arc::new(frona::tool::mcp::supervisor::McpSupervisor::new(
            state.mcp_service.clone(),
            state.mcp_manager.clone(),
        ));
        let mcp_config = SupervisorConfig {
            health_check_interval: std::time::Duration::from_secs(
                config.mcp.health_check_interval_secs,
            ),
            max_restart_attempts: config.mcp.max_restart_attempts,
            hibernate_after: None,
        };
        let shutdown = state.shutdown_token.clone();
        let notif = state.notification_service.clone();
        let broadcast = state.broadcast_service.clone();
        tokio::spawn(async move {
            run(mcp_supervisor, shutdown, notif, broadcast, mcp_config).await;
        });

        // Channels: boot-restore + level-triggered re-arm of any enabled channel whose
        // supervisor task is absent (replaces the old scheduler reconcile sweep). The
        // per-channel event-driven reconnect loop is separate and unaffected.
        let channel_config = SupervisorConfig {
            health_check_interval: std::time::Duration::from_secs(60),
            // Transient reconnects are handled forever by the inner loop; the outer loop
            // only re-arms absent tasks, so it must never give up.
            max_restart_attempts: u32::MAX,
            hibernate_after: None,
        };
        let shutdown = state.shutdown_token.clone();
        let notif = state.notification_service.clone();
        let broadcast = state.broadcast_service.clone();
        let channel_supervisor = state.channel_supervisor.clone();
        tokio::spawn(async move {
            run(
                channel_supervisor,
                shutdown,
                notif,
                broadcast,
                channel_config,
            )
            .await;
        });
    }

    // The scheduler always runs (poll/cleanup jobs); memory maintenance is
    // registered by the active memory service itself (which resolves its own
    // compaction model group).
    let scheduler = Arc::new(Scheduler::new(state.clone()));
    scheduler.start();
    info!(poll_secs = config.scheduler.poll_secs, "Scheduler started");

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
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                HeaderName::from_static("content-type"),
                HeaderName::from_static("authorization"),
            ])
            .allow_credentials(true)
    });

    let has_users = state.user_service.has_users().await.unwrap_or(true);
    if !has_users && !frona::auth::can_create_users(&config) {
        error!(
            "Cannot start: no users exist and no path to create one is available. \
             Set FRONA_AUTH_ALLOW_REGISTRATION=true (and ensure sso.disable_local_auth is false) \
             to allow the first user to register, or enable SSO so the first admin can sign in \
             via the configured identity provider."
        );
        std::process::exit(1);
    }
    if !has_users {
        info!("No users found — registration redirect active. Restart after setup.");
    }

    if let Err(e) = state.user_group_service.seed_built_in().await {
        error!(error = %e, "Failed to seed built-in user groups");
        std::process::exit(1);
    }
    if let Err(e) = state.user_service.ensure_admin_invariant().await {
        error!(error = %e, "Failed to ensure admin invariant");
        std::process::exit(1);
    }

    let mut api = axum::Router::new()
        .merge(routes::admin::router())
        .merge(routes::activity::router())
        .merge(routes::auth::router())
        .merge(routes::well_known::router())
        .merge(routes::agents::router())
        .merge(routes::apps::router())
        .merge(routes::spaces::router())
        .merge(routes::chats::router())
        .merge(routes::commands::router())
        .merge(routes::channels::router())
        .merge(routes::contacts::router())
        .merge(routes::messages::router())
        .merge(routes::tasks::router())
        .merge(routes::browser::router())
        .merge(routes::navigation::router())
        .merge(routes::notifications::router())
        .merge(routes::policies::router())
        .merge(routes::signals::router())
        .merge(routes::skills::router())
        .merge(routes::tools::router())
        .merge(routes::usage::router())
        .merge(routes::files::router())
        .merge(routes::share::router())
        .merge(routes::preview::router())
        .merge(routes::metrics::router())
        .merge(routes::vaults::router())
        .merge(routes::mcp::router())
        .merge(routes::voice::router())
        .merge(routes::system::router())
        .merge(routes::config::router())
        .merge(routes::provider_models::router());
    // Register the PKM sync API only when PKM is the active backend - a Basic install
    // never exposes `/api/memory/pkm/*`, so no request-time is-PKM probe is needed.
    if config
        .memory
        .backend
        .unwrap_or(frona::core::config::MemoryBackend::Basic)
        == frona::core::config::MemoryBackend::Pkm
    {
        api = api
            .merge(routes::memory_pkm::router())
            .merge(routes::memory_pkm_read::router());
    }
    let mut api = api
        .layer(DefaultBodyLimit::max(config.server.max_body_size_bytes))
        .layer(axum::middleware::from_fn(track_http_metrics))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            shutdown_gate,
        ));
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
        .with_state(state.clone())
        .fallback_service(
            ServeDir::new(&config.server.static_dir).fallback(axum::routing::get({
                let static_dir = PathBuf::from(&config.server.static_dir);
                move |uri: Uri| {
                    let static_dir = static_dir.clone();
                    async move { html_fallback(static_dir, uri.path().to_owned()).await }
                }
            })),
        );

    let api: axum::Router = if !has_users {
        api.layer(axum::middleware::from_fn(setup_redirect))
    } else {
        api
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], config.server.port));
    info!("Starting on {addr}");

    let shutdown_state = state.clone();
    let shutdown_signal = async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => { info!("Received SIGINT"); }
            _ = sigterm.recv() => { info!("Received SIGTERM"); }
        }

        info!("Initiating graceful shutdown...");
        let app_ids = shutdown_state
            .app_service
            .manager()
            .get_managed_app_ids()
            .await;
        for app_id in app_ids {
            if let Err(error) = shutdown_state.app_service.manager().stop_app(&app_id).await {
                tracing::warn!(%error, %app_id, "Failed to stop app during shutdown");
            }
        }
        shutdown_state.shutdown_token.cancel();
    };

    let listener = tokio::net::TcpListener::bind(addr)
        .await?
        .tap_io(|tcp_stream| {
            if let Err(err) = tcp_stream.set_nodelay(true) {
                tracing::trace!("failed to set TCP_NODELAY on incoming connection: {err:#}");
            }
        });
    axum::serve(
        listener,
        api.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;

    if let Err(error) = app_supervisor_handle.await {
        tracing::warn!(%error, "App supervisor failed during shutdown");
    }

    info!("HTTP server stopped, draining in-flight work...");
    frona::core::shutdown::graceful_drain(&state).await;

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
