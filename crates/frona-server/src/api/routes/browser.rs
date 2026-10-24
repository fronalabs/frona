use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router, body::Body};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::time::Duration;
use tower_http::timeout::TimeoutBody;

use super::super::error::ApiError;
use crate::api::cookie::extract_refresh_token_from_cookie_header;
use crate::api::middleware::auth::AuthUser;
use crate::auth::models::Claims;
use crate::core::config::BrowserConfig;
use crate::core::error::{AppError, AuthErrorCode};
use crate::core::principal::PrincipalKind;
use crate::core::state::AppState;
use crate::credential::vault::models::{Credential, CredentialData};

// Only time spent waiting for a requested upstream frame counts, not downstream backpressure.
const DEBUGGER_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_COOKIE: &str = "browser_session";
// Browserless reads this setting before loading its UI. No backend address or token enters JS.
const ROUTER_SCRIPT: &str = r#"const base = new URL('.', location.href).href;
const key = 'browserless-debugger:' + location.origin + location.pathname;
let settings = {}; try { settings = JSON.parse(localStorage.getItem(key) || '{}'); } catch {}
settings.apiSettings = {...settings.apiSettings, baseURL: base};
localStorage.setItem(key, JSON.stringify(settings));"#;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/browser/snapshot", get(snapshot))
        .route("/api/auth/browser/{credential_id}", get(auth_gate))
        .route("/api/browser/debugger/{credential_id}", get(document))
        .route("/api/browser/debugger/{credential_id}/", get(document))
        .route(
            "/api/browser/debugger/{credential_id}/{*path}",
            get(resource),
        )
}

async fn snapshot(auth: AuthUser, State(state): State<AppState>) -> Result<Response, ApiError> {
    // Match BrowserTool's profile selection without exposing profile credentials.
    let provider = state
        .vault_service
        .list_credentials(&auth.user_id)
        .await?
        .into_iter()
        .next()
        .map(|credential| credential.provider)
        .unwrap_or_else(|| "default".to_string());
    let Some(connection) = state
        .browser_session_manager
        .existing_connection(&auth.handle, &provider)
        .await
    else {
        return Ok((StatusCode::NO_CONTENT, [("cache-control", "no-store")]).into_response());
    };
    let capture = tokio::time::timeout(Duration::from_secs(10), connection.screenshot_bytes(false))
        .await
        .map_err(|_| AppError::Browser("Browser snapshot timed out".into()))?;
    let bytes = match capture {
        Ok(bytes) => bytes,
        Err(frona_browser::Error::NoActivePage) => {
            return Ok((StatusCode::NO_CONTENT, [("cache-control", "no-store")]).into_response());
        }
        Err(_) => return Err(AppError::Browser("Browser snapshot unavailable".into()).into()),
    };
    Ok((
        [("content-type", "image/png"), ("cache-control", "no-store")],
        bytes,
    )
        .into_response())
}

