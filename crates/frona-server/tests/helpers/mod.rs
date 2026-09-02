// Each integration test file compiles as its own binary and pulls in this
// helpers module; helpers used only by other test files surface as dead-code
// warnings in every binary that doesn't reference them. Silencing module-wide.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use frona::core::metrics;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::Usage;
use frona::inference::config::{ModelGroup, RetryConfig};
use frona::inference::error::InferenceError;
use frona::inference::provider::{ModelProvider, ModelRef, SUBMIT_TOOL_NAME};
use frona::inference::registry::ModelProviderRegistry;
use frona::policy::service::PolicyService;
use frona::tool::manager::ToolManager;
use frona::tool::{AgentTool, InferenceContext, ToolDefinition, ToolOutput};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

pub async fn seed_reconciled_entity(
    db: &Surreal<Db>,
    user_id: &str,
    path: &str,
    name: &str,
    description: &str,
    attributes: &serde_json::Value,
) -> Result<(), ()> {
    let repo = frona::db::repo::pkm::PkmRepo::new(db.clone(), 1);
    let existing = repo.entity_by_path(user_id, path).await.unwrap().unwrap();
    let name = name.trim();
    let renamed = !name.is_empty() && name != existing.name;
    let final_name = if renamed {
        name.to_string()
    } else {
        existing.name.clone()
    };
    let mut aliases = existing.aliases;
    if renamed {
        aliases.insert(existing.name);
    }
    let search_text =
        frona::memory::pkm::model::derive_search_text(&final_name, description, &aliases);
    db.query(
        "UPDATE type::record('knowledge_entity', $id) SET
            name = $name, description = $description, search_text = $search_text,
            aliases = $aliases, attributes = $attributes",
    )
    .bind(("id", existing.id))
    .bind(("name", final_name))
    .bind(("description", description.to_string()))
    .bind(("search_text", search_text))
    .bind(("aliases", aliases))
    .bind(("attributes", attributes.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();
    Ok(())
}

pub async fn mark_entity_rendered(db: &Surreal<Db>, user_id: &str, path: &str) -> Result<(), ()> {
    db.query(
        "UPDATE knowledge_entity SET rendered_at = $now
         WHERE user_id = $user_id AND path = $path",
    )
    .bind(("now", chrono::Utc::now()))
    .bind(("user_id", user_id.to_string()))
    .bind(("path", path.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
    Ok(())
}

pub async fn seed_entity_kinds(
    db: &Surreal<Db>,
    user_id: &str,
    path: &str,
    kinds: &[String],
) -> Result<(), ()> {
    db.query(
        "UPDATE knowledge_entity SET kinds = $kinds, updated_at = $now
         WHERE user_id = $user_id AND path = $path",
    )
    .bind(("kinds", kinds.to_vec()))
    .bind(("now", chrono::Utc::now()))
    .bind(("user_id", user_id.to_string()))
    .bind(("path", path.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
    Ok(())
}

pub async fn seed_asserted_entity_link(
    db: &Surreal<Db>,
    user_id: &str,
    from: &str,
    to: &str,
    relation: &str,
) -> Result<(), ()> {
    let link = frona::memory::pkm::model::KnowledgeEntityLink {
        id: frona::core::repository::new_id(),
        user_id: user_id.to_string(),
        from_entity_path: from.to_string(),
        to_entity_path: to.to_string(),
        relation: relation.to_string(),
        source_memory_ids: Vec::new(),
        origin: frona::memory::pkm::model::LinkOrigin::Asserted,
        created_at: chrono::Utc::now(),
    };
    let _: Option<surrealdb::types::Value> = db
        .create(("knowledge_entity_link", link.id.clone()))
        .content(link)
        .await
        .unwrap();
    Ok(())
}

pub async fn commit_checkpointed_extract_patch(
    repo: &frona::db::repo::pkm::PkmRepo,
    user_id: &str,
    batch: &frona::db::repo::pkm::IngestBatch,
    watermark: Option<(&str, chrono::DateTime<chrono::Utc>)>,
    short_memory_ids: &[String],
) -> frona::db::repo::pkm::IngestCounts {
    use frona::memory::pkm::{ConsolidationStageState, IngestState, KnowledgeConsolidationRecord};

    let now = chrono::Utc::now();
    let mut record = KnowledgeConsolidationRecord {
        id: frona::core::repository::new_id(),
        consolidation_id: frona::core::repository::new_id(),
        user_id: user_id.to_string(),
        state: ConsolidationStageState::Ingest(IngestState::default()),
        stats: Default::default(),
        attempts: 0,
        restart_count: 0,
        failure: None,
        next_attempt_at: now,
        updated_at: now,
    };
    record.stats.absorb_ingest_batch(batch);
    let watermarks = watermark
        .map(|(chat_id, until)| (chat_id.to_string(), until))
        .into_iter()
        .collect::<Vec<_>>();
    let counts = repo
        .commit_extract_patch_with_checkpoint(
            user_id,
            batch,
            &watermarks,
            short_memory_ids,
            &record,
        )
        .await
        .unwrap();
    record.stats.absorb_ingest_counts(&counts);
    repo.save_consolidation_record(&record).await.unwrap();
    counts
}

/// Test-only UsageService with an empty model catalog, fresh broadcast,
/// real DB table. Sufficient to satisfy constructor signatures; in tests that
/// don't assert against the inference_usage table this is a complete stub.
pub fn test_usage_service(db: &Surreal<Db>) -> frona::inference::usage::UsageService {
    frona::inference::usage::UsageService::new(
        frona::inference::metadata::ModelCatalogStore::new(
            frona::inference::metadata::ModelCatalogSnapshot::empty(),
        ),
        SurrealRepo::new(db.clone()),
        frona::chat::broadcast::BroadcastService::new(),
    )
}

/// Test-only UsageContext for fixtures that only need to satisfy a signature.
pub fn test_usage_ctx() -> frona::inference::usage::UsageContext {
    frona::inference::usage::UsageContext::new(
        frona::inference::usage::InferenceKind::Title {
            agent_id: "test-agent".to_string(),
            chat_id: "test-chat".to_string(),
        },
        "test-user",
        "primary".to_string(),
    )
}

pub fn test_policy_service(db: &Surreal<Db>) -> PolicyService {
    let schema = frona::policy::schema::build_schema();
    let repo: Arc<dyn frona::policy::repository::PolicyRepository> =
        Arc::new(SurrealRepo::<frona::policy::models::Policy>::new(
            db.clone(),
        ));
    let tool_manager = Arc::new(ToolManager::new(false));
    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    PolicyService::new(repo, schema, tool_manager, storage, user_service)
}
use rig_core::completion::message::{ToolCall, ToolFunction, UserContent};
use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};
use serde_json::Value;
use tokio::sync::mpsc;

pub enum MockResponse {
    Text(String),
    TextWithReasoning(String, String),
    ToolCalls(Vec<(String, String, Value)>),
    Error(InferenceError),
    /// Consume this response and remain in flight until the caller is cancelled. Useful
    /// for crash-resume tests that need an exact durable boundary.
    Pending,
    /// Stay in flight, then delay the future's drop when cancellation arrives. This lets
    /// concurrency tests observe the caller while it still owns its operation guard.
    PendingWithDropDelay(std::time::Duration),
    /// Hold this response until every participant reaches the same point. This proves
    /// model calls overlap without adding timing-sensitive sleeps to concurrency tests.
    Barrier(Arc<tokio::sync::Barrier>, Box<MockResponse>),
    /// Route a response by user-message content when concurrent calls may reach the
    /// provider in a different order from the work that spawned them.
    ForUserText {
        text: String,
        response: Box<MockResponse>,
    },
}

pub struct MockModelProvider {
    responses: Mutex<Vec<MockResponse>>,
    pub call_count: Mutex<usize>,
    /// The `chat_history` of the most recent call, which is what the conversation sent
    /// back, which is the only way to assert on messages a loop appends internally
    /// (tool results, feedback) rather than returns.
    last_history: Mutex<Vec<RigMessage>>,
    histories: Mutex<Vec<Vec<RigMessage>>>,
    tool_histories: Mutex<Vec<Vec<RigToolDefinition>>>,
}

impl MockModelProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: Mutex::new(0),
            last_history: Mutex::new(Vec::new()),
            histories: Mutex::new(Vec::new()),
            tool_histories: Mutex::new(Vec::new()),
        }
    }

    /// The history the provider was handed on its most recent call.
    pub fn last_history(&self) -> Vec<RigMessage> {
        self.last_history.lock().unwrap().clone()
    }

    pub fn histories(&self) -> Vec<Vec<RigMessage>> {
        self.histories.lock().unwrap().clone()
    }

    pub fn tool_histories(&self) -> Vec<Vec<RigToolDefinition>> {
        self.tool_histories.lock().unwrap().clone()
    }

    /// Append a response to the queue after construction for tests that only
    /// know the expected payload (e.g. a memory id) after some setup runs.
    pub fn enqueue(&self, response: MockResponse) {
        self.responses.lock().unwrap().push(response);
    }

    pub fn prepend(&self, response: MockResponse) {
        self.responses.lock().unwrap().insert(0, response);
    }

    fn next_response(&self) -> MockResponse {
        let mut responses = self.responses.lock().unwrap();
        *self.call_count.lock().unwrap() += 1;
        if responses.is_empty() {
            MockResponse::Text("default response".into())
        } else {
            responses.remove(0)
        }
    }

    fn next_response_for_history(&self, history: &[RigMessage]) -> MockResponse {
        let mut responses = self.responses.lock().unwrap();
        *self.call_count.lock().unwrap() += 1;
        let matching = responses.iter().position(|response| match response {
            MockResponse::ForUserText { text, .. } => history.iter().any(|message| {
                let RigMessage::User { content } = message else {
                    return false;
                };
                content.iter().any(
                    |item| matches!(item, UserContent::Text(value) if value.text.contains(text)),
                )
            }),
            _ => false,
        });
        let fallback = responses
            .iter()
            .position(|response| !matches!(response, MockResponse::ForUserText { .. }));
        let Some(index) = matching.or(fallback) else {
            return if responses.is_empty() {
                MockResponse::Text("default response".into())
            } else {
                MockResponse::Error(InferenceError::InferenceFailed(
                    "mock: no response matched the user message".into(),
                ))
            };
        };
        match responses.remove(index) {
            MockResponse::ForUserText { response, .. } => *response,
            response => response,
        }
    }

    pub fn calls(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

#[async_trait]
impl ModelProvider for MockModelProvider {
    async fn inference(
        &self,
        _model: &ModelRef,
        _system_prompt: &str,
        chat_history: Vec<RigMessage>,
        tools: Vec<RigToolDefinition>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<frona::inference::provider::InferenceOutput, InferenceError> {
        *self.last_history.lock().unwrap() = chat_history.clone();
        self.histories.lock().unwrap().push(chat_history.clone());
        self.tool_histories.lock().unwrap().push(tools);
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_use_prompt_tokens: 0,
        };
        let response = match self.next_response_for_history(&chat_history) {
            MockResponse::Barrier(barrier, response) => {
                barrier.wait().await;
                *response
            }
            response => response,
        };
        let content = match response {
            MockResponse::Text(t) => vec![AssistantContent::text(&t)],
            MockResponse::TextWithReasoning(text, reasoning) => vec![
                AssistantContent::Reasoning(rig_core::completion::message::Reasoning::new(
                    &reasoning,
                )),
                AssistantContent::text(&text),
            ],
            MockResponse::ToolCalls(calls) => calls
                .into_iter()
                .map(|(id, name, args)| {
                    AssistantContent::ToolCall(ToolCall::new(
                        rig_core::completion::message::ToolCallId::new_or_mint(id),
                        ToolFunction::new(name, args),
                    ))
                })
                .collect(),
            MockResponse::Error(e) => return Err(e),
            MockResponse::Pending => std::future::pending().await,
            MockResponse::PendingWithDropDelay(delay) => pending_with_drop_delay(delay).await,
            MockResponse::Barrier(_, _) => unreachable!("nested mock barriers are unsupported"),
            MockResponse::ForUserText { .. } => {
                unreachable!("history-routed responses are unwrapped before execution")
            }
        };
        Ok(frona::inference::provider::InferenceOutput::new(
            content, usage,
        ))
    }

    async fn stream_inference(
        &self,
        _model: &ModelRef,
        _system_prompt: &str,
        _chat_history: Vec<RigMessage>,
        _tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<frona::inference::provider::StreamToken>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<frona::inference::provider::InferenceOutput, InferenceError> {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_use_prompt_tokens: 0,
        };
        let response = match self.next_response() {
            MockResponse::Barrier(barrier, response) => {
                barrier.wait().await;
                *response
            }
            response => response,
        };
        let content = match response {
            MockResponse::Text(t) => {
                let _ = token_tx
                    .send(frona::inference::provider::StreamToken::Text(t.clone()))
                    .await;
                vec![AssistantContent::text(t)]
            }
            MockResponse::TextWithReasoning(text, reasoning) => {
                let _ = token_tx
                    .send(frona::inference::provider::StreamToken::Reasoning(
                        reasoning.clone(),
                    ))
                    .await;
                let _ = token_tx
                    .send(frona::inference::provider::StreamToken::Text(text.clone()))
                    .await;
                vec![
                    AssistantContent::Reasoning(rig_core::completion::message::Reasoning::new(
                        &reasoning,
                    )),
                    AssistantContent::text(text),
                ]
            }
            MockResponse::ToolCalls(calls) => calls
                .into_iter()
                .map(|(id, name, args)| {
                    AssistantContent::ToolCall(ToolCall::new(
                        rig_core::completion::message::ToolCallId::new_or_mint(id),
                        ToolFunction::new(name, args),
                    ))
                })
                .collect(),
            MockResponse::Error(e) => return Err(e),
            MockResponse::Pending => std::future::pending().await,
            MockResponse::PendingWithDropDelay(delay) => pending_with_drop_delay(delay).await,
            MockResponse::Barrier(_, _) => unreachable!("nested mock barriers are unsupported"),
            MockResponse::ForUserText { .. } => {
                unreachable!("history-routed responses require non-streaming inference")
            }
        };
        Ok(frona::inference::provider::InferenceOutput::new(
            content, usage,
        ))
    }

    async fn structured_inference(
        &self,
        _model: &ModelRef,
        _system_prompt: &str,
        _chat_history: Vec<RigMessage>,
        _schema: serde_json::Value,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        let response = match self.next_response() {
            MockResponse::Barrier(barrier, response) => {
                barrier.wait().await;
                *response
            }
            response => response,
        };
        match response {
            MockResponse::ToolCalls(mut calls) => {
                let (_id, name, args) = calls.pop().ok_or_else(|| {
                    InferenceError::InferenceFailed("mock: empty ToolCalls".into())
                })?;
                // Structured output arrives as a call to the submit tool. Enforced here
                // even though this path could ignore the name, because the REAL tool loop
                // (`structured_inference_with_tools` / `structured_conversation`)
                // dispatches on it: a test encoding the wrong name passes here and fails
                // there, silently. That divergence is how the playbook capture path went
                // untested.
                if name != SUBMIT_TOOL_NAME {
                    return Err(InferenceError::InferenceFailed(format!(
                        "mock structured_inference: tool call named `{name}`, but structured \
                         output must use `{SUBMIT_TOOL_NAME}` — the real tool loop dispatches \
                         on this name, so any other value only works on this path"
                    )));
                }
                Ok(args)
            }
            MockResponse::Error(e) => Err(e),
            MockResponse::Pending => std::future::pending().await,
            MockResponse::PendingWithDropDelay(delay) => pending_with_drop_delay(delay).await,
            MockResponse::Barrier(_, _) => unreachable!("nested mock barriers are unsupported"),
            MockResponse::ForUserText { .. } => {
                unreachable!("history-routed responses require conversation inference")
            }
            _ => Err(InferenceError::InferenceFailed(
                "mock structured_inference: queue head is not a ToolCalls response".into(),
            )),
        }
    }
}

async fn pending_with_drop_delay<T>(delay: std::time::Duration) -> T {
    struct DelayOnDrop(std::time::Duration);
    impl Drop for DelayOnDrop {
        fn drop(&mut self) {
            std::thread::sleep(self.0);
        }
    }
    let _delay = DelayOnDrop(delay);
    std::future::pending().await
}

pub struct MockInternalTool {
    pub tool_name: String,
    responses: Mutex<Vec<String>>,
}

impl MockInternalTool {
    pub fn new(name: &str, responses: Vec<String>) -> Self {
        Self {
            tool_name: name.to_string(),
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl AgentTool for MockInternalTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: self.tool_name.clone(),
            provider_id: self.tool_name.clone(),
            description: format!("Mock tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        let mut responses = self.responses.lock().unwrap();
        let text = if responses.is_empty() {
            "mock result".to_string()
        } else {
            responses.remove(0)
        };
        Ok(ToolOutput::text(text))
    }
}

pub struct MockAttachmentTool {
    pub tool_name: String,
    pub attachment: frona::storage::Attachment,
}

impl MockAttachmentTool {
    pub fn new(name: &str, attachment: frona::storage::Attachment) -> Self {
        Self {
            tool_name: name.to_string(),
            attachment,
        }
    }
}

#[async_trait]
impl AgentTool for MockAttachmentTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: self.tool_name.clone(),
            provider_id: self.tool_name.clone(),
            description: format!("Attachment tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Ok(ToolOutput::text("file produced").with_attachment(self.attachment.clone()))
    }
}

pub struct MockExternalTool {
    pub tool_name: String,
}

impl MockExternalTool {
    pub fn new(name: &str) -> Self {
        Self {
            tool_name: name.to_string(),
        }
    }
}

#[async_trait]
impl AgentTool for MockExternalTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: self.tool_name.clone(),
            provider_id: self.tool_name.clone(),
            description: format!("External tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Ok(ToolOutput::text("external result").as_pending_external())
    }
}

pub struct MockFailingTool {
    pub tool_name: String,
}

impl MockFailingTool {
    pub fn new(name: &str) -> Self {
        Self {
            tool_name: name.to_string(),
        }
    }
}

#[async_trait]
impl AgentTool for MockFailingTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: self.tool_name.clone(),
            provider_id: self.tool_name.clone(),
            description: format!("Failing tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Err(frona::core::error::AppError::Tool("tool failed".into()))
    }
}

pub fn mock_context() -> InferenceContext {
    let broadcast = frona::chat::broadcast::BroadcastService::new();
    let event_sender = broadcast.create_event_sender("test-user", "test-chat", None);
    InferenceContext::new(
        frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "test@test.com".into(),
            name: "Test".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        frona::agent::models::Agent {
            id: "test-agent".into(),
            user_id: "test-user".into(),
            handle: frona::handle!("test-agent"),
            name: "Test Agent".into(),
            description: String::new(),
            model_group: "primary".into(),
            enabled: true,
            skills: None,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: Default::default(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        frona::chat::models::Chat {
            id: "test-chat".into(),
            user_id: "test-user".into(),
            space_id: None,
            task_id: None,
            agent_id: "test-agent".into(),
            title: None,
            archived_at: None,
            channel_id: None,
            channel_external_id: None,
            metadata: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        event_sender,
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
}

pub fn test_model_group() -> ModelGroup {
    ModelGroup {
        name: "test".into(),
        main: ModelRef {
            provider: "mock".into(),
            model_id: "test-model".into(),
        },
        fallbacks: vec![],
        max_tokens: Some(4096),
        temperature: None,
        context_window: 128_000,
        retry: RetryConfig {
            max_retries: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            max_backoff_ms: 10,
        },
        inference: Default::default(),
    }
}

pub fn test_model_group_with_fallback(fallback_provider: &str, fallback_model: &str) -> ModelGroup {
    let mut group = test_model_group();
    group.fallbacks.push(ModelRef {
        provider: fallback_provider.into(),
        model_id: fallback_model.into(),
    });
    group
}

/// Backwards-compatible name kept for the many call sites that pre-date the
/// service refactor. Returns an `UsageService` backed by a
/// process-wide in-memory DB so tests that don't assert on the table just work.
///
/// The DB is created on a **separate worker thread** so we don't trip
/// tokio's "cannot start a runtime from within a runtime" guard. Every
/// `#[tokio::test]` call site already lives inside a runtime, and nested
/// `block_on` panics there.
pub fn test_metrics_ctx() -> frona::inference::usage::UsageService {
    use std::sync::OnceLock;
    static TEST_DB: OnceLock<Surreal<Db>> = OnceLock::new();
    let db = TEST_DB.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("test runtime");
            let db = rt.block_on(async {
                let db = Surreal::new::<surrealdb::engine::local::Mem>(())
                    .await
                    .expect("test db");
                frona::db::init::setup_schema(&db).await.expect("schema");
                db
            });
            tx.send(db).expect("send db back");
            // The in-memory SurrealDB engine owns tasks spawned on this
            // runtime. Keep its worker thread alive for the process lifetime;
            // dropping the runtime leaves later database requests pending.
            rt.block_on(std::future::pending::<()>());
        });
        rx.recv().expect("recv db")
    });
    test_usage_service(db)
}

pub fn test_registry_with_provider(
    name: &str,
    provider: Arc<dyn ModelProvider>,
) -> ModelProviderRegistry {
    let mut providers = HashMap::new();
    providers.insert(name.to_string(), provider);
    let model_groups = HashMap::new();
    ModelProviderRegistry::for_testing(providers, model_groups)
}

pub fn test_registry_with_group(
    provider_name: &str,
    provider: Arc<dyn ModelProvider>,
    group_name: &str,
    group: ModelGroup,
) -> ModelProviderRegistry {
    let mut providers = HashMap::new();
    providers.insert(provider_name.to_string(), provider);
    let mut model_groups = HashMap::new();
    model_groups.insert(group_name.to_string(), group);
    ModelProviderRegistry::for_testing(providers, model_groups)
}

pub fn init_metrics() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        metrics::setup_metrics_recorder();
    });
}

