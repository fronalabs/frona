//! Sanitization boundary for transcript/tool evidence exposed to Playbook Author.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use frona_derive::agent_tool;

use crate::agent::prompt::PromptLoader;
use crate::chat::message::models::{Message, MessageRole};
use crate::chat::message::repository::MessageRepository;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::db::repo::tool_calls::SurrealToolCallRepo;
use crate::db::repo::tool_calls::ToolCallRepository;
use crate::inference::tool_call::ToolCall;
use crate::memory::pkm::consolidation::candidates::{
    RESOLUTION_PROMPT_LIMIT, Request, Search, Subject,
};
use crate::memory::pkm::consolidation::tools::EntityToolView;
use crate::memory::pkm::consolidation::view::EntityViewManager;
use crate::memory::pkm::model::{EntityCategory, PLAYBOOK_KIND_IRI};
use crate::memory::pkm::model::{EvidenceSource, KnowledgeMemory, memory_bullet};
use crate::tool::{InferenceContext, ToolOutput, str_arg};

const REDACTED: &str = "[REDACTED]";

pub(crate) fn redact_text(input: &str, configured_secrets: &[String]) -> String {
    let mut out = input.to_string();
    for secret in configured_secrets
        .iter()
        .filter(|secret| !secret.is_empty())
    {
        out = out.replace(secret, REDACTED);
    }
    rtb_redact::string(&out).into_owned()
}

pub(crate) fn redact_json(
    value: &serde_json::Value,
    configured_secrets: &[String],
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(key) {
                        serde_json::Value::String(REDACTED.to_string())
                    } else {
                        redact_json(value, configured_secrets)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| redact_json(value, configured_secrets))
                .collect(),
        ),
        serde_json::Value::String(value) => {
            serde_json::Value::String(redact_text(value, configured_secrets))
        }
        value => value.clone(),
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "accesskey",
        "privatekey",
        "clientsecret",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

pub(crate) struct GetToolOutputTool {
    pub prompts: PromptLoader,
    pub tool_calls: Arc<SurrealToolCallRepo>,
    /// Prompt-local id -> durable invocation id.
    pub allowed: HashMap<String, String>,
    pub remaining: AtomicUsize,
    pub total: Arc<AtomicUsize>,
}

#[derive(Default)]
pub(crate) struct TranscriptLookup {
    pub chats: HashMap<String, Vec<Message>>,
    pub tool_calls: HashMap<String, Vec<ToolCall>>,
    pub cursors: HashMap<String, (String, usize)>,
}

pub(crate) struct ReadTranscriptTool {
    pub prompts: PromptLoader,
    pub lookup: Arc<TranscriptLookup>,
    pub remaining: AtomicUsize,
    pub total: Arc<AtomicUsize>,
}

pub(crate) struct ReadMemoryContextTool {
    pub prompts: PromptLoader,
    pub memories: HashMap<String, KnowledgeMemory>,
    pub messages: crate::db::repo::messages::SurrealMessageRepo,
    pub tool_calls: Arc<SurrealToolCallRepo>,
    pub total: Arc<AtomicUsize>,
}

#[agent_tool(name = "read_memory_context", dir = "pkm")]
impl ReadMemoryContextTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if !consume(&self.total) {
            return Ok(ToolOutput::text(
                "Tool budget exhausted; submit the best resolution now.",
            ));
        }
        let local = str_arg(&arguments, "id").unwrap_or_default();
        let Some(memory) = self.memories.get(local) else {
            return Ok(ToolOutput::text("Unknown prompt-local memory id."));
        };
        let mut out = format!(
            "kind={:?}\ncontent={}\nevidence={}",
            memory.kind,
            redact_text(&memory.content, &[]),
            redact_text(&format!("{:?}", memory.evidence), &[]),
        );
        for evidence in &memory.evidence {
            let EvidenceSource::AgentMessage {
                chat_id,
                message_id,
                ..
            } = &evidence.source
            else {
                continue;
            };
            let messages = self.messages.find_by_chat_id(chat_id).await?;
            let tool_calls = self.tool_calls.find_by_chat_id(chat_id).await?;
            let Some(index) = messages
                .iter()
                .position(|message| &message.id == message_id)
            else {
                continue;
            };
            let start = index.saturating_sub(5);
            let end = (index + 6).min(messages.len());
            out.push_str("\nsource_context:\n");
            for message in &messages[start..end] {
                let role = match message.role {
                    MessageRole::User => "user",
                    MessageRole::Agent => "agent",
                    _ => "event",
                };
                let text = super::super::transcript::message_text(
                    &message.id,
                    &message.content,
                    &tool_calls,
                );
                out.push_str(&format!("[{role}] {}\n", redact_text(&text, &[])));
            }
        }
        Ok(ToolOutput::text(out))
    }
}

