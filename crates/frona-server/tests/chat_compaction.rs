use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use frona::chat::compactor::{
    ChatCompactor, ChatSummarizer, CompactionContext, CompactionOutcome, SummaryRequest,
};
use frona::chat::message::models::{Message, MessageRole, MessageStatus};
use frona::chat::message::repository::MessageRepository;
use frona::core::error::AppError;
use frona::core::repository::Repository;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::db::repo::tool_calls::ToolCallRepository;
use frona::inference::conversation::{ConversationBuilder, ConversationContext};
use frona_text::prompt::PromptLoader;
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

#[derive(Default)]
struct Summary {
    inputs: Mutex<Vec<String>>,
}
#[async_trait]
impl ChatSummarizer for Summary {
    async fn summarize(&self, request: SummaryRequest<'_>) -> Result<String, AppError> {
        self.inputs
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.history).unwrap());
        Ok("[SUMMARY]".into())
    }
}

async fn fixture() -> (Surreal<Db>, TestCompactor, SurrealMessageRepo, Arc<Summary>) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let mut chat = helpers::mock_context().chat.unwrap();
    chat.id = "chat".into();
    chat.user_id = "user".into();
    chat.agent_id = "agent".into();
    SurrealRepo::new(db.clone()).create(&chat).await.unwrap();
    let messages = SurrealRepo::new(db.clone());
    let summary = Arc::new(Summary::default());
    let compactor = ChatCompactor::new(
        SurrealRepo::new(db.clone()),
        messages.clone(),
        summary.clone(),
        PromptLoader::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
        ),
        helpers::empty_agent_context_manager(&db),
    );
    let state = helpers::app_state::build(&db).await;
    let compactor = TestCompactor {
        inner: compactor,
        db: db.clone(),
        state,
    };
    (db, compactor, messages, summary)
}

struct TestCompactor {
    inner: ChatCompactor,
    db: Surreal<Db>,
    state: helpers::app_state::Fixture,
}
impl std::ops::Deref for TestCompactor {
    type Target = ChatCompactor;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl TestCompactor {
    async fn compact(
        &self,
        user: &str,
        chat: &str,
        agent: &str,
        system: &str,
        window: usize,
        output: usize,
    ) -> Result<CompactionOutcome, AppError> {
        let loaded = self.load_conversation(chat).await?;
        let state = &self.state.state;
        let builder = DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
        };
        let tools: frona::db::repo::tool_calls::SurrealToolCallRepo =
            SurrealRepo::new(self.db.clone());
        let mut model_history = builder
            .build(
                &loaded.messages,
                &tools.find_by_chat_id(chat).await?,
                &ConversationContext {
                    agent_id: agent.into(),
                    user_id: user.into(),
                    model_config: helpers::test_model_group().main,
                },
                loaded.summary.as_deref(),
            )
            .await;
        model_history.set_summary_context(loaded.agent_context.as_deref());
        let mut group = helpers::test_model_group();
        group.context_window = window;
        group.main.request_settings.max_tokens = Some(output as u64);
        let mut ctx = helpers::mock_context();
        ctx.user.id = user.into();
        ctx.agent.id = agent.into();
        ctx.agent.user_id = user.into();
        self.inner
            .compact_chat(
                chat,
                CompactionContext {
                    agent_context: ctx.agent_context_scope(),
                    system_prompt: system,
                    model_group: &group,
                    model_history: &model_history,
                    tools: &[],
                },
                frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default()),
            )
            .await
    }
}

fn row(id: &str, seconds: i64, role: MessageRole, content: &str) -> Message {
    let mut msg = Message::builder("chat", role, content.into())
        .agent_id("agent".into())
        .build();
    msg.agent_context_signature = Some(
        frona::agent::context::AgentContextSnapshot::new(vec![])
            .unwrap()
            .signature,
    );
    msg.id = id.into();
    msg.created_at = chrono::DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap();
    msg
}

#[tokio::test]
async fn latest_request_survives_target_overflow_and_empty_placeholder() {
    let (_, compactor, messages, summary) = fixture().await;
    let old = row("old", 0, MessageRole::User, &"old ".repeat(100));
    let request = row("request", 1, MessageRole::User, &"active ".repeat(45));
    let mut placeholder = row("placeholder", 2, MessageRole::Agent, "");
    placeholder.status = Some(MessageStatus::Executing);
    for msg in [&old, &request, &placeholder] {
        messages.create(msg).await.unwrap();
    }
    let outcome = compactor
        .compact("user", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(outcome.compacted);
    assert_eq!(
        outcome.model_history.row_ids,
        vec![request.id.clone(), placeholder.id.clone()]
    );
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![request, placeholder]
    );
    assert!(!summary.inputs.lock().unwrap()[0].contains("active"));
}

#[tokio::test]
async fn filtered_rows_do_not_shift_persisted_split() {
    let (_, compactor, messages, _) = fixture().await;
    let filtered = row("filtered", 0, MessageRole::System, "");
    let old = row("old", 1, MessageRole::User, &"old ".repeat(100));
    let request = row("request", 2, MessageRole::User, "active");
    for msg in [&old, &filtered, &request] {
        messages.create(msg).await.unwrap();
    }
    let outcome = compactor
        .compact("user", "chat", "agent", "", 100, 0)
        .await
        .unwrap();
    assert!(outcome.compacted);
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![request]
    );
}

mod helpers;

use frona::agent::harness::Harness;
use frona::chat::command::builtin::compact::CompactCommand;
use frona::chat::command::{Command, CommandContext, CommandOutcome};
use frona::chat::repository::ChatSummaryRepository;
use frona::chat::service::ChatService;
use frona::chat::session::{CancellationToken, ChatSessionContext};
use frona::{
    db::repo::chat_summaries::SurrealChatSummaryRepo,
    inference::conversation::DefaultConversationBuilder, storage::Attachment,
};
use frona_harness::inference::provider::ModelProvider;
use helpers::{MockModelProvider, MockResponse};
use rig_core::completion::message::{DocumentSourceKind, UserContent};
use rig_core::completion::{AssistantContent, Message as RigMessage};

// Compare provider capture against Rig's independently prepared wire view while
// keeping assertions about the persisted/canonical conversation separate.
fn native_history(
    history: &[RigMessage],
    system: &str,
    model: &str,
    tools: &[rig_core::completion::ToolDefinition],
) -> Vec<RigMessage> {
    use rig_core::{
        operation::Completion,
        test_utils::MockScript,
        wire::{Operation, Wire},
    };
    let mut request = rig_core::completion::CompletionRequest::new("");
    request.chat_history = std::iter::once(Arc::new(RigMessage::system(system)))
        .chain(history.iter().cloned().map(Arc::new))
        .collect();
    request.tools = tools.iter().cloned().map(Arc::new).collect();
    Completion::prepare(
        request,
        &MockScript::new("fixture").with_id(model).describe(),
    )
    .unwrap()
    .chat_history
    .into_iter()
    .skip(1)
    .map(Arc::unwrap_or_clone)
    .collect()
}

struct App {
    db: Surreal<Db>,
    fixture: helpers::app_state::Fixture,
    harness: Harness,
    chat_service: ChatService,
    model: Arc<MockModelProvider>,
    other_models: Vec<Arc<MockModelProvider>>,
    user: frona::auth::User,
    chat: frona::chat::models::Chat,
}

impl App {
    async fn new() -> Self {
        Self::with_window(18_000).await
    }

    async fn with_window(window: usize) -> Self {
        Self::with_route(window, true).await
    }

    async fn with_route(window: usize, legacy_groups: bool) -> Self {
        Self::with_provider(
            window,
            legacy_groups,
            frona_harness::config::ProviderModel::Generic,
        )
        .await
    }

    async fn with_provider(
        window: usize,
        legacy_groups: bool,
        provider: frona_harness::config::ProviderModel,
    ) -> Self {
        Self::with_provider_and_events(window, legacy_groups, provider, None).await
    }

