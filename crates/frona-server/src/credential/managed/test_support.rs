use super::*;
use crate::db::repo::managed_vault::SurrealManagedVaultRepo;
use std::sync::Arc;
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

pub(super) async fn db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    db
}

pub(crate) async fn vault(db: &Surreal<Db>, connection_id: &str, secret: &str) -> ManagedVault {
    use crate::core::repository::Repository;
    use crate::credential::vault::models::{VaultConnection, VaultProviderType};
    let repo = crate::db::repo::generic::SurrealRepo::<VaultConnection>::new(db.clone());
    if repo.find_by_id(connection_id).await.unwrap().is_none() {
        repo.create(&VaultConnection {
            id: connection_id.into(),
            user_id: if connection_id == GLOBAL_CONNECTION_ID {
                "system"
            } else {
                "alice"
            }
            .into(),
            name: connection_id.into(),
            provider: VaultProviderType::Managed,
            config_encrypted: Vec::new(),
            nonce: Vec::new(),
            enabled: true,
            system_managed: connection_id == GLOBAL_CONNECTION_ID,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    }
    ManagedVault::new(
        Arc::new(SurrealManagedVaultRepo::new(db.clone())),
        secret,
        connection_id.into(),
    )
}

pub(crate) async fn credentials(
    db: Surreal<Db>,
    secret: &str,
) -> crate::inference::credential::store::ProviderCredentials {
    crate::inference::credential::store::ProviderCredentials::new(
        vault(&db, GLOBAL_CONNECTION_ID, secret).await,
        Arc::new(super::resolver::ManagedResolver::new(
            super::integration::registered(),
        )),
    )
}
