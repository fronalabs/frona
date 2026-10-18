use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreUserInfoClaims};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
    RedirectUrl, Scope, SubjectIdentifier,
};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::models::OAuthIdentity;
use super::repository::OAuthRepository;
use crate::auth::token::service::TokenService;
use crate::auth::{AuthService, User, UserService};
use crate::core::config::Config;
use crate::core::error::{AppError, AuthErrorCode};
use crate::credential::keypair::service::KeyPairService;

const PENDING_STATE_TTL: Duration = Duration::from_secs(10 * 60);

struct PendingState {
    nonce: Nonce,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct OAuthService {
    authority: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    allow_unknown_email_verification: bool,
    signups_match_email: bool,
    pending_states: Arc<Mutex<HashMap<String, PendingState>>>,
    repo: Arc<dyn OAuthRepository>,
    redirect_uri: String,
    http: openidconnect::reqwest::Client,
}

impl OAuthService {
    pub fn new(config: &Config, repo: Arc<dyn OAuthRepository>) -> Result<Self, AppError> {
        let authority = config.sso.authority.clone().ok_or_else(|| {
            AppError::Validation("FRONA_SSO_AUTHORITY is required when SSO is enabled".into())
        })?;
        let client_id = config.sso.client_id.clone().ok_or_else(|| {
            AppError::Validation("FRONA_SSO_CLIENT_ID is required when SSO is enabled".into())
        })?;
        let client_secret = config.sso.client_secret.clone().ok_or_else(|| {
            AppError::Validation("FRONA_SSO_CLIENT_SECRET is required when SSO is enabled".into())
        })?;

        let scopes: Vec<String> = config
            .sso
            .scopes
            .split_whitespace()
            .map(String::from)
            .collect();
        let base = config.server.public_base_url();
        if base.is_empty() {
            return Err(AppError::Validation(
                "SSO requires server.base_url or server.backend_url to be set".into(),
            ));
        }
        let redirect_uri = format!("{base}/api/auth/sso/callback");

        Ok(Self {
            authority,
            client_id,
            client_secret,
            scopes,
            allow_unknown_email_verification: config.sso.allow_unknown_email_verification,
            signups_match_email: config.sso.signups_match_email,
            pending_states: Arc::new(Mutex::new(HashMap::new())),
            repo,
            redirect_uri,
            http: openidconnect::reqwest::Client::new(),
        })
    }

    fn issuer_url(&self) -> Result<IssuerUrl, AppError> {
        IssuerUrl::new(self.authority.clone())
            .map_err(|e| AppError::Internal(format!("Invalid SSO authority URL: {e}")))
    }

    fn redirect_url(&self) -> Result<RedirectUrl, AppError> {
        RedirectUrl::new(self.redirect_uri.clone())
            .map_err(|e| AppError::Internal(format!("Invalid redirect URI: {e}")))
    }

    pub async fn get_authorization_url(&self) -> Result<(String, String, String), AppError> {
        let http_client = self.http.clone();
        let issuer_url = self.issuer_url()?;

        let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, &http_client)
            .await
            .map_err(|e| AppError::Internal(format!("OIDC discovery failed: {e}")))?;

        let client = openidconnect::core::CoreClient::from_provider_metadata(
            provider_metadata,
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
        )
        .set_redirect_uri(self.redirect_url()?);

        let mut auth_request = client.authorize_url(
            openidconnect::AuthenticationFlow::<openidconnect::core::CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );

        for scope in &self.scopes {
            auth_request = auth_request.add_scope(Scope::new(scope.clone()));
        }

        let (auth_url, csrf_state, nonce) = auth_request.url();

        let csrf_secret = csrf_state.secret().clone();
        let nonce_secret = nonce.secret().clone();

        self.store_pending_state(csrf_secret.clone(), nonce).await;

        Ok((auth_url.to_string(), csrf_secret, nonce_secret))
    }

