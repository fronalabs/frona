use frona::api::db::setup_schema;
use frona::credential::key_rotation::{KeyRotation, derive_key};
use frona::credential::vault::service::{decrypt_password, encrypt_password};

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use chrono::Utc;

async fn setup_db() -> surrealdb::Surreal<surrealdb::engine::local::Db> {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    setup_schema(&db).await.unwrap();
    db
}

fn encrypt_blob(data: &[u8], key: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher.encrypt(nonce, data).unwrap();
    (encrypted, nonce_bytes.to_vec())
}

fn decrypt_blob(data: &[u8], nonce: &[u8], key: &[u8; 32]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let n = Nonce::from_slice(nonce);
    cipher.decrypt(n, data).unwrap()
}

async fn insert_vault_connection(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
    config_encrypted: Vec<u8>,
    nonce: Vec<u8>,
) {
    let now = Utc::now();
    db.query(
        "CREATE type::record('vault_connection', $id) SET \
         user_id = 'user1', name = 'test', provider = 'local', \
         config_encrypted = $enc, nonce = $nonce, \
         enabled = true, system_managed = false, \
         created_at = $now, updated_at = $now",
    )
    .bind(("id", id.to_string()))
    .bind(("enc", config_encrypted))
    .bind(("nonce", nonce))
    .bind(("now", now))
    .await
    .unwrap();
}

async fn insert_credential(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
    password_encrypted: &str,
) {
    let now = Utc::now();
    let data = serde_json::json!({
        "type": "UsernamePassword",
        "data": {
            "username": "testuser",
            "password_encrypted": password_encrypted
        }
    });
    db.query(
        "CREATE type::record('credential', $id) SET \
         user_id = 'user1', name = 'test-cred', provider = 'local', \
         data = $data, created_at = $now, updated_at = $now",
    )
    .bind(("id", id.to_string()))
    .bind(("data", data))
    .bind(("now", now))
    .await
    .unwrap();
}

async fn insert_keypair(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
    private_key_enc: Vec<u8>,
    nonce: Vec<u8>,
) {
    let now = Utc::now();
    db.query(
        "CREATE type::record('keypair', $id) SET \
         owner = $id, public_key_bytes = <bytes>'', \
         private_key_enc = $enc, nonce = $nonce, \
         active = true, created_at = $now, updated_at = $now",
    )
    .bind(("id", id.to_string()))
    .bind(("enc", private_key_enc))
    .bind(("nonce", nonce))
    .bind(("now", now))
    .await
    .unwrap();
}

async fn get_vault_connection_encrypted(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
) -> (Vec<u8>, Vec<u8>) {
    let mut result = db
        .query("SELECT config_encrypted, nonce FROM type::record('vault_connection', $id)")
        .bind(("id", id.to_string()))
        .await
        .unwrap();
    let row: Option<serde_json::Value> = result.take(0).unwrap();
    let row = row.unwrap();
    let enc: Vec<u8> = row["config_encrypted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    let nonce: Vec<u8> = row["nonce"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    (enc, nonce)
}

async fn get_credential_password(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
) -> String {
    let mut result = db
        .query("SELECT data FROM type::record('credential', $id)")
        .bind(("id", id.to_string()))
        .await
        .unwrap();
    let row: Option<serde_json::Value> = result.take(0).unwrap();
    let row = row.unwrap();
    row["data"]["data"]["password_encrypted"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn get_keypair_encrypted(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    id: &str,
) -> (Vec<u8>, Vec<u8>) {
    let mut result = db
        .query("SELECT private_key_enc, nonce FROM type::record('keypair', $id)")
        .bind(("id", id.to_string()))
        .await
        .unwrap();
    let row: Option<serde_json::Value> = result.take(0).unwrap();
    let row = row.unwrap();
    let enc: Vec<u8> = row["private_key_enc"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    let nonce: Vec<u8> = row["nonce"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    (enc, nonce)
}

#[tokio::test]
async fn first_run_stores_secret_no_rotation() {
    let db = setup_db().await;
    let secret = "my-secret-key";

    let rotation = KeyRotation::check(&db, secret).await.unwrap();
    assert!(rotation.is_none(), "First run should not trigger rotation");

    // Verify secret was stored
    let mut result = db
        .query("SELECT `value` FROM runtime_config WHERE `key` = 'encryption_secret' LIMIT 1")
        .await
        .unwrap();
    let row: Option<serde_json::Value> = result.take(0).unwrap();
    let stored = row.unwrap()["value"].as_str().unwrap().to_string();
    assert_eq!(stored, secret);
}

#[tokio::test]
async fn same_secret_no_rotation() {
    let db = setup_db().await;
    let secret = "my-secret-key";

    // First run stores it
    KeyRotation::check(&db, secret).await.unwrap();
    // Second run with same secret
    let rotation = KeyRotation::check(&db, secret).await.unwrap();
    assert!(rotation.is_none(), "Same secret should not trigger rotation");
}

#[tokio::test]
async fn different_secret_triggers_rotation() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";

    KeyRotation::check(&db, old_secret).await.unwrap();
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap();
    assert!(
        rotation.is_some(),
        "Different secret should trigger rotation"
    );
}

#[tokio::test]
async fn rotate_vault_connections() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";
    let old_key = derive_key(old_secret);
    let new_key = derive_key(new_secret);

    let plaintext = b"vault-config-data";
    let (enc, nonce) = encrypt_blob(plaintext, &old_key);
    insert_vault_connection(&db, "vc1", enc, nonce).await;

    // Store old secret first
    KeyRotation::check(&db, old_secret).await.unwrap();

    // Trigger rotation
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();

    assert_eq!(report.vault_connections.success, 1);
    assert_eq!(report.vault_connections.failed, 0);
    assert!(report.all_succeeded());

    // Verify decryption with new key works
    let (new_enc, new_nonce) = get_vault_connection_encrypted(&db, "vc1").await;
    let decrypted = decrypt_blob(&new_enc, &new_nonce, &new_key);
    assert_eq!(decrypted, plaintext);
}

#[tokio::test]
async fn rotate_credentials() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";
    let old_key = derive_key(old_secret);
    let new_key = derive_key(new_secret);

    let password = "my-database-password";
    let enc_b64 = encrypt_password(password, &old_key).unwrap();
    insert_credential(&db, "cred1", &enc_b64).await;

    KeyRotation::check(&db, old_secret).await.unwrap();
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();

    assert_eq!(report.credentials.success, 1);
    assert_eq!(report.credentials.failed, 0);

    let new_enc_b64 = get_credential_password(&db, "cred1").await;
    let decrypted = decrypt_password(&new_enc_b64, &new_key).unwrap();
    assert_eq!(decrypted, password);
}

#[tokio::test]
async fn rotate_keypairs() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";
    let old_key = derive_key(old_secret);
    let new_key = derive_key(new_secret);

    let private_key_data = b"32-byte-private-key-data-padded!";
    let (enc, nonce) = encrypt_blob(private_key_data, &old_key);
    insert_keypair(&db, "kp1", enc, nonce).await;

    KeyRotation::check(&db, old_secret).await.unwrap();
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();

    assert_eq!(report.keypairs.success, 1);
    assert_eq!(report.keypairs.failed, 0);

    let (new_enc, new_nonce) = get_keypair_encrypted(&db, "kp1").await;
    let decrypted = decrypt_blob(&new_enc, &new_nonce, &new_key);
    assert_eq!(decrypted, private_key_data);
}

#[tokio::test]
async fn rotation_updates_runtime_config() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";

    KeyRotation::check(&db, old_secret).await.unwrap();
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    rotation.run().await.unwrap();

    // Subsequent check should see no rotation needed
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap();
    assert!(rotation.is_none(), "After rotation, same secret should not trigger again");
}

