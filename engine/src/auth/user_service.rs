use crate::core::config::CacheConfig;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::db::repo::users::SurrealUserRepo;

use super::models::User;
use super::UserRepository;

#[derive(Clone)]
pub struct UserService {
    repo: SurrealUserRepo,
    cache: moka::future::Cache<String, User>,
}

impl UserService {
    pub fn new(repo: SurrealUserRepo, cache_config: &CacheConfig) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(cache_config.entity_max_capacity)
            .time_to_live(std::time::Duration::from_secs(cache_config.entity_ttl_secs))
            .build();
        Self { repo, cache }
    }

    pub async fn find_by_id(&self, id: &str) -> Result<Option<User>, AppError> {
        if let Some(user) = self.cache.get(id).await {
            return Ok(Some(user));
        }
        let result = self.repo.find_by_id(id).await?;
        if let Some(ref user) = result {
            self.cache.insert(id.to_string(), user.clone()).await;
        }
        Ok(result)
    }

    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>, AppError> {
        self.repo.find_by_email(email).await
    }

    pub async fn find_by_username(&self, username: &str) -> Result<Option<User>, AppError> {
        self.repo.find_by_username(username).await
    }

    pub async fn create(&self, user: &User) -> Result<User, AppError> {
        self.repo.create(user).await
    }

    pub async fn update(&self, user: &User) -> Result<User, AppError> {
        let result = self.repo.update(user).await?;
        self.cache.invalidate(&user.id).await;
        Ok(result)
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        self.cache.invalidate(id).await;
        self.repo.delete(id).await
    }
}