    async fn with_provider_and_events(
        window: usize,
        legacy_groups: bool,
        provider: frona_harness::config::ProviderModel,
        events: Option<frona::chat::broadcast::BroadcastService>,
    ) -> Self {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let fixture = helpers::app_state::build(&db).await;
        let state = &fixture.state;
        let events = events.unwrap_or_else(|| state.broadcast_service.clone());
        // These tests exercise compaction on initialized histories; legacy
        // initialization has its own transactional tests in agent_context.rs.
        frona::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo(db.clone())
            .ensure_stored(&frona::agent::context::AgentContextSnapshot::new(vec![]).unwrap())
            .await
            .unwrap();
        let model = Arc::new(MockModelProvider::new(vec![]));
        let mut conversation = helpers::test_model_group();
        conversation.name = "conversation".into();
        conversation.context_window = window;
        conversation.main.model_id = "conversation-model".into();
        conversation.main.provider = provider;
        conversation.main.request_settings.max_tokens = Some(4096);
        conversation.main.request_settings.temperature = Some(0.37);
        let mut providers = std::collections::HashMap::from([(
            "mock".into(),
            model.clone() as Arc<dyn ModelProvider>,
        )]);
        let mut target = conversation.clone();
        target.name = "target".into();
        target.context_window = 18_000;
        target.main.model_id = "target-model".into();
        target.main.request_settings.temperature = Some(0.21);
        let mut groups = std::collections::HashMap::from([
            ("conversation".into(), conversation),
            ("target".into(), target),
        ]);
        let mut other_models = Vec::new();
        if legacy_groups {
            for name in ["primary", "compaction"] {
                let provider = Arc::new(MockModelProvider::new(vec![]));
                let mut group = helpers::test_model_group();
                group.name = name.into();
                group.main.provider_handle = frona::core::Handle::try_new(name).unwrap().into();
                group.main.provider = name.into();
                group.main.model_id = format!("unused-{name}-model");
                providers.insert(name.into(), provider.clone() as Arc<dyn ModelProvider>);
                groups.insert(name.into(), group);
                other_models.push(provider);
            }
        }
        let registry = helpers::test_model_service(providers, groups).await;
        let agent_prompt_builder = frona::agent::prompt::AgentPromptBuilder::new(
            state.prompts.clone(),
            state.storage_service.clone(),
            helpers::empty_memory_service(),
            state.skill_service.clone(),
        );
        let chat_service = ChatService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            state.agent_service.clone(),
            registry,
            state.storage_service.clone(),
            state.user_service.clone(),
            state.prompts.clone(),
            events.clone(),
            state.presign_service.clone(),
            state.usage_service.clone(),
            helpers::empty_agent_context_manager(&state.db),
            agent_prompt_builder,
        );
        let harness = Harness::new(
            chat_service.clone(),
            state.user_service.clone(),
            state.storage_service.clone(),
            state.agent_service.clone(),
            helpers::test_memory_service(state, &db),
            state.task_service.clone(),
            state.mcp_service.clone(),
            state.tool_manager.clone(),
            state.policy_service.clone(),
            events.clone(),
            state.active_sessions.clone(),
            state.execution_registry.clone(),
            state.shutdown_token.clone(),
            state.prompts.clone(),
            state.config.clone(),
            state.usage_service.clone(),
        );
        let context = helpers::mock_context();
        let user = context.user;
        let mut agent = context.agent;
        agent.prompt = Some("You are a helpful assistant.".into());
        agent.model_group = "conversation".into();
        let mut chat = context.chat.unwrap();
        chat.id = "chat".into();
        chat.title = Some("Compaction regression".into());
        SurrealRepo::new(db.clone()).create(&user).await.unwrap();
        SurrealRepo::new(db.clone()).create(&agent).await.unwrap();
        SurrealRepo::new(db.clone()).create(&chat).await.unwrap();
        chat_service
            .agent_context_manager()
            .prepare(
                &frona::agent::context::AgentContextScope {
                    user: &user,
                    agent: &agent,
                    space_id: chat.space_id.as_deref(),
                    task: None,
                },
                None,
                &mut Default::default(),
            )
            .await
            .unwrap();
        Self {
            db,
            fixture,
            harness,
            chat_service,
            model,
            other_models,
            user,
            chat,
        }
    }

    fn messages(&self) -> SurrealMessageRepo {
        SurrealRepo::new(self.db.clone())
    }
    fn summaries(&self) -> SurrealChatSummaryRepo {
        SurrealRepo::new(self.db.clone())
    }

    fn summary_indices(&self) -> Vec<usize> {
        self.model.histories().iter().enumerate().filter_map(|(index, history)| {
            matches!(history.last(), Some(RigMessage::User { content }) if content.iter().any(|item| matches!(item, UserContent::Text(text) if text.text.contains("Do not call tools or continue the conversation.")))).then_some(index)
        }).collect()
    }

    fn summary_calls(&self) -> usize {
        self.summary_indices().len()
    }
    fn inference_calls(&self) -> usize {
        self.model.calls() - self.summary_calls()
    }
    fn summary_history(&self) -> Vec<RigMessage> {
        self.model.histories()[*self.summary_indices().last().unwrap()].clone()
    }
    fn summary_system_prompt(&self) -> String {
        self.model.system_prompts()[*self.summary_indices().last().unwrap()].clone()
    }
    fn summary_tools(&self) -> Vec<rig_core::completion::request::ToolDefinition> {
        self.model.tool_histories()[*self.summary_indices().last().unwrap()].clone()
    }

    async fn assert_summary_usage(&self, group: &str, model: &str) {
        let rows: Vec<serde_json::Value> = self
            .db
            .query("SELECT * FROM inference_usage WHERE kind_tag = 'Compaction'")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(!rows.is_empty());
        for usage in rows {
            assert_eq!(usage["model_group"], group);
            assert_eq!(usage["model_id"], model);
            assert_eq!(usage["provider"], "mock");
            assert_eq!(usage["chat_id"], "chat");
            assert_eq!(usage["user_id"], self.user.id);
        }
        assert!(
            self.other_models
                .iter()
                .all(|provider| provider.calls() == 0)
        );
    }

    async fn session(&self) -> Result<ChatSessionContext, AppError> {
        let state = &self.fixture.state;
        ChatSessionContext::build(
            &self.harness,
            &self.user.id,
            self.chat.clone(),
            None,
            CancellationToken::new(),
            Arc::new(DefaultConversationBuilder {
                user_service: state.user_service.clone(),
                storage_service: state.storage_service.clone(),
                agent_service: state.agent_service.clone(),
            }),
            &[],
            Default::default(),
        )
        .await
    }

    async fn manual(&self, session: &mut ChatSessionContext) -> Result<CommandOutcome, AppError> {
        let mut chat = self.chat.clone();
        let mut request = row("command", 99, MessageRole::User, "/compact");
        let mut response = row("response", 100, MessageRole::Agent, "");
        CompactCommand
            .run(
                "",
                &mut CommandContext {
                    harness: &self.harness,
                    session,
                    user: &self.user,
                    chat: &mut chat,
                    request: &mut request,
                    response: &mut response,
                },
            )
            .await
    }

    async fn history(&self, role: MessageRole, attachment_only: bool) -> Vec<Message> {
        // One old row exceeds the retention target while the whole native
        // prefix fits the conversation model's own context window.
        for i in 0..4 {
            self.messages()
                .create(&row(
                    &format!("old-{i}"),
                    i,
                    MessageRole::User,
                    &format!("OLD-{i} ").repeat(if i == 3 { 5800 } else { 1 }),
                ))
                .await
                .unwrap();
        }
        let image_path = self
            .fixture
            .state
            .storage_service
            .resolve_virtual_path(&frona::storage::VirtualPath::user(
                &self.user.handle,
                "diagram.png",
            ))
            .unwrap();
        tokio::fs::create_dir_all(image_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&image_path, b"exact-image-content")
            .await
            .unwrap();
        let attachments = vec![
            Attachment {
                filename: "diagram.png".into(),
                content_type: "image/png".into(),
                size_bytes: 19,
                owner: format!("user:{}", self.user.id),
                path: "diagram.png".into(),
                url: None,
            },
            Attachment {
                filename: "report.txt".into(),
                content_type: "text/plain".into(),
                size_bytes: 10,
                owner: format!("user:{}", self.user.id),
                path: "report.txt".into(),
                url: None,
            },
        ];
        let mut request = row(
            "active-request",
            10,
            role,
            if attachment_only {
                ""
            } else {
                "EXACT ACTIVE REQUEST: keep spacing  and punctuation!?"
            },
        );
        request.attachments = attachments;
        let filtered = row("filtered", 11, MessageRole::System, "");
        let mut tool_message = row(
            "tool-message",
            12,
            MessageRole::Agent,
            "Following assistant text",
        );
        tool_message.agent_id = Some(self.chat.agent_id.clone());
        tool_message.status = Some(MessageStatus::Completed);
        let mut placeholder = row("placeholder", 13, MessageRole::Agent, "");
        placeholder.agent_id = Some(self.chat.agent_id.clone());
        placeholder.status = Some(MessageStatus::Executing);
        let retained = vec![request, filtered, tool_message, placeholder];
        for msg in &retained {
            self.messages().create(msg).await.unwrap();
        }
        let call = tool(&retained[2], "retained tool result", 0);
        SurrealRepo::new(self.db.clone())
            .create(&call)
            .await
            .unwrap();
        retained
    }

    async fn assert_context(&self, retained: &[Message], session: ChatSessionContext) {
        let reloaded = self
            .chat_service
            .compactor()
            .load_conversation("chat")
            .await
            .unwrap();
        assert!(reloaded.summary.is_some());
        assert_eq!(reloaded.messages, retained);
        // Cross the actual inference boundary, including its history truncation.
        let history = session.rig_history;
        let canonical = serde_json::to_value(&history).unwrap();
        let expected = native_history(&history, &session.system_prompt, "conversation-model", &[]);
        frona::inference::text_inference(
            &session.model_group,
            &session.system_prompt,
            history.clone(),
            &self.fixture.state.usage_service,
            &helpers::test_usage_ctx(),
        )
        .await
        .unwrap();
        let actual = self.model.last_history();
        assert_eq!(actual, expected);
        assert_eq!(serde_json::to_value(&history).unwrap(), canonical);
        let active = &retained[0];
        let texts: Vec<&str> = actual
            .iter()
            .flat_map(|msg| match msg {
                RigMessage::User { content } => content
                    .iter()
                    .filter_map(|c| {
                        if let UserContent::Text(t) = c {
                            Some(t.text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        let expected_prefix = if active.role == MessageRole::LiveCall {
            format!("[LIVE_CALL] {}", active.content)
        } else {
            active.content.clone()
        };
        let exact = texts
            .iter()
            .filter(|t| t.starts_with(&format!("{expected_prefix}\n<files>\n")))
            .collect::<Vec<_>>();
        assert_eq!(exact.len(), 1, "active request must occur exactly once");
        assert!(exact[0].contains("report.txt"));
        let images = actual
            .iter()
            .flat_map(|msg| match msg {
                RigMessage::User { content } => content
                    .iter()
                    .filter_map(|c| {
                        if let UserContent::Image(image) = c {
                            Some(image)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].data,
            DocumentSourceKind::Base64("ZXhhY3QtaW1hZ2UtY29udGVudA==".into())
        );
        // Text-only inference projects tool pairs to text; the canonical
        // conversation must still retain the exact callable pair and order.
        let call_at = history.iter().position(|m| matches!(m, RigMessage::Assistant(rig_core::message::AssistantMessage { content, .. }) if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(call) if call.function.name == "read_file")))).unwrap();
        assert!(
            matches!(&history[call_at + 1], RigMessage::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
        );
        assert!(
            matches!(&history[call_at + 2], RigMessage::Assistant(rig_core::message::AssistantMessage { content, .. }) if content.iter().any(|c| matches!(c, AssistantContent::Text(t) if t.text == "Following assistant text")))
        );
    }
}

fn tool(message: &Message, result: &str, turn: u32) -> frona::inference::tool_call::ToolCall {
    frona::inference::tool_call::ToolCall {
        id: format!("tool-{}-{turn}", message.id),
        chat_id: message.chat_id.clone(),
        message_id: message.id.clone(),
        turn,
        provider_item_id: None,
        provider_call_id: format!("call-{}-{turn}", message.id),
        name: "read_file".into(),
        arguments: serde_json::json!({"path": "report.txt"}),
        result: result.into(),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: Some("Reading report".into()),
        signature: None,
        turn_reasoning: None,
        created_at: message.created_at,
    }
}

#[tokio::test]
async fn automatic_session_preserves_all_input_kinds_and_attachment_only_requests() {
    for role in [
        MessageRole::User,
        MessageRole::Contact,
        MessageRole::LiveCall,
        MessageRole::TaskCompletion,
    ] {
        for attachment_only in [false, true] {
            let app = App::new().await;
            let retained = app.history(role.clone(), attachment_only).await;
            let session = app.session().await.unwrap();
            assert_eq!(app.summary_calls(), 1);
            app.assert_summary_usage("conversation", "conversation-model")
                .await;
            app.assert_context(&retained, session).await;
            // A second build reloads the durable cutoff and must neither duplicate
            // the request nor summarize it again.
            let session = app.session().await.unwrap();
            app.assert_context(&retained, session).await;
            assert_eq!(app.summary_calls(), 1);
        }
    }
}

#[tokio::test]
async fn manual_command_persists_then_reloads_exact_request_and_tools() {
    for role in [
        MessageRole::User,
        MessageRole::Contact,
        MessageRole::LiveCall,
        MessageRole::TaskCompletion,
    ] {
        let app = App::with_window(128_000).await;
        let retained = app.history(role, false).await;
        let mut session = app.session().await.unwrap();
        session.model_group.context_window = 18_000;
        assert!(
            app.summaries()
                .find_by_chat_id("chat")
                .await
                .unwrap()
                .is_none()
        );
        let result = app.manual(&mut session).await.unwrap();
        assert!(
            matches!(result, CommandOutcome::Message(text) if text == "Compacted older messages into a summary.")
        );
        assert_eq!(app.summary_calls(), 1);
        app.assert_context(&retained, app.session().await.unwrap())
            .await;
    }
}

#[tokio::test]
async fn equal_timestamps_keep_whole_boundary_group_and_reload_in_stable_order() {
    let (_, compactor, messages, _) = fixture().await;
    let old = row("old", 0, MessageRole::User, &"old ".repeat(150));
    let boundary = row("a-boundary", 1, MessageRole::Agent, "keep this tied row");
    let request = row("b-request", 1, MessageRole::User, &"active ".repeat(40));
    let following = row("c-placeholder", 1, MessageRole::Agent, "");
    // Deliberately insert out of ID order; timestamps alone cannot order these.
    for msg in [&following, &request, &old, &boundary] {
        messages.create(msg).await.unwrap();
    }
    let out = compactor
        .compact("u", "chat", "agent", "", 140, 0)
        .await
        .unwrap();
    assert!(out.compacted);
    let expected = vec![boundary, request, following];
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        expected
    );
    for _ in 0..3 {
        assert_eq!(
            compactor.load_conversation("chat").await.unwrap().messages,
            expected
        );
    }
}

#[tokio::test]
async fn tool_results_drive_budget_and_are_summarized_with_their_owning_row() {
    let (db, compactor, messages, summary) = fixture().await;
    let mut old = row("old-tool", 0, MessageRole::Agent, "");
    old.status = Some(MessageStatus::Executing);
    let result = "expensive result ".repeat(100);
    let call = tool(&old, &result, 0);
    SurrealRepo::new(db).create(&call).await.unwrap();
    let active = row("request", 1, MessageRole::User, "active");
    for msg in [&old, &active] {
        messages.create(msg).await.unwrap();
    }
    let out = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(out.compacted, "tool result alone must trigger compaction");
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![active]
    );
    let inputs = summary.inputs.lock().unwrap();
    assert!(inputs[0].contains(&result));
    assert!(inputs[0].contains("read_file"));
    assert!(inputs[0].contains("report.txt"));
}

#[tokio::test]
async fn commands_and_empty_events_do_not_replace_the_substantive_request() {
    let (_, compactor, messages, summary) = fixture().await;
    let old = row("old", 0, MessageRole::User, &"old ".repeat(100));
    let active = row("active", 1, MessageRole::User, &"keep ".repeat(55));
    let empty = row("empty", 2, MessageRole::TaskCompletion, "");
    let mut command = row("command", 3, MessageRole::User, "/compact");
    command.command = Some(frona::chat::message::models::MessageCommand::Command {
        name: "compact".into(),
        args: String::new(),
    });
    let following = row("following", 4, MessageRole::Agent, "");
    for msg in [&old, &active, &empty, &command, &following] {
        messages.create(msg).await.unwrap();
    }
    let out = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(out.compacted);
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![active, empty, command, following]
    );
    assert!(!summary.inputs.lock().unwrap()[0].contains("keep"));
}

#[tokio::test]
async fn repeated_compaction_incorporates_previous_summary_and_advances_only_past_old_requests() {
    let (_, compactor, messages, summary) = fixture().await;
    for msg in [
        row("old", 0, MessageRole::User, &"old ".repeat(200)),
        row("first", 1, MessageRole::User, "first active"),
    ] {
        messages.create(&msg).await.unwrap();
    }
    let first = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(first.compacted);
    messages
        .create(&row(
            "older-now",
            2,
            MessageRole::Agent,
            &"history ".repeat(100),
        ))
        .await
        .unwrap();
    let newest = row("newest", 3, MessageRole::Contact, "new active");
    messages.create(&newest).await.unwrap();
    let second = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(second.compacted);
    assert_eq!(second.model_history.row_ids, vec![newest.id.clone()]);
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![newest]
    );
    {
        let inputs = summary.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(inputs[1].contains("<conversation_summary>\\n[SUMMARY]\\n</conversation_summary>"));
        assert!(inputs[1].contains("first active"));
        assert!(!inputs[1].contains("old old"));
        assert!(!inputs[1].contains("new active"));
    }
    assert_eq!(messages.find_by_chat_id("chat").await.unwrap().len(), 4);
}

#[tokio::test]
async fn exact_post_summary_budget_is_inclusive_and_overflow_does_not_publish() {
    use frona_harness::inference::context::{estimate_message_tokens, estimate_tokens};
    for extra in [0, 1] {
        let (db, compactor, messages, summary) = fixture().await;
        let request = row("request", 1, MessageRole::User, &"active ".repeat(45));
        let budget = estimate_tokens("")
            + estimate_message_tokens(&RigMessage::user(
                "<conversation_summary>\n[SUMMARY]\n</conversation_summary>",
            ))
            + estimate_message_tokens(&RigMessage::user(&request.content));
        messages
            .create(&row("old", 0, MessageRole::User, &"old ".repeat(200)))
            .await
            .unwrap();
        messages.create(&request).await.unwrap();
        let out = compactor
            .compact("u", "chat", "agent", "", budget - extra, 0)
            .await;
        let persisted = SurrealChatSummaryRepo::new(db)
            .find_by_chat_id("chat")
            .await
            .unwrap();
        if extra == 0 {
            assert!(out.unwrap().compacted);
            assert!(persisted.is_some());
            assert_eq!(
                compactor.load_conversation("chat").await.unwrap().messages,
                vec![request]
            );
        } else {
            assert!(
                matches!(out, Err(AppError::Validation(message)) if message.contains("context limit"))
            );
            assert!(persisted.is_none());
            assert_eq!(
                messages.find_by_chat_id("chat").await.unwrap().last(),
                Some(&request)
            );
        }
        assert_eq!(summary.inputs.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn oversized_protected_request_errors_before_summarization_and_remains_recoverable() {
    let (db, compactor, messages, summary) = fixture().await;
    let mut request = row("request", 1, MessageRole::User, &"huge ".repeat(1000));
    request.attachments.push(Attachment {
        filename: "image.png".into(),
        content_type: "image/png".into(),
        size_bytes: 12,
        owner: "user:u".into(),
        path: "image.png".into(),
        url: None,
    });
    let placeholder = row("placeholder", 2, MessageRole::Agent, "");
    for msg in [&request, &placeholder] {
        messages.create(msg).await.unwrap();
    }
    let result = compactor.compact("u", "chat", "agent", "", 120, 0).await;
    assert!(
        matches!(result, Err(AppError::Validation(message)) if message.contains("context limit"))
    );
    assert!(summary.inputs.lock().unwrap().is_empty());
    assert!(
        SurrealChatSummaryRepo::new(db)
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![request, placeholder]
    );
}

#[tokio::test]
async fn manual_and_automatic_failures_never_report_success_or_lose_requests() {
    for manual in [false, true] {
        for persistence_failure in [false, true] {
            let app = App::with_window(if manual { 128_000 } else { 18_000 }).await;
            let retained = app.history(MessageRole::User, false).await;
            let mut session = if manual {
                Some(app.session().await.unwrap())
            } else {
                None
            };
            if let Some(session) = session.as_mut() {
                session.model_group.context_window = 18_000;
            }
            let all_before = app.messages().find_by_chat_id("chat").await.unwrap();
            if persistence_failure {
                app.db
                    .query("DEFINE FIELD content ON chat_summary TYPE string ASSERT false")
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            } else {
                app.model.enqueue(MockResponse::Error(
                    frona_harness::inference::error::InferenceError::InferenceFailed(
                        "injected summarizer failure".into(),
                    ),
                ));
            }
            let error = if manual {
                app.manual(session.as_mut().unwrap())
                    .await
                    .err()
                    .expect("manual failure must not return success acknowledgement")
            } else {
                app.session()
                    .await
                    .err()
                    .expect("automatic failure must not prepare an inference turn")
            };
            if persistence_failure {
                assert!(
                    matches!(&error, AppError::Inference(error) if matches!(error.as_ref(), frona_harness::inference::InferenceError::Stop(frona_harness::ExecutionStop::Storage(_)))),
                    "{error}"
                );
            } else {
                assert!(
                    error.to_string().contains("injected summarizer failure"),
                    "{error}"
                );
            }
            assert!(
                app.summaries()
                    .find_by_chat_id("chat")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                app.messages().find_by_chat_id("chat").await.unwrap(),
                all_before
            );
            assert!(all_before.ends_with(&retained));
            assert_eq!(app.inference_calls(), 0);
        }
    }
}

#[tokio::test]
async fn text_send_persists_attachments_and_sends_current_request_exactly_once() {
    let app = App::new().await;
    let retained = app.history(MessageRole::User, false).await;
    // Both paths now use the same standing prompt. Cross the trigger while
    // still fitting the conversation model's own window.
    let mut old = app.messages().find_by_id("old-3").await.unwrap().unwrap();
    old.content = "OLD-3 ".repeat(6600);
    app.messages().update(&old).await.unwrap();
    let attachment = retained[0].attachments[0].clone();
    let content = "new request after compaction";
    app.chat_service
        .send_message(
            &app.user.id,
            "chat",
            frona::chat::message::models::SendMessageRequest {
                content: content.into(),
                attachments: vec![attachment.clone()],
                metadata: None,
            },
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        app.model.system_prompts().last(),
        Some(&app.summary_system_prompt())
    );
    assert!(app.model.tool_histories().last().unwrap().is_empty());
    assert!(app.summary_tools().is_empty());
    assert!(
        serde_json::to_string(&app.summary_history())
            .unwrap()
            .contains("preceding conversation")
    );
    app.assert_summary_usage("conversation", "conversation-model")
        .await;
    assert!(
        app.model
            .model_names()
            .iter()
            .all(|name| name == "conversation-model")
    );
    let all = app.messages().find_by_chat_id("chat").await.unwrap();
    let request = all.iter().find(|m| m.content == content).unwrap();
    assert_eq!(request.attachments, vec![attachment]);
    let history = app.model.last_history();
    assert_eq!(history.iter().filter(|m| matches!(m, RigMessage::User { content: items } if items.iter().any(|c| matches!(c, UserContent::Text(t) if t.text == request.inference_content())))).count(), 1);
    assert_eq!(
        history
            .iter()
            .flat_map(|m| match m {
                RigMessage::User { content } => content.iter().collect::<Vec<_>>(),
                _ => vec![],
            })
            .filter(|c| matches!(c, UserContent::Image(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn session_rejects_initial_history_that_inference_would_truncate() {
    use frona_harness::inference::context::estimate_tokens;
    let app = App::new().await;
    let session = app.session().await.unwrap();
    let budget =
        (session.model_group.context_window - 4096 - estimate_tokens(&session.system_prompt))
            * session.model_group.inference.history_truncation_pct
            / 100;
    let request = row(
        "active",
        1,
        MessageRole::User,
        &"x".repeat((budget + 1 - 4) * 4),
    );
    let placeholder = row("placeholder", 2, MessageRole::Agent, "");
    for msg in [&request, &placeholder] {
        app.messages().create(msg).await.unwrap();
    }
    let error = app
        .session()
        .await
        .err()
        .expect("must reject before truncation");
    assert!(
        matches!(error, AppError::Validation(text) if text.contains("configured history limit"))
    );
    assert_eq!(
        app.messages().find_by_chat_id("chat").await.unwrap(),
        vec![request, placeholder]
    );
    assert_eq!(app.inference_calls(), 0);
    assert_eq!(app.summary_calls(), 0);
}

#[tokio::test]
async fn persistence_update_failure_preserves_previous_summary_and_can_be_retried() {
    let (db, compactor, messages, summary) = fixture().await;
    messages
        .create(&row("old", 0, MessageRole::User, &"old ".repeat(100)))
        .await
        .unwrap();
    messages
        .create(&row("first", 1, MessageRole::User, "first request"))
        .await
        .unwrap();
    compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    let summaries = SurrealChatSummaryRepo::new(db.clone());
    let before = summaries.find_by_chat_id("chat").await.unwrap().unwrap();
    messages
        .create(&row(
            "old-again",
            2,
            MessageRole::User,
            &"old again ".repeat(100),
        ))
        .await
        .unwrap();
    let request = row("latest", 3, MessageRole::User, "latest request");
    messages.create(&request).await.unwrap();
    let all_before = messages.find_by_chat_id("chat").await.unwrap();
    db.query("DEFINE FIELD content ON chat_summary TYPE string ASSERT false")
        .await
        .unwrap()
        .check()
        .unwrap();
    let result = compactor.compact("u", "chat", "agent", "", 120, 0).await;
    assert!(
        matches!(result, Err(AppError::Inference(error)) if matches!(error.as_ref(), frona_harness::inference::InferenceError::Stop(frona_harness::ExecutionStop::Storage(_))))
    );
    let after = summaries.find_by_chat_id("chat").await.unwrap().unwrap();
    assert_eq!(after.content, before.content);
    assert_eq!(after.compacted_until, before.compacted_until);
    assert_eq!(messages.find_by_chat_id("chat").await.unwrap(), all_before);
    assert_eq!(
        compactor
            .load_conversation("chat")
            .await
            .unwrap()
            .messages
            .last(),
        Some(&request)
    );
    db.query("REMOVE FIELD content ON chat_summary")
        .await
        .unwrap()
        .check()
        .unwrap();
    let result = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(result.compacted);
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![request]
    );
    assert_eq!(summary.inputs.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn compaction_trigger_boundary_is_exclusive() {
    for extra in [0, 4, 8] {
        let (_, compactor, messages, summary) = fixture().await;
        // System=4, old=65(+0,1,2), active=10: below, at, and above the 80-token trigger.
        let old = row("old", 0, MessageRole::User, &"x".repeat(244 + extra));
        let request = row("request", 1, MessageRole::User, &"a".repeat(24));
        for msg in [&old, &request] {
            messages.create(msg).await.unwrap();
        }
        let out = compactor
            .compact("u", "chat", "agent", "", 100, 0)
            .await
            .unwrap();
        assert_eq!(out.compacted, extra == 8);
        assert_eq!(
            summary.inputs.lock().unwrap().len(),
            usize::from(extra == 8)
        );
        assert_eq!(
            compactor
                .load_conversation("chat")
                .await
                .unwrap()
                .messages
                .last(),
            Some(&request)
        );
    }
}

impl App {
    fn builder(&self) -> Box<dyn frona::inference::conversation::ConversationBuilder> {
        let state = &self.fixture.state;
        Box::new(DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
        })
    }

    async fn command_rows(&self, name: &str) {
        let mut command = row("command", 20, MessageRole::User, &format!("/{name}"));
        command.command = Some(frona::chat::message::models::MessageCommand::Command {
            name: name.into(),
            args: String::new(),
        });
        self.messages().create(&command).await.unwrap();
        let mut response = row("command-response", 21, MessageRole::Agent, "");
        response.agent_id = Some(self.chat.agent_id.clone());
        response.status = Some(MessageStatus::Executing);
        self.messages().create(&response).await.unwrap();
    }
}

#[tokio::test]
async fn agent_model_fallback_covers_api_tasks_and_heartbeats() {
    use frona::inference::conversation::{HeartbeatConversationBuilder, TaskConversationBuilder};

    for path in ["stream", "text", "task", "heartbeat"] {
        for (requested, has_primary, expected) in [
            ("reasoning", true, Some("primary")),
            ("conversation", true, Some("conversation")),
            ("reasoning", false, None),
        ] {
            let app = App::with_route(128_000, has_primary).await;
            let repo = SurrealRepo::<frona::agent::models::Agent>::new(app.db.clone());
            let mut agent = repo.find_by_id(&app.chat.agent_id).await.unwrap().unwrap();
            agent.model_group = requested.into();
            if path == "heartbeat" {
                agent.heartbeat_chat_id = Some(app.chat.id.clone());
            }
            repo.update(&agent).await.unwrap();

            let result = if path == "text" {
                app.chat_service
                    .send_message(
                        &app.user.id,
                        &app.chat.id,
                        frona::chat::message::models::SendMessageRequest {
                            content: "Hello".into(),
                            attachments: vec![],
                            metadata: None,
                        },
                        Default::default(),
                        Default::default(),
                    )
                    .await
                    .map(|_| ())
            } else {
                app.next_request().await;
                let state = &app.fixture.state;
                let builder: Box<dyn ConversationBuilder> = match path {
                    "task" => Box::new(TaskConversationBuilder {
                        user_service: state.user_service.clone(),
                        storage_service: state.storage_service.clone(),
                        agent_service: state.agent_service.clone(),
                        continuation_prompt: Some("Complete the task".into()),
                    }),
                    "heartbeat" => Box::new(HeartbeatConversationBuilder {
                        user_service: state.user_service.clone(),
                        storage_service: state.storage_service.clone(),
                        agent_service: state.agent_service.clone(),
                        continuation_prompt: None,
                        heartbeat_content: "Check for updates".into(),
                    }),
                    _ => app.builder(),
                };
                app.harness
                    .run_loop(
                        &app.user.id,
                        &app.chat.id,
                        "new-response",
                        CancellationToken::new(),
                        builder,
                        &[],
                        None,
                        Default::default(),
                    )
                    .await
                    .map(|_| ())
            };

            match expected {
                Some(group) => {
                    result.unwrap_or_else(|error| panic!("{path}/{requested}: {error}"));
                    assert_eq!(app.model.calls(), usize::from(group == "conversation"));
                    assert_eq!(app.other_models[0].calls(), usize::from(group == "primary"));
                    assert_eq!(app.other_models[1].calls(), 0);
                }
                None => {
                    assert!(
                        matches!(&result, Err(AppError::Inference(error)) if matches!(
                            error.as_ref(),
                            frona_harness::inference::InferenceError::ModelGroupNotFound(name)
                                if name == requested
                        )),
                        "{path}/{requested}: {result:?}"
                    );
                    assert_eq!(app.model.calls(), 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn harness_dispatches_clear_after_normal_session_build() {
    let app = App::new().await;
    let request = row("active", 1, MessageRole::User, "ordinary request");
    app.messages().create(&request).await.unwrap();
    app.command_rows("clear").await;
    let out = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[],
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        out.inference,
        frona::inference::InferenceResponse::Handled
    ));
    assert!(app.messages().find_by_id("active").await.unwrap().is_none());
    assert_eq!(app.summary_calls(), 0);
    assert_eq!(app.inference_calls(), 0);
}

#[tokio::test]
async fn harness_compacts_before_dispatching_compact_command() {
    let app = App::new().await;
    let retained = app.history(MessageRole::User, false).await;
    app.command_rows("compact").await;
    let outcome = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[],
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.response.content,
        "Chat is already at optimal size — no compaction needed."
    );
    assert_eq!(app.summary_calls(), 1);
    assert_eq!(app.inference_calls(), 0);
    let loaded = app
        .chat_service
        .compactor()
        .load_conversation("chat")
        .await
        .unwrap();
    assert!(loaded.summary.is_some());
    assert!(loaded.messages.starts_with(&retained));
    assert_eq!(loaded.messages[retained.len()].id, "command");
    assert_eq!(loaded.messages[retained.len() + 1].id, "command-response");
}

#[tokio::test]
async fn harness_validates_contact_context_after_channel_prompt_is_appended() {
    use frona::inference::conversation::ChannelConversationBuilder;
    for oversized_prompt in [false, true] {
        let app = App::new().await;
        let request = row("active", 1, MessageRole::Contact, "exact contact request");
        let placeholder = row("placeholder", 2, MessageRole::Agent, "");
        for msg in [&request, &placeholder] {
            app.messages().create(msg).await.unwrap();
        }
        let state = &app.fixture.state;
        let builder = Box::new(ChannelConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
            channel: "email".into(),
            sender: Some("sender@example.test".into()),
            inbound_prompt: Some(if oversized_prompt {
                "inbound instructions ".repeat(10000)
            } else {
                "Reply to this email".into()
            }),
        });
        let result = app
            .harness
            .run_loop(
                &app.user.id,
                "chat",
                "placeholder",
                CancellationToken::new(),
                builder,
                &[frona::tool::registry::ToolFilter::AllowList(&[])],
                None,
                Default::default(),
            )
            .await;
        if oversized_prompt {
            assert!(
                matches!(result, Err(AppError::Validation(text)) if text.contains("context limit"))
            );
            assert_eq!(app.inference_calls(), 0);
        } else {
            assert!(result.is_ok());
            assert_eq!(app.inference_calls(), 1);
            assert!(app.model.last_history().iter().any(|m| matches!(m, RigMessage::User { content } if content.iter().any(|c| matches!(c, UserContent::Text(t) if t.text == request.content)))));
        }
        assert_eq!(
            app.messages().find_by_id("active").await.unwrap(),
            Some(request)
        );
    }
}

struct HugePromptCommand;
#[async_trait]
impl Command for HugePromptCommand {
    fn name(&self) -> &str {
        "large"
    }
    fn description(&self) -> &str {
        "Test a command's rendered prompt budget"
    }
    async fn run(&self, _: &str, _: &mut CommandContext<'_>) -> Result<CommandOutcome, AppError> {
        Ok(CommandOutcome::Prompt("rendered command ".repeat(10000)))
    }
}

#[tokio::test]
async fn harness_validates_rendered_command_before_inference() {
    let app = App::new().await;
    app.command_rows("large").await;
    let mut registry = frona::chat::command::CommandRegistry::new();
    registry.register(Arc::new(HugePromptCommand));
    let outcome = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[],
            Some(Arc::new(registry)),
            Default::default(),
        )
        .await;
    assert!(
        matches!(outcome, Err(AppError::Validation(text)) if text.contains("configured history limit"))
    );
    assert_eq!(app.inference_calls(), 0);
    assert_eq!(
        app.messages()
            .find_by_id("command")
            .await
            .unwrap()
            .unwrap()
            .content,
        "/large"
    );
}

struct ContextPromptCommand;
#[async_trait]
impl Command for ContextPromptCommand {
    // Override the built-in to verify context-specific registration takes precedence.
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Return a prompt with custom session context"
    }
    async fn run(&self, _: &str, ctx: &mut CommandContext<'_>) -> Result<CommandOutcome, AppError> {
        ctx.session
            .rig_history
            .insert(0, RigMessage::user("custom session context"));
        Ok(CommandOutcome::Prompt("rendered command request".into()))
    }
}

#[tokio::test]
async fn context_prompt_command_keeps_automatic_compaction_and_session_mutations() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    app.command_rows("compact").await;
    let mut registry = frona::chat::command::CommandRegistry::new();
    registry.register(Arc::new(ContextPromptCommand));
    let out = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[frona::tool::registry::ToolFilter::AllowList(&[])],
            Some(Arc::new(registry)),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        out.inference,
        frona::inference::InferenceResponse::Completed { .. }
    ));
    assert_eq!(app.summary_calls(), 1);
    let history = app.model.last_history();
    let saved = app.messages().find_by_id("command").await.unwrap().unwrap();
    assert!(
        saved
            .inference_content()
            .ends_with("rendered command request")
    );
    for text in ["custom session context", saved.inference_content()] {
        assert_eq!(history.iter().filter(|m| matches!(m, RigMessage::User { content } if content.iter().any(|c| matches!(c, UserContent::Text(t) if t.text == text)))).count(), 1);
    }
    let resumed = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[frona::tool::registry::ToolFilter::AllowList(&[])],
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        resumed.inference,
        frona::inference::InferenceResponse::Completed { .. }
    ));
    assert_eq!(
        app.messages()
            .find_by_id("command")
            .await
            .unwrap()
            .unwrap()
            .processed_content,
        saved.processed_content
    );
}

#[tokio::test]
async fn switch_agent_command_compacts_and_keeps_the_target_agent_context() {
    let app = App::with_window(128_000).await;
    app.fixture.state.policy_service.create_policy(&app.user.id,
        r#"@id("switch-tools") permit(principal, action == Policy::Action::"invoke_tool", resource) when { resource == Policy::Tool::"read" || resource == Policy::Tool::"write" };"#,
    ).await.unwrap();
    assert_eq!(
        app.session()
            .await
            .unwrap()
            .tool_registry
            .definitions()
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["mcpctl", "read", "write"].into_iter().collect()
    );
    app.history(MessageRole::User, false).await;
    let mut agent = helpers::mock_context().agent;
    agent.id = "other-agent".into();
    agent.handle = frona::handle!("other-agent");
    agent.prompt = Some("You are the selected agent.".into());
    agent.model_group = "target".into();
    SurrealRepo::new(app.db.clone())
        .create(&agent)
        .await
        .unwrap();
    app.command_rows("other-agent").await;
    let mut command = app.messages().find_by_id("command").await.unwrap().unwrap();
    command.content = "/other-agent selected request".into();
    command.command = Some(frona::chat::message::models::MessageCommand::Command {
        name: "other-agent".into(),
        args: "selected request".into(),
    });
    app.messages().update(&command).await.unwrap();
    let out = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            Box::new(frona::inference::conversation::ChannelConversationBuilder {
                user_service: app.fixture.state.user_service.clone(),
                storage_service: app.fixture.state.storage_service.clone(),
                agent_service: app.fixture.state.agent_service.clone(),
                channel: "email".into(),
                sender: None,
                inbound_prompt: Some("SWITCH CHANNEL PROMPT".into()),
            }),
            &[frona::tool::registry::ToolFilter::AllowList(&[])],
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        out.inference,
        frona::inference::InferenceResponse::Completed { .. }
    ));
    assert_eq!(
        app.model
            .system_prompts()
            .last()
            .unwrap()
            .matches("SWITCH CHANNEL PROMPT")
            .count(),
        1
    );
    assert!(
        app.model
            .system_prompts()
            .last()
            .unwrap()
            .contains("You are the selected agent.")
    );
    assert!(app.model.tool_histories().last().unwrap().is_empty());
    assert_eq!(
        app.summary_system_prompt()
            .matches("SWITCH CHANNEL PROMPT")
            .count(),
        1
    );
    assert!(app.summary_tools().is_empty());
    assert_eq!(out.response.agent_id.as_deref(), Some("other-agent"));
    assert_eq!(app.summary_calls(), 1);
    assert_eq!(
        app.model.model_names(),
        vec!["target-model", "target-model"]
    );
    assert_eq!(
        app.model.request_settings(),
        vec![(Some(4096), Some(0.21)); 2]
    );
    app.assert_summary_usage("target", "target-model").await;
    let saved = app.messages().find_by_id("command").await.unwrap().unwrap();
    assert!(saved.inference_content().ends_with("selected request"));
    assert!(app.model.last_history().iter().any(|m| matches!(m, RigMessage::User { content } if content.iter().any(|c| matches!(c, UserContent::Text(t) if t.text == saved.inference_content())))));
    assert_eq!(
        app.chat_service
            .get_chat(&app.user.id, "chat")
            .await
            .unwrap()
            .agent_id,
        app.chat.agent_id
    );
}

#[tokio::test]
async fn switch_agent_preserves_task_and_heartbeat_builders() {
    use frona::inference::conversation::{HeartbeatConversationBuilder, TaskConversationBuilder};

    for heartbeat in [false, true] {
        let app = App::with_window(128_000).await;
        let mut agent = helpers::mock_context().agent;
        agent.id = "other-agent".into();
        agent.handle = frona::handle!("other-agent");
        agent.prompt = Some("Selected continuation agent".into());
        agent.model_group = "target".into();
        SurrealRepo::new(app.db.clone())
            .create(&agent)
            .await
            .unwrap();
        app.messages()
            .create(&row(
                "task-instruction",
                -1,
                MessageRole::Agent,
                "Original task intent",
            ))
            .await
            .unwrap();
        let mut prior_response = row(
            "target-response",
            0,
            MessageRole::Agent,
            "Target prior response",
        );
        prior_response.agent_id = Some(agent.id.clone());
        prior_response.status = Some(MessageStatus::Completed);
        app.messages().create(&prior_response).await.unwrap();
        app.command_rows("other-agent").await;
        let mut command = app.messages().find_by_id("command").await.unwrap().unwrap();
        command.command = Some(frona::chat::message::models::MessageCommand::Command {
            name: "other-agent".into(),
            args: "Selected command request".into(),
        });
        app.messages().update(&command).await.unwrap();
        // Deliberately identical to the stored command: replacement must use its
        // mapped row rather than the last matching user message.
        let continuation_prompt = Some("/other-agent".into());
        let builder: Box<dyn ConversationBuilder> = if heartbeat {
            Box::new(HeartbeatConversationBuilder {
                user_service: app.fixture.state.user_service.clone(),
                storage_service: app.fixture.state.storage_service.clone(),
                agent_service: app.fixture.state.agent_service.clone(),
                continuation_prompt,
                heartbeat_content: "Original heartbeat checklist".into(),
            })
        } else {
            Box::new(TaskConversationBuilder {
                user_service: app.fixture.state.user_service.clone(),
                storage_service: app.fixture.state.storage_service.clone(),
                agent_service: app.fixture.state.agent_service.clone(),
                continuation_prompt,
            })
        };
        let outcome = app
            .harness
            .run_loop(
                &app.user.id,
                "chat",
                "command-response",
                CancellationToken::new(),
                builder,
                &[frona::tool::registry::ToolFilter::AllowList(&[])],
                None,
                Default::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome.inference,
            frona::inference::InferenceResponse::Completed { .. }
        ));
        assert_eq!(outcome.response.agent_id.as_deref(), Some("other-agent"));
        assert_eq!(app.model.model_names(), vec!["target-model"]);
        assert!(app.model.system_prompts()[0].contains("Selected continuation agent"));
        assert!(app.model.tool_histories()[0].is_empty());
        let history = app.model.last_history();
        assert!(history.iter().any(|m| matches!(m, RigMessage::Assistant(rig_core::message::AssistantMessage { content, .. }) if content.iter().any(|c| matches!(c, AssistantContent::Text(t) if t.text == "Target prior response")))));
        let user_texts: Vec<_> = history
            .iter()
            .flat_map(|message| match message {
                RigMessage::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        let saved = app.messages().find_by_id("command").await.unwrap().unwrap();
        assert!(
            saved
                .inference_content()
                .ends_with("Selected command request")
        );
        assert_eq!(
            user_texts
                .iter()
                .filter(|text| **text == saved.inference_content())
                .count(),
            1
        );
        if heartbeat {
            assert!(
                user_texts
                    .last()
                    .unwrap()
                    .contains("<heartbeat_instructions>\n/other-agent")
            );
            assert!(
                user_texts
                    .last()
                    .unwrap()
                    .contains("Original heartbeat checklist")
            );
        } else {
            assert!(user_texts.contains(&"<task>\nOriginal task intent\n</task>"));
            assert_eq!(*user_texts.last().unwrap(), "/other-agent");
        }
        assert_eq!(
            app.messages()
                .find_by_id("command")
                .await
                .unwrap()
                .unwrap()
                .content,
            "/other-agent"
        );
    }
}

/// Compare requests captured at the provider boundary, not a text transcript or
/// a second implementation of conversation conversion.
#[tokio::test]
async fn compaction_reuses_conversation_model_and_ignores_legacy_groups() {
    for legacy_groups in [false, true] {
        for channel in [false, true] {
            let app = App::with_route(128_000, legacy_groups).await;
            app.fixture.state.policy_service.create_policy(&app.user.id,
                r#"@id("capture-read-file") permit(principal, action == Policy::Action::"invoke_tool", resource) when { resource == Policy::Tool::"read" || resource == Policy::Tool::"write" };"#,
            ).await.unwrap();
            let mut retained = app.history(MessageRole::User, false).await;
            let mut old_attachment = row(
                "historical-image",
                1,
                MessageRole::User,
                "Historical image and file",
            );
            old_attachment.attachments = retained[0].attachments.clone();
            app.messages().create(&old_attachment).await.unwrap();
            let mut old_assistant = row(
                "historical-tools",
                1,
                MessageRole::Agent,
                "Historical final answer",
            );
            old_assistant.agent_id = Some(app.chat.agent_id.clone());
            old_assistant.status = Some(MessageStatus::Completed);
            old_assistant.reasoning = Some(frona::chat::message::models::Reasoning {
                id: None,
                raw: None,
                content: "historical reasoning".into(),
                signature: Some("opaque signature".into()),
            });
            app.messages().create(&old_assistant).await.unwrap();
            let mut call = tool(&old_assistant, "EXACT HISTORICAL TOOL RESULT", 0);
            call.turn_reasoning = old_assistant.reasoning.clone();
            SurrealRepo::new(app.db.clone())
                .create(&call)
                .await
                .unwrap();
            let make_builder = || -> Box<dyn ConversationBuilder> {
                if channel {
                    Box::new(frona::inference::conversation::ChannelConversationBuilder {
                        user_service: app.fixture.state.user_service.clone(),
                        storage_service: app.fixture.state.storage_service.clone(),
                        agent_service: app.fixture.state.agent_service.clone(),
                        channel: "email".into(),
                        sender: Some("sender@example.test".into()),
                        inbound_prompt: Some("EXACT CHANNEL INSTRUCTIONS".into()),
                    })
                } else {
                    app.builder()
                }
            };
            app.harness
                .run_loop(
                    &app.user.id,
                    "chat",
                    "placeholder",
                    CancellationToken::new(),
                    make_builder(),
                    &[],
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            // The completed model call records its exact provider turn before
            // compaction. Every other retained field and the row order must stay
            // unchanged, including this snapshot when the tail is reloaded.
            retained
                .iter_mut()
                .find(|message| message.id == "placeholder")
                .unwrap()
                .provider_turns
                .insert(
                    "0".into(),
                    serde_json::to_value(
                        rig_core::message::AssistantMessage::new(vec![AssistantContent::text(
                            "default response",
                        )])
                        .with_origin(Some(rig_core::message::Origin::new(
                            rig_core::test_utils::MOCK_API,
                            "fixture",
                            "conversation-model",
                        )))
                        .with_stop(Some(rig_core::message::StopReason::Stop)),
                    )
                    .unwrap(),
                );
            let normal_history = app.model.last_history();
            let normal_system = app.model.system_prompts().last().unwrap().clone();
            let normal_tools = app.model.tool_histories().last().unwrap().clone();
            let normal_settings = *app.model.request_settings().last().unwrap();
            assert_eq!(
                normal_tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                ["mcpctl", "read", "write"].into_iter().collect()
            );
            let builder = make_builder();
            let mut session = ChatSessionContext::build(
                &app.harness,
                &app.user.id,
                app.chat.clone(),
                None,
                CancellationToken::new(),
                Arc::from(builder),
                &[],
                Default::default(),
            )
            .await
            .unwrap();
            let canonical = serde_json::to_string(&session.rig_history).unwrap();
            assert!(canonical.contains("opaque signature"));
            session.model_group.context_window = 18_000;
            app.manual(&mut session).await.unwrap();
            assert_eq!(
                app.messages()
                    .find_by_id(&old_assistant.id)
                    .await
                    .unwrap()
                    .unwrap(),
                old_assistant,
                "compaction must not rewrite the signed source turn",
            );
            let provider = &app.model;
            let compact_history = provider.last_history();
            let prefix = &compact_history[..compact_history.len() - 1];
            assert_eq!(prefix, &normal_history[..prefix.len()]);
            assert_eq!(provider.system_prompts().last().unwrap(), &normal_system);
            assert_eq!(provider.tool_histories().last().unwrap(), &normal_tools);
            assert_eq!(provider.model_names().last().unwrap(), "conversation-model");
            assert_eq!(normal_settings, (Some(4096), Some(0.37)));
            assert_eq!(
                *provider.request_settings().last().unwrap(),
                normal_settings
            );
            app.assert_summary_usage("conversation", "conversation-model")
                .await;
            let native = serde_json::to_string(prefix).unwrap();
            for exact in [
                "call-historical-tools-0",
                "EXACT HISTORICAL TOOL RESULT",
                "historical reasoning",
                "report.txt",
                "ZXhhY3QtaW1hZ2UtY29udGVudA==",
            ] {
                assert!(native.contains(exact), "missing {exact}: {native}");
            }
            // The fixture uses a different dialect than the historical
            // Anthropic turn: Rig projects its reasoning text, not its signature.
            assert!(!native.contains("opaque signature"));
            assert!(!native.contains("EXACT ACTIVE REQUEST"));
            let instruction = serde_json::to_string(compact_history.last().unwrap()).unwrap();
            assert!(instruction.contains("preceding conversation"));
            assert!(instruction.contains("Do not call tools"));
            assert_eq!(app.summary_calls(), 1);
            assert!(
                app.other_models
                    .iter()
                    .all(|provider| provider.calls() == 0)
            );
            let loaded = app
                .chat_service
                .compactor()
                .load_conversation("chat")
                .await
                .unwrap();
            assert_eq!(loaded.messages, retained);
            let builder = make_builder();
            let reload = ChatSessionContext::build(
                &app.harness,
                &app.user.id,
                app.chat.clone(),
                None,
                CancellationToken::new(),
                Arc::from(builder),
                &[],
                Default::default(),
            )
            .await
            .unwrap();
            assert_eq!(reload.rig_history, session.rig_history);
            // A later compaction carries the persisted summary verbatim as the
            // beginning of the actual cached prefix, and still excludes new intent.
            for message in [
                row(
                    "later-answer",
                    14,
                    MessageRole::Agent,
                    &"later answer ".repeat(2600),
                ),
                row("later-request", 15, MessageRole::User, "LATEST NEW INTENT"),
                row("later-placeholder", 16, MessageRole::Agent, ""),
            ] {
                app.messages().create(&message).await.unwrap();
            }
            app.harness
                .run_loop(
                    &app.user.id,
                    "chat",
                    "later-placeholder",
                    CancellationToken::new(),
                    make_builder(),
                    &[],
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            let normal = app.model.last_history();
            let builder = make_builder();
            let mut later = ChatSessionContext::build(
                &app.harness,
                &app.user.id,
                app.chat.clone(),
                None,
                CancellationToken::new(),
                Arc::from(builder),
                &[],
                Default::default(),
            )
            .await
            .unwrap();
            let previous_summary =
                frona_harness::inference::compaction::summary_message("default response", None);
            assert_eq!(later.rig_history[0], previous_summary);
            later.model_group.context_window = 18_000;
            app.manual(&mut later).await.unwrap();
            let compact = provider.last_history();
            assert_eq!(&compact[..compact.len() - 1], &normal[..compact.len() - 1]);
            let RigMessage::User {
                content: summary_content,
            } = previous_summary
            else {
                panic!("summary must be a user message");
            };
            assert!(matches!(&compact[0], RigMessage::User { content }
                if content.starts_with(&summary_content)));
            assert!(
                !serde_json::to_string(&compact)
                    .unwrap()
                    .contains("LATEST NEW INTENT")
            );
            let builder = make_builder();
            let reload = ChatSessionContext::build(
                &app.harness,
                &app.user.id,
                app.chat.clone(),
                None,
                CancellationToken::new(),
                Arc::from(builder),
                &[],
                Default::default(),
            )
            .await
            .unwrap();
            assert_eq!(reload.rig_history, later.rig_history);
        }
    }
}

#[tokio::test]
async fn task_summary_reload_preserves_retained_tool_pairs_and_continuation() {
    use frona::inference::conversation::TaskConversationBuilder;
    let app = App::with_window(128_000).await;
    let retained = app.history(MessageRole::User, false).await;
    let initial = row(
        "task-instruction",
        -1,
        MessageRole::Agent,
        "Original task instruction",
    );
    app.messages().create(&initial).await.unwrap();
    let make_builder = || -> Box<dyn ConversationBuilder> {
        Box::new(TaskConversationBuilder {
            user_service: app.fixture.state.user_service.clone(),
            storage_service: app.fixture.state.storage_service.clone(),
            agent_service: app.fixture.state.agent_service.clone(),
            continuation_prompt: Some("Continue task exactly".into()),
        })
    };
    let builder = make_builder();
    let mut session = ChatSessionContext::build(
        &app.harness,
        &app.user.id,
        app.chat.clone(),
        None,
        CancellationToken::new(),
        Arc::from(builder),
        &[],
        Default::default(),
    )
    .await
    .unwrap();
    session.model_group.context_window = 18_000;
    app.manual(&mut session).await.unwrap();
    app.assert_summary_usage("conversation", "conversation-model")
        .await;
    let builder = make_builder();
    let reload = ChatSessionContext::build(
        &app.harness,
        &app.user.id,
        app.chat.clone(),
        None,
        CancellationToken::new(),
        Arc::from(builder),
        &[],
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(session.rig_history, reload.rig_history);
    let native = serde_json::to_string(&reload.rig_history).unwrap();
    assert!(native.contains("call-tool-message-0"));
    assert!(native.contains("retained tool result"));
    assert!(native.contains("Continue task exactly"));
    assert!(!native.contains("<task>"));
    assert_eq!(
        app.chat_service
            .compactor()
            .load_conversation("chat")
            .await
            .unwrap()
            .messages,
        retained
    );
}

#[tokio::test]
async fn empty_or_tool_summary_never_advances_cutoff() {
    let mixed = MockResponse::Content(vec![
        AssistantContent::text("Summary text with a forbidden tool request"),
        AssistantContent::ToolCall(rig_core::completion::message::ToolCall::new(
            rig_core::completion::message::CallId::from_wire("mixed-call"),
            rig_core::completion::message::ToolFunction::new(
                "read".try_into().unwrap(),
                serde_json::json!({"path":"must-not-read"}),
            ),
        )),
    ]);
    for response in [
        MockResponse::Text(" \n ".into()),
        MockResponse::ToolCalls(vec![(
            "summary-call".into(),
            "read_file".into(),
            serde_json::json!({"path":"must-not-read"}),
        )]),
        mixed,
    ] {
        let app = App::new().await;
        app.history(MessageRole::User, false).await;
        let before = app.messages().find_by_chat_id("chat").await.unwrap();
        app.model.enqueue(response);
        assert!(app.session().await.is_err());
        assert!(
            app.summaries()
                .find_by_chat_id("chat")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            before,
            app.messages().find_by_chat_id("chat").await.unwrap()
        );
        assert_eq!(app.summary_calls(), 1);
        assert_eq!(app.inference_calls(), 0);
    }
}

#[tokio::test]
async fn compaction_candidates_never_truncate_and_fallback_preserves_usage_scope() {
    use frona::chat::compactor::TextInferenceSummarizer;
    for fallback_fits in [false, true] {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let main = Arc::new(MockModelProvider::new(vec![]));
        let fallback = Arc::new(MockModelProvider::new(vec![]));
        let mut group = helpers::test_model_group_with_fallback("fallback", "fallback-model");
        group.name = "conversation".into();
        group.context_window = 1000;
        group.main.request_settings.max_tokens = Some(800);
        group.fallbacks[0].request_settings.max_tokens = Some(if fallback_fits { 10 } else { 800 });
        let service = helpers::test_model_service(
            [
                ("mock".into(), main.clone() as Arc<dyn ModelProvider>),
                (
                    "fallback".into(),
                    fallback.clone() as Arc<dyn ModelProvider>,
                ),
            ]
            .into(),
            [("conversation".into(), group)].into(),
        )
        .await;
        let history = vec![
            RigMessage::user("Keep exact history ".repeat(100)),
            RigMessage::user("Summarize preceding conversation"),
        ];
        let tools = vec![rig_core::completion::request::ToolDefinition {
            name: rig_core::message::ToolName::new("read_file").unwrap(),
            description: "Original tool description".into(),
            parameters: serde_json::json!({"type":"object"}),
            annotations: Default::default(),
        }];
        let group = service
            .resolve(&frona_harness::inference::ModelRef("conversation".into()))
            .unwrap();
        let result = TextInferenceSummarizer::new(helpers::test_usage_service(&db))
            .summarize(SummaryRequest {
                input_token_estimate: None,
                model_group: &group,
                user_id: "user",
                agent_id: "agent",
                chat_id: "chat",
                system_prompt: "Exact system",
                history: history.clone(),
                tools: tools.clone(),
            })
            .await;
        assert_eq!(
            main.calls(),
            0,
            "undersized main must never receive a truncated prefix"
        );
        if fallback_fits {
            assert_eq!(result.unwrap(), "default response");
            assert_eq!(
                fallback.last_history(),
                native_history(&history, "Exact system", "fallback-model", &tools),
            );
            assert_eq!(
                history,
                vec![
                    RigMessage::user("Keep exact history ".repeat(100)),
                    RigMessage::user("Summarize preceding conversation"),
                ]
            );
            assert_eq!(fallback.tool_histories(), vec![tools]);
            assert_eq!(fallback.system_prompts(), vec!["Exact system"]);
            let rows: Vec<serde_json::Value> = db
                .query("SELECT * FROM inference_usage")
                .await
                .unwrap()
                .take(0)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["model_group"], "conversation");
            let usage = serde_json::to_string(&rows).unwrap();
            assert!(usage.contains("Compaction"));
            assert!(usage.contains("fallback-model"));
            assert!(usage.contains("chat"));
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Complete input exceeds context limit")
            );
            assert_eq!(fallback.calls(), 0);
        }
    }
}

#[tokio::test]
async fn unreadable_tool_history_cannot_publish_a_partial_summary() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    app.db
        .query("UPDATE tool_call SET result = 42;")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(app.chat_service.get_tool_calls("chat").await.is_err());
    assert!(app.session().await.is_err());
    assert!(
        app.summaries()
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(app.summary_calls(), 0);
}

#[tokio::test]
async fn persisted_summary_under_threshold_is_loaded_without_resummarizing() {
    let (db, compactor, messages, summarizer) = fixture().await;
    let old = row("old", 0, MessageRole::User, &"old ".repeat(100));
    let request = row("request", 1, MessageRole::User, "active request");
    for message in [&old, &request] {
        messages.create(message).await.unwrap();
    }
    let first = compactor
        .compact("u", "chat", "agent", "", 120, 0)
        .await
        .unwrap();
    assert!(first.compacted);
    let summaries: SurrealChatSummaryRepo = SurrealRepo::new(db);
    let persisted = summaries.find_by_chat_id("chat").await.unwrap().unwrap();
    assert_eq!(persisted.content, "[SUMMARY]");
    assert_eq!(persisted.compacted_until, old.created_at);
    assert_eq!(persisted.item_count, 1);

    let second = compactor
        .compact("u", "chat", "agent", "", 100_000, 0)
        .await
        .unwrap();
    assert!(!second.compacted);
    assert_eq!(second.model_history.summary, first.model_history.summary);
    assert_eq!(
        compactor.load_conversation("chat").await.unwrap().messages,
        vec![request.clone()]
    );
    assert_eq!(second.model_history.history, first.model_history.history);
    assert_eq!(
        serde_json::to_value(summaries.find_by_chat_id("chat").await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(persisted).unwrap()
    );
    let reloaded = compactor.load_conversation("chat").await.unwrap();
    assert_eq!(reloaded.summary.as_deref(), Some("[SUMMARY]"));
    assert_eq!(reloaded.messages, vec![request.clone()]);
    assert_eq!(
        messages.find_by_chat_id("chat").await.unwrap(),
        vec![old, request]
    );
    assert_eq!(summarizer.inputs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn manual_compaction_uses_the_resolved_conversation_request_settings() {
    let app = App::with_window(128_000).await;
    app.history(MessageRole::User, false).await;
    let mut session = app.session().await.unwrap();
    session.model_group.context_window = 17_000;
    session.model_group.main.request_settings.max_tokens = Some(3072);
    session.model_group.main.request_settings.temperature = Some(0.19);
    app.manual(&mut session).await.unwrap();
    assert_eq!(app.summary_calls(), 1);
    assert_eq!(app.model.request_settings(), vec![(Some(3072), Some(0.19))]);
    app.assert_summary_usage("conversation", "conversation-model")
        .await;
}

#[tokio::test]
async fn oversized_conversation_prefix_cannot_use_a_larger_legacy_compaction_model() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    let mut old = app.messages().find_by_id("old-3").await.unwrap().unwrap();
    old.content = "oversized old history ".repeat(20_000);
    app.messages().update(&old).await.unwrap();
    let before = app.messages().find_by_chat_id("chat").await.unwrap();
    let error = app
        .session()
        .await
        .err()
        .expect("oversized prefix must fail");
    assert!(
        error
            .to_string()
            .contains("Complete input exceeds context limit")
    );
    assert_eq!(app.model.calls(), 0);
    assert!(
        app.other_models
            .iter()
            .all(|provider| provider.calls() == 0)
    );
    assert!(
        app.summaries()
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        app.messages().find_by_chat_id("chat").await.unwrap(),
        before
    );
}

impl App {
    async fn recorded_stats(&self, usage: frona_harness::inference::Usage) {
        let group = self
            .chat_service
            .model_providers()
            .resolve(&frona_harness::inference::ModelRef("conversation".into()))
            .unwrap();
        self.fixture
            .state
            .usage_service
            .record(
                &frona::inference::UsageContext::new(
                    frona::inference::InferenceKind::ToolTurn {
                        agent_id: self.chat.agent_id.clone(),
                        chat_id: "chat".into(),
                        message_id: "tool-message".into(),
                        turn_index: 1,
                    },
                    &self.user.id,
                    "conversation",
                ),
                &group.main,
                &usage,
                0,
                Default::default(),
            )
            .await;
    }

    async fn resize_old_rows(&self, characters: usize) {
        for id in ["old-2", "old-3"] {
            let mut old = self.messages().find_by_id(id).await.unwrap().unwrap();
            old.content = format!("{id} {}", "x".repeat(characters));
            self.messages().update(&old).await.unwrap();
        }
    }

    async fn next_request(&self) {
        self.messages()
            .create(&row(
                "new-request",
                20,
                MessageRole::User,
                "NEW EXACT REQUEST",
            ))
            .await
            .unwrap();
        let mut response = row("new-response", 21, MessageRole::Agent, "");
        response.agent_id = Some(self.chat.agent_id.clone());
        response.status = Some(MessageStatus::Executing);
        self.messages().create(&response).await.unwrap();
    }

    async fn run_next(&self) -> Result<frona::agent::harness::AgentLoopOutcome, AppError> {
        self.harness
            .run_loop(
                &self.user.id,
                "chat",
                "new-response",
                CancellationToken::new(),
                self.builder(),
                &[],
                None,
                Default::default(),
            )
            .await
    }
}

fn provider_usage(total: u64) -> frona_harness::inference::Usage {
    frona_harness::inference::Usage::new()
        .input_tokens(Some(total.saturating_sub(5)))
        .output_tokens(Some(5))
        .total_tokens(Some(total))
}

#[tokio::test]
async fn provider_counts_correct_both_over_and_underestimated_history_through_inference() {
    for (characters, measured, compacted) in [
        (40_000, 6000, false),
        (8000, 13000, true),
        (40_000, 13000, true),
    ] {
        let app = App::new().await;
        app.history(MessageRole::User, false).await;
        app.resize_old_rows(characters).await;
        app.recorded_stats(provider_usage(measured)).await;
        app.next_request().await;
        app.run_next().await.unwrap();
        assert_eq!(app.summary_calls(), usize::from(compacted));
        let outgoing = serde_json::to_string(&app.model.last_history()).unwrap();
        assert!(outgoing.contains("NEW EXACT REQUEST"));
        assert!(
            outgoing.contains("old-3"),
            "calibrated split should keep the newer old row"
        );
        assert_eq!(outgoing.contains("old-2"), !compacted);
        let loaded = app
            .chat_service
            .compactor()
            .load_conversation("chat")
            .await
            .unwrap();
        assert_eq!(
            loaded.messages.iter().any(|row| row.id == "old-2"),
            !compacted
        );
        assert!(loaded.messages.iter().any(|row| row.id == "old-3"));
        assert!(loaded.messages.iter().any(|row| row.id == "new-request"));
        if compacted {
            let summary = app.summary_history();
            let text = serde_json::to_string(&summary).unwrap();
            assert!(text.contains("old-2"));
            assert!(!text.contains("old-3"));
            assert!(!text.contains("NEW EXACT REQUEST"));
        }
    }
}

#[tokio::test]
async fn nonstream_send_uses_provider_estimate_without_trimming_the_original_history() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    app.resize_old_rows(40_000).await;
    app.recorded_stats(provider_usage(6000)).await;
    app.chat_service
        .send_message(
            &app.user.id,
            "chat",
            frona::chat::message::models::SendMessageRequest {
                content: "NEW NONSTREAM REQUEST".into(),
                attachments: vec![],
                metadata: None,
            },
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(app.summary_calls(), 0);
    let outgoing = serde_json::to_string(&app.model.last_history()).unwrap();
    assert!(outgoing.contains("old-2"));
    assert!(outgoing.contains("old-3"));
    assert!(outgoing.contains("NEW NONSTREAM REQUEST"));
}

async fn fallback_statistics_fixture(usage: Option<frona_harness::inference::Usage>) -> App {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    if let Some(usage) = usage {
        app.recorded_stats(usage).await;
    }
    app
}

async fn assert_estimated_trigger_and_split(app: &App) {
    app.next_request().await;
    let request = app
        .messages()
        .find_by_id("new-request")
        .await
        .unwrap()
        .unwrap();
    app.run_next().await.unwrap();
    assert_eq!(app.summary_calls(), 1);
    let loaded = app
        .chat_service
        .compactor()
        .load_conversation("chat")
        .await
        .unwrap();
    assert!(!loaded.messages.iter().any(|row| row.id == "old-3"));
    assert_eq!(
        loaded.messages.iter().find(|row| row.id == "new-request"),
        Some(&request)
    );
    let outgoing = serde_json::to_string(&app.model.last_history()).unwrap();
    assert!(outgoing.contains("NEW EXACT REQUEST"));
    assert!(!outgoing.contains("old-3"));
}

#[tokio::test]
async fn absent_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(None).await;
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn invalid_token_counts_use_estimated_trigger_and_split() {
    // Zero usage, missing total, inconsistent total, incomplete output, and overflow.
    for usage in [
        Default::default(),
        provider_usage(6000).total_tokens(Some(0)),
        provider_usage(6000).total_tokens(Some(100)),
        provider_usage(6000).output_tokens(Some(0)),
        provider_usage(i64::MAX as u64),
    ] {
        let app = fallback_statistics_fixture(Some(usage)).await;
        assert_estimated_trigger_and_split(&app).await;
    }
}

#[tokio::test]
async fn different_model_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET model_ref = 'mock/other'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn different_provider_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET provider = 'other'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn different_model_group_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET model_group = 'other'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn different_agent_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET agent_id = 'other'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn missing_usage_message_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET message_id = 'missing'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn nonfinal_tool_turn_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET turn_index = 9")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn gapped_tool_history_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE tool_call SET turn = 2; UPDATE inference_usage SET turn_index = 3")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn unreadable_statistics_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    app.db
        .query("UPDATE inference_usage SET total_tokens = 'broken'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn unfinished_usage_message_uses_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    let mut message = app
        .messages()
        .find_by_id("tool-message")
        .await
        .unwrap()
        .unwrap();
    message.status = Some(MessageStatus::Executing);
    app.messages().update(&message).await.unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn statistics_older_than_summary_use_estimated_trigger_and_split() {
    let app = fallback_statistics_fixture(Some(provider_usage(6000))).await;
    let old = app.messages().find_by_id("old-0").await.unwrap().unwrap();
    let now = chrono::Utc::now();
    app.summaries()
        .create(&frona::chat::models::ChatSummary {
            agent_context_signature: None,
            id: "prior-summary".into(),
            user_id: app.user.id.clone(),
            chat_id: "chat".into(),
            content: "Existing summary".into(),
            compacted_until: old.created_at,
            item_count: 1,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    assert_estimated_trigger_and_split(&app).await;
}

#[tokio::test]
async fn cached_provider_totals_are_normalized_without_adding_cache_tokens_twice() {
    use frona_harness::config::ProviderModel;
    for (provider, input, cached, writes, total, expected_compaction) in [
        (ProviderModel::Generic, 5995, 5995, 0, 6000, false),
        (
            ProviderModel::Anthropic {
                params: Default::default(),
            },
            0,
            5995,
            0,
            6000,
            false,
        ),
        (
            ProviderModel::Anthropic {
                params: Default::default(),
            },
            1000,
            6000,
            995,
            8000,
            false,
        ),
        // max(input,cached) would accept this inconsistent disjoint total.
        (
            ProviderModel::Anthropic {
                params: Default::default(),
            },
            4000,
            4000,
            0,
            6000,
            true,
        ),
    ] {
        for legacy in [false, true] {
            let app = App::with_provider(18_000, true, provider.clone()).await;
            app.history(MessageRole::User, false).await;
            app.recorded_stats(
                frona_harness::inference::Usage::new()
                    .input_tokens(Some(input))
                    .cached_input_tokens(Some(cached))
                    .cache_creation_input_tokens(Some(writes))
                    .output_tokens(Some(5))
                    .total_tokens(Some(total)),
            )
            .await;
            if legacy {
                app.db
                    .query("UPDATE inference_usage SET input_tokens = $input")
                    .bind(("input", input))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            frona::db::migrations::run_migrations(&app.db)
                .await
                .unwrap();
            app.next_request().await;
            app.run_next().await.unwrap();
            assert_eq!(app.summary_calls(), usize::from(expected_compaction));
        }
    }
}

#[tokio::test]
async fn command_history_mutation_invalidates_calibration_before_provider_call() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    app.resize_old_rows(40_000).await;
    app.recorded_stats(provider_usage(6000)).await;
    app.command_rows("compact").await;
    let mut registry = frona::chat::command::CommandRegistry::new();
    registry.register(Arc::new(ContextPromptCommand));
    let result = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[],
            Some(Arc::new(registry)),
            Default::default(),
        )
        .await;
    assert!(matches!(result, Err(AppError::Validation(_))));
    assert_eq!(app.model.calls(), 0);
}

#[tokio::test]
async fn calibrated_summary_does_not_lend_its_estimate_to_a_fallback_model() {
    let app = App::with_window(128_000).await;
    app.history(MessageRole::User, false).await;
    app.resize_old_rows(40_000).await;
    app.recorded_stats(provider_usage(13000)).await;
    app.next_request().await;
    let mut session = app.session().await.unwrap();
    session.model_group.context_window = 18_000;
    let mut fallback = session.model_group.main.clone();
    fallback.model_id = "fallback-model".into();
    fallback.request_settings.max_tokens = Some(12_000);
    session.model_group.fallbacks.push(fallback);
    session.model_group.retry.max_retries = 0;
    app.model.enqueue(MockResponse::Error(
        frona_harness::inference::error::InferenceError::InferenceFailed(
            "main provider failed".into(),
        ),
    ));
    let result = app.manual(&mut session).await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("Complete input exceeds context limit")
    );
    assert_eq!(app.model.calls(), 1);
    assert_eq!(app.model.model_names(), vec!["conversation-model"]);
    assert!(
        app.summaries()
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn completed_nonstream_response_statistics_anchor_the_next_compaction() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    app.resize_old_rows(8000).await;
    app.model.set_usage(provider_usage(13000));
    let first = app
        .chat_service
        .send_message(
            &app.user.id,
            "chat",
            frona::chat::message::models::SendMessageRequest {
                content: "FIRST NONSTREAM REQUEST".into(),
                attachments: vec![],
                metadata: None,
            },
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(app.summary_calls(), 0);
    let response = first.last().unwrap();
    let usage: Vec<serde_json::Value> = app
        .db
        .query("SELECT * FROM inference_usage WHERE kind_tag = 'Text'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["message_id"], response.id);
    app.chat_service
        .send_message(
            &app.user.id,
            "chat",
            frona::chat::message::models::SendMessageRequest {
                content: "SECOND NONSTREAM REQUEST".into(),
                attachments: vec![],
                metadata: None,
            },
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(app.summary_calls(), 1);
    let outgoing = serde_json::to_string(&app.model.last_history()).unwrap();
    assert!(outgoing.contains("SECOND NONSTREAM REQUEST"));
    assert!(outgoing.contains("old-3"));
    assert!(!outgoing.contains("old-2"));
}

#[tokio::test]
async fn only_latest_final_tool_turn_counts_and_initial_tool_request_keeps_full_history() {
    let app = App::new().await;
    app.fixture.state.policy_service.create_policy(&app.user.id,
        r#"@id("calibrated-read") permit(principal, action == Policy::Action::"invoke_tool", resource) when { resource == Policy::Tool::"read" };"#,
    ).await.unwrap();
    app.history(MessageRole::User, false).await;
    app.resize_old_rows(40_000).await;
    app.recorded_stats(provider_usage(100_000)).await;
    app.db
        .query("UPDATE inference_usage SET turn_index = 0")
        .await
        .unwrap()
        .check()
        .unwrap();
    app.recorded_stats(provider_usage(6000)).await;
    app.next_request().await;
    app.run_next().await.unwrap();
    assert_eq!(app.summary_calls(), 0);
    assert_eq!(app.model.calls(), 1);
    assert_eq!(
        app.model.tool_histories()[0]
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["mcpctl", "read"].into_iter().collect()
    );
    let outgoing = serde_json::to_string(&app.model.histories()[0]).unwrap();
    assert!(outgoing.contains("old-2"));
    assert!(outgoing.contains("old-3"));
    assert!(outgoing.contains("NEW EXACT REQUEST"));
    let usage: Vec<serde_json::Value> = app
        .db
        .query("SELECT * FROM inference_usage WHERE message_id = 'new-response'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["kind_tag"], "ToolTurn");
    assert_eq!(usage[0]["turn_index"], 0);
}

#[tokio::test]
async fn fresh_request_and_injected_continuation_are_never_scaled_by_old_usage() {
    for continuation in [false, true] {
        let app = App::new().await;
        app.history(MessageRole::User, false).await;
        app.resize_old_rows(40_000).await;
        let builder: Box<dyn ConversationBuilder> = if continuation {
            app.messages().delete("placeholder").await.unwrap();
            let mut instruction = row(
                "initial-task",
                -1,
                MessageRole::Agent,
                "Original task instruction",
            );
            instruction.agent_id = Some(app.chat.agent_id.clone());
            app.messages().create(&instruction).await.unwrap();
            Box::new(frona::inference::conversation::TaskConversationBuilder {
                user_service: app.fixture.state.user_service.clone(),
                storage_service: app.fixture.state.storage_service.clone(),
                agent_service: app.fixture.state.agent_service.clone(),
                continuation_prompt: Some("fresh continuation ".repeat(4000)),
            })
        } else {
            app.next_request().await;
            let mut request = app
                .messages()
                .find_by_id("new-request")
                .await
                .unwrap()
                .unwrap();
            request.content = "fresh protected request ".repeat(4000);
            app.messages().update(&request).await.unwrap();
            app.builder()
        };
        app.recorded_stats(provider_usage(6000)).await;
        let before = app.messages().find_by_chat_id("chat").await.unwrap();
        let result = ChatSessionContext::build(
            &app.harness,
            &app.user.id,
            app.chat.clone(),
            None,
            CancellationToken::new(),
            Arc::from(builder),
            &[],
            Default::default(),
        )
        .await;
        assert!(
            matches!(result, Err(AppError::Validation(_))),
            "continuation={continuation}"
        );
        assert_eq!(app.model.calls(), 0);
        assert!(
            app.summaries()
                .find_by_chat_id("chat")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            app.messages().find_by_chat_id("chat").await.unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn resumed_response_excludes_later_inputs_and_cannot_compact_their_prefix() {
    for needs_compaction in [false, true] {
        let app = App::with_window(if needs_compaction { 18_000 } else { 50_000 }).await;
        app.history(MessageRole::User, false).await;
        let later = row(
            "future",
            20,
            MessageRole::User,
            "FUTURE INPUT MUST NOT LEAK",
        );
        app.messages().create(&later).await.unwrap();
        let state = &app.fixture.state;
        let result = app
            .harness
            .run_loop(
                &app.user.id,
                "chat",
                "placeholder",
                CancellationToken::new(),
                Box::new(DefaultConversationBuilder {
                    user_service: state.user_service.clone(),
                    storage_service: state.storage_service.clone(),
                    agent_service: state.agent_service.clone(),
                }),
                &[],
                None,
                Default::default(),
            )
            .await;
        if needs_compaction {
            assert!(matches!(result, Err(AppError::Conflict(_))));
            assert_eq!(app.summary_calls(), 0);
        } else {
            assert!(result.is_ok());
            assert_eq!(app.inference_calls(), 1);
            assert!(
                !serde_json::to_string(&app.model.last_history())
                    .unwrap()
                    .contains("FUTURE INPUT MUST NOT LEAK")
            );
        }
        assert!(
            app.summaries()
                .find_by_chat_id("chat")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            app.messages().find_by_id(&later.id).await.unwrap(),
            Some(later)
        );
    }
}
#[tokio::test]
async fn compaction_keeps_input_received_during_summary() {
    let app = App::new().await;
    app.history(MessageRole::User, false).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    app.model.enqueue(MockResponse::Barrier(
        barrier.clone(),
        Box::new(MockResponse::Text("summary".into())),
    ));
    let append = async {
        while app.summary_calls() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let message = app
            .chat_service
            .create_stream_user_message(
                &app.user.id,
                "chat",
                "new input during summary",
                vec![],
                None,
            )
            .await
            .unwrap();
        barrier.wait().await;
        message
    };
    let (result, input) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(app.session(), append)
    })
    .await
    .expect("summary and input must finish independently");
    result.unwrap();
    let summary = app
        .summaries()
        .find_by_chat_id("chat")
        .await
        .unwrap()
        .unwrap();
    let input = app.messages().find_by_id(&input.id).await.unwrap().unwrap();
    assert!(input.created_at > summary.updated_at);
    let replay = app
        .chat_service
        .compactor()
        .load_conversation("chat")
        .await
        .unwrap();
    assert!(replay.messages.contains(&input));
    assert!(replay.agent_context.is_some());
}

/// A builder may retain native history that cannot be represented by the SDK's
/// live response events. Loading it must not invent origins or normalize IDs.
struct NativePrefix(Vec<RigMessage>);

#[async_trait]
impl ConversationBuilder for NativePrefix {
    async fn build(
        &self,
        messages: &[Message],
        _: &[frona::inference::tool_call::ToolCall],
        _: &ConversationContext,
        summary: Option<&str>,
    ) -> frona::inference::conversation::ModelConversationHistory {
        assert!(summary.is_none());
        assert_eq!(messages.len(), 1);
        frona::inference::conversation::ModelConversationHistory {
            history: self.0.clone(),
            row_ids: messages.iter().map(|row| row.id.clone()).collect(),
            row_ends: vec![self.0.len()],
            summary: None,
            summary_end: 0,
        }
    }
}

#[tokio::test]
async fn persistent_request_preserves_native_ids_signatures_system_and_mixed_results() {
    use frona::chat::conversation::{PersistentConversation, PersistentRequest};
    use rig_core::completion::message::{
        CallId, Reasoning, ToolCall, ToolFunction, ToolResult, ToolResultContent,
    };
    let app = App::new().await;
    let saved = row("source", 0, MessageRole::User, "raw input");
    app.messages().create(&saved).await.unwrap();
    let call = AssistantContent::ToolCall(ToolCall::from_wire(
        "call-9", ToolFunction::new("original-name".try_into().unwrap(), serde_json::json!({"nested": [1, null, "exact"]}))
    )).with_native(serde_json::json!({"id":"item-17", "signature":"opaque+/=\nsignature", "provider":"opaque"}));
    let mut origin = rig_core::message::Origin::new("openai.responses", "openai", "fixture");
    origin.response_id = Some("native-assistant-id".into());
    let native = vec![
        RigMessage::System {
            content: "native system".into(),
        },
        RigMessage::user("frozen input"),
        RigMessage::Assistant(rig_core::message::AssistantMessage::new(vec![
            AssistantContent::Reasoning(Reasoning::new("native reasoning"))
                .with_native(serde_json::json!({"type":"thinking", "thinking":"native reasoning", "signature":"reason-signature"})),
            call,
        ]).with_origin(origin)),
        RigMessage::User {
            content: vec![
                UserContent::ToolResult(ToolResult {
                    call: CallId::from_wire("call-9"),
                    name: "repaired-name".try_into().unwrap(),
                    is_error: false,
                    content: vec![ToolResultContent::text("exact result")],
                }),
                UserContent::text("mixed feedback"),
            ],
        },
    ];
    let builder = NativePrefix(native.clone());
    let group = helpers::test_model_group();
    let context = ConversationContext {
        user_id: app.user.id.clone(),
        agent_id: app.chat.agent_id.clone(),
        model_config: group.main.clone(),
    };
    let scope = helpers::mock_context();
    for _ in 0..2 {
        let mut conversation = PersistentConversation::load(
            &app.chat_service,
            "chat",
            Some("source"),
            &builder,
            &context,
        )
        .await
        .unwrap();
        let request = conversation
            .prepare(
                app.chat_service.compactor(),
                PersistentRequest {
                    agent_context: scope.agent_context_scope(),
                    system_prompt: "outer system",
                    model_group: &group,
                    tools: &[],
                },
                frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(request.messages).unwrap(),
            serde_json::to_value(&native).unwrap()
        );
        assert_eq!(request.system, "outer system");
        assert!(!conversation.was_compacted());
        assert_eq!(conversation.model_history().row_ids, vec!["source"]);
        assert_eq!(conversation.stored_messages(), std::slice::from_ref(&saved));
    }
    assert_eq!(
        app.messages().find_by_id("source").await.unwrap(),
        Some(saved)
    );
    assert_eq!(app.model.calls(), 0);
    assert!(
        app.summaries()
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn persistent_request_compacts_then_reopens_the_saved_prefix_and_row_mapping() {
    use frona::chat::conversation::{PersistentConversation, PersistentRequest};
    let app = App::with_window(14_000).await;
    let retained = app.history(MessageRole::User, false).await;
    let tool_repo = frona::db::repo::tool_calls::SurrealToolCallRepo::new(app.db.clone());
    let mut call = tool_repo
        .find_by_message_id(&retained[2].id)
        .await
        .unwrap()
        .remove(0);
    call.provider_item_id = Some("fc_persisted_item".into());
    call.turn_text = None;
    // A legacy Responses turn retains the reasoning ciphertext alongside its
    // following call's item ID. Gemini call signatures use a different dialect.
    call.turn_reasoning = Some(frona::chat::message::models::Reasoning {
        id: Some("rs_persisted_reasoning".into()),
        content: String::new(),
        signature: None,
        raw: Some(serde_json::json!({
            "id":"rs_persisted_reasoning",
            "content":[{"type":"encrypted","content":"persisted-opaque+/=reasoning"}]
        })),
    });
    tool_repo.update(&call).await.unwrap();
    let saved_calls = app.chat_service.get_tool_calls("chat").await.unwrap();
    let before = app.messages().find_by_chat_id("chat").await.unwrap();
    let builder = app.builder();
    let group = app
        .chat_service
        .model_providers()
        .resolve_with_fallback(
            &frona_harness::inference::ModelRef("conversation".into()),
            &frona_harness::inference::ModelRef::PRIMARY,
        )
        .unwrap();
    let context = ConversationContext {
        user_id: app.user.id.clone(),
        agent_id: app.chat.agent_id.clone(),
        model_config: group.main.clone(),
    };
    let scope = helpers::mock_context();
    let mut conversation =
        PersistentConversation::load(&app.chat_service, "chat", None, builder.as_ref(), &context)
            .await
            .unwrap();
    let request = conversation
        .prepare(
            app.chat_service.compactor(),
            PersistentRequest {
                agent_context: scope.agent_context_scope(),
                system_prompt: "system",
                model_group: &group,
                tools: &[],
            },
            frona_harness::ExecutionControl::new(&frona_harness::DefaultBudget::default()),
        )
        .await
        .unwrap();
    assert!(conversation.was_compacted());
    let serialized = serde_json::to_string(&request.messages).unwrap();
    assert!(serialized.contains("fc_persisted_item"));
    assert!(serialized.contains("rs_persisted_reasoning"));
    assert!(serialized.contains("persisted-opaque+/=reasoning"));
    assert_eq!(
        serde_json::to_value(app.chat_service.get_tool_calls("chat").await.unwrap()).unwrap(),
        serde_json::to_value(saved_calls).unwrap()
    );
    assert_eq!(conversation.stored_messages(), retained);
    assert_eq!(
        conversation.model_history().row_ids,
        retained
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>()
    );
    let saved_summary = app
        .summaries()
        .find_by_chat_id("chat")
        .await
        .unwrap()
        .unwrap();
    assert!(saved_summary.agent_context_signature.is_some());
    let prefix = serde_json::to_string(&request.messages[0]).unwrap();
    assert!(prefix.contains("<conversation_summary>"));
    let snapshot = app
        .chat_service
        .agent_context_manager()
        .load(saved_summary.agent_context_signature.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        request.messages[0],
        frona_harness::inference::compaction::summary_message(
            &saved_summary.content,
            Some(&snapshot.render()),
        )
    );
    let calls = app.model.calls();
    let reopened =
        PersistentConversation::load(&app.chat_service, "chat", None, builder.as_ref(), &context)
            .await
            .unwrap();
    assert_eq!(reopened.model_history().history, request.messages);
    assert_eq!(reopened.stored_messages(), conversation.stored_messages());
    assert_eq!(
        app.messages().find_by_chat_id("chat").await.unwrap(),
        before
    );
    assert_eq!(
        app.summaries()
            .find_by_chat_id("chat")
            .await
            .unwrap()
            .unwrap(),
        saved_summary
    );
    assert_eq!(app.model.calls(), calls);
}

#[tokio::test]
async fn persistent_summary_and_main_call_consume_one_shared_allowance() {
    use frona::inference::InferenceResponse;
    use frona::inference::RuntimeControls;
    use frona_harness::BudgetLimits;
    use frona_harness::DefaultBudget;
    use frona_harness::ExecutionStop;
    use frona_harness::FailureKind;
    use frona_harness::RunOutcome;
    for direct_reply in [false, true] {
        let app = App::new().await;
        app.history(MessageRole::User, false).await;
        let budget = Arc::new(DefaultBudget::new(BudgetLimits {
            model_attempts: Some(1),
            ..BudgetLimits::unlimited()
        }));
        let controls = RuntimeControls::new(budget.clone());
        let report = controls.report();
        if direct_reply {
            let error = app
                .chat_service
                .send_message(
                    &app.user.id,
                    &app.chat.id,
                    frona::chat::message::models::SendMessageRequest {
                        content: "new request".into(),
                        attachments: vec![],
                        metadata: None,
                    },
                    controls,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            let AppError::Inference(error) = error else {
                panic!("expected typed model control")
            };
            assert!(matches!(
                error.into_execution_stop(),
                Ok(ExecutionStop::BudgetExhausted(_))
            ));
        } else {
            let outcome = app
                .harness
                .run_loop(
                    &app.user.id,
                    "chat",
                    "placeholder",
                    CancellationToken::new(),
                    app.builder(),
                    &[],
                    None,
                    controls,
                )
                .await
                .unwrap();
            assert!(matches!(
                outcome.inference,
                InferenceResponse::Incomplete {
                    reason: ExecutionStop::BudgetExhausted(_),
                    ..
                }
            ));
        }
        assert_eq!(app.summary_calls(), 1);
        assert_eq!(app.inference_calls(), 0);
        assert_eq!(budget.snapshot().model_attempts, 1);
        let report = report.snapshot();
        assert_eq!(
            report.requests.len(),
            2,
            "summary and refused main are separate logical requests"
        );
        assert_eq!(report.attempts.len(), 1);
        assert!(matches!(
            report.outcome,
            Some(RunOutcome::Incomplete {
                kind: FailureKind::Budget,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn required_summary_and_reply_writes_report_storage_stops() {
    use frona::inference::RuntimeControls;
    use frona_harness::ExecutionStop;
    use frona_harness::FailureKind;
    use frona_harness::RunOutcome;
    for summary in [true, false] {
        let app = App::with_window(if summary { 18_000 } else { 128_000 }).await;
        if summary {
            app.history(MessageRole::User, false).await;
        }
        app.db
            .query(if summary {
                "DEFINE FIELD content ON chat_summary TYPE string ASSERT false"
            } else {
                "DEFINE FIELD content ON message TYPE string ASSERT $value != 'default response'"
            })
            .await
            .unwrap()
            .check()
            .unwrap();
        let controls = RuntimeControls::default();
        let report = controls.report();
        let error = if summary {
            app.harness
                .run_loop(
                    &app.user.id,
                    "chat",
                    "placeholder",
                    CancellationToken::new(),
                    app.builder(),
                    &[],
                    None,
                    controls,
                )
                .await
                .err()
                .unwrap()
        } else {
            app.chat_service
                .send_message(
                    &app.user.id,
                    "chat",
                    frona::chat::message::models::SendMessageRequest {
                        content: "input".into(),
                        attachments: vec![],
                        metadata: None,
                    },
                    controls,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err()
        };
        assert!(
            matches!(error, AppError::Inference(ref error) if matches!(error.as_ref(), frona_harness::inference::InferenceError::Stop(ExecutionStop::Storage(_)))),
            "{error:?}"
        );
        assert!(matches!(
            report.snapshot().outcome,
            Some(RunOutcome::Incomplete {
                kind: FailureKind::Storage,
                ..
            })
        ));
        assert_eq!(app.inference_calls(), usize::from(!summary));
    }
}

#[tokio::test]
async fn switched_agent_tools_use_the_persisted_executing_owner() {
    let app = App::with_window(128_000).await;
    app.fixture.state.policy_service.create_policy(&app.user.id, r#"@id("switch-write") permit(principal, action == Policy::Action::"invoke_tool", resource) when { resource == Policy::Tool::"write" };"#).await.unwrap();
    let mut other = helpers::mock_context().agent;
    other.id = "other-agent".into();
    other.handle = frona::handle!("other-agent");
    other.model_group = "conversation".into();
    other.prompt = Some("Selected agent".into());
    SurrealRepo::new(app.db.clone())
        .create(&other)
        .await
        .unwrap();
    app.command_rows("other-agent").await;
    app.model.enqueue(MockResponse::ToolCalls(vec![(
        "write-call".into(),
        "write".into(),
        serde_json::json!({"path":"switch.txt","content":"target-owned"}),
    )]));
    let outcome = app
        .harness
        .run_loop(
            &app.user.id,
            "chat",
            "command-response",
            CancellationToken::new(),
            app.builder(),
            &[],
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.inference,
        frona::inference::InferenceResponse::Completed { .. }
    ));
    let response = app
        .messages()
        .find_by_id("command-response")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.agent_id.as_deref(), Some("other-agent"));
    let calls = app
        .chat_service
        .get_tool_calls_by_message(&response.id)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].success, "{calls:?}");
}

#[tokio::test]
async fn cancellation_during_final_runtime_write_keeps_durable_evidence_but_report_is_incomplete() {
    use frona::chat::broadcast::BroadcastEventKind;
    use frona::chat::broadcast::BroadcastService;
    use frona::core::execution::ExecutionKind;
    use frona::core::execution::NewExecution;
    use frona::inference::InferenceEventKind;
    use frona::inference::RuntimeControls;
    use frona_harness::FailureKind;
    use frona_harness::RunOutcome;
    for direct_reply in [true, false] {
        let cancellation = CancellationToken::new();
        // A receiver waker runs synchronously when the production event bus
        // publishes. Drain nonterminal events and rearm before the final write.
        struct CancelOnTerminal {
            receiver: std::sync::Mutex<
                tokio::sync::mpsc::UnboundedReceiver<frona::chat::broadcast::BroadcastEvent>,
            >,
            cancellation: CancellationToken,
        }
        impl std::task::Wake for CancelOnTerminal {
            fn wake(self: Arc<Self>) {
                Self::wake_by_ref(&self);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                let waker = std::task::Waker::from(self.clone());
                let mut context = std::task::Context::from_waker(&waker);
                let mut receiver = self.receiver.lock().unwrap();
                while let std::task::Poll::Ready(Some(event)) = receiver.poll_recv(&mut context) {
                    let terminal = match event.kind {
                        BroadcastEventKind::ChatMessage { message } => {
                            message.status == Some(MessageStatus::Completed)
                        }
                        BroadcastEventKind::Inference(InferenceEventKind::Done { .. }) => true,
                        _ => false,
                    };
                    if terminal {
                        self.cancellation.cancel();
                    }
                }
            }
        }
        let events = BroadcastService::new();
        let observer = Arc::new(CancelOnTerminal {
            receiver: std::sync::Mutex::new(events.subscribe_raw()),
            cancellation: cancellation.clone(),
        });
        std::task::Wake::wake_by_ref(&observer);
        let app = App::with_provider_and_events(
            128_000,
            true,
            frona_harness::config::ProviderModel::Generic,
            Some(events),
        )
        .await;
        let controls = RuntimeControls::default();
        let report = controls.report();
        if direct_reply {
            let error = app
                .chat_service
                .send_message(
                    &app.user.id,
                    "chat",
                    frona::chat::message::models::SendMessageRequest {
                        content: "input".into(),
                        attachments: vec![],
                        metadata: None,
                    },
                    controls,
                    cancellation.clone(),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, AppError::Inference(error) if error.is_cancelled()));
        } else {
            app.next_request().await;
            app.harness
                .run_turn_with_execution(
                    &app.user.id,
                    "chat",
                    "new-response",
                    cancellation.clone(),
                    app.builder(),
                    &[],
                    None,
                    NewExecution {
                        title: "test".into(),
                        agent_name: None,
                        kind: ExecutionKind::Inference,
                        action: None,
                        source: None,
                        related_chat_ids: vec!["chat".into()],
                        can_cancel: true,
                    },
                    controls,
                )
                .await;
        }
        assert!(cancellation.is_cancelled());
        assert!(
            matches!(
                report.snapshot().outcome,
                Some(RunOutcome::Incomplete {
                    kind: FailureKind::Cancelled,
                    ..
                })
            ),
            "{:?}",
            report.snapshot()
        );
        let messages = app.messages().find_by_chat_id("chat").await.unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message.status == Some(MessageStatus::Completed)
                    && message.content == "default response"),
            "durable completion cannot be rolled back by late cancellation"
        );
    }
}