/// An SSE frame received from the broadcast dispatcher, parsed back into
/// event name + JSON data for assertion.
pub struct SseFrame {
    pub event: String,
    pub data: Value,
}

/// Convert an axum SSE `Event` to its wire-format string by running it
/// through a one-shot Sse body, the same way axum itself serializes events.
async fn event_to_string(event: axum::response::sse::Event) -> String {
    use axum::response::IntoResponse;
    use axum::response::sse::Sse;
    use http_body_util::BodyExt;

    let stream = futures::stream::once(async { Ok::<_, std::convert::Infallible>(event) });
    let sse = Sse::new(stream);
    let response = sse.into_response();
    let body = response.into_body();
    let collected = body.collect().await.unwrap();
    String::from_utf8(collected.to_bytes().to_vec()).unwrap()
}

/// Parse an SSE wire-format string into field name/value pairs, using the
/// same approach as axum's own test suite.
fn parse_sse_text(payload: &str) -> Option<SseFrame> {
    let mut event_name = String::new();
    let mut data_parts = Vec::new();

    for line in payload.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim_start();
            match key {
                "event" => event_name = value.to_string(),
                "data" => data_parts.push(value.to_string()),
                _ => {}
            }
        }
    }

    if event_name.is_empty() {
        return None;
    }

    let joined = data_parts.join("\n");
    let data: Value = serde_json::from_str(&joined).unwrap_or(Value::Null);

    Some(SseFrame {
        event: event_name,
        data,
    })
}

