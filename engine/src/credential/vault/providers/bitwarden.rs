use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct BitwardenVaultProvider {
    client_id: String,
    client_secret: String,
    master_password: String,
    server_url: Option<String>,
    session_key: Mutex<Option<String>>,
    home_dir: PathBuf,
}

#[derive(Deserialize)]
struct BwItem {
    id: String,
    name: String,
    login: Option<BwLogin>,
    notes: Option<String>,
    #[serde(default)]
    fields: Vec<BwField>,
}

#[derive(Deserialize)]
struct BwLogin {
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct BwField {
    name: Option<String>,
    value: Option<String>,
}

#[derive(Deserialize)]
struct BwListItem {
    id: String,
    name: String,
    login: Option<BwLoginSummary>,
}

#[derive(Deserialize)]
struct BwLoginSummary {
    username: Option<String>,
}

impl BitwardenVaultProvider {
    pub fn new(
        client_id: String,
        client_secret: String,
        master_password: String,
        server_url: Option<String>,
        home_dir: PathBuf,
    ) -> Result<Self, AppError> {
        std::fs::create_dir_all(&home_dir)
            .map_err(|e| AppError::Tool(format!("Failed to create Bitwarden home dir: {e}")))?;
        Ok(Self {
            client_id,
            client_secret,
            master_password,
            server_url,
            session_key: Mutex::new(None),
            home_dir,
        })
    }

    async fn run_bw(&self, args: &[&str], session: &str) -> Result<String, AppError> {
        let output = Command::new("bw")
            .args(args)
            .arg("--session")
            .arg(session)
            .env("HOME", &self.home_dir)
            .output()
            .await
            .map_err(|e| AppError::Tool(format!("Failed to run `bw` CLI: {e}. Is the Bitwarden CLI installed?")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Not found") || stderr.contains("not found") {
                return Err(AppError::NotFound(format!("Bitwarden item not found: {stderr}")));
            }
            return Err(AppError::Tool(format!("Bitwarden CLI error: {stderr}")));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| AppError::Tool(format!("Bitwarden CLI output not valid UTF-8: {e}")))
    }

    async fn ensure_session(&self) -> Result<String, AppError> {
        {
            let guard = self.session_key.lock().await;
            if let Some(ref key) = *guard {
                return Ok(key.clone());
            }
        }

        if let Some(ref url) = self.server_url {
            let output = Command::new("bw")
                .args(["config", "server", url])
                .env("HOME", &self.home_dir)
                .output()
                .await
                .map_err(|e| AppError::Tool(format!("Failed to configure Bitwarden server: {e}")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AppError::Tool(format!("Bitwarden config server failed: {stderr}")));
            }
        }

        let login_output = Command::new("bw")
            .args(["login", "--apikey"])
            .env("BW_CLIENTID", &self.client_id)
            .env("BW_CLIENTSECRET", &self.client_secret)
            .env("HOME", &self.home_dir)
            .output()
            .await
            .map_err(|e| AppError::Tool(format!("Failed to run `bw login`: {e}")))?;

        if !login_output.status.success() {
            let stderr = String::from_utf8_lossy(&login_output.stderr);
            if !stderr.contains("You are already logged in") {
                return Err(AppError::Tool(format!("Bitwarden login failed: {stderr}")));
            }
        }

        let unlock_output = Command::new("bw")
            .args(["unlock", "--passwordenv", "BW_PASSWORD", "--raw"])
            .env("BW_PASSWORD", &self.master_password)
            .env("HOME", &self.home_dir)
            .output()
            .await
            .map_err(|e| AppError::Tool(format!("Failed to run `bw unlock`: {e}")))?;

        if !unlock_output.status.success() {
            let stderr = String::from_utf8_lossy(&unlock_output.stderr);
            return Err(AppError::Tool(format!("Bitwarden unlock failed: {stderr}")));
        }

        let session = String::from_utf8(unlock_output.stdout)
            .map_err(|e| AppError::Tool(format!("Bitwarden session key not valid UTF-8: {e}")))?
            .trim()
            .to_string();

        let mut guard = self.session_key.lock().await;
        *guard = Some(session.clone());

        Ok(session)
    }
}

#[async_trait]
impl VaultProvider for BitwardenVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let session = self.ensure_session().await?;

        let mut args = vec!["list", "items"];
        if !query.is_empty() {
            args.push("--search");
            args.push(query);
        }

        let json = self.run_bw(&args, &session).await?;
        let items: Vec<BwListItem> = serde_json::from_str(&json)
            .map_err(|e| AppError::Tool(format!("Failed to parse Bitwarden item list: {e}")))?;

        let results: Vec<VaultItem> = items
            .into_iter()
            .take(max_results)
            .map(|item| VaultItem {
                id: item.id,
                name: item.name,
                username: item.login.and_then(|l| l.username),
            })
            .collect();

        Ok(results)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let session = self.ensure_session().await?;

        let json = self.run_bw(&["get", "item", item_id], &session).await?;
        let item: BwItem = serde_json::from_str(&json)
            .map_err(|e| AppError::Tool(format!("Failed to parse Bitwarden item: {e}")))?;

        let username = item.login.as_ref().and_then(|l| l.username.clone());
        let password = item.login.as_ref().and_then(|l| l.password.clone());

        let mut fields = HashMap::new();
        for field in &item.fields {
            if let (Some(name), Some(value)) = (&field.name, &field.value)
                && !name.is_empty()
            {
                fields.insert(name.clone(), value.clone());
            }
        }

        Ok(VaultSecret {
            id: item.id,
            name: item.name,
            username,
            password,
            notes: item.notes,
            fields,
        })
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        self.ensure_session().await?;
        Ok(())
    }
}