    async fn store_pending_state(&self, state: String, nonce: Nonce) {
        let mut pending_states = self.pending_states.lock().await;
        let now = Instant::now();
        pending_states.retain(|_, pending| pending.expires_at > now);
        pending_states.insert(
            state,
            PendingState {
                nonce,
                expires_at: now + PENDING_STATE_TTL,
            },
        );
    }

    async fn consume_state(&self, state: &str) -> Result<Nonce, AppError> {
        let mut pending_states = self.pending_states.lock().await;
        let now = Instant::now();
        // Removal and expiry validation share the lock: concurrent callbacks cannot
        // reuse a state, and pruning is only housekeeping, not the expiry check.
        let pending = pending_states.remove(state);
        pending_states.retain(|_, pending| pending.expires_at > now);
        pending
            .filter(|pending| pending.expires_at > now)
            .map(|pending| pending.nonce)
            .ok_or_else(|| AppError::Auth {
                message: "Invalid or expired SSO state".into(),
                code: AuthErrorCode::CsrfFailed,
            })
    }

    pub async fn handle_callback(
        &self,
        code: &str,
        state: &str,
        user_service: &UserService,
        _keypair_svc: &KeyPairService,
        _token_svc: &TokenService,
    ) -> Result<(User, bool), AppError> {
        let nonce = self.consume_state(state).await?;

        let http_client = self.http.clone();
        let issuer_url = self.issuer_url()?;

        let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, &http_client)
            .await
            .map_err(|e| AppError::Internal(format!("OIDC discovery failed: {e}")))?;

        let client = CoreClient::from_provider_metadata(
            provider_metadata,
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
        )
        .set_redirect_uri(self.redirect_url()?);

