use crate::db::repo::spaces::SurrealSpaceRepo;
use crate::core::error::AppError;
use crate::core::repository::Repository;

use super::models::{CreateSpaceRequest, SpaceResponse, UpdateSpaceRequest};
use super::models::Space;
use super::repository::SpaceRepository;

#[derive(Clone)]
pub struct SpaceService {
    repo: SurrealSpaceRepo,
}

impl SpaceService {
    pub fn new(repo: SurrealSpaceRepo) -> Self {
        Self { repo }
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateSpaceRequest,
    ) -> Result<SpaceResponse, AppError> {
        let now = chrono::Utc::now();
        let space = Space {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: req.name,
            created_at: now,
            updated_at: now,
        };

        let space = self.repo.create(&space).await?;
        Ok(space.into())
    }

    pub async fn list(
        &self,
        user_id: &str,
    ) -> Result<Vec<SpaceResponse>, AppError> {
        let spaces = self.repo.find_by_user_id(user_id).await?;
        Ok(spaces.into_iter().map(Into::into).collect())
    }

    pub async fn update(
        &self,
        user_id: &str,
        space_id: &str,
        req: UpdateSpaceRequest,
    ) -> Result<SpaceResponse, AppError> {
        let mut space = self
            .repo
            .find_by_id(space_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Space not found".into()))?;

        if space.user_id != user_id {
            return Err(AppError::Forbidden("Not your space".into()));
        }

        if let Some(name) = req.name {
            space.name = name;
        }
        space.updated_at = chrono::Utc::now();

        let space = self.repo.update(&space).await?;
        Ok(space.into())
    }

    pub async fn delete(
        &self,
        user_id: &str,
        space_id: &str,
    ) -> Result<(), AppError> {
        let space = self
            .repo
            .find_by_id(space_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Space not found".into()))?;

        if space.user_id != user_id {
            return Err(AppError::Forbidden("Not your space".into()));
        }

        self.repo.delete(space_id).await
    }
}
