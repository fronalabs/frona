use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use sha2::{Digest, Sha256};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::{error, info, warn};

use crate::core::error::AppError;

const RUNTIME_CONFIG_KEY: &str = "encryption_secret";

pub fn derive_key(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

#[derive(Debug)]
pub struct RotationReport {
    pub vault_connections: RotationCounts,
    pub credentials: RotationCounts,
    pub keypairs: RotationCounts,
}

#[derive(Debug)]
pub struct RotationCounts {
    pub success: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl RotationReport {
    pub fn all_succeeded(&self) -> bool {
        self.vault_connections.failed == 0
            && self.credentials.failed == 0
            && self.keypairs.failed == 0
    }
}

pub struct KeyRotation {
    old_key: [u8; 32],
    new_key: [u8; 32],
    new_secret: String,
    db: Surreal<Db>,
}

impl KeyRotation {
    pub async fn check(db: &Surreal<Db>, config_secret: &str) -> Result<Option<Self>, AppError> {
        let mut result = db
            .query("SELECT `value` FROM runtime_config WHERE `key` = $key LIMIT 1")
            .bind(("key", RUNTIME_CONFIG_KEY.to_string()))
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read runtime_config: {e}")))?;

        let row: Option<serde_json::Value> = result
            .take(0)
            .map_err(|e| AppError::Internal(format!("Failed to parse runtime_config: {e}")))?;

        let stored_secret =
            row.and_then(|v| v.get("value").and_then(|v| v.as_str().map(String::from)));

        match stored_secret {
            None => {
                info!("No stored encryption secret found, saving current secret to runtime_config");
                store_secret(db, config_secret).await?;
                Ok(None)
            }
            Some(ref s) if s == config_secret => Ok(None),
            Some(old_secret) => {
                info!("Encryption secret changed, key rotation required");
                Ok(Some(Self {
                    old_key: derive_key(&old_secret),
                    new_key: derive_key(config_secret),
                    new_secret: config_secret.to_string(),
                    db: db.clone(),
                }))
            }
        }
    }

    pub async fn run(self) -> Result<RotationReport, AppError> {
        let vc = self.rotate_vault_connections().await;
        let cr = self.rotate_credentials().await;
        let kp = self.rotate_keypairs().await;

        let report = RotationReport {
            vault_connections: vc,
            credentials: cr,
            keypairs: kp,
        };

        info!(
            vault_connections = ?report.vault_connections,
            credentials = ?report.credentials,
            keypairs = ?report.keypairs,
            "Key rotation complete"
        );

        if report.all_succeeded() {
            store_secret(&self.db, &self.new_secret).await?;
            info!("Updated runtime_config with new encryption secret");
        } else {
            warn!(
                "Some records failed to rotate — runtime_config NOT updated, will retry on next restart"
            );
        }

        Ok(report)
    }

    async fn rotate_vault_connections(&self) -> RotationCounts {
        let rows: Vec<serde_json::Value> = match self
            .db
            .query("SELECT meta::id(id) as rid, config_encrypted, nonce FROM vault_connection WHERE array::len(config_encrypted) > 0")
            .await
            .and_then(|mut r| r.take(0))
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to query vault_connection for rotation");
                return RotationCounts {
                    success: 0,
                    skipped: 0,
                    failed: 1,
                };
            }
        };

        let mut counts = RotationCounts {
            success: 0,
            skipped: 0,
            failed: 0,
        };

        for row in rows {
            let Some(rid) = row.get("rid").and_then(|v| v.as_str()) else {
                counts.failed += 1;
                continue;
            };
            let (Some(enc_bytes), Some(nonce_bytes)) = (
                json_to_bytes(row.get("config_encrypted")),
                json_to_bytes(row.get("nonce")),
            ) else {
                counts.skipped += 1;
                continue;
            };

            match self.reencrypt_blob(&enc_bytes, &nonce_bytes) {
                Ok((new_enc, new_nonce)) => {
                    if let Err(e) = self
                        .db
                        .query("UPDATE type::record('vault_connection', $rid) SET config_encrypted = $enc, nonce = $nonce, updated_at = $now")
                        .bind(("rid", rid.to_string()))
                        .bind(("enc", new_enc))
                        .bind(("nonce", new_nonce))
                        .bind(("now", Utc::now()))
                        .await
                    {
                        error!(id = rid, error = %e, "Failed to update vault_connection");
                        counts.failed += 1;
                    } else {
                        counts.success += 1;
                    }
                }
                Err(RotateError::AlreadyRotated) => {
                    counts.skipped += 1;
                }
                Err(RotateError::Failed(e)) => {
                    error!(id = rid, error = e, "Failed to re-encrypt vault_connection config");
                    counts.failed += 1;
                }
            }
        }

        counts
    }

    async fn rotate_credentials(&self) -> RotationCounts {
        let rows: Vec<serde_json::Value> = match self
            .db
            .query("SELECT meta::id(id) as rid, data FROM credential")
            .await
            .and_then(|mut r| r.take(0))
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to query credential for rotation");
                return RotationCounts {
                    success: 0,
                    skipped: 0,
                    failed: 1,
                };
            }
        };

        let mut counts = RotationCounts {
            success: 0,
            skipped: 0,
            failed: 0,
        };

        for row in rows {
            let Some(rid) = row.get("rid").and_then(|v| v.as_str()) else {
                counts.failed += 1;
                continue;
            };
            let Some(data) = row.get("data") else {
                counts.skipped += 1;
                continue;
            };

            let data_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let data_obj = data.get("data");

            match data_type {
                "BrowserProfile" => {
                    counts.skipped += 1;
                }
                "UsernamePassword" => {
                    let Some(enc_b64) = data_obj
                        .and_then(|d| d.get("password_encrypted"))
                        .and_then(|v| v.as_str())
                    else {
                        counts.skipped += 1;
                        continue;
                    };

                    match self.reencrypt_b64(enc_b64) {
                        Ok(new_b64) => {
                            let mut new_data = data.clone();
                            new_data["data"]["password_encrypted"] =
                                serde_json::Value::String(new_b64);
                            if let Err(e) =
                                self.update_credential_data(rid, &new_data).await
                            {
                                error!(id = rid, error = e, "Failed to update credential");
                                counts.failed += 1;
                            } else {
                                counts.success += 1;
                            }
                        }
                        Err(RotateError::AlreadyRotated) => counts.skipped += 1,
                        Err(RotateError::Failed(e)) => {
                            error!(id = rid, error = e, "Failed to re-encrypt credential password");
                            counts.failed += 1;
                        }
                    }
                }
                "ApiKey" => {
                    let Some(enc_b64) = data_obj
                        .and_then(|d| d.get("key_encrypted"))
                        .and_then(|v| v.as_str())
                    else {
                        counts.skipped += 1;
                        continue;
                    };

                    match self.reencrypt_b64(enc_b64) {
                        Ok(new_b64) => {
                            let mut new_data = data.clone();
                            new_data["data"]["key_encrypted"] =
                                serde_json::Value::String(new_b64);
                            if let Err(e) =
                                self.update_credential_data(rid, &new_data).await
                            {
                                error!(id = rid, error = e, "Failed to update credential");
                                counts.failed += 1;
                            } else {
                                counts.success += 1;
                            }
                        }
                        Err(RotateError::AlreadyRotated) => counts.skipped += 1,
                        Err(RotateError::Failed(e)) => {
                            error!(id = rid, error = e, "Failed to re-encrypt credential key");
                            counts.failed += 1;
                        }
                    }
                }
                _ => {
                    counts.skipped += 1;
                }
            }
        }

        counts
    }

    async fn rotate_keypairs(&self) -> RotationCounts {
        let rows: Vec<serde_json::Value> = match self
            .db
            .query("SELECT meta::id(id) as rid, private_key_enc, nonce FROM keypair")
            .await
            .and_then(|mut r| r.take(0))
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to query keypair for rotation");
                return RotationCounts {
                    success: 0,
                    skipped: 0,
                    failed: 1,
                };
            }
        };

        let mut counts = RotationCounts {
            success: 0,
            skipped: 0,
            failed: 0,
        };

        for row in rows {
            let Some(rid) = row.get("rid").and_then(|v| v.as_str()) else {
                counts.failed += 1;
                continue;
            };
            let (Some(enc_bytes), Some(nonce_bytes)) = (
                json_to_bytes(row.get("private_key_enc")),
                json_to_bytes(row.get("nonce")),
            ) else {
                counts.skipped += 1;
                continue;
            };

            match self.reencrypt_blob(&enc_bytes, &nonce_bytes) {
                Ok((new_enc, new_nonce)) => {
                    if let Err(e) = self
                        .db
                        .query("UPDATE type::record('keypair', $rid) SET private_key_enc = $enc, nonce = $nonce, updated_at = $now")
                        .bind(("rid", rid.to_string()))
                        .bind(("enc", new_enc))
                        .bind(("nonce", new_nonce))
                        .bind(("now", Utc::now()))
                        .await
                    {
                        error!(id = rid, error = %e, "Failed to update keypair");
                        counts.failed += 1;
                    } else {
                        counts.success += 1;
                    }
                }
                Err(RotateError::AlreadyRotated) => {
                    counts.skipped += 1;
                }
                Err(RotateError::Failed(e)) => {
                    error!(id = rid, error = e, "Failed to re-encrypt keypair");
                    counts.failed += 1;
                }
            }
        }

        counts
    }

    fn reencrypt_blob(
        &self,
        ciphertext: &[u8],
        nonce_bytes: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), RotateError> {
        let plaintext = decrypt_with_key(ciphertext, nonce_bytes, &self.old_key).map_err(|_| {
            match decrypt_with_key(ciphertext, nonce_bytes, &self.new_key) {
                Ok(_) => RotateError::AlreadyRotated,
                Err(e) => RotateError::Failed(format!("Decryption failed with both keys: {e}")),
            }
        })?;

        encrypt_with_key(&plaintext, &self.new_key)
    }

    fn reencrypt_b64(&self, encrypted_b64: &str) -> Result<String, RotateError> {
        let combined = URL_SAFE_NO_PAD
            .decode(encrypted_b64)
            .map_err(|e| RotateError::Failed(format!("Base64 decode failed: {e}")))?;

        if combined.len() < 12 {
            return Err(RotateError::Failed("Encrypted data too short".into()));
        }

        let (nonce_bytes, ciphertext) = combined.split_at(12);

        let plaintext = decrypt_with_key(ciphertext, nonce_bytes, &self.old_key).map_err(|_| {
            match decrypt_with_key(ciphertext, nonce_bytes, &self.new_key) {
                Ok(_) => RotateError::AlreadyRotated,
                Err(e) => RotateError::Failed(format!("Decryption failed with both keys: {e}")),
            }
        })?;

        let (encrypted, new_nonce) = encrypt_with_key(&plaintext, &self.new_key)?;
        let mut new_combined = new_nonce;
        new_combined.extend_from_slice(&encrypted);
        Ok(URL_SAFE_NO_PAD.encode(&new_combined))
    }

    async fn update_credential_data(
        &self,
        rid: &str,
        data: &serde_json::Value,
    ) -> Result<(), String> {
        self.db
            .query(
                "UPDATE type::record('credential', $rid) SET data = $data, updated_at = $now",
            )
            .bind(("rid", rid.to_string()))
            .bind(("data", data.clone()))
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[derive(Debug)]
enum RotateError {
    AlreadyRotated,
    Failed(String),
}

fn decrypt_with_key(
    ciphertext: &[u8],
    nonce_bytes: &[u8],
    key: &[u8; 32],
) -> Result<Vec<u8>, String> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|e| format!("AES init failed: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("Decryption failed: {e}"))
}

fn encrypt_with_key(
    plaintext: &[u8],
    key: &[u8; 32],
) -> Result<(Vec<u8>, Vec<u8>), RotateError> {
    let new_nonce_bytes: [u8; 12] = rand::random();
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| RotateError::Failed(format!("AES init failed: {e}")))?;
    let nonce = Nonce::from_slice(&new_nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| RotateError::Failed(format!("Encryption failed: {e}")))?;
    Ok((encrypted, new_nonce_bytes.to_vec()))
}

