use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateSpaceRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSpaceRequest {
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SpaceResponse {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<super::models::Space> for SpaceResponse {
    fn from(space: super::models::Space) -> Self {
        Self {
            id: space.id,
            name: space.name,
            created_at: space.created_at,
            updated_at: space.updated_at,
        }
    }
}
