//! Small projections used to capture context without loading conversation bodies.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use super::generic::SurrealRepo;
use crate::{
    agent::context::AgentContextSignature,
    chat::message::models::{Message, MessageCommand, MessageRole},
    core::{error::AppError, repository::Repository},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MessageContextRow {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub role: MessageRole,
    pub agent_id: Option<String>,
    #[serde(default)]
    #[surreal(default)]
    pub agent_context_signature: Option<AgentContextSignature>,
    #[serde(default)]
    #[surreal(default)]
    pub command: Option<MessageCommand>,
}

pub struct MessageContextPosition {
    pub last: Option<MessageContextRow>,
    pub latest_entry: Option<MessageContextRow>,
}

const COLUMNS: &str =
    "meta::id(id) AS id, created_at, role, agent_id, agent_context_signature, command";
fn db_error(error: impl std::fmt::Display) -> AppError {
    AppError::Database(error.to_string())
}

impl SurrealRepo<Message> {
    /// An autonomous input and its assistant placeholder must both exist or neither.
    pub async fn create_with_context(
        &self,
        message: &Message,
        context: Option<&Message>,
    ) -> Result<Message, AppError> {
        let Some(context) = context else {
            return self.create(message).await;
        };
        let tx = self.db().clone().begin().await.map_err(db_error)?;
        for row in [context, message] {
            let _: Option<surrealdb::types::Value> = tx
                .create(("message", row.id.clone()))
                .content(row.clone())
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)?;
        Ok(message.clone())
    }

    pub async fn clear_with_summary(&self, chat_id: &str) -> Result<(), AppError> {
        self.db().query("BEGIN TRANSACTION; DELETE message WHERE chat_id = $chat; DELETE chat_summary WHERE chat_id = $chat; UPDATE type::record('chat', $chat) UNSET agent_context_state; COMMIT TRANSACTION;")
            .bind(("chat", chat_id.to_owned())).await.map_err(db_error)?.check().map_err(db_error)?;
        Ok(())
    }

    /// Bounded result: the last message and last finalized context entry.
    pub async fn context_position(
        &self,
        chat_id: &str,
    ) -> Result<MessageContextPosition, AppError> {
        self.context_position_before(chat_id, None).await
    }

    /// Bound lookup to the input being finalized, excluding later accepted turns.
    pub async fn context_position_before(
        &self,
        chat_id: &str,
        before: Option<&Message>,
    ) -> Result<MessageContextPosition, AppError> {
        let boundary = if before.is_some() {
            "AND (created_at < $before_at OR (created_at = $before_at AND meta::id(id) < $before_id))"
        } else {
            ""
        };
        let mut result = self.db().query(format!(
            "SELECT {COLUMNS} FROM message WHERE chat_id = $chat {boundary} ORDER BY created_at DESC, id DESC LIMIT 1;
             SELECT {COLUMNS} FROM message WHERE chat_id = $chat {boundary} AND
                 (processed_content != NONE AND processed_content != NULL)
                 ORDER BY created_at DESC, id DESC LIMIT 1;"
         )).bind(("chat", chat_id.to_owned()))
            .bind(("before_at", before.map(|message| message.created_at)))
            .bind(("before_id", before.map(|message| message.id.clone())))
            .await.map_err(db_error)?;
        Ok(MessageContextPosition {
            last: result.take(0).map_err(db_error)?,
            latest_entry: result.take(1).map_err(db_error)?,
        })
    }
}
