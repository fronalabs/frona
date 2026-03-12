use crate::auth::jwt::JwtService;
use crate::auth::UserService;
use crate::chat::message::models::MessageResponse;
use crate::core::error::AppError;
use crate::credential::keypair::service::KeyPairService;

use crate::api::files::PresignClaims;
use crate::api::files::attachment_url_segment;

#[derive(Clone)]
pub struct PresignService {
    keypair_svc: KeyPairService,
    jwt_svc: JwtService,
    user_service: UserService,
    issuer_url: String,
    expiry_secs: u64,
}

impl PresignService {
    pub fn new(
        keypair_svc: KeyPairService,
        user_service: UserService,
        issuer_url: String,
        expiry_secs: u64,
    ) -> Self {
        Self {
            keypair_svc,
            jwt_svc: JwtService::new(),
            user_service,
            issuer_url,
            expiry_secs,
        }
    }

    pub async fn sign(
        &self,
        owner: &str,
        path: &str,
        user_id: &str,
        username: &str,
    ) -> Result<String, AppError> {
        let segment = match attachment_url_segment(owner, path, Some(username)) {
            Some(s) => s,
            None => return Ok(String::new()),
        };

        let keypair_owner = format!("user:{user_id}");
        let (encoding_key, kid) = self.keypair_svc.get_signing_key(&keypair_owner).await?;

        let exp = (chrono::Utc::now().timestamp() as u64 + self.expiry_secs) as usize;
        let claims = PresignClaims {
            sub: user_id.to_string(),
            owner: owner.to_string(),
            path: path.to_string(),
            exp,
        };

        let token = self.jwt_svc.sign(&claims, &encoding_key, &kid)?;
        Ok(format!(
            "{}/api/files/{segment}?presign={token}",
            self.issuer_url
        ))
    }

    pub async fn sign_by_user_id(
        &self,
        owner: &str,
        path: &str,
        user_id: &str,
    ) -> Result<String, AppError> {
        let username = self.resolve_username(user_id).await?;
        self.sign(owner, path, user_id, &username).await
    }

    pub async fn verify(&self, token: &str) -> Result<PresignClaims, AppError> {
        let header = self.jwt_svc.decode_unverified_header(token)?;
        let kid = header
            .kid
            .ok_or_else(|| AppError::Auth("Token missing kid".into()))?;

        let decoding_key = self.keypair_svc.get_verifying_key(&kid).await?;
        self.jwt_svc.verify::<PresignClaims>(token, &decoding_key)
    }

    async fn resolve_username(&self, user_id: &str) -> Result<String, AppError> {
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User {user_id} not found")))?;
        Ok(user.username)
    }
}

pub async fn presign_response(
    svc: &PresignService,
    msg: &mut MessageResponse,
    user_id: &str,
    username: &str,
) {
    for att in &mut msg.attachments {
        match svc.sign(&att.owner, &att.path, user_id, username).await {
            Ok(url) if !url.is_empty() => att.url = Some(url),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %att.path, "Failed to presign attachment");
            }
        }
    }
}

pub async fn presign_response_by_user_id(
    svc: &PresignService,
    msg: &mut MessageResponse,
    user_id: &str,
) {
    for att in &mut msg.attachments {
        match svc.sign_by_user_id(&att.owner, &att.path, user_id).await {
            Ok(url) if !url.is_empty() => att.url = Some(url),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %att.path, "Failed to presign attachment");
            }
        }
    }
}
