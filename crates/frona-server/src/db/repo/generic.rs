use crate::core::error::AppError;
use crate::core::repository::{Entity, Repository};
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use surrealdb::types::{QueryError, RecordId, SurrealValue};

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[derive(Clone)]
pub struct SurrealRepo<T: Entity> {
    // Cloning a Surreal handle opens a new session. Repository clones share
    // their session; callers can explicitly clone db() for an isolated one.
    db: Arc<Surreal<Db>>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Entity> SurrealRepo<T> {
    pub fn new(db: Surreal<Db>) -> Self {
        Self {
            db: Arc::new(db),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn db(&self) -> &Surreal<Db> {
        self.db.as_ref()
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
        // A final status write can race deletion after a worker is cancelled.
        // Retry only database conflicts, whose transaction did not commit.
        const ATTEMPTS: usize = 4;
        for attempt in 0..ATTEMPTS {
            let result: Result<Option<surrealdb::types::Value>, _> =
                self.db.delete((T::table(), id)).await;
            match result {
                Ok(_) => return Ok(()),
                Err(error)
                    if matches!(error.query_details(), Some(QueryError::TransactionConflict))
                        && attempt + 1 < ATTEMPTS =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(5 << attempt)).await;
                }
                Err(error) => return Err(AppError::Database(error.to_string())),
            }
        }
        unreachable!("the last attempt returns its result")
    }
}
