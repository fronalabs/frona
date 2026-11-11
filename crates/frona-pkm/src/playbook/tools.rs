//! Sanitization boundary for transcript/tool evidence exposed to Playbook Author.

use crate::tools::{allowance::ResearchAllowance, description, entity};
use frona_text::redaction::redact_text;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{Invocation, KnowledgeSnapshot, MessageRole, TranscriptMessage};
use crate::Error;
use crate::candidates::{
    CandidateEvidence, RESOLUTION_PROMPT_LIMIT, RankedCandidate, Subject, rank_candidates,
};
use crate::model::{
    EntityCategory, EvidenceSource, KnowledgeMemory, PLAYBOOK_KIND_IRI, memory_bullet,
};
use frona_harness::{ToolError, ToolRegistry, tool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn str_arg<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
pub(crate) struct GetToolOutputTool {
    pub tool_calls: Arc<HashMap<String, Invocation>>,
    /// Prompt-local id -> durable invocation id.
    pub allowed: HashMap<String, String>,
    pub remaining: ResearchAllowance,
    pub total: ResearchAllowance,
}

#[derive(Default)]
pub(crate) struct TranscriptLookup {
    pub chats: HashMap<String, Vec<TranscriptMessage>>,
    pub tool_calls: HashMap<String, Vec<Invocation>>,
    pub cursors: HashMap<String, (String, usize)>,
}

pub(crate) struct ReadTranscriptTool {
    pub lookup: Arc<TranscriptLookup>,
    pub remaining: ResearchAllowance,
    pub total: ResearchAllowance,
}

pub(crate) struct ReadMemoryContextTool {
    pub memories: HashMap<String, KnowledgeMemory>,
    pub snapshot: Arc<KnowledgeSnapshot>,
    pub total: ResearchAllowance,
}

impl ReadMemoryContextTool {
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() {
            return Ok(String::from(
                "Tool budget exhausted; submit the best resolution now.",
            ));
        }
        let local = str_arg(&arguments, "id").unwrap_or_default();
        let Some(memory) = self.memories.get(local) else {
            return Ok(String::from("Unknown prompt-local memory id."));
        };
        let mut out = format!(
            "kind={:?}\ncontent={}\nevidence={}",
            memory.kind,
            redact_text(&memory.content),
            redact_text(&format!("{:?}", memory.evidence)),
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
            let Some(conversation) = self.snapshot.conversations.get(chat_id) else {
                continue;
            };
            let messages = &conversation.messages;
            let tool_calls = &conversation.invocations;
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
                let text =
                    super::transcript::message_text(&message.id, &message.content, tool_calls);
                out.push_str(&format!("[{role}] {}\n", redact_text(&text)));
            }
        }
        Ok(out)
    }
}

impl ReadTranscriptTool {
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() || !self.remaining.try_acquire() {
            return Ok(String::from("Transcript expansion budget exhausted."));
        }
        let cursor = str_arg(&arguments, "cursor").unwrap_or_default();
        let Some((chat_id, index)) = self.lookup.cursors.get(cursor) else {
            return Ok(String::from("Unknown transcript cursor."));
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
            let text = super::transcript::message_text(&message.id, &message.content, tool_calls);
            out.push_str(&format!("[{local} {role}] {}\n", redact_text(&text)));
        }
        out.push_str(&format!("remaining_calls={}", self.remaining.remaining()));
        Ok(out)
    }
}

#[derive(Default)]
pub(crate) struct PlaybookLookup {
    pub read_paths: std::sync::RwLock<std::collections::HashSet<String>>,
}

pub(crate) struct FindPlaybooksTool {
    pub snapshot: Arc<KnowledgeSnapshot>,
    pub eligible_playbook_paths: Option<Arc<HashSet<String>>>,
    pub subject_path: Option<String>,
    pub total: ResearchAllowance,
}

impl FindPlaybooksTool {
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() {
            return Ok(String::from(
                "Tool budget exhausted; submit the best Playbook now.",
            ));
        }
        let query = str_arg(&arguments, "query").unwrap_or_default();
        let hits = ranked_playbooks(
            &self.snapshot,
            self.subject_path.as_deref().unwrap_or_default(),
            query,
            self.eligible_playbook_paths.as_deref(),
        );
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
        Ok(out)
    }
}

pub(crate) struct ReadPlaybookTool {
    pub snapshot: Arc<KnowledgeSnapshot>,
    pub lookup: Arc<PlaybookLookup>,
    pub total: ResearchAllowance,
}

