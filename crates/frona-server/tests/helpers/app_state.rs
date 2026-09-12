//! Fixtures use the production AppState constructor, including built-in tool initialization.
use frona::core::config::{Config, ConfigService};
use frona::core::state::AppState;
use std::sync::Arc;
use surrealdb::{Surreal, engine::local::Db};

pub struct Fixture {
    pub state: AppState,
    _directory: tempfile::TempDir,
}

/// Load local catalogs without downloading, just as startup does before refresh.
pub fn catalogs(config: &Config) -> frona_model_catalog::sources::CatalogSources {
    frona::initialize_tls();
    let bundled = std::path::Path::new(&config.storage.shared_config_dir).join("catalogs");
    frona_model_catalog::sources::CatalogSources::load_with_bundled(
        std::path::Path::new(&config.storage.cache_dir),
        Some(&bundled),
    )
}

pub async fn build(db: &Surreal<Db>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().to_string_lossy();
    let resources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources");
    let mut config = Config::default();
    config.auth.encryption_secret = "test-secret".into();
    config.storage.data_dir = base.to_string();
    config.storage.cache_dir = format!("{base}/cache");
    config.storage.skills_dir = format!("{base}/skills");
    config.storage.shared_config_dir = resources.to_string_lossy().into_owned();
    let mut loaded = ConfigService::load(directory.path().join("config.yaml")).unwrap();
    loaded.config = config.clone();
    let config_service = ConfigService::new(loaded).unwrap();
    let state = AppState::new(
        db.clone(),
        config_service,
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        frona::storage::StorageService::new(&config),
        frona::core::metrics::setup_metrics_recorder(),
        Arc::new(
            frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                80.0, 80.0, 90.0, 90.0,
            ),
        ),
        catalogs(&config),
    );
    state.init_signal_service();
    state.tool_manager.init(&state);
    state.vault_service.sync_config_connections().await.unwrap();
    Fixture {
        state,
        _directory: directory,
    }
}
