pub struct Config {
    pub port: u16,
    pub jwt_secret: String,
    pub surreal_path: String,
    pub static_dir: String,
    pub models_config_path: String,
    pub agents_config_dir: String,
    pub browserless_ws_url: String,
    pub browser_profiles_path: String,
    pub workspaces_base_path: String,
    pub tools_config_path: String,
    pub skills_config_dir: String,
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
            agents_config_dir: std::env::var("FRONA_AGENTS_CONFIG_DIR")
                .unwrap_or_else(|_| "data/agents".into()),
            browserless_ws_url: std::env::var("BROWSERLESS_WS_URL")
                .unwrap_or_else(|_| "ws://localhost:3333".into()),
            browser_profiles_path: std::env::var("BROWSER_PROFILES_PATH")
                .unwrap_or_else(|_| "/profiles".into()),
            workspaces_base_path: std::env::var("WORKSPACES_BASE_PATH")
                .unwrap_or_else(|_| "data/workspaces".into()),
            tools_config_path: std::env::var("FRONA_TOOLS_CONFIG")
                .unwrap_or_else(|_| "data/tools.json".into()),
            skills_config_dir: std::env::var("FRONA_SKILLS_CONFIG_DIR")
                .unwrap_or_else(|_| "engine/config".into()),
        }
    }
}