/// Parse a single axum SSE `Event` into an `SseFrame`.
pub async fn parse_sse_frame(event: axum::response::sse::Event) -> Option<SseFrame> {
    let text = event_to_string(event).await;
    parse_sse_text(&text)
}

/// Drain all pending SSE events from a receiver, parse each into `SseFrame`.
pub async fn drain_sse_frames(
    rx: &mut mpsc::UnboundedReceiver<Result<axum::response::sse::Event, std::convert::Infallible>>,
) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    while let Ok(Ok(event)) = rx.try_recv() {
        if let Some(frame) = parse_sse_frame(event).await {
            frames.push(frame);
        }
    }
    frames
}

/// Create a minimal ChatService backed by an in-memory SurrealDB for tool loop tests.
pub async fn test_chat_service() -> frona::chat::service::ChatService {
    use frona::db::repo::generic::SurrealRepo;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();

    let config = frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };

    let storage = frona::storage::StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let user_service = frona::auth::UserService::new(SurrealRepo::new(db.clone()), &config.cache);
    let agent_service = frona::agent::service::AgentService::new(
        SurrealRepo::new(db.clone()),
        &config.cache,
        resource_manager.clone(),
        test_policy_service(&db),
        user_service.clone(),
    );
    let provider_registry = frona::inference::registry::ModelProviderRegistry::for_testing(
        HashMap::new(),
        HashMap::new(),
    );

    let usage_service = test_usage_service(&db);

    let keypair_repo: SurrealRepo<frona::credential::keypair::models::KeyPair> =
        SurrealRepo::new(db.clone());
    let keypair_service = frona::credential::keypair::service::KeyPairService::new(
        &config.auth.encryption_secret,
        std::sync::Arc::new(keypair_repo),
    );
    let presign_service = frona::credential::presign::PresignService::new(
        keypair_service,
        user_service.clone(),
        "http://localhost:0".to_string(),
        300,
    );

    frona::chat::service::ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        agent_service,
        provider_registry,
        storage,
        user_service,
        frona::agent::prompt::PromptLoader::new(&base),
        frona::chat::broadcast::BroadcastService::new(),
        presign_service,
        usage_service,
    )
}