#[agent_tool(name = "read_transcript", dir = "pkm")]
impl ReadTranscriptTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if !consume(&self.total)
            || self
                .remaining
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
        {
            return Ok(ToolOutput::text("Transcript expansion budget exhausted."));
        }
        let cursor = str_arg(&arguments, "cursor").unwrap_or_default();
        let Some((chat_id, index)) = self.lookup.cursors.get(cursor) else {
            return Ok(ToolOutput::text("Unknown transcript cursor."));
        };
        let messages = &self.lookup.chats[chat_id];
        let tool_calls = self
            .lookup
            .tool_calls
            .get(chat_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let limit = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 20) as usize;
        let direction = str_arg(&arguments, "direction").unwrap_or("before");
        let (start, end) = if direction == "after" {
            (*index, (index + limit).min(messages.len()))
        } else {
            (index.saturating_sub(limit.saturating_sub(1)), index + 1)
        };
        let mut out = String::new();
        for (message_index, message) in messages[start..end].iter().enumerate() {
            let absolute = start + message_index;
            let local = self
                .lookup
                .cursors
                .iter()
                .find_map(|(local, held)| {
                    (held == &(chat_id.clone(), absolute)).then_some(local.as_str())
                })
                .unwrap_or("?");
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Agent => "agent",
                MessageRole::System => "system",
                _ => "event",
            };
            let text =
                super::super::transcript::message_text(&message.id, &message.content, tool_calls);
            out.push_str(&format!("[{local} {role}] {}\n", redact_text(&text, &[])));
        }
        out.push_str(&format!(
            "remaining_calls={}",
            self.remaining.load(Ordering::Relaxed)
        ));
        Ok(ToolOutput::text(out))
    }
}

#[derive(Default)]
pub(crate) struct PlaybookLookup {
    pub read_paths: std::sync::RwLock<std::collections::HashSet<String>>,
}

pub(crate) struct FindPlaybooksTool {
    pub prompts: PromptLoader,
    pub repo: EntityViewManager,
    pub view: Option<Arc<EntityToolView>>,
    pub eligible_playbook_paths: Option<Arc<HashSet<String>>>,
    pub subject_path: Option<String>,
    pub total: Arc<AtomicUsize>,
}

