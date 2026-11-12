//! Load the effective knowledge and source conversations required by portable playbook tools.
use std::collections::{BTreeMap, BTreeSet};

use super::super::context::ConsolidationContext;
use crate::chat::message::repository::MessageRepository;
use crate::core::error::AppError;
use crate::db::repo::{
    messages::SurrealMessageRepo,
    tool_calls::{SurrealToolCallRepo, ToolCallRepository},
};
use crate::memory::pkm::model::{EvidenceSource, KnowledgeEntity, KnowledgeMemory};
use frona_pkm::playbook::{
    ConversationSnapshot, Invocation, KnowledgeSnapshot, MessageRole, TranscriptMessage,
};

pub(super) async fn load(
    ctx: &ConsolidationContext,
    entities: Vec<KnowledgeEntity>,
    evidence_memories: &[KnowledgeMemory],
    messages: &SurrealMessageRepo,
    tool_calls: &SurrealToolCallRepo,
) -> Result<KnowledgeSnapshot, AppError> {
    let graph = ctx.view.portable_snapshot(&ctx.repo).await?;
    let mut memories_by_path = BTreeMap::new();
    for entity in &entities {
        let mut memories = graph.memories_for_entity(&entity.path);
        for memory in graph.memories_by_ids(&entity.source_memory_ids) {
            if !memories.iter().any(|known| known.id == memory.id) {
                memories.push(memory);
            }
        }
        memories_by_path.insert(entity.path.clone(), memories);
    }
    let chats: BTreeSet<_> = evidence_memories
        .iter()
        .flat_map(|memory| &memory.evidence)
        .filter_map(|evidence| match &evidence.source {
            EvidenceSource::UserMessage { chat_id, .. }
            | EvidenceSource::AgentMessage { chat_id, .. } => Some(chat_id.clone()),
            _ => None,
        })
        .collect();
    let mut conversations = BTreeMap::new();
    for chat_id in chats {
        let source_messages = messages.find_by_chat_id(&chat_id).await?;
        let invocations = tool_calls
            .find_by_chat_id(&chat_id)
            .await?
            .into_iter()
            .map(|call| Invocation {
                id: call.id,
                message_id: call.message_id,
                name: call.name,
                arguments: call.arguments,
                result: call.result,
                turn: call.turn,
                created_at: call.created_at,
                turn_text: call.turn_text,
            })
            .collect();
        let messages = source_messages
            .into_iter()
            .map(|message| TranscriptMessage {
                id: message.id,
                role: match message.role {
                    crate::chat::message::models::MessageRole::User => MessageRole::User,
                    crate::chat::message::models::MessageRole::Agent => MessageRole::Agent,
                    crate::chat::message::models::MessageRole::System => MessageRole::System,
                    _ => MessageRole::Event,
                },
                content: message.content,
            })
            .collect();
        conversations.insert(
            chat_id,
            ConversationSnapshot {
                messages,
                invocations,
            },
        );
    }
    Ok(KnowledgeSnapshot {
        entities: entities.into_iter().map(Into::into).collect(),
        redirects: ctx.view.redirects().await?,
        memories_by_path,
        conversations,
    })
}