        let token_response = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| AppError::Internal(format!("Token endpoint configuration error: {e}")))?
            .request_async(&http_client)
            .await
            .map_err(|e| AppError::Internal(format!("Token exchange failed: {e}")))?;

        let id_token = token_response
            .extra_fields()
            .id_token()
            .ok_or_else(|| AppError::Auth {
                message: "No ID token in response".into(),
                code: AuthErrorCode::TokenFailed,
            })?;

        let id_token_verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&id_token_verifier, &nonce)
            .map_err(|e| AppError::Auth {
                message: format!("ID token validation failed: {e}"),
                code: AuthErrorCode::TokenInvalid,
            })?;

        let external_sub = claims.subject().to_string();
        let mut external_email = claims
            .email()
            .map(|e| AuthService::normalize_email(e.as_str()));
        let mut external_name = pick_name(
            claims.name().and_then(|n| n.get(None)).map(|n| n.as_str()),
            claims
                .given_name()
                .and_then(|n| n.get(None))
                .map(|n| n.as_str()),
            claims
                .family_name()
                .and_then(|n| n.get(None))
                .map(|n| n.as_str()),
            claims.preferred_username().map(|n| n.as_str()),
        );

        if !self.allow_unknown_email_verification
            && let Some(verified) = claims.email_verified()
            && !verified
        {
            return Err(AppError::Auth {
                message: "Email not verified by SSO provider".into(),
                code: AuthErrorCode::EmailNotVerified,
            });
        }

        if let Some(identity) = self.repo.find_identity_by_sub(&external_sub).await? {
            match user_service.find_by_id(&identity.user_id).await? {
                Some(user) => {
                    if user.deactivated_at.is_some() {
                        return Err(AppError::Auth {
                            message: "Account deactivated".into(),
                            code: AuthErrorCode::AccountDeactivated,
                        });
                    }
                    return Ok((user, false));
                }
                None => {
                    tracing::warn!(
                        identity_id = %identity.id,
                        user_id = %identity.user_id,
                        "Dropping orphaned SSO identity whose user no longer exists"
                    );
                    self.repo.delete(&identity.id).await?;
                }
            }
        }

        let mut matched_user: Option<User> = None;
        if self.signups_match_email
            && let Some(ref email) = external_email
        {
            matched_user = user_service.find_by_email(email).await?;
        }

        // Fall back to the userinfo endpoint when the ID token alone didn't
        // yield a mergeable email. Some IdPs (e.g. configurations that emit
        // bare-sub ID tokens) only return email/name from userinfo.
        if self.signups_match_email && matched_user.is_none() {
            match fetch_userinfo(&client, &http_client, &token_response, &external_sub).await {
                Ok(Some(info)) => {
                    let info_email = info
                        .email()
                        .map(|e| AuthService::normalize_email(e.as_str()));
                    let info_name = pick_name(
                        info.name().and_then(|n| n.get(None)).map(|n| n.as_str()),
                        info.given_name()
                            .and_then(|n| n.get(None))
                            .map(|n| n.as_str()),
                        info.family_name()
                            .and_then(|n| n.get(None))
                            .map(|n| n.as_str()),
                        info.preferred_username().map(|n| n.as_str()),
                    );
                    if external_name.is_none() {
                        external_name = info_name;
                    }
                    if let Some(email) = info_email {
                        if external_email.as_deref() != Some(email.as_str()) {
                            matched_user = user_service.find_by_email(&email).await?;
                        }
                        if external_email.is_none() {
                            external_email = Some(email);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "UserInfo fetch failed, falling back to ID token claims only");
                }
            }
        }

        if let Some(existing_user) = matched_user {
            if existing_user.deactivated_at.is_some() {
                return Err(AppError::Auth {
                    message: "Account deactivated".into(),
                    code: AuthErrorCode::AccountDeactivated,
                });
            }
            let now = Utc::now();
            let identity = OAuthIdentity {
                id: crate::core::repository::new_id(),
                user_id: existing_user.id.clone(),
                external_sub,
                external_email: external_email.clone(),
                external_name,
                created_at: now,
                updated_at: now,
            };
            self.repo.create(&identity).await?;
            return Ok((existing_user, false));
        }

        let now = Utc::now();
        let base_handle = if let Some(ref email) = external_email {
            AuthService::derive_handle_from_email(email)
        } else {
            format!("sso-{external_sub}")
        };
        let handle = AuthService::generate_unique_handle(user_service, &base_handle).await?;

        let new_user = User {
            id: crate::core::repository::new_id(),
            handle,
            email: external_email
                .clone()
                .unwrap_or_else(|| format!("sso-{external_sub}@unknown")),
            name: external_name
                .clone()
                .or_else(|| {
                    external_email
                        .as_deref()
                        .and_then(|e| e.split('@').next())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "SSO User".to_string()),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        };
        let user = user_service.create(&new_user).await?;
        user_service.ensure_admin_invariant().await?;

        let identity = OAuthIdentity {
            id: crate::core::repository::new_id(),
            user_id: user.id.clone(),
            external_sub,
            external_email,
            external_name,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&identity).await?;

        Ok((user, true))
    }
}

