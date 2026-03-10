use std::collections::HashMap;
use std::sync::Arc;

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::Aead,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{DecodingKey, EncodingKey};
use tokio::sync::RwLock;

use crate::credential::key_rotation::derive_key;

use super::models::KeyPair;
use super::repository::KeyPairRepository;
use crate::core::error::AppError;

#[derive(Clone)]
pub struct KeyPairService {
    encryption_key: [u8; 32],
    repo: Arc<dyn KeyPairRepository>,
    signing_cache: Arc<RwLock<HashMap<String, (EncodingKey, String)>>>,
    verifying_cache: Arc<RwLock<HashMap<String, DecodingKey>>>,
}

impl KeyPairService {
    pub fn new(encryption_secret: &str, repo: Arc<dyn KeyPairRepository>) -> Self {
        let encryption_key = derive_key(encryption_secret);

        Self {
            encryption_key,
            repo,
            signing_cache: Arc::new(RwLock::new(HashMap::new())),
            verifying_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_or_create(&self, owner: &str) -> Result<KeyPair, AppError> {
        if let Some(kp) = self.repo.find_active_by_owner(owner).await? {
            return Ok(kp);
        }

        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let private_bytes = signing_key.to_bytes();
        let public_bytes = signing_key.verifying_key().to_bytes();

        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);

        let encrypted = cipher
            .encrypt(nonce, private_bytes.as_ref())
            .map_err(|e| AppError::Internal(format!("Encryption failed: {e}")))?;

        let now = chrono::Utc::now();
        let kp = KeyPair {
            id: uuid::Uuid::new_v4().to_string(),
            owner: owner.to_string(),
            public_key_bytes: public_bytes.to_vec(),
            private_key_enc: encrypted,
            nonce: nonce_bytes.to_vec(),
            active: true,
            created_at: now,
            updated_at: now,
        };

        self.repo.create(&kp).await
    }

    pub async fn get_signing_key(
        &self,
        owner: &str,
    ) -> Result<(EncodingKey, String), AppError> {
        {
            let cache = self.signing_cache.read().await;
            if let Some(cached) = cache.get(owner) {
                return Ok(cached.clone());
            }
        }

        let kp = self.get_or_create(owner).await?;
        let private_bytes = match self.decrypt_private_key(&kp) {
            Ok(bytes) => bytes,
            Err(AppError::Decryption(e)) => {
                tracing::warn!(
                    owner = owner,
                    error = %e,
                    "Failed to decrypt keypair, regenerating (encryption secret changed or stored key corrupted)"
                );
                self.signing_cache.write().await.remove(owner);
                self.verifying_cache.write().await.remove(owner);
                self.repo.delete(&kp.id).await?;
                let new_kp = self.get_or_create(owner).await?;
                self.decrypt_private_key(&new_kp)?
            }
            Err(e) => return Err(e),
        };

        let kid = kp.owner.clone();

        // Build minimal PKCS#8 wrapper for Ed25519 private key
        let mut pkcs8 = Vec::with_capacity(48);
        pkcs8.extend_from_slice(&[
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ]);
        pkcs8.extend_from_slice(&private_bytes);

        let encoding_key = EncodingKey::from_ed_der(&pkcs8);
        let result = (encoding_key.clone(), kid.clone());
        self.signing_cache
            .write()
            .await
            .insert(owner.to_string(), result.clone());
        Ok(result)
    }

    pub async fn get_verifying_key(&self, kid: &str) -> Result<DecodingKey, AppError> {
        {
            let cache = self.verifying_cache.read().await;
            if let Some(cached) = cache.get(kid) {
                return Ok(cached.clone());
            }
        }

        let kp = self
            .repo
            .find_by_kid(kid)
            .await?
            .ok_or_else(|| AppError::Auth(format!("Key not found for kid: {kid}")))?;

        let key = DecodingKey::from_ed_der(&kp.public_key_bytes);
        self.verifying_cache
            .write()
            .await
            .insert(kid.to_string(), key.clone());
        Ok(key)
    }

    pub async fn list_jwks(&self) -> Result<Vec<serde_json::Value>, AppError> {
        let keys = self.repo.find_all_active().await?;
        let mut jwks = Vec::with_capacity(keys.len());
        for kp in keys {
            let x = URL_SAFE_NO_PAD.encode(&kp.public_key_bytes);
            jwks.push(serde_json::json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "x": x,
                "kid": kp.owner,
                "use": "sig",
                "alg": "EdDSA",
            }));
        }
        Ok(jwks)
    }

    fn decrypt_private_key(&self, kp: &KeyPair) -> Result<[u8; 32], AppError> {
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce = Nonce::from_slice(&kp.nonce);
        let decrypted = cipher
            .decrypt(nonce, kp.private_key_enc.as_ref())
            .map_err(|e| AppError::Decryption(format!("Decryption failed: {e}")))?;

        let mut key = [0u8; 32];
        key.copy_from_slice(&decrypted);
        Ok(key)
    }
}
