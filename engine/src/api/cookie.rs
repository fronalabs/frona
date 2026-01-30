use axum::http::HeaderValue;

const COOKIE_NAME: &str = "token";

pub fn make_auth_cookie(token: &str, max_age_secs: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_secs}"
    ))
    .expect("valid cookie header")
}

pub fn make_clear_cookie() -> HeaderValue {
    HeaderValue::from_static("token=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

pub fn extract_token_from_cookie_header(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let pair = pair.trim();
        pair.strip_prefix("token=")
    })
}