fn pick_name(
    name: Option<&str>,
    given_name: Option<&str>,
    family_name: Option<&str>,
    preferred_username: Option<&str>,
) -> Option<String> {
    fn trimmed(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(s) = trimmed(name) {
        return Some(s.to_string());
    }
    let given = trimmed(given_name);
    let family = trimmed(family_name);
    if given.is_some() || family.is_some() {
        let mut out = String::new();
        if let Some(g) = given {
            out.push_str(g);
        }
        if let Some(f) = family {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(f);
        }
        return Some(out);
    }
    trimmed(preferred_username).map(str::to_string)
}

async fn fetch_userinfo(
    client: &CoreClient<
        openidconnect::EndpointSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointMaybeSet,
        openidconnect::EndpointMaybeSet,
    >,
    http_client: &openidconnect::reqwest::Client,
    token_response: &openidconnect::core::CoreTokenResponse,
    expected_sub: &str,
) -> Result<Option<CoreUserInfoClaims>, String> {
    let request = match client.user_info(
        token_response.access_token().to_owned(),
        Some(SubjectIdentifier::new(expected_sub.to_string())),
    ) {
        Ok(req) => req,
        Err(_) => return Ok(None),
    };
    request
        .request_async(http_client)
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use surrealdb::Surreal;
    use surrealdb::engine::local::Db;

    use super::*;
    use crate::db::repo::generic::SurrealRepo;

    fn service() -> OAuthService {
        let mut config = Config::default();
        config.sso.authority = Some("http://unused.invalid".into());
        config.sso.client_id = Some("test-client".into());
        config.sso.client_secret = Some("test-secret".into());
        config.server.backend_url = Some("http://localhost:3001".into());
        // State-map tests need no database connection or provider HTTP requests.
        let repo = SurrealRepo::new(Surreal::<Db>::init());
        OAuthService::new(&config, Arc::new(repo)).unwrap()
    }

    fn assert_csrf<T: std::fmt::Debug>(result: Result<T, AppError>) {
        assert!(
            matches!(
                result,
                Err(AppError::Auth {
                    code: AuthErrorCode::CsrfFailed,
                    ..
                })
            ),
            "expected rejected SSO state, got {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn state_deadline_is_exclusive_without_another_authorization() {
        let service = service();
        for (state, elapsed, valid) in [
            ("before", PENDING_STATE_TTL - Duration::from_nanos(1), true),
            ("at", PENDING_STATE_TTL, false),
            ("after", PENDING_STATE_TTL + Duration::from_nanos(1), false),
        ] {
            service
                .store_pending_state(state.into(), Nonce::new(format!("nonce-{state}")))
                .await;
            tokio::time::advance(elapsed).await;
            let consumed = service.consume_state(state).await;
            if valid {
                assert_eq!(consumed.unwrap().secret(), "nonce-before");
            } else {
                assert_csrf(consumed);
            }
            assert_csrf(service.consume_state(state).await);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn state_expiring_while_waiting_for_the_lock_is_rejected() {
        let service = service();
        service
            .store_pending_state("waiting".into(), Nonce::new("nonce".into()))
            .await;
        let guard = service.pending_states.lock().await;
        let callback = service.consume_state("waiting");
        tokio::pin!(callback);
        // Poll the consumer while the map is locked, then reach its deadline.
        assert!(futures::poll!(&mut callback).is_pending());
        tokio::time::advance(PENDING_STATE_TTL).await;
        drop(guard);
        assert_csrf(callback.await);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_state_consumption_prunes_expired_states_but_keeps_live_states() {
        let service = service();
        service
            .store_pending_state("expired".into(), Nonce::new("old-nonce".into()))
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        service
            .store_pending_state("live".into(), Nonce::new("live-nonce".into()))
            .await;
        tokio::time::advance(PENDING_STATE_TTL - Duration::from_secs(1)).await;
        assert_csrf(service.consume_state("unknown").await);
        let pending = service.pending_states.lock().await;
        assert!(!pending.contains_key("expired"));
        assert!(pending.contains_key("live"));
    }

    #[tokio::test(start_paused = true)]
    async fn storing_state_sets_ten_minute_deadline_and_prunes_old_states() {
        let service = service();
        service
            .store_pending_state("old".into(), Nonce::new("old-nonce".into()))
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        service
            .store_pending_state("live".into(), Nonce::new("live-nonce".into()))
            .await;
        tokio::time::advance(PENDING_STATE_TTL - Duration::from_secs(1)).await;
        let now = Instant::now();
        service
            .store_pending_state("new".into(), Nonce::new("new-nonce".into()))
            .await;
        let pending = service.pending_states.lock().await;
        assert!(!pending.contains_key("old"));
        assert!(pending.contains_key("live"));
        assert_eq!(pending["new"].nonce.secret(), "new-nonce");
        assert_eq!(pending["new"].expires_at, now + PENDING_STATE_TTL);
    }
}
