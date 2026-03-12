use async_trait::async_trait;

use crate::agent::skill::models::Skill;
use crate::agent::skill::repository::SkillRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealSkillRepo = SurrealRepo<Skill>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl SkillRepository for SurrealRepo<Skill> {
    async fn find_by_name(
        &self,
        agent_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Skill>, AppError> {
        let (query, bindings) = match agent_id {
            Some(aid) => (
                format!(
                    "{SELECT_CLAUSE} FROM skill WHERE agent_id = $agent_id AND name = $name LIMIT 1"
                ),
                vec![
                    ("agent_id".to_string(), serde_json::json!(aid)),
                    ("name".to_string(), serde_json::json!(name)),
                ],
            ),
            None => (
                format!(
                    "{SELECT_CLAUSE} FROM skill WHERE agent_id IS NONE AND name = $name LIMIT 1"
                ),
                vec![("name".to_string(), serde_json::json!(name))],
            ),
        };

        let mut q = self.db().query(&query);
        for (key, val) in bindings {
            q = q.bind((key, val));
        }

        let mut result = q.await.map_err(|e| AppError::Database(e.to_string()))?;

        let skill: Option<Skill> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(skill)
    }

    async fn find_by_agent(
        &self,
        agent_id: Option<&str>,
    ) -> Result<Vec<Skill>, AppError> {
        let (query, bindings) = match agent_id {
            Some(aid) => (
                format!("{SELECT_CLAUSE} FROM skill WHERE agent_id = $agent_id"),
                vec![("agent_id".to_string(), serde_json::json!(aid))],
            ),
            None => (
                format!("{SELECT_CLAUSE} FROM skill WHERE agent_id IS NONE"),
                vec![],
            ),
        };

        let mut q = self.db().query(&query);
        for (key, val) in bindings {
            q = q.bind((key, val));
        }

        let mut result = q.await.map_err(|e| AppError::Database(e.to_string()))?;

        let skills: Vec<Skill> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(skills)
    }
}