#[tokio::test]
async fn retry_skips_already_rotated_records() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";
    let old_key = derive_key(old_secret);
    let new_key = derive_key(new_secret);

    // Insert two vault connections
    let plaintext = b"config-data";
    let (enc1, nonce1) = encrypt_blob(plaintext, &old_key);
    let (enc2, nonce2) = encrypt_blob(plaintext, &old_key);
    insert_vault_connection(&db, "vc-a", enc1, nonce1).await;
    insert_vault_connection(&db, "vc-b", enc2, nonce2).await;

    KeyRotation::check(&db, old_secret).await.unwrap();

    // First rotation
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();
    assert_eq!(report.vault_connections.success, 2);

    // Manually reset runtime_config to old secret to simulate partial failure retry
    db.query(
        "DELETE FROM runtime_config WHERE `key` = 'encryption_secret'; \
         CREATE runtime_config SET `key` = 'encryption_secret', `value` = $value, updated_at = $now",
    )
    .bind(("value", old_secret.to_string()))
    .bind(("now", Utc::now()))
    .await
    .unwrap();

    // Retry rotation - already-rotated records should be skipped
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();
    assert_eq!(report.vault_connections.skipped, 2);
    assert_eq!(report.vault_connections.success, 0);
    assert_eq!(report.vault_connections.failed, 0);

    // Verify data is still decryptable with new key
    let (enc, nonce) = get_vault_connection_encrypted(&db, "vc-a").await;
    let decrypted = decrypt_blob(&enc, &nonce, &new_key);
    assert_eq!(decrypted, plaintext);
}

#[tokio::test]
async fn browser_profile_credentials_skipped() {
    let db = setup_db().await;
    let old_secret = "old-secret";
    let new_secret = "new-secret";

    // Insert a BrowserProfile credential (no encrypted data)
    let now = Utc::now();
    let data = serde_json::json!({ "type": "BrowserProfile" });
    db.query(
        "CREATE type::record('credential', $id) SET \
         user_id = 'user1', name = 'browser', provider = 'browser', \
         data = $data, created_at = $now, updated_at = $now",
    )
    .bind(("id", "bp1".to_string()))
    .bind(("data", data))
    .bind(("now", now))
    .await
    .unwrap();

    KeyRotation::check(&db, old_secret).await.unwrap();
    let rotation = KeyRotation::check(&db, new_secret).await.unwrap().unwrap();
    let report = rotation.run().await.unwrap();

    assert_eq!(report.credentials.skipped, 1);
    assert_eq!(report.credentials.success, 0);
    assert_eq!(report.credentials.failed, 0);
}