async fn credential_for_user(
    state: &AppState,
    credential_id: &str,
    user_id: &str,
) -> Result<Credential, ApiError> {
    let credential = state
        .vault_service
        .find_credential_by_id(credential_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Credential not found".into()))?;
    if credential.user_id != user_id {
        return Err(AppError::Forbidden("Not your credential".into()).into());
    }
    if !matches!(credential.data, CredentialData::BrowserProfile) {
        return Err(AppError::Validation("Credential is not a browser profile".into()).into());
    }
    Ok(credential)
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    cookie::Cookie::split_parse(headers.get("cookie")?.to_str().ok()?)
        .flatten()
        .find(|cookie| cookie.name() == SESSION_COOKIE)
        .map(|cookie| cookie.value().to_owned())
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Claims, ApiError> {
    let claims = if let Some(header) = headers.get("authorization") {
        let token = header
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::Auth {
                message: "Invalid authorization format".into(),
                code: AuthErrorCode::InvalidCredentials,
            })?;
        state
            .token_service
            .validate_api(&state.keypair_service, token)
            .await?
    } else {
        let token = session_cookie(headers).ok_or_else(|| AppError::Auth {
            message: "Missing browser session".into(),
            code: AuthErrorCode::InvalidCredentials,
        })?;
        state
            .token_service
            .validate_access(&state.keypair_service, &token)
            .await?
    };
    if claims.principal.kind != PrincipalKind::User {
        return Err(AppError::Forbidden("This endpoint requires a user principal".into()).into());
    }
    Ok(claims)
}

async fn auth_gate(
    State(state): State<AppState>,
    Path(credential_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !safe_id(&credential_id) {
        return Err(AppError::NotFound("Credential not found".into()).into());
    }
    let gate = format!("/api/auth/browser/{credential_id}");
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let claims = match extract_refresh_token_from_cookie_header(cookie_header) {
        Some(token) => state
            .token_service
            .validate_refresh(&state.keypair_service, token)
            .await
            .ok(),
        None => None,
    };
    let Some(claims) = claims else {
        let return_to = format!("{}{gate}", state.config.server.public_base_url());
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("redirect", &return_to)
            .finish();
        return Ok(Redirect::temporary(&format!(
            "{}/login?{query}",
            state.config.server.public_frontend_url()
        ))
        .into_response());
    };
    credential_for_user(&state, &credential_id, &claims.sub).await?;
    let user = state
        .user_service
        .find_by_id(&claims.sub)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    if user.deactivated_at.is_some() {
        return Err(AppError::Forbidden("User is deactivated".into()).into());
    }
    let token = state
        .token_service
        .create_access_token(&state.keypair_service, &user, "browser_session")
        .await?;
    let path = format!("/api/browser/debugger/{credential_id}/");
    let cookie = cookie::Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .secure(
            state
                .config
                .server
                .public_base_url()
                .starts_with("https://"),
        )
        .path(path.clone())
        .max_age(cookie::time::Duration::seconds(
            state.config.auth.access_token_expiry_secs as i64,
        ))
        .build();
    Ok((
        [(axum::http::header::SET_COOKIE, cookie.to_string())],
        Redirect::temporary(&path),
    )
        .into_response())
}

async fn document(
    State(state): State<AppState>,
    Path(credential_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    if !safe_id(&credential_id) {
        return Err(AppError::NotFound("Credential not found".into()).into());
    }
    // Normal links cannot set a bearer header; the auth gate receives the existing refresh cookie.
    if !request.headers().contains_key("authorization")
        && session_cookie(request.headers()).is_none()
    {
        return Ok(
            Redirect::temporary(&format!("/api/auth/browser/{credential_id}")).into_response(),
        );
    }
    let claims = match authenticate(&state, request.headers()).await {
        Ok(claims) => claims,
        Err(_) if !request.headers().contains_key("authorization") && !is_websocket(&request) => {
            return Ok(
                Redirect::temporary(&format!("/api/auth/browser/{credential_id}")).into_response(),
            );
        }
        Err(error) => return Err(error),
    };
    let credential = credential_for_user(&state, &credential_id, &claims.sub).await?;
    if is_websocket(&request) {
        return websocket(&state, &claims, &credential, None, request).await;
    }
    if !request.uri().path().ends_with('/') {
        return Ok(
            Redirect::temporary(&format!("/api/browser/debugger/{credential_id}/")).into_response(),
        );
    }
    let config = browser_config(&state)?;
    let profile = config.profile_path(&claims.handle, &credential.provider);
    let url = config
        .http_url(
            &["debugger", ""],
            config.api_token.as_deref(),
            &[("--user-data-dir", &profile.to_string_lossy())],
        )
        .map_err(|_| AppError::Browser("Invalid Browserless debugger URL".into()))?;
    proxy(config, url).await
}

async fn resource(
    State(state): State<AppState>,
    Path((credential_id, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let claims = authenticate(&state, request.headers()).await?;
    let credential = credential_for_user(&state, &credential_id, &claims.sub).await?;
    if path == "sessions" {
        let sessions = state
            .browser_session_manager
            .debugger_sessions(&claims.handle, &credential.provider)
            .await?;
        let pages: Vec<_> = sessions
            .into_iter()
            .filter(|session| session.session_type.as_deref() == Some("page"))
            .filter_map(|session| {
                session.id.filter(|id| safe_id(id)).map(|id| {
                    serde_json::json!({
                        "id": id, "title": escape_html(&session.title), "url": session.url,
                    })
                })
            })
            .collect();
        return Ok(Json(pages).into_response());
    }
    if path == "router.js" {
        return Ok((
            [
                ("content-type", "application/javascript; charset=utf-8"),
                ("cache-control", "no-store"),
            ],
            ROUTER_SCRIPT,
        )
            .into_response());
    }
    if let Some(id) = path.strip_prefix("devtools/page/") {
        if !safe_id(id) || !is_websocket(&request) {
            return Err(AppError::NotFound("Browser target not found".into()).into());
        }
        return websocket(&state, &claims, &credential, Some(id.to_owned()), request).await;
    }
    // Only static debugger/devtools resources. Never forward management endpoints or caller queries.
    if !safe_asset(&path) {
        return Err(AppError::NotFound("Debugger resource not found".into()).into());
    }
    let mut segments: Vec<_> = path.split('/').collect();
    if !path.starts_with("devtools/") {
        segments.insert(0, "debugger");
    }
    let config = browser_config(&state)?;
    let url = config
        .http_url(&segments, config.api_token.as_deref(), &[])
        .map_err(|_| AppError::Browser("Invalid Browserless resource URL".into()))?;
    proxy(config, url).await
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn safe_asset(path: &str) -> bool {
    path.len() <= 512
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        && path.rsplit('.').next().is_some_and(|extension| {
            matches!(
                extension,
                "js" | "css"
                    | "html"
                    | "json"
                    | "map"
                    | "wasm"
                    | "png"
                    | "svg"
                    | "ico"
                    | "woff"
                    | "woff2"
                    | "ttf"
            )
        })
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn browser_config(state: &AppState) -> Result<&BrowserConfig, ApiError> {
    state
        .browser_session_manager
        .config()
        .ok_or_else(|| AppError::Browser("Browser is not configured".into()).into())
}

async fn proxy(config: &BrowserConfig, url: url::Url) -> Result<Response, ApiError> {
    let client = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::get(url.as_str())
        .body(Body::empty())
        .map_err(|_| AppError::Browser("Failed to build Browserless request".into()))?;
    let resp = tokio::time::timeout(
        Duration::from_millis(config.connection_timeout_ms),
        client.request(req),
    )
    .await
    .map_err(|_| AppError::Http {
        status: 504,
        message: "Browserless debugger response timed out".into(),
    })?
    .map_err(|_| AppError::Browser("Failed to proxy to Browserless".into()))?;
    if !resp.status().is_success() {
        return Err(AppError::Browser(format!(
            "Browserless debugger returned HTTP {}",
            resp.status()
        ))
        .into());
    }
    let (mut parts, body) = resp.into_parts();
    // Browserless auth and internal redirects must never become browser-visible credentials.
    parts.headers.remove("set-cookie");
    parts.headers.remove("location");
    Ok(Response::from_parts(
        parts,
        Body::new(TimeoutBody::new(DEBUGGER_BODY_IDLE_TIMEOUT, body)),
    ))
}

fn is_websocket(request: &Request) -> bool {
    request
        .headers()
        .get("upgrade")
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

async fn websocket(
    state: &AppState,
    claims: &Claims,
    credential: &Credential,
    page_id: Option<String>,
    mut request: Request,
) -> Result<Response, ApiError> {
    let config = browser_config(state)?;
    // Include profile initialization/discovery in the handshake deadline.
    tokio::time::timeout(Duration::from_millis(config.connection_timeout_ms), async {
        // Cookies authenticate upgrades, so reject cross-origin browser WebSocket requests.
        if let Some(origin) = request.headers().get("origin") {
            let origin = origin
                .to_str()
                .ok()
                .and_then(|value| url::Url::parse(value).ok());
            let host = request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok());
            let public_url = url::Url::parse(&state.config.server.public_base_url()).ok();
            let scheme = public_url
                .as_ref()
                .map(url::Url::scheme)
                .or_else(|| request.uri().scheme_str())
                .unwrap_or("http");
            if !origin.is_some_and(|url| {
                url.scheme() == scheme
                    && url.username().is_empty()
                    && url.password().is_none()
                    && Some(&url[url::Position::BeforeHost..url::Position::AfterPort]) == host
            }) {
                return Err(AppError::Forbidden("Cross-origin debugger connection".into()).into());
            }
        }
        // Ensure a profile exists; connection() shares the agent's existing browser and never launches from caller options.
        let _connection = state
            .browser_session_manager
            .connection(&claims.handle, &credential.provider)
            .await?;
        let sessions = state
            .browser_session_manager
            .debugger_sessions(&claims.handle, &credential.provider)
            .await?;
        let path = if let Some(id) = page_id {
            if !sessions
                .iter()
                .any(|session| session.id.as_deref() == Some(&id))
            {
                return Err(AppError::Forbidden(
                    "Browser target does not belong to this profile".into(),
                )
                .into());
            }
            vec!["devtools".to_owned(), "page".to_owned(), id]
        } else {
            let id = sessions
                .iter()
                .filter_map(|session| session.browser_ws_endpoint.as_deref())
                .filter_map(|endpoint| url::Url::parse(endpoint).ok())
                .find_map(|url| {
                    url.path()
                        .strip_prefix("/devtools/browser/")
                        .filter(|id| safe_id(id))
                        .map(str::to_owned)
                })
                .ok_or_else(|| AppError::Browser("Browser profile has no live target".into()))?;
            vec!["devtools".to_owned(), "browser".to_owned(), id]
        };
        let segments: Vec<_> = path.iter().map(String::as_str).collect();
        let url = config
            .http_url(&segments, config.api_token.as_deref(), &[])
            .map_err(|_| AppError::Browser("Invalid Browserless target URL".into()))?;
        let mut upstream = Request::get(url.as_str()).body(Body::empty()).unwrap();
        for name in [
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
        ] {
            if let Some(value) = request.headers().get(name) {
                upstream.headers_mut().insert(name, value.clone());
            }
        }
        let client = Client::builder(TokioExecutor::new()).build_http();
        let mut response = client
            .request(upstream)
            .await
            .map_err(|_| AppError::Browser("Browserless debugger connection failed".into()))?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(AppError::Browser("Browserless debugger refused connection".into()).into());
        }
        let downstream = hyper::upgrade::on(&mut request);
        let upstream = hyper::upgrade::on(&mut response);
        let mut result = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        for name in ["connection", "upgrade", "sec-websocket-accept"] {
            if let Some(value) = response.headers().get(name) {
                result = result.header(name, value);
            }
        }
        tokio::spawn(async move {
            if let (Ok(downstream), Ok(upstream)) = tokio::join!(downstream, upstream) {
                let _ = tokio::io::copy_bidirectional(
                    &mut TokioIo::new(downstream),
                    &mut TokioIo::new(upstream),
                )
                .await;
            }
        });
        Ok(result.body(Body::empty()).unwrap())
    })
    .await
    .map_err(|_| AppError::Http {
        status: 504,
        message: "Browserless debugger connection timed out".into(),
    })?
}
