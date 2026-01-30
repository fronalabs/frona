use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};

use crate::dto::Claims;
use crate::error::AppError;
use crate::models::User;

pub struct JwtService {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

impl JwtService {
    pub fn new(secret: &str) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
        }
    }

    pub fn generate(&self, user: &User) -> Result<String, AppError> {
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: user.id.clone(),
            email: user.email.clone(),
            iat: now,
            exp: now + 24 * 3600, // 24 hours
        };

        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AppError::Internal(format!("Token generation failed: {e}")))
    }

    pub fn validate(&self, token: &str) -> Result<Claims, AppError> {
        let data = decode::<Claims>(token, &self.decoding_key, &Validation::default())
            .map_err(|e| AppError::Auth(format!("Invalid token: {e}")))?;
        Ok(data.claims)
    }
}
