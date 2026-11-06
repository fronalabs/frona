pub const ENDPOINT: &str = "https://api.githubcopilot.com";
pub fn protocol(model: &str) -> crate::config::ApiSurface {
    if model.to_ascii_lowercase().contains("codex") {
        crate::config::ApiSurface::Responses
    } else {
        crate::config::ApiSurface::Completions
    }
}
