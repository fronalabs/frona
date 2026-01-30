use async_trait::async_trait;

use crate::error::AppError;

pub trait Entity: Clone + Send + Sync {
    fn table() -> &'static str;
    fn id(&self) -> &str;
}

#[async_trait]
pub trait Repository<T: Send + Sync>: Send + Sync {
    async fn create(&self, entity: &T) -> Result<T, AppError>;
    async fn find_by_id(&self, id: &str) -> Result<Option<T>, AppError>;
    async fn update(&self, entity: &T) -> Result<T, AppError>;
    async fn delete(&self, id: &str) -> Result<(), AppError>;
}
