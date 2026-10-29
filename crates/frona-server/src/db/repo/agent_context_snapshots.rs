use crate::{
    agent::context::{AgentContextSignature, AgentContextSnapshot},
    core::error::AppError,
};
use surrealdb::{Surreal, engine::local::Db, types::RecordId};

#[derive(Clone)]
pub struct SurrealAgentContextSnapshotRepo(pub Surreal<Db>);

impl SurrealAgentContextSnapshotRepo {
    pub async fn find(
        &self,
        signature: &AgentContextSignature,
    ) -> Result<Option<AgentContextSnapshot>, AppError> {
        let mut response = self
            .0
            .query("SELECT * OMIT id FROM $record")
            .bind((
                "record",
                RecordId::new("agent_context_snapshot", signature.0.clone()),
            ))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        response
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub async fn ensure_stored(&self, snapshot: &AgentContextSnapshot) -> Result<(), AppError> {
        if let Some(existing) = self.find(&snapshot.signature).await? {
            return verify(&existing, snapshot);
        }
        let result = self
            .0
            .create(("agent_context_snapshot", snapshot.signature.0.as_str()))
            .content(snapshot.clone())
            .await;
        match result {
            Ok::<Option<surrealdb::types::Value>, _>(_) => Ok(()),
            Err(error) => match self.find(&snapshot.signature).await? {
                Some(existing) => verify(&existing, snapshot),
                None => Err(AppError::Database(error.to_string())),
            },
        }
    }
}

fn verify(
    existing: &AgentContextSnapshot,
    proposed: &AgentContextSnapshot,
) -> Result<(), AppError> {
    if existing == proposed {
        Ok(())
    } else {
        Err(AppError::Database(
            "Agent context signature collision".into(),
        ))
    }
}
