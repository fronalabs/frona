use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub browserless_ws_url: String,
    pub profiles_base_path: String,
    pub connection_timeout_ms: u64,
}

impl BrowserConfig {
    pub fn ws_url_for_profile(&self, user_id: &str, provider: &str) -> String {
        let user_data_dir = self.profile_path(user_id, provider);
        format!(
            "{}?--user-data-dir={}",
            self.browserless_ws_url,
            user_data_dir.display()
        )
    }

    pub fn http_base_url(&self) -> String {
        self.browserless_ws_url
            .replace("ws://", "http://")
            .replace("wss://", "https://")
    }

    pub fn debugger_url_for_credential(&self, credential_id: &str) -> String {
        format!("/api/browser/debugger/{credential_id}")
    }

    pub fn profile_path(&self, user_id: &str, provider: &str) -> PathBuf {
        PathBuf::from(&self.profiles_base_path)
            .join(user_id)
            .join(provider)
    }
}