#[agent_tool(name = "find_playbooks", dir = "pkm")]
impl FindPlaybooksTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if !consume(&self.total) {
            return Ok(ToolOutput::text(
                "Tool budget exhausted; submit the best Playbook now.",
            ));
        }
        let query = str_arg(&arguments, "query").unwrap_or_default();
        let eligible_paths = self
            .eligible_playbook_paths
            .as_deref()
            .map(|paths| paths.iter().cloned().collect());
        let additional_candidates = self
            .view
            .as_ref()
            .map(|view| {
                view.entities()
                    .iter()
                    .filter(|entity| entity.category == EntityCategory::Playbook)
                    .map(|entity| crate::memory::pkm::model::EntityHit {
                        path: entity.path.clone(),
                        origin: entity.origin,
                        category: entity.category,
                        kinds: entity.kinds.clone(),
                        name: entity.name.clone(),
                        description: entity.description.clone(),
                        aliases: entity.aliases.clone(),
                        search_name_tokens: entity.search_name_tokens.clone(),
                        search_assertions: entity.search_assertions.clone(),
                        body: entity.body.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let subject = Subject::from_parts(
            self.subject_path.clone().unwrap_or_default(),
            query.to_string(),
            std::iter::empty(),
            String::new(),
            EntityCategory::Playbook,
            vec![PLAYBOOK_KIND_IRI.to_string()],
            std::iter::empty(),
        );
        let hits = Search::new(self.repo.clone())
            .find_candidates(
                Request {
                    subject,
                    eligible_paths,
                    additional_candidates,
                    forced_paths: Vec::new(),
                    limit: RESOLUTION_PROMPT_LIMIT,
                },
                |entity| entity.kinds = vec![PLAYBOOK_KIND_IRI.to_string()],
                |_, _| Some(3),
            )
            .await?;
        let mut out = String::new();
        for ranked in hits {
            let hit = ranked.entity;
            out.push_str(&format!(
                "{} — {} — {}\n",
                hit.path, hit.name, hit.description
            ));
        }
        if out.is_empty() {
            out.push_str("(no Playbooks found)");
        }
        Ok(ToolOutput::text(out))
    }
}

pub(crate) struct ReadPlaybookTool {
    pub prompts: PromptLoader,
    pub repo: EntityViewManager,
    pub memories: Arc<crate::db::repo::pkm::PkmRepo>,
    pub user_id: String,
    pub view: Option<Arc<EntityToolView>>,
    pub lookup: Arc<PlaybookLookup>,
    pub total: Arc<AtomicUsize>,
}

#[agent_tool(name = "read_playbook", dir = "pkm")]
impl ReadPlaybookTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if !consume(&self.total) {
            return Ok(ToolOutput::text(
                "Tool budget exhausted; submit the best Playbook now.",
            ));
        }
        let path = str_arg(&arguments, "path").unwrap_or_default();
        let entity = if let Some(view) = &self.view {
            view.entity(path)
        } else {
            self.repo
                .entity_by_path(path)
                .await?
                .map(|entity| entity.as_knowledge_entity())
        };
        let Some(entity) = entity.filter(|entity| entity.category == EntityCategory::Playbook)
        else {
            return Ok(ToolOutput::text("Unknown Playbook path."));
        };
        let path = entity.path.clone();
        self.lookup
            .read_paths
            .write()
            .expect("playbook lookup poisoned")
            .insert(path.clone());
        let mut memories_by_id = std::collections::BTreeMap::new();
        for memory in self
            .memories
            .memories_for_entity(&self.user_id, &entity.path)
            .await?
            .into_iter()
            .chain(
                self.memories
                    .memories_by_ids(&self.user_id, &entity.source_memory_ids)
                    .await?,
            )
        {
            memories_by_id.insert(memory.id.clone(), memory);
        }
        let source_memories = if memories_by_id.is_empty() {
            "(none)".to_string()
        } else {
            memories_by_id
                .values()
                .map(memory_bullet)
                .collect::<String>()
        };
        let body = if entity.body.trim().is_empty() {
            "(not authored yet)"
        } else {
            &entity.body
        };
        Ok(ToolOutput::text(format!(
            "path={}\nname={}\ndescription={}\nsource_memories:\n{}\nbody:\n{}",
            entity.path,
            entity.name,
            entity.description,
            source_memories,
            redact_text(body, &[])
        )))
    }
}

#[agent_tool(name = "get_tool_output", dir = "pkm")]
impl GetToolOutputTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if !consume(&self.total)
            || self
                .remaining
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
        {
            return Ok(ToolOutput::text(
                "Tool-output budget exhausted; submit the best Playbook now.",
            ));
        }
        let local = str_arg(&arguments, "id").unwrap_or_default();
        let Some(durable) = self.allowed.get(local) else {
            return Ok(ToolOutput::text("Unknown local tool-call id."));
        };
        let Some(call) = self.tool_calls.find_by_id(durable).await? else {
            return Ok(ToolOutput::text("That invocation has no recorded output."));
        };
        let offset = arguments
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let sanitized = redact_text(&call.result, &[]);
        let chars = sanitized.chars().skip(offset).collect::<String>();
        let cap = 8_000usize;
        let returned = chars.chars().take(cap).collect::<String>();
        let truncated = chars.chars().count() > cap;
        let remaining = self.remaining.load(Ordering::Relaxed);
        Ok(ToolOutput::text(format!(
            "output:\n{returned}\n\ntruncated={truncated} next_offset={} remaining_calls={remaining}",
            offset + returned.chars().count(),
        )))
    }
}

