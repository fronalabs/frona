use async_trait::async_trait;

use crate::agent::prompt::models::Prompt;
use crate::agent::prompt::repository::PromptRepository;
use crate::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealPromptRepo = SurrealRepo<Prompt>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl PromptRepository for SurrealRepo<Prompt> {
    async fn find_by_agent_and_name(
        &self,
        agent_id: &str,
        name: &str,
    ) -> Result<Option<Prompt>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM prompt WHERE agent_id = $agent_id AND name = $name LIMIT 1"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("agent_id", agent_id.to_string()))
            .bind(("name", name.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let prompt: Option<Prompt> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(prompt)
    }
}
