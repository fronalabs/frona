use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use tower::ServiceExt as _;
use tower_http::services::ServeDir;

use crate::api::cookie::{extract_app_session_from_cookie_header, make_app_session_cookie};
use crate::app::models::AppStatus;
use crate::core::state::AppState;

pub(crate) async fn auth_gate(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let redirect_url = uri
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("redirect=")))
        .filter(|u| u.starts_with('/') && !u.starts_with("//"))
        .unwrap_or("/");

    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let refresh_token = crate::api::cookie::extract_refresh_token_from_cookie_header(cookie_header);

    let Some(refresh_token) = refresh_token else {
        let login_url = build_login_redirect(&state, redirect_url);
        return Redirect::temporary(&login_url).into_response();
    };

    let claims = match state
        .token_service
        .validate_refresh(&state.keypair_service, refresh_token)
        .await
    {
        Ok(c) => c,
        _ => {
            let login_url = build_login_redirect(&state, redirect_url);
            return Redirect::temporary(&login_url).into_response();
        }
    };

    let user = match state.user_service.find_by_id(&claims.sub).await {
        Ok(Some(u)) => u,
        _ => {
            let login_url = build_login_redirect(&state, redirect_url);
            return Redirect::temporary(&login_url).into_response();
        }
    };

    let app_session_jwt = match state
        .token_service
        .create_access_token(&state.keypair_service, &user, "app_session")
        .await
    {
        Ok(jwt) => jwt,
        Err(_) => {
            let login_url = build_login_redirect(&state, redirect_url);
            return Redirect::temporary(&login_url).into_response();
        }
    };

    let secure = state
        .config
        .server
        .base_url
        .as_ref()
        .is_some_and(|u| u.starts_with("https"));

    let cookie = make_app_session_cookie(
        &app_session_jwt,
        state.config.auth.access_token_expiry_secs,
        secure,
    );

    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("location", redirect_url)
        .header("set-cookie", cookie)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn build_login_redirect(state: &AppState, app_redirect: &str) -> String {
    let frontend_url = state.config.server.public_frontend_url();
    let base_url = state.config.server.public_base_url();
    if frontend_url.is_empty() {
        return format!("/login?redirect={app_redirect}");
    }
    let gate_url = if base_url.is_empty() {
        format!("/api/auth/apps?redirect={app_redirect}")
    } else {
        format!("{base_url}/api/auth/apps?redirect={app_redirect}")
    };
    let encoded_gate = gate_url.replace('&', "%26");
    format!("{frontend_url}/login?redirect={encoded_gate}")
}

pub(crate) async fn proxy_app_root(
    State(state): State<AppState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_app_inner(state, handle, String::new(), headers, request).await
}