/// Build a `BasicMemoryService` for tests that construct a `Harness` directly
/// (Harness still owns the memory service). Reuses the state's mock registry.
pub fn test_memory_service(
    state: &frona::core::state::AppState,
    db: &Surreal<Db>,
) -> std::sync::Arc<dyn frona::memory::service::MemoryService> {
    std::sync::Arc::new(frona::memory::basic::BasicMemoryService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        std::sync::Arc::new(state.chat_service.provider_registry().clone()),
        state.prompts.clone(),
        state.usage_service.clone(),
        frona::core::config::MemoryConfig::default(),
    ))
}

/// Create an `EventSender` backed by a real `BroadcastService` with a
/// registered SSE session, returning both the sender and the SSE receiver.
/// This exercises the full production path: serialize → dispatch → fan-out.
pub async fn test_event_sender() -> (
    frona::chat::broadcast::EventSender,
    mpsc::UnboundedReceiver<Result<axum::response::sse::Event, std::convert::Infallible>>,
    frona::chat::broadcast::BroadcastService,
) {
    let broadcast = frona::chat::broadcast::BroadcastService::new();
    let event_sender = broadcast.create_event_sender("test-user", "test-chat", None);

    let (tx, rx) = mpsc::unbounded_channel();
    broadcast.register_session("test-user", tx).await;

    (event_sender, rx, broadcast)
}

