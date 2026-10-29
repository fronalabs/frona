use std::collections::BTreeMap;

use crate::chat::broadcast::{BroadcastService, EntityAction};
use crate::chat::repository::ChatRepository;
use crate::core::error::AppError;
use crate::core::metadata::apply_metadata_patch;
use crate::core::repository::Repository;
use crate::db::repo::chats::SurrealChatRepo;
use crate::db::repo::spaces::SurrealSpaceRepo;

use super::models::Space;
use super::models::{CreateSpaceRequest, SpaceResponse, UpdateSpaceRequest};
use super::repository::SpaceRepository;

#[derive(Clone)]
pub struct SpaceService {
    repo: SurrealSpaceRepo,
    chat_repo: SurrealChatRepo,
    broadcast: BroadcastService,
}

impl SpaceService {
    pub fn new(
        repo: SurrealSpaceRepo,
        chat_repo: SurrealChatRepo,
        broadcast: BroadcastService,
    ) -> Self {
        Self {
            repo,
            chat_repo,
            broadcast,
        }
    }

    fn broadcast_update(&self, space: &Space, action: EntityAction) {
        self.broadcast.broadcast_entity_updated(
            &space.user_id,
            "space",
            &space.id,
            action,
            Some(space.id.clone()),
            None,
        );
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateSpaceRequest,
    ) -> Result<SpaceResponse, AppError> {
        let now = chrono::Utc::now();
        let space = Space {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            name: req.name,
            metadata: req.metadata.unwrap_or_default(),
            created_at: now,
            updated_at: now,
        };

        let space = self.repo.create(&space).await?;
        self.broadcast_update(&space, EntityAction::Created);
        Ok(space.into())
    }

    pub async fn get(&self, user_id: &str, space_id: &str) -> Result<Space, AppError> {
        let space = self
            .repo
            .find_by_id(space_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Space not found".into()))?;
        if space.user_id != user_id {
            return Err(AppError::Forbidden("Not your space".into()));
        }
        Ok(space)
    }

    pub async fn find_by_id(&self, space_id: &str) -> Result<Option<Space>, AppError> {
        self.repo.find_by_id(space_id).await
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<SpaceResponse>, AppError> {
        let spaces = self.repo.find_by_user_id(user_id).await?;
        let mut responses = Vec::with_capacity(spaces.len());
        for space in spaces {
            let mut response: SpaceResponse = space.into();
            response.chat_count = self
                .chat_repo
                .find_all_by_space_id(&response.id)
                .await?
                .len();
            responses.push(response);
        }
        Ok(responses)
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
        if let Some(patch) = req.metadata {
            apply_metadata_patch(&mut space.metadata, patch);
        }
        space.updated_at = chrono::Utc::now();

        let space = self.repo.update(&space).await?;
        self.broadcast_update(&space, EntityAction::Updated);
        Ok(space.into())
    }

    pub async fn patch_metadata(
        &self,
        space_id: &str,
        patch: BTreeMap<String, serde_json::Value>,
    ) -> Result<Space, AppError> {
        let mut space = self
            .repo
            .find_by_id(space_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Space not found".into()))?;
        apply_metadata_patch(&mut space.metadata, patch);
        space.updated_at = chrono::Utc::now();
        let saved = self.repo.update(&space).await?;
        self.broadcast_update(&saved, EntityAction::Updated);
        Ok(saved)
    }

    pub async fn delete(&self, user_id: &str, space_id: &str) -> Result<(), AppError> {
        let space = self
            .repo
            .find_by_id(space_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Space not found".into()))?;

        if space.user_id != user_id {
            return Err(AppError::Forbidden("Not your space".into()));
        }

        // Deleting each chat row triggers the database's chat cleanup events for
        // messages, tool calls, credential bindings, summaries, and other
        // ephemeral chat-owned data before the containing space is removed.
        // Inference usage and extracted memory are intentionally retained.
        for chat in self.chat_repo.find_all_by_space_id(space_id).await? {
            self.chat_repo.delete(&chat.id).await?;
        }
        self.repo.delete(space_id).await?;
        self.broadcast_update(&space, EntityAction::Deleted);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::models::Chat;
    use crate::space::models::CreateSpaceRequest;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    #[tokio::test]
    async fn delete_removes_all_chats_and_chat_owned_rows() {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let chat_repo = SurrealChatRepo::new(db.clone());
        let service = SpaceService::new(
            SurrealSpaceRepo::new(db.clone()),
            chat_repo.clone(),
            BroadcastService::new(),
        );
        let space = service
            .create(
                "user-1",
                CreateSpaceRequest {
                    name: "Channel space".into(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        for (id, archived_at) in [("active", None), ("archived", Some(Utc::now()))] {
            chat_repo
                .create(&Chat {
                    agent_context_state: Default::default(),
                    id: id.into(),
                    user_id: "user-1".into(),
                    space_id: Some(space.id.clone()),
                    task_id: None,
                    agent_id: "agent-1".into(),
                    title: None,
                    archived_at,
                    channel_id: None,
                    channel_external_id: None,
                    metadata: BTreeMap::new(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                })
                .await
                .unwrap();
        }
        db.query("CREATE message:one SET chat_id = 'active'; CREATE tool_call:one SET chat_id = 'active'")
            .await
            .unwrap()
            .check()
            .unwrap();

        assert_eq!(service.list("user-1").await.unwrap()[0].chat_count, 2);
        service.delete("user-1", &space.id).await.unwrap();

        assert!(service.find_by_id(&space.id).await.unwrap().is_none());
        assert!(
            chat_repo
                .find_all_by_space_id(&space.id)
                .await
                .unwrap()
                .is_empty()
        );
        let mut related = db
            .query("SELECT * FROM message; SELECT * FROM tool_call")
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = related.take(0).unwrap();
        let tool_calls: Vec<serde_json::Value> = related.take(1).unwrap();
        assert!(messages.is_empty());
        assert!(tool_calls.is_empty());
    }
}
