use crate::core::error::AppError;
use crate::credential::managed::Status;
use crate::credential::managed::repository::ManagedVaultRepository;
use crate::credential::vault::models::{VaultConnection, VaultProviderType};
use async_trait::async_trait;
use serde_json::Value;
use surrealdb::{Surreal, engine::local::Db};
use uuid::Uuid;

#[derive(Clone)]
pub struct SurrealManagedVaultRepo {
    db: Surreal<Db>,
}

impl SurrealManagedVaultRepo {
    pub fn new(db: Surreal<Db>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ManagedVaultRepository for SurrealManagedVaultRepo {
    async fn ensure_available(&self, connection_id: &str) -> Result<(), AppError> {
        let row: Option<VaultConnection> = self
            .db
            .query("SELECT *, meta::id(id) as id FROM type::record('vault_connection', $connection_id)")
            .bind(("connection_id", connection_id.to_owned()))
            .await
            .map_err(database)?
            .check()
            .map_err(database)?
            .take(0)
            .map_err(database)?;
        let available = row.is_some_and(|row| {
            row.enabled
                && row.provider == VaultProviderType::Managed
                && (connection_id != crate::credential::managed::GLOBAL_CONNECTION_ID
                    || (row.system_managed && row.user_id == "system"))
        });
        if !available {
            return Err(AppError::Forbidden("managed vault unavailable".into()));
        }
        Ok(())
    }

    async fn delete_connection(&self, connection_id: &str, user_id: &str) -> Result<(), AppError> {
        self.db.query("BEGIN TRANSACTION;
            LET $vault = (SELECT * FROM type::record('vault_connection', $connection_id))[0];
            IF $vault = NONE OR $vault.system_managed = true OR $vault.user_id != $user_id OR $vault.provider != $managed_provider {
                THROW 'managed vault unavailable';
            };
            LET $items = SELECT item_id FROM managed_credential WHERE connection_id = $connection_id LIMIT 1;
            IF array::len($items) > 0 { THROW 'delete the credentials before deleting this managed vault'; };
            DELETE vault_grant WHERE connection_id = $connection_id;
            DELETE principal_credential_binding WHERE connection_id = $connection_id;
            DELETE type::record('vault_connection', $connection_id);
            COMMIT TRANSACTION;")
            .bind(("connection_id", connection_id.to_owned()))
            .bind(("user_id", user_id.to_owned()))
            .bind(("managed_provider", VaultProviderType::Managed))
            .await.map_err(database)?.check().map_err(|error| AppError::Conflict(error.to_string()))?;
        Ok(())
    }

    async fn write(
        &self,
        connection_id: &str,
        mut row: Value,
        expected_version: Option<Uuid>,
    ) -> Result<(), AppError> {
        row["connection_id"] = connection_id.into();
        self.db
            .query("BEGIN TRANSACTION;
                LET $vault = (SELECT * FROM type::record('vault_connection', $connection_id))[0];
                IF $vault = NONE OR $vault.enabled != true OR $vault.provider != $managed_provider OR ($connection_id = 'managed' AND ($vault.system_managed != true OR $vault.user_id != 'system')) { THROW 'managed vault unavailable'; };
                UPDATE type::record('vault_connection', $connection_id) SET managed_revision = rand::uuid();
                LET $current = (SELECT * FROM managed_credential WHERE item_id = $row.item_id)[0];
                IF $expected = NONE OR $expected = NULL {
                    IF $current != NONE { THROW 'credential already exists'; };
                    CREATE managed_credential CONTENT $row;
                } ELSE {
                    IF $current = NONE OR $current.connection_id != $connection_id OR $current.deleted = true OR $current.version != $expected {
                        THROW 'credential version changed';
                    };
                    UPDATE managed_credential CONTENT $row WHERE connection_id = $connection_id AND item_id = $row.item_id;
                };
                COMMIT TRANSACTION;")
            .bind(("connection_id", connection_id.to_owned()))
            .bind(("row", row))
            .bind(("managed_provider", VaultProviderType::Managed))
            .bind(("expected", expected_version.map(|id| id.to_string())))
            .await.map_err(database)?.check().map_err(database)?;
        Ok(())
    }

    async fn delete_by_id(
        &self,
        connection_id: &str,
        id: Uuid,
        expected_version: Uuid,
    ) -> Result<(), AppError> {
        self.db
            .query("BEGIN TRANSACTION;
                LET $current = (SELECT * FROM managed_credential WHERE connection_id = $connection_id AND item_id = $id)[0];
                IF $current = NONE OR $current.deleted = true OR $current.version != $expected {
                    THROW 'credential version changed';
                };
                DELETE managed_credential WHERE connection_id = $connection_id AND item_id = $id;
                COMMIT TRANSACTION;")
            .bind(("connection_id", connection_id.to_owned()))
            .bind(("id", id.to_string()))
            .bind(("expected", expected_version.to_string()))
            .await.map_err(database)?.check().map_err(database)?;
        Ok(())
    }

    async fn row_by_id(&self, connection_id: &str, id: Uuid) -> Result<Option<Value>, AppError> {
        self.db
            .query("SELECT * FROM managed_credential WHERE connection_id = $connection_id AND item_id = $id AND deleted = false")
            .bind(("connection_id", connection_id.to_owned()))
            .bind(("id", id.to_string()))
            .await.map_err(database)?.check().map_err(database)?
            .take(0).map_err(database)
    }

    async fn list(&self, connection_id: &str) -> Result<Vec<Status>, AppError> {
        let rows: Vec<Value> = self.db
            .query("SELECT * OMIT ciphertext, nonce FROM managed_credential WHERE connection_id = $connection_id AND deleted != true")
            .bind(("connection_id", connection_id.to_owned()))
            .await
            .map_err(database)?
            .check()
            .map_err(database)?
            .take(0)
            .map_err(database)?;
        rows.into_iter()
            .map(|r| serde_json::from_value(r).map_err(internal))
            .collect()
    }
}

fn database(error: impl std::fmt::Display) -> AppError {
    AppError::Database(error.to_string())
}

fn internal(error: impl std::fmt::Display) -> AppError {
    AppError::Internal(error.to_string())
}