pub(crate) async fn proxy_app_path(
    State(state): State<AppState>,
    Path((handle, sub_path)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_app_inner(state, handle, sub_path, headers, request).await
}

async fn proxy_app_inner(
    state: AppState,
    handle: String,
    sub_path: String,
    headers: HeaderMap,
    request: Request,
) -> Response {
    tracing::debug!(handle = %handle, sub_path = %sub_path, "Proxy: incoming request");

    let user_id = match authenticate_proxy_request(&state, &headers).await {
        Some(uid) => uid,
        None => {
            let original_uri = request.uri().to_string();
            let gate_url = format!("/api/auth/apps?redirect={original_uri}");
            return Redirect::temporary(&gate_url).into_response();
        }
    };

    let parsed_handle = match crate::core::Handle::try_new(&handle) {
        Ok(h) => h,
        Err(_) => {
            tracing::warn!(handle = %handle, "Proxy: malformed handle");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let app = match state
        .app_service
        .find_by_user_handle(&user_id, &parsed_handle)
        .await
    {
        Ok(Some(app)) => {
            tracing::debug!(handle = %handle, app_id = %app.id, status = ?app.status, kind = %app.kind, "Proxy: app found");
            app
        }
        Ok(None) => {
            tracing::warn!(handle = %handle, "Proxy: app not found for user");
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::warn!(handle = %handle, error = %e, "Proxy: app lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    state.app_service.manager().record_access(&app.id).await;

    let app_handle = app.handle.to_string();
    match app.kind.as_str() {
        "static" => serve_static(&state, &app, &sub_path, request).await,
        _ => {
            if app.status == AppStatus::Hibernated {
                return handle_hibernated_app(&state, &app, &sub_path, request).await;
            }

            let port = match app.port {
                Some(p) => p,
                None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };

            if !matches!(app.status, AppStatus::Running) {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }

            forward_to_port(port, &sub_path, &app_handle, request).await
        }
    }
}

async fn authenticate_proxy_request(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(token) = auth_header.strip_prefix("Bearer ")
        && let Ok(claims) = state
            .token_service
            .validate_api(&state.keypair_service, token)
            .await
    {
        return Some(claims.sub);
    }

    let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    let token = extract_app_session_from_cookie_header(cookie_header)?;
    state
        .token_service
        .validate_access(&state.keypair_service, token)
        .await
        .ok()
        .map(|c| c.sub)
}

async fn serve_static(
    state: &AppState,
    app: &crate::app::models::App,
    sub_path: &str,
    request: Request,
) -> Response {
    let static_dir = app.static_dir.as_deref().unwrap_or("dist");
    let Ok(agent) = state.agent_service.get(&app.user_id, &app.agent_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(user) = state
        .user_service
        .find_by_id(&app.user_id)
        .await
        .ok()
        .flatten()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let workspace_path = state
        .storage_service
        .agent_workspace_path(&user.handle, &agent.handle);
    let serve_path = workspace_path.join(static_dir);

    if !serve_path.exists() {
        tracing::warn!(app_id = %app.id, path = %serve_path.display(), "Proxy: static dir not found");
        return StatusCode::NOT_FOUND.into_response();
    }

    let path = if sub_path.is_empty() { "/" } else { sub_path };
    let (mut parts, body) = request.into_parts();
    parts.uri = path.parse().unwrap_or(Uri::from_static("/"));
    let req = Request::from_parts(parts, body);

    let service = ServeDir::new(&serve_path).append_index_html_on_directories(true);

    match service.oneshot(req).await {
        Ok(resp) => {
            let status = resp.status();
            if status == StatusCode::NOT_FOUND
                && path == "/"
                && let Some(fallback) = find_html_fallback(&serve_path)
            {
                return fallback;
            }
            if status == StatusCode::NOT_FOUND {
                tracing::warn!(app_id = %app.id, serve_path = %serve_path.display(), sub_path = %path, "Proxy: static file not found");
            }
            resp.into_response()
        }
        Err(e) => {
            tracing::error!(app_id = %app.id, error = %e, "Proxy: static serve error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn find_html_fallback(serve_path: &std::path::Path) -> Option<Response> {
    let entries = std::fs::read_dir(serve_path).ok()?;
    let html_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
        })
        .collect();

    if html_files.len() == 1 {
        let content = std::fs::read(html_files[0].path()).ok()?;
        return Some(
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html; charset=utf-8")
                .body(Body::from(content))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }

    None
}

async fn handle_hibernated_app(
    state: &AppState,
    app: &crate::app::models::App,
    sub_path: &str,
    original_request: Request,
) -> Response {
    let manifest: crate::app::models::AppManifest =
        match serde_json::from_value(app.manifest.clone()) {
            Ok(m) => m,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

    let command = match &app.command {
        Some(c) => c.clone(),
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let result = state
        .app_service
        .manager()
        .start_app(
            &app.id,
            &app.agent_id,
            &app.user_id,
            &command,
            &manifest,
            Vec::new(),
        )
        .await;

    match result {
        Ok((port, pid)) => {
            let _ = state
                .app_service
                .update_status(&app.id, AppStatus::Running, Some(port), Some(pid))
                .await;

            let health = manifest
                .health_check
                .as_ref()
                .map(|h| {
                    (
                        h.path.clone(),
                        h.effective_initial_delay(),
                        h.effective_timeout(),
                    )
                })
                .unwrap_or_else(|| ("/".to_string(), 5, 2));

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(health.1);

            let hc = crate::app::models::HealthCheck {
                path: health.0,
                interval_secs: Some(1),
                timeout_secs: Some(health.2),
                initial_delay_secs: Some(0),
                failure_threshold: None,
            };

            loop {
                if tokio::time::Instant::now() >= deadline {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if state.app_service.manager().health_check(port, &hc).await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            forward_to_port(port, sub_path, app.handle.as_str(), original_request).await
        }
        Err(_) => {
            let _ = state
                .app_service
                .update_status(&app.id, AppStatus::Failed, None, None)
                .await;
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn rewrite_location(value: &str, app_prefix: &str) -> Option<String> {
    if let Some(path) = value.strip_prefix("http://127.0.0.1") {
        let path = path.find('/').map(|i| &path[i..]).unwrap_or("/");
        return Some(format!(
            "{app_prefix}{}",
            path.strip_prefix('/').unwrap_or(path)
        ));
    }

    if value.starts_with('/') {
        return Some(format!(
            "{app_prefix}{}",
            value.strip_prefix('/').unwrap_or(value)
        ));
    }

    None
}

async fn forward_to_port(
    port: u16,
    path: &str,
    app_handle: &str,
    original_request: Request,
) -> Response {
    let query = original_request.uri().query();
    let uri = match (path.is_empty(), query) {
        (true, None) => format!("http://127.0.0.1:{port}/"),
        (true, Some(q)) => format!("http://127.0.0.1:{port}/?{q}"),
        (false, None) => format!("http://127.0.0.1:{port}/{path}"),
        (false, Some(q)) => format!("http://127.0.0.1:{port}/{path}?{q}"),
    };

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };

    let (parts, body) = original_request.into_parts();

    let reqwest_method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let mut upstream_req = client.request(reqwest_method, &uri);

    for (key, value) in &parts.headers {
        match key.as_str() {
            "host" | "connection" | "transfer-encoding" | "authorization" | "cookie" => continue,
            _ => {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_ref())
                    && let Ok(val) = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                {
                    upstream_req = upstream_req.header(name, val);
                }
            }
        }
    }

    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes);
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    let app_prefix = format!("/apps/{app_handle}/");
    let mut builder = Response::builder().status(status);
    for (key, value) in upstream_resp.headers() {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(key.as_ref())
            && let Ok(val) = HeaderValue::from_bytes(value.as_bytes())
        {
            if (name == "location" || name == "content-location")
                && let Ok(loc_str) = value.to_str()
                && let Some(rewritten) = rewrite_location(loc_str, &app_prefix)
                && let Ok(new_val) = HeaderValue::from_str(&rewritten)
            {
                builder = builder.header(name, new_val);
                continue;
            }
            builder = builder.header(name, val);
        }
    }

    match upstream_resp.bytes().await {
        Ok(body) => builder
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_location_absolute_path() {
        assert_eq!(
            rewrite_location("/login", "/apps/x/"),
            Some("/apps/x/login".to_string())
        );
    }

    #[test]
    fn rewrite_location_root() {
        assert_eq!(
            rewrite_location("/", "/apps/x/"),
            Some("/apps/x/".to_string())
        );
    }

    #[test]
    fn rewrite_location_relative_path() {
        assert_eq!(rewrite_location("next-page", "/apps/x/"), None);
    }

    #[test]
    fn rewrite_location_external_url() {
        assert_eq!(
            rewrite_location("https://example.com/foo", "/apps/x/"),
            None
        );
    }

    #[test]
    fn rewrite_location_localhost_url() {
        assert_eq!(
            rewrite_location("http://127.0.0.1:3456/dashboard", "/apps/x/"),
            Some("/apps/x/dashboard".to_string())
        );
    }
}
