#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub jwt_secret: String,
    pub surreal_path: String,
    pub static_dir: String,
    pub models_config_path: String,
    pub browserless_ws_url: String,
    pub browser_profiles_path: String,
    pub workspaces_base_path: String,
    pub files_base_path: String,
    pub shared_config_dir: String,
    pub max_concurrent_tasks: usize,
    pub scheduler_space_compaction_secs: u64,
    pub scheduler_insight_compaction_secs: u64,
    pub scheduler_poll_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "3001".into())
                .parse()
                .expect("PORT must be a number"),
            jwt_secret: std::env::var("JWT_SECRET")
                .unwrap_or_else(|_| "dev-secret-change-in-production".into()),
            surreal_path: std::env::var("SURREAL_PATH").unwrap_or_else(|_| "data/db".into()),
            static_dir: std::env::var("STATIC_DIR").unwrap_or_else(|_| "frontend/out".into()),
            models_config_path: std::env::var("FRONA_MODELS_CONFIG")
                .unwrap_or_else(|_| "data/models.json".into()),
            browserless_ws_url: std::env::var("BROWSERLESS_WS_URL")
                .unwrap_or_else(|_| "ws://localhost:3333".into()),
            browser_profiles_path: std::env::var("BROWSER_PROFILES_PATH")
                .unwrap_or_else(|_| "/profiles".into()),
            workspaces_base_path: std::env::var("WORKSPACES_BASE_PATH")
                .unwrap_or_else(|_| "data/workspaces".into()),
            files_base_path: std::env::var("FILES_BASE_PATH")
                .unwrap_or_else(|_| "data/files".into()),
            shared_config_dir: std::env::var("FRONA_SHARED_CONFIG")
                .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/config").into()),
            max_concurrent_tasks: std::env::var("MAX_CONCURRENT_TASKS")
                .unwrap_or_else(|_| "10".into())
                .parse()
                .expect("MAX_CONCURRENT_TASKS must be a number"),
            scheduler_space_compaction_secs: std::env::var("SCHEDULER_SPACE_COMPACTION_SECS")
                .unwrap_or_else(|_| "3600".into())
                .parse()
                .expect("SCHEDULER_SPACE_COMPACTION_SECS must be a number"),
            scheduler_insight_compaction_secs: std::env::var("SCHEDULER_INSIGHT_COMPACTION_SECS")
                .unwrap_or_else(|_| "7200".into())
                .parse()
                .expect("SCHEDULER_INSIGHT_COMPACTION_SECS must be a number"),
            scheduler_poll_secs: std::env::var("SCHEDULER_POLL_SECS")
                .unwrap_or_else(|_| "60".into())
                .parse()
                .expect("SCHEDULER_POLL_SECS must be a number"),
        }
    }
}