/// Build a `Harness` whose inference is wired to `mock_provider` (via a mock-registry
/// `ChatService`), for exercising the harness-routed PKM consolidation path. All other
/// services are real in-memory ones from a throwaway `AppState`.
pub fn test_harness(
    db: &Surreal<Db>,
    config: &frona::core::config::Config,
    mock_provider: Arc<dyn ModelProvider>,
) -> Arc<frona::agent::harness::Harness> {
    use frona::agent::harness::Harness;
    use frona::chat::service::ChatService;
    use frona::core::state::AppState;

    init_metrics();
    let metrics_handle = metrics::setup_metrics_recorder();
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let storage = frona::storage::StorageService::new(config);
    let mut state = AppState::new(
        db.clone(),
        config,
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
    );

    // ChatService wired to the mock provider so all harness inference hits it.
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), mock_provider);
    let mut groups = HashMap::new();
    groups.insert("test".to_string(), test_model_group());
    let mock_registry = ModelProviderRegistry::for_testing(providers, groups);
    let chat_service = ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        state.agent_service.clone(),
        mock_registry,
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
    );
    state.chat_service = chat_service.clone();
    let memory_service = test_memory_service(&state, db);
    Arc::new(Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        memory_service,
        state.skill_service.clone(),
        state.task_service.clone(),
        state.vault_service.clone(),
        state.mcp_service.clone(),
        state.tool_manager.clone(),
        state.policy_service.clone(),
        state.broadcast_service.clone(),
        state.active_sessions.clone(),
        state.shutdown_token.clone(),
        state.prompts.clone(),
        state.config.clone(),
        state.usage_service.clone(),
    ))
}
