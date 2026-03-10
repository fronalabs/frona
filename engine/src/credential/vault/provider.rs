use std::path::PathBuf;

use async_trait::async_trait;

use crate::core::error::AppError;

use super::models::{VaultConnectionConfig, VaultItem, VaultProviderType, VaultSecret};
use super::providers::{
    bitwarden::BitwardenVaultProvider,
    hashicorp::HashicorpVaultProvider,
    keepass::KeePassVaultProvider,
    keeper::KeeperVaultProvider,
    local::LocalVaultProvider,
    onepassword::OnePasswordVaultProvider,
};

#[async_trait]
pub trait VaultProvider: Send + Sync {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError>;
    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError>;
    async fn test_connection(&self) -> Result<(), AppError>;
}

pub fn create_vault_provider(
    provider_type: VaultProviderType,
    config: VaultConnectionConfig,
    home_dir: PathBuf,
) -> Result<Box<dyn VaultProvider>, AppError> {
    match provider_type {
        VaultProviderType::Local => Err(AppError::Internal(
            "Local vault provider must be constructed directly with a credential repository".into(),
        )),
        VaultProviderType::OnePassword => match config {
            VaultConnectionConfig::OnePassword {
                service_account_token,
                default_vault_id,
            } => Ok(Box::new(OnePasswordVaultProvider::new(
                service_account_token,
                default_vault_id,
                home_dir,
            ))),
            _ => Err(AppError::Validation("Invalid config for 1Password".into())),
        },
        VaultProviderType::Bitwarden => match config {
            VaultConnectionConfig::Bitwarden {
                client_id,
                client_secret,
                master_password,
                server_url,
            } => Ok(Box::new(BitwardenVaultProvider::new(
                client_id,
                client_secret,
                master_password,
                server_url,
                home_dir,
            )?)),
            _ => Err(AppError::Validation("Invalid config for Bitwarden".into())),
        },
        VaultProviderType::Hashicorp => match config {
            VaultConnectionConfig::Hashicorp {
                address, token, mount_path,
            } => Ok(Box::new(HashicorpVaultProvider::new(address, token, mount_path)?)),
            _ => Err(AppError::Validation("Invalid config for HashiCorp Vault".into())),
        },
        VaultProviderType::KeePass => match config {
            VaultConnectionConfig::KeePass {
                file_path,
                master_password,
            } => Ok(Box::new(KeePassVaultProvider::new(file_path, master_password))),
            _ => Err(AppError::Validation("Invalid config for KeePass".into())),
        },
        VaultProviderType::Keeper => match config {
            VaultConnectionConfig::Keeper {
                app_key, server,
            } => Ok(Box::new(KeeperVaultProvider::new(app_key, server))),
            _ => Err(AppError::Validation("Invalid config for Keeper".into())),
        },
    }
}

pub fn create_local_provider(
    credential_repo: std::sync::Arc<dyn super::repository::CredentialRepository>,
    encryption_key: [u8; 32],
    user_id: String,
) -> Box<dyn VaultProvider> {
    Box::new(LocalVaultProvider::new(credential_repo, encryption_key, user_id))
}
