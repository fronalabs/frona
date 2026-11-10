#![allow(dead_code)]
pub mod provider;
#[path = "../../../frona-harness/tests/helpers/rig.rs"]
pub mod rig;

// Each integration test file compiles as its own binary and pulls in this
// helpers module; helpers used only by other test files surface as dead-code
// warnings in every binary that doesn't reference them. Silencing module-wide.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use frona::core::metrics;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::provider::service::ModelProviderService;
use frona::policy::service::PolicyService;
use frona::tool::{AgentTool, InferenceContext, ToolDefinition, ToolOutput};
use frona_harness::{
    config::RetryConfig,
    inference::{
        ModelGroup, Usage,
        error::InferenceError,
        provider::{ModelConfig, ModelProvider},
    },
};
use surrealdb::{Surreal, engine::local::Db};

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
        frona_model_catalog::ModelCatalogStore::new(
            frona_model_catalog::ModelCatalogSnapshot::empty(),
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

pub async fn test_policy_service(db: &Surreal<Db>) -> PolicyService {
    let schema = frona::policy::schema::build_schema();
    let repo: Arc<dyn frona::policy::repository::PolicyRepository> =
        Arc::new(SurrealRepo::<frona::policy::models::Policy>::new(
            db.clone(),
        ));
    let tool_fixture = app_state::build(db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();
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
    Content(Vec<AssistantContent>),
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

#[derive(Clone)]
pub struct MockModelProvider {
    responses: Arc<Mutex<Vec<MockResponse>>>,
    pub call_count: Arc<Mutex<usize>>,
    /// The `chat_history` of the most recent call, which is what the conversation sent
    /// back, which is the only way to assert on messages a loop appends internally
    /// (tool results, feedback) rather than returns.
    last_history: Arc<Mutex<Vec<RigMessage>>>,
    histories: Arc<Mutex<Vec<Vec<RigMessage>>>>,
    tool_histories: Arc<Mutex<Vec<Vec<RigToolDefinition>>>>,
    system_prompts: Arc<Mutex<Vec<String>>>,
    model_names: Arc<Mutex<Vec<String>>>,
    request_settings: Arc<Mutex<Vec<(Option<u64>, Option<f64>)>>>,
    usage: Arc<Mutex<Usage>>,
}

impl MockModelProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            call_count: Arc::new(Mutex::new(0)),
            last_history: Arc::new(Mutex::new(Vec::new())),
            histories: Arc::new(Mutex::new(Vec::new())),
            tool_histories: Arc::new(Mutex::new(Vec::new())),
            system_prompts: Arc::new(Mutex::new(Vec::new())),
            model_names: Arc::new(Mutex::new(Vec::new())),
            request_settings: Arc::new(Mutex::new(Vec::new())),
            usage: Arc::new(Mutex::new(
                Usage::new()
                    .input_tokens(Some(10))
                    .output_tokens(Some(5))
                    .total_tokens(Some(15)),
            )),
        }
    }

    pub fn set_usage(&self, usage: Usage) {
        *self.usage.lock().unwrap() = usage;
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

    pub fn system_prompts(&self) -> Vec<String> {
        self.system_prompts.lock().unwrap().clone()
    }
    pub fn model_names(&self) -> Vec<String> {
        self.model_names.lock().unwrap().clone()
    }

    pub fn request_settings(&self) -> Vec<(Option<u64>, Option<f64>)> {
        self.request_settings.lock().unwrap().clone()
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
    async fn resolve(
        &self,
        model: &ModelConfig,
    ) -> Result<frona_harness::inference::provider::ResolvedModel, InferenceError> {
        // Typed application stops originate at the resolver boundary, not in a
        // provider wire error. Preserve them without converting controls to text.
        if self.responses.lock().unwrap().first().is_some_and(
            |response| matches!(response, MockResponse::Error(error) if error.is_terminal()),
        ) {
            let MockResponse::Error(error) = self.next_response() else {
                unreachable!()
            };
            return Err(error);
        }
        let provider = self.clone();
        let id = model.model_id.clone();
        Ok(frona_harness::inference::provider::ResolvedModel::new(
            rig::model(id.clone(), move |request, mode| {
                let provider = provider.clone();
                let id = id.clone();
                rig_core::driver::Opening::new(async move {
                    let mut messages = request
                        .chat_history
                        .into_iter()
                        .map(|message| (*message).clone())
                        .collect::<Vec<_>>();
                    let system = if matches!(messages.first(), Some(RigMessage::System { .. })) {
                        match messages.remove(0) {
                            RigMessage::System { content } => content,
                            _ => unreachable!(),
                        }
                    } else {
                        String::new()
                    };
                    provider.system_prompts.lock().unwrap().push(system);
                    provider.model_names.lock().unwrap().push(id);
                    provider
                        .request_settings
                        .lock()
                        .unwrap()
                        .push((request.max_tokens, request.temperature));
                    *provider.last_history.lock().unwrap() = messages.clone();
                    provider.histories.lock().unwrap().push(messages.clone());
                    provider.tool_histories.lock().unwrap().push(
                        request
                            .tools
                            .into_iter()
                            .map(|tool| (*tool).clone())
                            .collect(),
                    );
                    let response = match mode {
                        rig_core::wire::Mode::Unary => {
                            provider.next_response_for_history(&messages)
                        }
                        rig_core::wire::Mode::Streaming => provider.next_response(),
                    };
                    let response = match response {
                        MockResponse::Barrier(barrier, response) => {
                            barrier.wait().await;
                            *response
                        }
                        response => response,
                    };
                    let content = match response {
                        MockResponse::Content(content) => content,
                        MockResponse::Text(text) => vec![AssistantContent::text(text)],
                        MockResponse::TextWithReasoning(text, reasoning) => vec![
                            AssistantContent::Reasoning(rig_core::message::Reasoning::new(
                                reasoning,
                            )),
                            AssistantContent::text(text),
                        ],
                        MockResponse::ToolCalls(calls) => calls
                            .into_iter()
                            .map(|(id, name, args)| {
                                AssistantContent::ToolCall(ToolCall::new(
                                    rig_core::message::CallId::from_wire(id),
                                    ToolFunction::new(
                                        name.try_into().expect("test tool name"),
                                        args,
                                    ),
                                ))
                            })
                            .collect(),
                        MockResponse::Error(error) => return Err(rig::error(error)),
                        MockResponse::Pending => std::future::pending().await,
                        MockResponse::PendingWithDropDelay(delay) => {
                            pending_with_drop_delay(delay).await
                        }
                        MockResponse::Barrier(_, _) => {
                            unreachable!("nested mock barriers are unsupported")
                        }
                        MockResponse::ForUserText { .. } => {
                            unreachable!("history-routed responses require non-streaming inference")
                        }
                    };
                    let usage = *provider.usage.lock().unwrap();
                    rig::reply(frona_harness::ModelOutput::new(
                        content,
                        frona_harness::UsageReport::Reported(usage),
                    ))
                    .await
                })
            }),
        ))
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
    pub task_event: Option<frona::inference::tool_call::TaskEvent>,
}

impl MockAttachmentTool {
    pub fn new(name: &str, attachment: frona::storage::Attachment) -> Self {
        Self {
            tool_name: name.to_string(),
            attachment,
            task_event: None,
        }
    }

    pub fn with_task_event(mut self, task_event: frona::inference::tool_call::TaskEvent) -> Self {
        self.task_event = Some(task_event);
        self
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
        let mut output = ToolOutput::text("file produced").with_attachment(self.attachment.clone());
        if let Some(task_event) = &self.task_event {
            output = output.with_task_event(task_event.clone());
        }
        Ok(output)
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
            agent_context_state: Default::default(),
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
        providers: Default::default(),
        name: "test".into(),
        main: ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: frona::core::Handle::const_validated("mock").into(),
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
    group.fallbacks.push(ModelConfig {
        request_settings: Default::default(),
        catalog_provider: String::new(),
        provider_handle: frona::core::Handle::try_new(fallback_provider)
            .unwrap()
            .into(),
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

pub fn test_providers(
    name: &str,
    provider: Arc<dyn ModelProvider>,
) -> Arc<HashMap<String, Arc<dyn ModelProvider>>> {
    Arc::new([(name.to_string(), provider)].into())
}

pub async fn test_model_service_with_group(
    provider_name: &str,
    provider: Arc<dyn ModelProvider>,
    group_name: &str,
    group: ModelGroup,
) -> ModelProviderService {
    test_model_service(
        [(provider_name.to_string(), provider)].into(),
        [(group_name.to_string(), group)].into(),
    )
    .await
}

pub async fn test_model_service(
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    mut groups: HashMap<String, ModelGroup>,
) -> ModelProviderService {
    let shared = Arc::new(providers.clone());
    for group in groups.values_mut() {
        group.providers = shared.clone();
    }
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let fixture = app_state::build(&db).await;
    let service = fixture.state.model_provider_service;
    ModelProviderService::new(
        service.directory,
        service.config_service,
        service.store,
        service.validator,
        service.runtime,
        service.active,
        groups,
        providers,
    )
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

/// Explicit empty source set for tests that do not exercise live catalog collection.
pub fn empty_agent_context_manager(db: &Surreal<Db>) -> frona::agent::context::AgentContextManager {
    frona::agent::context::AgentContextManager::new(
        vec![],
        frona::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo(db.clone()),
    )
}

/// Static memory is outside the subject of these isolated chat tests.
pub fn empty_memory_service() -> std::sync::Arc<dyn frona::memory::service::MemoryService> {
    struct EmptyMemory;
    #[async_trait::async_trait]
    impl frona::memory::service::MemoryService for EmptyMemory {
        fn tools(&self) -> Vec<std::sync::Arc<dyn frona::tool::AgentTool>> {
            vec![]
        }
        async fn instructions(
            &self,
            _: &frona::auth::User,
        ) -> Result<String, frona::core::error::AppError> {
            Ok(String::new())
        }
        async fn collect(
            &self,
            _: &frona::agent::context::AgentContextScope<'_>,
        ) -> Result<Vec<frona::agent::context::AgentContextSection>, frona::core::error::AppError>
        {
            Ok(vec![])
        }
    }
    std::sync::Arc::new(EmptyMemory)
}

pub fn chat_service_with_context(
    state: &frona::core::state::AppState,
    manager: frona::agent::context::AgentContextManager,
) -> frona::chat::service::ChatService {
    let agent_prompt_builder = frona::agent::prompt::AgentPromptBuilder::new(
        state.prompts.clone(),
        state.storage_service.clone(),
        test_memory_service(state, &state.db),
        state.skill_service.clone(),
    );
    frona::chat::service::ChatService::new(
        SurrealRepo::new(state.db.clone()),
        SurrealRepo::new(state.db.clone()),
        SurrealRepo::new(state.db.clone()),
        state.agent_service.clone(),
        state.model_provider_service.clone(),
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
        manager,
        agent_prompt_builder,
    )
}

/// Create a minimal ChatService backed by an in-memory SurrealDB for tool loop tests.
pub async fn test_chat_service() -> frona::chat::service::ChatService {
    build_test_chat_service(None).await
}

pub async fn test_chat_service_for_turn(message_id: &str) -> frona::chat::service::ChatService {
    build_test_chat_service(Some(message_id)).await
}

pub async fn seed_tool_turn(db: &Surreal<Db>, ctx: &InferenceContext, message_id: &str) {
    use frona::chat::message::models::Message;
    use frona::chat::message::models::MessageRole;
    use frona::chat::message::models::MessageStatus;
    use frona::core::repository::Repository;
    SurrealRepo::new(db.clone())
        .create(&ctx.user)
        .await
        .unwrap();
    SurrealRepo::new(db.clone())
        .create(&ctx.agent)
        .await
        .unwrap();
    let chat = ctx.chat.as_ref().unwrap();
    SurrealRepo::new(db.clone()).create(chat).await.unwrap();
    let mut message = Message::builder(&chat.id, MessageRole::Agent, String::new())
        .agent_id(ctx.agent.id.clone())
        .status(MessageStatus::Executing)
        .build();
    message.id = message_id.into();
    SurrealRepo::new(db.clone()).create(&message).await.unwrap();
}

async fn build_test_chat_service(message_id: Option<&str>) -> frona::chat::service::ChatService {
    use frona::db::repo::generic::SurrealRepo;
    use surrealdb::{Surreal, engine::local::Mem};

    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();

    if let Some(id) = message_id {
        seed_tool_turn(&db, &mock_context(), id).await;
    }

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
        test_policy_service(&db).await,
        user_service.clone(),
    );
    let provider_registry = test_model_service(HashMap::new(), HashMap::new()).await;

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

    let skill_service = frona::agent::skill::service::SkillService::new(
        frona::agent::skill::registry::SkillRegistryClient::new(
            reqwest::Client::new(),
            format!("{base}/cache"),
        ),
        frona::agent::skill::resolver::SkillResolver::new(&base, storage.clone(), &base),
        storage.clone(),
        &base,
        &config.cache,
    );

    let agent_prompt_builder = frona::agent::prompt::AgentPromptBuilder::new(
        frona::agent::prompt::PromptLoader::new(&base),
        storage.clone(),
        empty_memory_service(),
        skill_service,
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
        empty_agent_context_manager(&db),
        agent_prompt_builder,
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
        std::sync::Arc::new(state.chat_service.model_providers().clone()),
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
pub async fn test_harness(
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
        {
            let directory = tempfile::tempdir().unwrap();
            let mut loaded =
                frona::core::config::ConfigService::load(directory.path().join("config.yaml"))
                    .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
        app_state::catalogs(config),
    );

    // ChatService wired to the mock provider so all harness inference hits it.
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), mock_provider);
    let mut groups = HashMap::new();
    groups.insert("test".to_string(), test_model_group());
    let mock_registry = test_model_service(providers, groups).await;
    let agent_prompt_builder = frona::agent::prompt::AgentPromptBuilder::new(
        state.prompts.clone(),
        state.storage_service.clone(),
        empty_memory_service(),
        state.skill_service.clone(),
    );
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
        empty_agent_context_manager(&state.db),
        agent_prompt_builder,
    );
    state.chat_service = chat_service.clone();
    let memory_service = test_memory_service(&state, db);
    Arc::new(Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        memory_service,
        state.task_service.clone(),
        state.mcp_service.clone(),
        state.tool_manager.clone(),
        state.policy_service.clone(),
        state.broadcast_service.clone(),
        state.active_sessions.clone(),
        state.execution_registry.clone(),
        state.shutdown_token.clone(),
        state.prompts.clone(),
        state.config.clone(),
        state.usage_service.clone(),
    ))
}

pub mod app_state;
