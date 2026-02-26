use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use openidconnect::core::{CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    RedirectUrl, Scope,
};
use tokio::sync::Mutex;

use super::models::OAuthIdentity;
use super::repository::OAuthRepository;
use crate::auth::token::service::TokenService;
use crate::auth::{AuthService, UserRepository};
use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::models::User;
use crate::credential::keypair::service::KeyPairService;

#[derive(Clone)]
pub struct OAuthService {
    authority: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    allow_unknown_email_verification: bool,
    signups_match_email: bool,
    pending_states: Arc<Mutex<HashMap<String, (String, Nonce)>>>,
    repo: Arc<dyn OAuthRepository>,
    redirect_uri: String,
}

impl OAuthService {
    pub fn new(config: &Config, repo: Arc<dyn OAuthRepository>) -> Result<Self, AppError> {
        let authority = config
            .sso
            .authority
            .clone()
            .ok_or_else(|| {
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
        let redirect_uri = format!("{}/api/auth/sso/callback", config.server.issuer_url);

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
        })
    }

    fn http_client(&self) -> Result<reqwest::Client, AppError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::Internal(format!("HTTP client error: {e}")))
    }

    fn issuer_url(&self) -> Result<IssuerUrl, AppError> {
        IssuerUrl::new(self.authority.clone())
            .map_err(|e| AppError::Internal(format!("Invalid SSO authority URL: {e}")))
    }

    fn redirect_url(&self) -> Result<RedirectUrl, AppError> {
        RedirectUrl::new(self.redirect_uri.clone())
            .map_err(|e| AppError::Internal(format!("Invalid redirect URI: {e}")))
    }

    pub async fn get_authorization_url(
        &self,
    ) -> Result<(String, String, String), AppError> {
        let http_client = self.http_client()?;
        let issuer_url = self.issuer_url()?;

        let provider_metadata =
            CoreProviderMetadata::discover_async(issuer_url, &http_client)
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

        self.pending_states
            .lock()
            .await
            .insert(csrf_secret.clone(), (nonce_secret.clone(), nonce));

        Ok((auth_url.to_string(), csrf_secret, nonce_secret))
    }

    pub async fn handle_callback(
        &self,
        code: &str,
        state: &str,
        user_repo: &dyn UserRepository,
        _keypair_svc: &KeyPairService,
        _token_svc: &TokenService,
    ) -> Result<(User, bool), AppError> {
        let (_nonce_secret, nonce) = self
            .pending_states
            .lock()
            .await
            .remove(state)
            .ok_or_else(|| AppError::Auth("Invalid or expired SSO state".into()))?;

        let http_client = self.http_client()?;
        let issuer_url = self.issuer_url()?;

        let provider_metadata =
            CoreProviderMetadata::discover_async(issuer_url, &http_client)
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
            .ok_or_else(|| AppError::Auth("No ID token in response".into()))?;

        let id_token_verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&id_token_verifier, &nonce)
            .map_err(|e| AppError::Auth(format!("ID token validation failed: {e}")))?;

        let external_sub = claims.subject().to_string();
        let external_email = claims.email().map(|e| e.to_string());
        let external_name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string());

        // Email verification gate
        if !self.allow_unknown_email_verification
            && let Some(verified) = claims.email_verified()
            && !verified
        {
            return Err(AppError::Auth(
                "Email not verified by SSO provider".into(),
            ));
        }

        // Look up existing identity
        if let Some(identity) = self.repo.find_identity_by_sub(&external_sub).await? {
            let user = user_repo
                .find_by_id(&identity.user_id)
                .await?
                .ok_or_else(|| AppError::Internal("Linked user not found".into()))?;
            return Ok((user, false));
        }

        // Try to match by email
        if self.signups_match_email
            && let Some(ref email) = external_email
            && let Some(existing_user) = user_repo.find_by_email(email).await?
        {
            let now = Utc::now();
            let identity = OAuthIdentity {
                id: uuid::Uuid::new_v4().to_string(),
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

        // Create new user
        let now = Utc::now();
        let base_username = if let Some(ref email) = external_email {
            AuthService::derive_username_from_email(email)
        } else {
            format!("sso-{external_sub}")
        };
        let username = AuthService::generate_unique_username(user_repo, &base_username).await?;

        let new_user = User {
            id: uuid::Uuid::new_v4().to_string(),
            username,
            email: external_email
                .clone()
                .unwrap_or_else(|| format!("sso-{external_sub}@unknown")),
            name: external_name
                .clone()
                .unwrap_or_else(|| "SSO User".to_string()),
            password_hash: String::new(),
            created_at: now,
            updated_at: now,
        };
        let user = user_repo.create(&new_user).await?;

        let identity = OAuthIdentity {
            id: uuid::Uuid::new_v4().to_string(),
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
