use crate::chat::message::models::Message;
use crate::chat::message::repository::MessageRepository;
use crate::core::error::AppError;
use crate::storage::Attachment;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::generic::SurrealRepo;

pub type SurrealMessageRepo = SurrealRepo<Message>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl MessageRepository for SurrealRepo<Message> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Vec<Message>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM message WHERE chat_id = $chat_id ORDER BY created_at ASC, id ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(messages)
    }

    async fn find_by_chat_id_after(
        &self,
        chat_id: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<Message>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM message WHERE chat_id = $chat_id AND created_at > $after ORDER BY created_at ASC, id ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("after", after))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(messages)
    }

    async fn delete_by_chat_id_before(
        &self,
        chat_id: &str,
        before: DateTime<Utc>,
    ) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM message WHERE chat_id = $chat_id AND created_at <= $before")
            .bind(("chat_id", chat_id.to_string()))
            .bind(("before", before))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }

    async fn delete_by_chat_id(&self, chat_id: &str) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM message WHERE chat_id = $chat_id")
            .bind(("chat_id", chat_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }

    async fn find_by_chat_id_page(
        &self,
        chat_id: &str,
        before: Option<DateTime<Utc>>,
        after: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<Message>, AppError> {
        let mut query = format!("{SELECT_CLAUSE} FROM message WHERE chat_id = $chat_id");
        let order_desc = after.is_none();

        if before.is_some() {
            query.push_str(" AND created_at < $before");
        }
        if after.is_some() {
            query.push_str(" AND created_at > $after");
        }

        if order_desc {
            query.push_str(" ORDER BY created_at DESC LIMIT $limit");
        } else {
            query.push_str(" ORDER BY created_at ASC LIMIT $limit");
        }

        let mut builder = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("limit", limit));

        if let Some(before) = before {
            builder = builder.bind(("before", before));
        }
        if let Some(after) = after {
            builder = builder.bind(("after", after));
        }

        let mut result = builder
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        if order_desc {
            messages.reverse();
        }

        Ok(messages)
    }

    async fn find_attachments_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Result<Vec<Attachment>, AppError> {
        let messages: Vec<Message> = self.find_by_chat_id(chat_id).await?;
        Ok(messages.into_iter().flat_map(|m| m.attachments).collect())
    }

    async fn find_due_deliveries(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Message>, AppError> {
        // SurrealValue unit variants serialize as tagged objects, so a raw
        // string compare in WHERE won't match - bind the typed enum.
        // `!= sent` covers both Pending and Failed (terminal-but-retryable).
        // Executing is handled by `resume_all_chats`, not this queue.
        use crate::chat::message::models::{DeliveryState, MessageStatus};
        let query = format!(
            "{SELECT_CLAUSE} FROM message
             WHERE delivery.state IS NOT NONE
               AND delivery.state != $sent
               AND delivery.next_attempt_at IS NOT NONE
               AND delivery.next_attempt_at <= $now
               AND status = $completed
             LIMIT $limit"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("sent", DeliveryState::Sent))
            .bind(("completed", MessageStatus::Completed))
            .bind(("now", now))
            .bind(("limit", limit as i64))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(messages)
    }

    async fn find_undelivered_completed_for_channel(
        &self,
        channel_id: &str,
    ) -> Result<Vec<Message>, AppError> {
        use crate::chat::message::models::{MessageRole, MessageStatus};
        // SELECT_CLAUSE projects `id` via `meta::id(id)` to String so the
        // Message struct (whose `id: String`) deserializes cleanly.
        let query = format!(
            "{SELECT_CLAUSE} FROM message
            WHERE role = $agent
              AND status = $completed
              AND delivery IS NONE
              AND chat_id IN (SELECT VALUE meta::id(id) FROM chat WHERE channel_id = $channel_id)
            ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("channel_id", channel_id.to_string()))
            .bind(("agent", MessageRole::Agent))
            .bind(("completed", MessageStatus::Completed))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(messages)
    }

    async fn resume_deliveries_for_channel(
        &self,
        channel_id: &str,
        now: DateTime<Utc>,
    ) -> Result<u64, AppError> {
        // See find_due_deliveries for the enum-binding rationale.
        // `meta::id(id)` projects to String so the count works without
        // deserializing the full Message (id is a record, not a string).
        use crate::chat::message::models::{DeliveryState, MessageStatus};
        let query = "UPDATE message
            SET delivery.next_attempt_at = $now
            WHERE chat_id IN (SELECT VALUE meta::id(id) FROM chat WHERE channel_id = $channel_id)
              AND delivery.state IS NOT NONE
              AND delivery.state != $sent
              AND delivery.next_attempt_at IS NOT NONE
              AND status = $completed
            RETURN meta::id(id) AS id";
        let mut result = self
            .db()
            .query(query)
            .bind(("channel_id", channel_id.to_string()))
            .bind(("now", now))
            .bind(("sent", DeliveryState::Sent))
            .bind(("completed", MessageStatus::Completed))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<serde_json::Value> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rows.len() as u64)
    }
}