impl ReadPlaybookTool {
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() {
            return Ok(String::from(
                "Tool budget exhausted; submit the best Playbook now.",
            ));
        }
        let path = str_arg(&arguments, "path").unwrap_or_default();
        let entity = self.snapshot.entity(path);
        let Some(entity) = entity.filter(|entity| entity.category == EntityCategory::Playbook)
        else {
            return Ok(String::from("Unknown Playbook path."));
        };
        let path = entity.path.clone();
        let memories_by_id = source_memories(&self.snapshot, &entity.path)?;
        let source_memories = if memories_by_id.is_empty() {
            "(none)".to_string()
        } else {
            memories_by_id
                .values()
                .map(|memory| memory_bullet(memory))
                .collect::<String>()
        };
        let body = if entity.body.trim().is_empty() {
            "(not authored yet)"
        } else {
            &entity.body
        };
        let output = format!(
            "path={}\nname={}\ndescription={}\nsource_memories:\n{}\nbody:\n{}",
            entity.path,
            entity.name,
            entity.description,
            source_memories,
            redact_text(body)
        );
        // A visible entity is not evidence of a completed read. Both memory
        // queries and output preparation must succeed before inspection can rely on it.
        self.lookup
            .read_paths
            .write()
            .expect("playbook lookup poisoned")
            .insert(path);
        Ok(output)
    }
}

impl GetToolOutputTool {
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() || !self.remaining.try_acquire() {
            return Ok(String::from(
                "Tool-output budget exhausted; submit the best Playbook now.",
            ));
        }
        let local = str_arg(&arguments, "id").unwrap_or_default();
        let Some(durable) = self.allowed.get(local) else {
            return Ok(String::from("Unknown local tool-call id."));
        };
        let Some(call) = self.tool_calls.get(durable) else {
            return Ok(String::from("That invocation has no recorded output."));
        };
        let offset = arguments
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let sanitized = redact_text(&call.result);
        let chars = sanitized.chars().skip(offset).collect::<String>();
        let cap = 8_000usize;
        let returned = chars.chars().take(cap).collect::<String>();
        let truncated = chars.chars().count() > cap;
        let remaining = self.remaining.remaining();
        Ok(format!(
            "output:\n{returned}\n\ntruncated={truncated} next_offset={} remaining_calls={remaining}",
            offset + returned.chars().count(),
        ))
    }
}

pub(super) fn ranked_playbooks(
    snapshot: &KnowledgeSnapshot,
    subject_path: &str,
    query: &str,
    eligible: Option<&HashSet<String>>,
) -> Vec<RankedCandidate> {
    let subject = Subject::from_parts(
        subject_path.into(),
        query.into(),
        std::iter::empty(),
        String::new(),
        EntityCategory::Playbook,
        vec![PLAYBOOK_KIND_IRI.into()],
        std::iter::empty(),
    );
    let candidates = snapshot
        .entities
        .iter()
        .filter(|entity| {
            entity.category == EntityCategory::Playbook
                && eligible.is_none_or(|paths| paths.contains(&entity.path))
        })
        .map(|entity| RankedCandidate {
            entity: crate::model::EntityHit {
                path: entity.path.clone(),
                origin: entity.origin,
                category: entity.category,
                kinds: vec![PLAYBOOK_KIND_IRI.into()],
                name: entity.name.clone(),
                description: entity.description.clone(),
                aliases: entity.aliases.clone(),
                body: entity.body.clone(),
                search_name_tokens: entity.search_name_tokens.clone(),
                search_assertions: entity.search_assertions.clone(),
            },
            assertions: entity.search_assertions.iter().cloned().collect(),
            evidence: CandidateEvidence {
                type_affinity: 3,
                ..Default::default()
            },
        })
        .collect();
    rank_candidates(&subject, candidates, RESOLUTION_PROMPT_LIMIT)
}

