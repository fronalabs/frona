use async_trait::async_trait;
use crate::core::error::AppError;
use crate::core::repository::{Entity, Repository};
use serde::Serialize;
use serde::de::DeserializeOwned;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use surrealdb::types::{RecordId, SurrealValue};

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[derive(Clone)]
pub struct SurrealRepo<T: Entity> {
    db: Surreal<Db>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Entity> SurrealRepo<T> {
    pub fn new(db: Surreal<Db>) -> Self {
        Self {
            db,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn db(&self) -> &Surreal<Db> {
        &self.db
    }
}

#[async_trait]
impl<T: Entity + Serialize + DeserializeOwned + SurrealValue> Repository<T> for SurrealRepo<T> {
    async fn create(&self, entity: &T) -> Result<T, AppError> {
        let entity = entity.clone();
        let id = entity.id().to_string();
        let _: Option<surrealdb::types::Value> = self
            .db
            .create((T::table(), &*id))
            .content(entity.clone())
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(entity)
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<T>, AppError> {
        let thing = RecordId::new(T::table(), id);
        let query = format!("{SELECT_CLAUSE} FROM {} WHERE id = $id LIMIT 1", T::table());
        let mut result = self
            .db
            .query(&query)
            .bind(("id", thing))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let entity: Option<T> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(entity)
    }

    async fn update(&self, entity: &T) -> Result<T, AppError> {
        let entity = entity.clone();
        let id = entity.id().to_string();
        let _: Option<surrealdb::types::Value> = self
            .db
            .update((T::table(), &*id))
            .content(entity.clone())
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(entity)
    }

    async fn delete(&self, id: &str) -> Result<(), AppError> {
        let _: Option<surrealdb::types::Value> = self
            .db
            .delete((T::table(), id))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }
}
