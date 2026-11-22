//! Test-only seeding of canonical and legacy storage; execution uses HistoryWriter.
use frona::{
    chat::message::models::Reasoning,
    core::{error::AppError, repository::Repository},
    db::repo::{messages::SurrealMessageRepo, tool_calls::SurrealToolCallRepo},
    inference::tool_call::ToolCall,
};
use surrealdb::{Surreal, engine::local::Db};
pub struct HistoryFixtures {
    message_repo: SurrealMessageRepo,
    tool_call_repo: SurrealToolCallRepo,
}
pub fn fixtures(db: &Surreal<Db>) -> HistoryFixtures {
    HistoryFixtures {
        message_repo: SurrealMessageRepo::new(db.clone()),
        tool_call_repo: SurrealToolCallRepo::new(db.clone()),
    }
}
impl HistoryFixtures {
    pub async fn begin_tool_call(
        &self,
        id: &str,
        chat_id: &str,
        message_id: &str,
        turn: u32,
        tool_call_id: &str,
        provider_item_id: Option<String>,
        signature: Option<String>,
        tool_name: &str,
        arguments: &serde_json::Value,
        description: Option<String>,
        turn_text: Option<String>,
        turn_reasoning: Option<Reasoning>,
    ) -> Result<ToolCall, AppError> {
        let te = ToolCall {
            id: id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: message_id.to_string(),
            turn,
            provider_call_id: tool_call_id.to_string(),
            provider_item_id,
            signature,
            name: tool_name.to_string(),
            arguments: arguments.clone(),
            result: String::new(),
            success: false,
            duration_ms: 0,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description,
            turn_text,
            turn_reasoning,
            created_at: chrono::Utc::now(),
        };
        self.tool_call_repo.create(&te).await?;
        Ok(te)
    }
    pub async fn finish_tool_call(
        &self,
        tool_call_id: &str,
        result: String,
        success: bool,
        duration_ms: u64,
        system_prompt: Option<String>,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        te.result = result;
        te.success = success;
        te.duration_ms = duration_ms;
        te.system_prompt = system_prompt;
        self.tool_call_repo.update(&te).await?;
        Ok(())
    }
    pub async fn set_hitl(
        &self,
        tool_call_id: &str,
        hitl: frona::inference::hitl::Hitl,
    ) -> Result<(), AppError> {
        let mut te = self
            .tool_call_repo
            .find_by_id(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Tool call not found".into()))?;
        te.hitl = Some(hitl);
        self.tool_call_repo.update(&te).await?;
        Ok(())
    }
    pub async fn record_provider_turn(
        &self,
        message_id: &str,
        turn: u32,
        assistant: &rig_core::message::AssistantMessage,
    ) -> Result<(), AppError> {
        let mut message = self
            .message_repo
            .find_by_id(message_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
        let turns = &mut message.provider_turns;
        if turns.contains_key(&turn.to_string()) {
            return Err(AppError::Conflict("Provider turn already recorded".into()));
        }
        turns.insert(
            turn.to_string(),
            serde_json::to_value(assistant).map_err(|error| {
                AppError::Internal(format!("Cannot preserve provider turn: {error}"))
            })?,
        );
        self.message_repo.update(&message).await?;
        Ok(())
    }
}