fn source_memories<'a>(
    snapshot: &'a KnowledgeSnapshot,
    path: &str,
) -> Result<std::collections::BTreeMap<String, &'a KnowledgeMemory>, ToolError> {
    let memories = snapshot.memories_by_path.get(path).ok_or_else(|| {
        ToolError::failed(format!(
            "Required source memories are unavailable for `{path}`"
        ))
    })?;
    Ok(memories
        .iter()
        .map(|memory| (memory.id.clone(), memory))
        .collect())
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct MemoryArguments {
    id: String,
}
#[derive(Serialize, Deserialize, JsonSchema)]
struct QueryArguments {
    query: String,
}
#[derive(Serialize, Deserialize, JsonSchema)]
struct PathArguments {
    path: String,
}
#[derive(Serialize, Deserialize, JsonSchema)]
struct OutputArguments {
    id: String,
    offset: Option<usize>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
struct TranscriptArguments {
    cursor: String,
    direction: Option<String>,
    limit: Option<usize>,
}

macro_rules! register {
    ($registry:ident, $name:literal, $arguments:ty, $tool:expr) => {{
        let implementation = Arc::new($tool);
        let tool = tool(
            $name,
            description(include_str!(concat!(
                "../../resources/playbook-tools/",
                $name,
                ".md"
            ))),
            move |arguments: $arguments| {
                let implementation = implementation.clone();
                async move {
                    let value = serde_json::to_value(arguments)
                        .map_err(|e| ToolError::failed(e.to_string()))?;
                    implementation.execute(value).await
                }
            },
        )
        .map_err(|e| Error::Internal(e.to_string()))?;
        $registry
            .register(tool)
            .map_err(|e| Error::Internal(e.to_string()))?;
    }};
}

pub(super) fn resolve_registry(
    snapshot: Arc<KnowledgeSnapshot>,
    memories: HashMap<String, KnowledgeMemory>,
    lookup: Arc<PlaybookLookup>,
    subject_path: String,
) -> Result<ToolRegistry<'static>, Error> {
    let mut registry = ToolRegistry::default();
    let total = ResearchAllowance::new(20);
    let eligible = Arc::new(
        snapshot
            .entities
            .iter()
            .filter(|entity| entity.category == EntityCategory::Playbook)
            .map(|entity| entity.path.clone())
            .collect(),
    );
    register!(
        registry,
        "read_memory_context",
        MemoryArguments,
        ReadMemoryContextTool {
            memories,
            snapshot: snapshot.clone(),
            total: total.clone()
        }
    );
    register!(
        registry,
        "find_playbooks",
        QueryArguments,
        FindPlaybooksTool {
            snapshot: snapshot.clone(),
            eligible_playbook_paths: Some(eligible),
            subject_path: Some(subject_path),
            total: total.clone()
        }
    );
    register!(
        registry,
        "read_playbook",
        PathArguments,
        ReadPlaybookTool {
            snapshot,
            lookup,
            total
        }
    );
    Ok(registry)
}

pub(super) fn author_registry(
    snapshot: Arc<KnowledgeSnapshot>,
    lookup: Arc<PlaybookLookup>,
    transcripts: Arc<TranscriptLookup>,
    allowed: HashMap<String, String>,
    prefixes: crate::ontology::PrefixMap,
) -> Result<ToolRegistry<'static>, Error> {
    let mut registry = ToolRegistry::default();
    let total = ResearchAllowance::new(40);
    let tool_calls = snapshot
        .conversations
        .values()
        .flat_map(|chat| chat.invocations.iter())
        .map(|call| (call.id.clone(), call.clone()))
        .collect();
    register!(
        registry,
        "get_tool_output",
        OutputArguments,
        GetToolOutputTool {
            tool_calls: Arc::new(tool_calls),
            allowed,
            remaining: ResearchAllowance::new(10),
            total: total.clone()
        }
    );
    register!(
        registry,
        "read_transcript",
        TranscriptArguments,
        ReadTranscriptTool {
            lookup: transcripts,
            remaining: ResearchAllowance::new(10),
            total: total.clone()
        }
    );
    register!(
        registry,
        "find_playbooks",
        QueryArguments,
        FindPlaybooksTool {
            snapshot: snapshot.clone(),
            eligible_playbook_paths: None,
            subject_path: None,
            total: total.clone()
        }
    );
    register!(
        registry,
        "read_playbook",
        PathArguments,
        ReadPlaybookTool {
            snapshot: snapshot.clone(),
            lookup,
            total: total.clone()
        }
    );
    register!(
        registry,
        "search_entities",
        QueryArguments,
        SearchEntitiesTool {
            snapshot: snapshot.clone(),
            total: total.clone(),
            prefixes: prefixes.clone()
        }
    );
    register!(
        registry,
        "read_entity",
        PathArguments,
        ReadEntityTool {
            snapshot,
            total,
            prefixes
        }
    );
    Ok(registry)
}

struct SearchEntitiesTool {
    snapshot: Arc<KnowledgeSnapshot>,
    total: ResearchAllowance,
    prefixes: crate::ontology::PrefixMap,
}
impl SearchEntitiesTool {
    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() {
            return Ok(
                "Research-tool budget exhausted. Submit the best complete result now.".into(),
            );
        }
        let query = str_arg(&arguments, "query").unwrap_or_default();
        Ok(entity::render_search(
            &self.snapshot.entities,
            query,
            &self.prefixes,
        ))
    }
}

struct ReadEntityTool {
    snapshot: Arc<KnowledgeSnapshot>,
    total: ResearchAllowance,
    prefixes: crate::ontology::PrefixMap,
}
impl ReadEntityTool {
    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        if !self.total.try_acquire() {
            return Ok(
                "Research-tool budget exhausted. Submit the best complete result now.".into(),
            );
        }
        let path = str_arg(&arguments, "path").unwrap_or_default();
        let Some(found) = self.snapshot.entity(path) else {
            return Ok(entity::missing(path));
        };
        let memories = source_memories(&self.snapshot, &found.path)?;
        Ok(entity::render(
            found,
            memories.into_values(),
            &self.prefixes,
        ))
    }
}