fn consume(counter: &AtomicUsize) -> bool {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::AgentTool;
    use std::path::PathBuf;
    use std::time::Duration;

    fn prompts() -> PromptLoader {
        PromptLoader::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("resources")
                .join("prompts"),
        )
    }

    #[test]
    fn every_playbook_conversation_tool_has_a_model_definition() {
        let prompts = prompts();
        for name in [
            "read_memory_context",
            "find_playbooks",
            "read_playbook",
            "read_transcript",
            "get_tool_output",
        ] {
            let path = format!("tools/pkm/{name}.md");
            assert!(
                crate::tool::load_tool_definition(&prompts, &path).is_some(),
                "{name} has no model definition"
            );
        }
    }

    #[tokio::test]
    async fn read_playbook_accepts_a_path_supplied_by_the_stage() {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;

        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let memories = Arc::new(crate::db::repo::pkm::PkmRepo::new(db, 8));
        let repo = EntityViewManager::new(
            crate::db::repo::pkm::PkmConsolidationStore::new(memories.clone())
                .scoped("test-run", "test-user"),
        );
        let lookup = Arc::new(PlaybookLookup::default());
        let mut pending = crate::memory::pkm::model::KnowledgeConsolidationEntity::pending(
            "test-run",
            "test-user",
            "programming/fetch-stock-close-with-yfinance",
            EntityCategory::Playbook,
            Vec::new(),
            Default::default(),
        );
        pending.name = "Fetch a stock close price with yfinance".into();
        pending.description = "Fetch the latest stock close with yfinance.".into();
        pending.kinds = vec![PLAYBOOK_KIND_IRI.into()];
        let view = Arc::new(EntityToolView::new(
            vec![pending.as_knowledge_entity()],
            usize::MAX,
            Arc::new(AtomicUsize::new(0)),
        ));
        let tool = ReadPlaybookTool {
            prompts: prompts(),
            repo,
            memories,
            user_id: "test-user".into(),
            view: Some(view),
            lookup,
            total: Arc::new(AtomicUsize::new(2)),
        };
        let context = InferenceContext::new_detached(
            crate::auth::User {
                id: "test-user".into(),
                handle: crate::handle!("test-user"),
                email: "test@example.com".into(),
                name: "Test User".into(),
                password_hash: String::new(),
                timezone: None,
                groups: Vec::new(),
                deactivated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::agent::models::Agent {
                id: "test-agent".into(),
                user_id: "test-user".into(),
                handle: crate::handle!("test-agent"),
                name: "Test Agent".into(),
                description: String::new(),
                model_group: "test".into(),
                enabled: true,
                sandbox_limits: None,
                max_concurrent_tasks: None,
                skills: None,
                avatar: None,
                identity: Default::default(),
                prompt: None,
                heartbeat_interval: None,
                next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            tokio_util::sync::CancellationToken::new(),
            tokio_util::sync::CancellationToken::new(),
        );

        let output = tool
            .execute(
                "read_playbook",
                serde_json::json!({"path":"programming/fetch-stock-close-with-yfinance"}),
                &context,
            )
            .await
            .unwrap()
            .text_content()
            .to_string();

        assert!(
            output.contains("Fetch a stock close price with yfinance"),
            "{output}"
        );
        assert!(output.contains("(not authored yet)"), "{output}");
    }

    #[test]
    fn recursively_redacts_sensitive_keys_and_free_text_tokens() {
        let value = serde_json::json!({
            "nested": {"api_key": "plain-value"},
            "command": "curl -H 'Authorization: Bearer sk-abcdefghijklmnopqrstuvwxyz123456' x",
            "configured": "use exact-secret-value here",
        });
        let redacted = redact_json(&value, &["exact-secret-value".into()]);
        let rendered = redacted.to_string();
        assert!(!rendered.contains("plain-value"));
        assert!(!rendered.contains("abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!rendered.contains("exact-secret-value"));
        assert!(rendered.contains(REDACTED));
    }

    #[test]
    fn find_playbooks_searches_pending_entity_view() {
        let (ready_send, ready_receive) = std::sync::mpsc::sync_channel(1);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(async {
                use surrealdb::Surreal;
                use surrealdb::engine::local::Mem;

                let db = Surreal::new::<Mem>(()).await.unwrap();
                crate::db::init::setup_schema(&db).await.unwrap();
                let committed = Arc::new(crate::db::repo::pkm::PkmRepo::new(db, 8));
                committed
                    .upsert_entity_skeleton(
                        "test-user",
                        "procedures/update-firmware",
                        EntityCategory::Playbook,
                        &[PLAYBOOK_KIND_IRI.to_string()],
                        "Update firmware",
                        "Update device firmware safely.",
                        &[],
                    )
                    .await
                    .unwrap();
                let repo = EntityViewManager::new(
                    crate::db::repo::pkm::PkmConsolidationStore::new(committed)
                        .scoped("test-run", "test-user"),
                );
                let mut pending = crate::memory::pkm::model::KnowledgeConsolidationEntity::pending(
                    "test-run",
                    "test-user",
                    "markets/retrieve-stock-close-price",
                    EntityCategory::Playbook,
                    Vec::new(),
                    Default::default(),
                );
                pending.name = "Retrieve stock close price locally".into();
                pending.description = "Retrieve a stock close with local yfinance.".into();
                pending.kinds = vec![PLAYBOOK_KIND_IRI.into()];
                let view = Arc::new(EntityToolView::new(
                    vec![pending.as_knowledge_entity()],
                    usize::MAX,
                    Arc::new(AtomicUsize::new(0)),
                ));
                let tool = FindPlaybooksTool {
                    prompts: prompts(),
                    repo,
                    view: Some(view),
                    eligible_playbook_paths: None,
                    subject_path: None,
                    total: Arc::new(AtomicUsize::new(4)),
                };
                let context = InferenceContext::new_detached(
                    crate::auth::User {
                        id: "test-user".into(),
                        handle: crate::handle!("test-user"),
                        email: "test@example.com".into(),
                        name: "Test User".into(),
                        password_hash: String::new(),
                        timezone: None,
                        groups: Vec::new(),
                        deactivated_at: None,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    },
                    crate::agent::models::Agent {
                        id: "test-agent".into(),
                        user_id: "test-user".into(),
                        handle: crate::handle!("test-agent"),
                        name: "Test Agent".into(),
                        description: String::new(),
                        model_group: "test".into(),
                        enabled: true,
                        sandbox_limits: None,
                        max_concurrent_tasks: None,
                        skills: None,
                        avatar: None,
                        identity: Default::default(),
                        prompt: None,
                        heartbeat_interval: None,
                        next_heartbeat_at: None,
                        heartbeat_chat_id: None,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    },
                    tokio_util::sync::CancellationToken::new(),
                    tokio_util::sync::CancellationToken::new(),
                );
                ready_send.send(()).unwrap();
                let output = tool
                    .execute(
                        "find_playbooks",
                        serde_json::json!({"query":"stock close"}),
                        &context,
                    )
                    .await?;
                Ok::<_, AppError>(output.text_content().to_string())
            });
            let _ = send.send(result);
        });

        // Database setup is unrelated to the search deadlock this test guards.
        // Start the watchdog only once the worker is ready to execute the tool.
        ready_receive.recv().expect("playbook fixture setup failed");
        let output = receive
            .recv_timeout(Duration::from_secs(2))
            .expect("find_playbooks deadlocked while searching the entity view")
            .unwrap();
        assert!(
            output.contains("markets/retrieve-stock-close-price"),
            "{output}"
        );
    }
}