fn json_to_bytes(val: Option<&serde_json::Value>) -> Option<Vec<u8>> {
    val.and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_u64().map(|n| n as u8))
                .collect()
        })
    })
}

async fn store_secret(db: &Surreal<Db>, secret: &str) -> Result<(), AppError> {
    db.query(
        "DELETE FROM runtime_config WHERE `key` = $key; \
         CREATE runtime_config SET `key` = $key, `value` = $value, updated_at = $now",
    )
    .bind(("key", RUNTIME_CONFIG_KEY.to_string()))
    .bind(("value", secret.to_string()))
    .bind(("now", Utc::now()))
    .await
    .map_err(|e| AppError::Internal(format!("Failed to store encryption secret: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_key_deterministic() {
        let k1 = derive_key("my-secret");
        let k2 = derive_key("my-secret");
        assert_eq!(k1, k2);
    }

    #[test]
    fn derive_key_different_for_different_secrets() {
        let k1 = derive_key("secret-a");
        let k2 = derive_key("secret-b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = derive_key("test-key");
        let plaintext = b"test-data";
        let (enc, nonce) = encrypt_with_key(plaintext, &key).unwrap();
        let dec = decrypt_with_key(&enc, &nonce, &key).unwrap();
        assert_eq!(dec, plaintext);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key_a = derive_key("key-a");
        let key_b = derive_key("key-b");

        let (enc, nonce) = encrypt_with_key(b"secret", &key_a).unwrap();
        let result = decrypt_with_key(&enc, &nonce, &key_b);
        assert!(result.is_err());
    }
}
