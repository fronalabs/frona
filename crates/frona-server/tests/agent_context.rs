//! Persisted context boundaries: no provider or network is required.
mod helpers;

use async_trait::async_trait;
use frona::agent::context::*;
use frona::chat::message::models::{Message, MessageCommand, MessageRole};
use frona::chat::message::repository::MessageRepository;
use frona::chat::models::ChatSummary;
use frona::chat::repository::ChatSummaryRepository;
use frona::chat::service::ChatService;
use frona::core::{error::AppError, repository::Repository};
use frona::db::repo::{
    agent_context_snapshots::SurrealAgentContextSnapshotRepo,
    chat_summaries::SurrealChatSummaryRepo, generic::SurrealRepo, messages::SurrealMessageRepo,
};
use frona::inference::conversation::{
    ConversationBuilder, ConversationContext, DefaultConversationBuilder, ModelConversationHistory,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CaptureState {
    captures: u32,
}

struct Source {
    text: Mutex<String>,
    fail: AtomicBool,
    calls: AtomicUsize,
}
#[async_trait]
impl AgentContextSource for Source {
    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut captured = state.get::<CaptureState>()?;
        captured.captures += 1;
        state.set(captured)?;
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Internal("source offline".into()));
        }
        Ok(vec![AgentContextSection::prose(
            AgentContextKind::UserProfile,
            "user_profile",
            format!(
                "{} / {} / {}",
                scope.user.id,
                scope.agent.id,
                self.text.lock().unwrap()
            ),
        )])
    }
}
struct App {
    db: Surreal<Db>,
    fixture: helpers::app_state::Fixture,
    service: ChatService,
    source: Arc<Source>,
}
impl App {
    async fn new() -> Self {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let fixture = helpers::app_state::build(&db).await;
        let ctx = helpers::mock_context();
        SurrealRepo::new(db.clone())
            .create(&ctx.user)
            .await
            .unwrap();
        SurrealRepo::new(db.clone())
            .create(&ctx.agent)
            .await
            .unwrap();
        SurrealRepo::new(db.clone())
            .create(&ctx.chat.unwrap())
            .await
            .unwrap();
        let source = Arc::new(Source {
            text: Mutex::new("A".into()),
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        });
        let manager = AgentContextManager::new(
            vec![
                source.clone(),
                Arc::new(TemporalContextSource {
                    default_timezone: fixture.state.config.server.timezone.clone(),
                }),
            ],
            SurrealAgentContextSnapshotRepo(db.clone()),
        );
        let service = helpers::chat_service_with_context(&fixture.state, manager);
        Self {
            db,
            fixture,
            service,
            source,
        }
    }
    async fn finalize_command_prompt(
        &self,
        message: &Message,
        rendered: &str,
    ) -> Result<Message, AppError> {
        let mut ctx = helpers::mock_context();
        let chat = frona::db::repo::chats::SurrealChatRepo::new(self.db.clone())
            .find_by_id(&message.chat_id)
            .await?
            .unwrap();
        ctx.user = self
            .fixture
            .state
            .user_service
            .find_by_id(&chat.user_id)
            .await?
            .unwrap();
        ctx.agent = self
            .fixture
            .state
            .agent_service
            .find_by_id(message.agent_id.as_deref().unwrap_or(&chat.agent_id))
            .await?
            .unwrap();
        ctx.task = match &chat.task_id {
            Some(id) => {
                frona::db::repo::tasks::SurrealTaskRepo::new(self.db.clone())
                    .find_by_id(id)
                    .await?
            }
            None => None,
        };
        ctx.chat = Some(chat);
        self.service
            .finalize_command_prompt(message, rendered, &ctx.agent_context_scope())
            .await
    }
    fn messages(&self) -> SurrealMessageRepo {
        SurrealRepo::new(self.db.clone())
    }
    fn summaries(&self) -> SurrealChatSummaryRepo {
        SurrealRepo::new(self.db.clone())
    }
    async fn input(&self, text: &str) -> Message {
        let response = self
            .service
            .create_stream_user_message("test-user", "test-chat", text, vec![], None)
            .await
            .unwrap();
        self.messages()
            .find_by_id(&response.id)
            .await
            .unwrap()
            .unwrap()
    }
    async fn history(&self) -> ModelConversationHistory {
        let loaded = self
            .service
            .compactor()
            .load_conversation("test-chat")
            .await
            .unwrap();
        let state = &self.fixture.state;
        let mut history = DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
        }
        .build(
            &loaded.messages,
            &[],
            &ConversationContext {
                agent_id: "test-agent".into(),
                user_id: "test-user".into(),
                model_config: helpers::test_model_group().main,
            },
            loaded.summary.as_deref(),
        )
        .await;
        history.set_summary_context(loaded.agent_context.as_deref());
        history
    }
}

#[tokio::test]
async fn temporal_timezone_change_is_persisted_only_with_a_saved_message() {
    let app = App::new().await;
    let first = app.input("first").await;
    let users = &app.fixture.state.user_service;
    let mut user = users.find_by_id("test-user").await.unwrap().unwrap();
    user.timezone = Some("Asia/Tokyo".into());
    users.update(&user).await.unwrap();
    let mut duplicate =
        Message::builder("test-chat", MessageRole::User, "duplicate".into()).build();
    duplicate.id = first.id.clone();
    assert!(
        app.service
            .persist_inbound_message(&duplicate)
            .await
            .is_err()
    );
    let changed = app.input("new timezone").await;
    assert!(
        changed
            .inference_content()
            .contains("user_timezone: Asia/Tokyo")
    );
    assert_ne!(
        changed.agent_context_signature,
        first.agent_context_signature
    );
    let next = app.input("unchanged").await;
    assert!(!next.inference_content().contains("<temporal_context>"));
    assert_eq!(next.processed_content.as_deref(), Some("unchanged"));
    let empty = Message::builder("test-chat", MessageRole::User, String::new()).build();
    app.service.persist_inbound_message(&empty).await.unwrap();
    assert_eq!(
        app.messages()
            .context_position("test-chat")
            .await
            .unwrap()
            .latest_entry
            .unwrap()
            .id,
        empty.id
    );
}

#[tokio::test]
async fn unresolved_task_rejects_the_turn_before_context_is_saved() {
    let app = App::new().await;
    let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
    let mut chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    chat.task_id = Some("missing-task".into());
    chats.update(&chat).await.unwrap();

    let error = app
        .service
        .create_stream_user_message("test-user", "test-chat", "hello", vec![], None)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::NotFound(_)));
    assert!(
        app.messages()
            .find_by_chat_id("test-chat")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        chat.agent_context_state,
    );
    assert_eq!(app.source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn old_chat_emits_temporal_context_on_its_next_turn() {
    let app = App::new().await;
    app.messages()
        .create(&Message::builder("test-chat", MessageRole::User, "old message".into()).build())
        .await
        .unwrap();
    assert!(
        app.input("new turn")
            .await
            .inference_content()
            .contains("<temporal_context>")
    );
    assert!(
        !app.input("same day")
            .await
            .inference_content()
            .contains("<temporal_context>")
    );
}

#[tokio::test]
async fn task_clock_is_still_included_when_date_and_timezone_are_unchanged() {
    let app = App::new().await;
    let now = chrono::Utc::now();
    let task: frona::agent::task::models::Task = serde_json::from_value(serde_json::json!({
        "id": "task", "user_id": "test-user", "agent_id": "test-agent",
        "title": "Task", "description": "Test task time", "status": "pending",
        "run_at": now, "created_at": now, "updated_at": now
    }))
    .unwrap();
    SurrealRepo::new(app.db.clone())
        .create(&task)
        .await
        .unwrap();
    let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
    let mut chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    chat.task_id = Some(task.id.clone());
    chats.update(&chat).await.unwrap();
    let first = app.input("first").await;
    let second = app.input("second").await;
    assert!(first.inference_content().contains("<temporal_context>"));
    assert!(!second.inference_content().contains("<temporal_context>"));
    for message in [&first, &second] {
        assert!(message.inference_content().contains("<task_time>"));
        assert!(message.inference_content().contains("- now:"));
        assert!(message.inference_content().contains("- scheduled_at:"));
    }
    assert_eq!(
        first.agent_context_signature,
        second.agent_context_signature
    );
    let snapshot = app
        .service
        .agent_context_manager()
        .load(first.agent_context_signature.as_ref().unwrap())
        .await
        .unwrap();
    assert!(snapshot.render().contains("<temporal_context>"));
    assert!(!snapshot.render().contains("<task_time>"));
    let original_history = app.history().await.history;
    let command = app
        .service
        .create_stream_user_message(
            "test-user",
            "test-chat",
            "/custom args",
            vec![],
            Some(MessageCommand::Command {
                name: "custom".into(),
                args: "args".into(),
            }),
        )
        .await
        .unwrap();
    let pending = app
        .messages()
        .find_by_id(&command.id)
        .await
        .unwrap()
        .unwrap();
    assert!(pending.agent_context_signature.is_none());
    let finalized = app
        .finalize_command_prompt(&pending, "expanded")
        .await
        .unwrap();
    assert_eq!(
        finalized.agent_context_signature,
        first.agent_context_signature
    );
    assert!(finalized.inference_content().contains("<task_time>"));
    app.source.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        app.finalize_command_prompt(&finalized, "ignored")
            .await
            .unwrap(),
        finalized
    );
    assert_eq!(
        &app.history().await.history[..original_history.len()],
        original_history
    );
    let mut result = app
        .db
        .query("SELECT * FROM agent_context_snapshot")
        .await
        .unwrap();
    let snapshots: Vec<surrealdb::types::Value> = result.take(0).unwrap();
    assert_eq!(snapshots.len(), 1);
}

#[tokio::test]
async fn unchanged_temporal_context_is_not_repeated_on_new_turns() {
    let app = App::new().await;
    let first = app.input("hello").await;
    assert!(first.inference_content().contains("<temporal_context>"));
    let prefix = app.history().await.history;
    *app.source.text.lock().unwrap() = "B".into();
    let second = app.input("test memory update").await;
    assert!(second.inference_content().contains(" / B"));
    assert!(!second.inference_content().contains("<temporal_context>"));
    let history = app.history().await.history;
    assert_eq!(&history[..prefix.len()], prefix);
}

#[tokio::test]
async fn append_updates_preserves_prefix_and_inherits_without_collecting() {
    let app = App::new().await;
    let first = app.input("hello").await;
    assert!(first.agent_context_signature.is_some());
    assert!(first.inference_content().contains(" / A"));
    let prefix = app.history().await.history;
    *app.source.text.lock().unwrap() = "B".into();
    let assistant = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let assistant = app
        .messages()
        .find_by_id(&assistant.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        assistant.agent_context_signature,
        first.agent_context_signature
    );
    assert_eq!(app.source.calls.load(Ordering::SeqCst), 1);
    let second = app.input("second").await;
    assert_ne!(
        second.agent_context_signature,
        first.agent_context_signature
    );
    assert!(second.processed_content.as_ref().unwrap().contains(" / B"));
    assert_eq!(app.history().await.history.first(), prefix.first());
    let history = app.history().await;
    assert_eq!(&history.history[..prefix.len()], prefix);
    let third = app.input("same sources").await;
    assert_eq!(
        third.agent_context_signature,
        second.agent_context_signature
    );
    assert!(!third.inference_content().contains("<user_profile>"));
    let before = serde_json::to_value(&app.history().await.history).unwrap();
    app.source.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        serde_json::to_value(app.history().await.history).unwrap(),
        before
    );
    assert_eq!(app.history().await.history.first(), prefix.first());
}

#[tokio::test]
async fn source_failure_does_not_accept_a_message_and_recovers() {
    let app = App::new().await;
    app.source.fail.store(true, Ordering::SeqCst);
    assert!(
        app.service
            .create_stream_user_message("test-user", "test-chat", "fail", vec![], None)
            .await
            .is_err()
    );
    assert!(
        app.messages()
            .find_by_chat_id("test-chat")
            .await
            .unwrap()
            .is_empty()
    );
    app.source.fail.store(false, Ordering::SeqCst);
    let first = app.input("accepted").await;
    app.source.fail.store(true, Ordering::SeqCst);
    assert!(
        app.service
            .create_stream_user_message("test-user", "test-chat", "fail", vec![], None)
            .await
            .is_err()
    );
    assert_eq!(
        app.messages().find_by_chat_id("test-chat").await.unwrap(),
        vec![first]
    );
}

#[tokio::test]
async fn old_chat_gets_context_on_the_new_turn_without_rewriting_history() {
    for with_summary in [false, true] {
        let app = App::new().await;
        let path =
            std::path::Path::new(&app.fixture.state.config.storage.skills_dir).join("existing");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            "Current skill must not replace historical text",
        )
        .unwrap();
        let mut old = Message::builder("test-chat", MessageRole::User, "/existing original".into())
            .command(MessageCommand::Skill {
                name: "existing".into(),
                prompt: "original".into(),
            })
            .build();
        old.created_at -= chrono::Duration::seconds(2);
        app.messages().create(&old).await.unwrap();
        let now = chrono::Utc::now();
        let summary = ChatSummary {
            id: "summary".into(),
            chat_id: "test-chat".into(),
            user_id: "test-user".into(),
            content: "earlier conversation".into(),
            item_count: 1,
            compacted_until: now - chrono::Duration::seconds(3),
            created_at: now,
            updated_at: now,
            agent_context_signature: None,
        };
        if with_summary {
            app.summaries().create(&summary).await.unwrap();
        }
        app.source.fail.store(true, Ordering::SeqCst);
        let before = app.history().await.history;
        assert_eq!(app.source.calls.load(Ordering::SeqCst), 0);
        assert!(
            serde_json::to_string(&before)
                .unwrap()
                .contains("/existing original")
        );
        assert!(
            !serde_json::to_string(&before)
                .unwrap()
                .contains("Current skill")
        );
        assert!(
            app.service
                .create_stream_user_message("test-user", "test-chat", "failed turn", vec![], None,)
                .await
                .is_err()
        );
        assert_eq!(
            app.messages().find_by_chat_id("test-chat").await.unwrap(),
            vec![old.clone()]
        );
        app.source.fail.store(false, Ordering::SeqCst);
        let first = app.input("new turn").await;
        assert!(first.inference_content().contains(" / A"));
        assert!(first.inference_content().contains("<temporal_context>"));
        assert!(first.agent_context_signature.is_some());
        assert_eq!(&app.history().await.history[..before.len()], before);
        let second = app.input("next turn").await;
        assert_eq!(
            second.agent_context_signature,
            first.agent_context_signature
        );
        assert!(!second.inference_content().contains("<user_profile>"));
        assert_eq!(
            app.messages().find_by_id(&old.id).await.unwrap().unwrap(),
            old
        );
        assert_eq!(
            app.summaries().find_by_chat_id("test-chat").await.unwrap(),
            with_summary.then_some(summary)
        );
        app.service
            .delete_messages_for_chat("test-user", "test-chat")
            .await
            .unwrap();
        assert!(
            app.messages()
                .find_by_chat_id("test-chat")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            app.summaries()
                .find_by_chat_id("test-chat")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            app.input("new baseline")
                .await
                .inference_content()
                .contains(" / A")
        );
    }
}

#[tokio::test]
async fn skill_invocation_is_frozen_after_file_changes_and_removal() {
    let app = App::new().await;
    let path = std::path::Path::new(&app.fixture.state.config.storage.skills_dir).join("frozen");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("SKILL.md"),
        "---\ndescription: Example\n---\nOriginal $ARGUMENTS",
    )
    .unwrap();
    let response = app
        .service
        .create_stream_user_message(
            "test-user",
            "test-chat",
            "/frozen argument",
            vec![],
            Some(MessageCommand::Skill {
                name: "frozen".into(),
                prompt: "argument".into(),
            }),
        )
        .await
        .unwrap();
    let saved = app
        .messages()
        .find_by_id(&response.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        saved
            .processed_content
            .as_ref()
            .unwrap()
            .contains("Original argument")
    );
    let before = serde_json::to_value(app.history().await.history).unwrap();
    std::fs::write(path.join("SKILL.md"), "Changed").unwrap();
    assert_eq!(
        serde_json::to_value(app.history().await.history).unwrap(),
        before
    );
    std::fs::remove_file(path.join("SKILL.md")).unwrap();
    assert_eq!(
        serde_json::to_value(app.history().await.history).unwrap(),
        before
    );
}

#[tokio::test]
async fn agent_switch_captures_the_target_and_next_turn_returns_to_default() {
    let app = App::new().await;
    let mut agent = helpers::mock_context().agent;
    agent.id = "other-agent".into();
    agent.handle = frona::handle!("other");
    SurrealRepo::new(app.db.clone())
        .create(&agent)
        .await
        .unwrap();
    let response = app
        .service
        .create_stream_user_message(
            "test-user",
            "test-chat",
            "/other hello",
            vec![],
            Some(MessageCommand::Command {
                name: "other".into(),
                args: "hello".into(),
            }),
        )
        .await
        .unwrap();
    let input = app
        .messages()
        .find_by_id(&response.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(input.agent_id.as_deref(), Some("other-agent"));
    let input = app.finalize_command_prompt(&input, "hello").await.unwrap();
    assert!(
        serde_json::to_string(&app.history().await.history)
            .unwrap()
            .contains("other-agent")
    );
    let response = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let response = app
        .messages()
        .find_by_id(&response.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.agent_id, input.agent_id);
    assert_eq!(
        response.agent_context_signature,
        input.agent_context_signature
    );
    let next = app.input("default again").await;
    assert_eq!(next.agent_id.as_deref(), Some("test-agent"));
    assert!(
        next.processed_content
            .as_ref()
            .unwrap()
            .contains("test-agent")
    );
}

struct Summary(AtomicUsize);
#[async_trait]
impl frona::chat::compactor::ChatSummarizer for Summary {
    async fn summarize(
        &self,
        _: frona::chat::compactor::SummaryRequest<'_>,
    ) -> Result<String, AppError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("summary".into())
    }
}

#[tokio::test]
async fn compaction_prepends_summary_and_live_context_and_preserves_retained_messages() {
    use frona::chat::compactor::{ChatCompactor, CompactionContext};
    let app = App::new().await;
    let first = app.input("initial").await;
    let start = chrono::Utc::now() - chrono::Duration::minutes(1);
    app.db
        .query("UPDATE message SET created_at = $time")
        .bind(("time", start))
        .await
        .unwrap()
        .check()
        .unwrap();
    for n in 0..12 {
        let mut row = Message::builder(
            "test-chat",
            if n % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Agent
            },
            "long history ".repeat(160),
        )
        .agent_id("test-agent".into())
        .build();
        row.created_at = start + chrono::Duration::seconds(n + 1);
        row.agent_context_signature = first.agent_context_signature.clone();
        app.messages().create(&row).await.unwrap();
    }
    *app.source.text.lock().unwrap() = "B".into();
    let last = app.input("latest request").await;
    let response = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let mut response = app
        .messages()
        .find_by_id(&response.id)
        .await
        .unwrap()
        .unwrap();
    let old_system = "standing".to_owned();
    let history = app.history().await;
    let summary = Arc::new(Summary(AtomicUsize::new(0)));
    let compactor = ChatCompactor::new(
        app.summaries(),
        app.messages(),
        summary.clone(),
        app.fixture.state.prompts.clone(),
        app.service.agent_context_manager().clone(),
    );
    let mut group = helpers::test_model_group();
    group.context_window = 4000;
    group.main.request_settings.max_tokens = Some(512);
    let ctx = helpers::mock_context();
    let context = CompactionContext {
        agent_context: ctx.agent_context_scope(),
        system_prompt: &old_system,
        model_group: &group,
        model_history: &history,
        tools: &[],
    };
    let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
    let previous_state = chats
        .find_by_id("test-chat")
        .await
        .unwrap()
        .unwrap()
        .agent_context_state;
    app.source.fail.store(true, Ordering::SeqCst);
    assert!(compactor.compact_chat("test-chat", context).await.is_err());
    assert_eq!(summary.0.load(Ordering::SeqCst), 0);
    assert!(
        app.summaries()
            .find_by_chat_id("test-chat")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        previous_state
    );
    app.source.fail.store(false, Ordering::SeqCst);
    *app.source.text.lock().unwrap() = "C".into();
    let result = compactor.compact_chat("test-chat", context).await.unwrap();
    assert!(result.compacted);
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state
            .get::<CaptureState>()
            .unwrap()
            .captures,
        previous_state.get::<CaptureState>().unwrap().captures + 1
    );
    assert_eq!(result.model_history.summary_end, 1);
    assert!(
        serde_json::to_string(&result.model_history.history[0])
            .unwrap()
            .contains("<conversation_summary>")
    );
    assert!(
        serde_json::to_string(&result.model_history.history[0])
            .unwrap()
            .contains(" / C")
    );
    // Budget trimming must retain the fresh context with the summary.
    let native = &result.model_history.history;
    let budget = frona::inference::context::estimate_tokens("")
        + frona::inference::context::estimate_message_tokens(&native[0])
        + frona::inference::context::estimate_message_tokens(native.last().unwrap());
    let trimmed =
        frona::inference::context::truncate_history(native.clone(), "", budget, 0, 100).unwrap();
    assert!(trimmed.len() < native.len());
    assert_eq!(trimmed.first(), native.first());
    let saved = app
        .summaries()
        .find_by_chat_id("test-chat")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(saved.agent_context_signature, last.agent_context_signature);
    assert!(saved.updated_at >= response.created_at);
    let loaded = compactor.load_conversation("test-chat").await.unwrap();
    assert_eq!(
        loaded.messages.iter().find(|m| m.id == last.id).unwrap(),
        &last
    );
    assert!(last.inference_content().contains(" / B"));
    assert!(loaded.agent_context.as_ref().unwrap().contains(" / C"));
    assert!(
        loaded
            .agent_context
            .as_ref()
            .unwrap()
            .contains("<temporal_context>")
    );
    assert_eq!(result.model_history.history, app.history().await.history);
    response.content = "reply after compaction".into();
    app.service.complete_agent_message(response).await.unwrap();
    let completed = app.history().await;
    assert_eq!(
        &completed.history[..result.model_history.history.len()],
        result.model_history.history
    );
    assert!(
        serde_json::to_string(completed.history.last().unwrap())
            .unwrap()
            .contains("reply after compaction")
    );
    let next = app.input("after compaction").await;
    assert!(!next.inference_content().contains("<temporal_context>"));
    assert!(next.created_at > saved.updated_at);
    assert_eq!(next.agent_context_signature, saved.agent_context_signature);
    assert!(!next.inference_content().contains("<user_profile>"));
    let after = app.history().await;
    assert_eq!(&after.history[..completed.history.len()], completed.history);

    assert!(
        !app.input("same day after compaction")
            .await
            .inference_content()
            .contains("<temporal_context>")
    );

    // A second compaction replaces the summary/context prefix, never retained text.
    *app.source.text.lock().unwrap() = "D".into();
    for _ in 0..8 {
        app.input(&"more history ".repeat(200)).await;
    }
    app.input("latest again").await;
    let before_rows = app.messages().find_by_chat_id("test-chat").await.unwrap();
    let history = app.history().await;
    *app.source.text.lock().unwrap() = "E".into();
    let second = compactor
        .compact_chat(
            "test-chat",
            CompactionContext {
                agent_context: ctx.agent_context_scope(),
                system_prompt: &old_system,
                model_group: &group,
                model_history: &history,
                tools: &[],
            },
        )
        .await
        .unwrap();
    assert!(second.compacted);
    let replayed = app.history().await;
    assert_eq!(second.model_history.history, replayed.history);
    assert!(
        serde_json::to_string(&replayed.history[0])
            .unwrap()
            .contains(" / E")
    );
    assert_eq!(
        app.messages().find_by_chat_id("test-chat").await.unwrap(),
        before_rows
    );
}

#[tokio::test]
async fn missing_snapshot_is_not_reconstructed_from_live_sources() {
    let app = App::new().await;
    let first = app.input("initial").await;
    let now = chrono::Utc::now();
    app.summaries()
        .create(&ChatSummary {
            agent_context_signature: first.agent_context_signature.clone(),
            id: "summary".into(),
            user_id: "test-user".into(),
            chat_id: "test-chat".into(),
            content: "earlier conversation".into(),
            compacted_until: first.created_at,
            item_count: 1,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    app.db
        .query("DELETE agent_context_snapshot")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        app.service
            .compactor()
            .load_conversation("test-chat")
            .await
            .is_err()
    );
    assert!(
        app.service
            .create_stream_user_message("test-user", "test-chat", "must fail", vec![], None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn production_sources_keep_scope_and_reject_failed_skill_discovery() {
    let app = App::new().await;
    let state = &app.fixture.state;
    let mut ctx = helpers::mock_context();
    // Sources must use the resolved session objects, even if storage differs.
    ctx.user.timezone = Some("Pacific/Auckland".into());
    let scope = ctx.agent_context_scope();
    let snapshot = state
        .chat_service
        .agent_context_manager()
        .current(&scope, &mut AgentContextState::default())
        .await
        .unwrap();
    assert!(
        snapshot
            .render()
            .contains("user_timezone: Pacific/Auckland")
    );
    let config = frona::chat::service::AgentConfig {
        system_prompt: "base".into(),
        model_group: ctx.agent.model_group.clone(),
        skills: ctx.agent.skills.clone(),
        sandbox_limits: ctx.agent.sandbox_limits.clone(),
        identity: ctx.agent.identity.clone(),
    };
    let prompt = state
        .chat_service
        .build_system_prompt(&config, &ctx.user, &ctx.agent, &[])
        .await
        .unwrap();
    let instructions = state.prompts.read("MEMORY.md").unwrap();
    assert!(!instructions.trim().is_empty());
    assert!(prompt.contains(instructions.trim()));
    assert!(!snapshot.render().contains(instructions.trim()));
    assert!(
        state
            .chat_service
            .agent_context_manager()
            .current(
                &AgentContextScope {
                    user: &frona::auth::User {
                        id: "another-user".into(),
                        ..ctx.user.clone()
                    },
                    ..scope
                },
                &mut AgentContextState::default()
            )
            .await
            .is_err()
    );

    // A corrupt source is distinguishable from an absent optional directory.
    // Use an uncached agent scope to exercise the existing resolver/cache path.
    let mut agent = helpers::mock_context().agent;
    agent.id = "broken-agent".into();
    agent.handle = frona::handle!("broken");
    SurrealRepo::new(app.db.clone())
        .create(&agent)
        .await
        .unwrap();
    let user = helpers::mock_context().user;
    let workspace = state
        .storage_service
        .agent_workspace(&user.handle, &agent.handle);
    workspace
        .write("skills/broken/SKILL.md", "valid before corruption")
        .unwrap();
    let skill_path = workspace.resolve_path("skills/broken/SKILL.md").unwrap();
    std::fs::write(skill_path, [0xff, 0xfe]).unwrap();
    assert!(
        state
            .chat_service
            .agent_context_manager()
            .current(
                &AgentContextScope {
                    agent: &agent,
                    ..scope
                },
                &mut AgentContextState::default()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_captures_can_repeat_updates_and_replay_exactly() {
    struct OverlappingSource(tokio::sync::Barrier);
    #[async_trait]
    impl AgentContextSource for OverlappingSource {
        async fn collect(
            &self,
            _: &AgentContextScope<'_>,
            _: &mut AgentContextState,
        ) -> Result<Vec<AgentContextSection>, AppError> {
            // Both requests must capture before either can persist its update.
            self.0.wait().await;
            Ok(vec![AgentContextSection::prose(
                AgentContextKind::UserProfile,
                "user_profile",
                "concurrent update".into(),
            )])
        }
    }
    let app = App::new().await;
    let baseline = app.input("initial").await;
    let source = Arc::new(OverlappingSource(tokio::sync::Barrier::new(2)));
    let manager = AgentContextManager::new(
        vec![source],
        SurrealAgentContextSnapshotRepo(app.db.clone()),
    );
    let service = helpers::chat_service_with_context(&app.fixture.state, manager);
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            service.create_stream_user_message("test-user", "test-chat", "A", vec![], None),
            service.create_stream_user_message("test-user", "test-chat", "B", vec![], None),
        )
    })
    .await
    .expect("captures must overlap without a per-chat lock");
    a.unwrap();
    b.unwrap();
    let rows = app.messages().find_by_chat_id("test-chat").await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], baseline);
    assert!((rows[1].created_at, &rows[1].id) < (rows[2].created_at, &rows[2].id));
    for row in &rows[1..] {
        assert!(row.inference_content().contains("concurrent update"));
        assert_ne!(
            row.agent_context_signature,
            baseline.agent_context_signature
        );
    }
    assert_eq!(
        rows[1].agent_context_signature,
        rows[2].agent_context_signature
    );
    let replay = app.history().await;
    assert_eq!(
        replay.row_ids,
        rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>()
    );
    for row in &rows {
        assert!(
            serde_json::to_string(&replay.history)
                .unwrap()
                .contains(&serde_json::to_string(row.inference_content()).unwrap())
        );
    }
    assert_eq!(replay.history, app.history().await.history);
    assert_eq!(
        rows,
        app.messages().find_by_chat_id("test-chat").await.unwrap()
    );
}

#[tokio::test]
async fn context_comparison_does_not_decode_message_bodies() {
    let app = App::new().await;
    let first = app.input("initial").await;
    // Deliberately invalid content fails full Message decoding. Context metadata
    // remains valid; comparing signatures must not touch that payload.
    app.db
        .query("UPDATE type::record('message', $id) SET content = 42")
        .bind(("id", first.id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(app.messages().find_by_id(&first.id).await.is_err());
    *app.source.text.lock().unwrap() = "B".into();
    let next = app.input("next").await;
    assert!(next.processed_content.as_ref().unwrap().contains(" / B"));
    let position = app.messages().context_position("test-chat").await.unwrap();
    assert_eq!(position.last.unwrap().id, next.id);
    assert_eq!(
        position.latest_entry.unwrap().agent_context_signature,
        next.agent_context_signature
    );
}

#[tokio::test]
async fn static_memory_guidance_is_independent_of_snapshot_signature() {
    use frona::memory::{basic::BasicMemoryService, service::MemoryService};
    let app = App::new().await;
    let state = &app.fixture.state;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("MEMORY.md");
    std::fs::write(&file, "First usage instructions").unwrap();
    let memory = BasicMemoryService::new(
        SurrealRepo::new(app.db.clone()),
        SurrealRepo::new(app.db.clone()),
        SurrealRepo::new(app.db.clone()),
        SurrealRepo::new(app.db.clone()),
        Arc::new(state.chat_service.model_providers().clone()),
        frona::agent::prompt::PromptLoader::new(dir.path()),
        state.usage_service.clone(),
        frona::core::config::MemoryConfig::default(),
    );
    let ctx = helpers::mock_context();
    let before =
        AgentContextSnapshot::new(memory.collect(&ctx.agent_context_scope()).await.unwrap())
            .unwrap();
    assert_eq!(
        memory.instructions(&ctx.user).await.unwrap(),
        "First usage instructions"
    );
    std::fs::write(&file, "Revised usage instructions").unwrap();
    let after =
        AgentContextSnapshot::new(memory.collect(&ctx.agent_context_scope()).await.unwrap())
            .unwrap();
    assert_eq!(before.signature, after.signature);
    assert_eq!(
        memory.instructions(&ctx.user).await.unwrap(),
        "Revised usage instructions"
    );
    assert!(!after.render().contains("usage instructions"));
    std::fs::write(&file, [0xff]).unwrap();
    assert!(memory.instructions(&ctx.user).await.is_err());
}

#[tokio::test]
async fn autonomous_context_is_saved_before_reply_and_remains_unchanged() {
    let app = App::new().await;
    let response = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let rows = app.messages().find_by_chat_id("test-chat").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].role, MessageRole::System);
    assert!(rows[0].content.is_empty());
    assert!(rows[0].inference_content().contains("<temporal_context>"));
    assert_eq!(
        rows[0].agent_context_signature,
        rows[1].agent_context_signature
    );
    let before = app.history().await.history;
    let mut reply = app
        .messages()
        .find_by_id(&response.id)
        .await
        .unwrap()
        .unwrap();
    reply.content = "autonomous response".into();
    app.service.complete_agent_message(reply).await.unwrap();
    let after = app.history().await.history;
    assert_eq!(&after[..before.len()], before);
    assert!(
        serde_json::to_string(after.last().unwrap())
            .unwrap()
            .contains("autonomous response")
    );
    assert_eq!(
        app.messages()
            .find_by_id(&rows[0].id)
            .await
            .unwrap()
            .unwrap(),
        rows[0]
    );
    let visible = app
        .service
        .list_messages("test-user", "test-chat")
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].content, "autonomous response");
    let page = app
        .service
        .list_messages_paginated("test-user", "test-chat", None, None, 1)
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert!(!page.has_more);
    // An unchanged autonomous turn records its snapshot without injecting text.
    app.service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let position = app.messages().context_position("test-chat").await.unwrap();
    let context = app
        .messages()
        .find_by_id(&position.latest_entry.unwrap().id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.role, MessageRole::System);
    assert_eq!(context.processed_content.as_deref(), Some(""));
    assert_eq!(app.history().await.history, after);
}

#[tokio::test]
async fn command_content_is_absent_until_finalized_and_reused_on_resume() {
    let app = App::new().await;
    let result = app
        .service
        .create_stream_user_message(
            "test-user",
            "test-chat",
            "/custom args",
            vec![],
            Some(MessageCommand::Command {
                name: "custom".into(),
                args: "args".into(),
            }),
        )
        .await
        .unwrap();
    let message = app
        .messages()
        .find_by_id(&result.id)
        .await
        .unwrap()
        .unwrap();
    assert!(message.processed_content.is_none());
    assert_eq!(message.inference_content(), "/custom args");
    // Context is collected once when the complete prompt is ready.
    assert!(message.agent_context_signature.is_none());
    assert_eq!(app.source.calls.load(Ordering::SeqCst), 0);
    let saved = app
        .finalize_command_prompt(&message, "expanded command")
        .await
        .unwrap();
    assert_eq!(saved.content, "/custom args");
    assert!(saved.inference_content().contains(" / A"));
    assert!(saved.inference_content().contains("<temporal_context>"));
    assert!(saved.inference_content().ends_with("expanded command"));
    assert!(!saved.inference_content().contains("/custom args"));
    app.source.fail.store(true, Ordering::SeqCst);
    let unchanged = app
        .finalize_command_prompt(&saved, "different expansion")
        .await
        .unwrap();
    assert_eq!(unchanged, saved);
    app.source.fail.store(false, Ordering::SeqCst);
    assert!(
        !app.input("next turn")
            .await
            .inference_content()
            .contains("<temporal_context>")
    );
}

#[tokio::test]
async fn deferred_command_uses_only_preceding_context_and_does_not_advance_comparison() {
    for has_baseline in [false, true] {
        let app = App::new().await;
        let baseline = if has_baseline {
            Some(app.input("baseline").await)
        } else {
            None
        };
        *app.source.text.lock().unwrap() = "B".into();
        let input = app
            .service
            .create_stream_user_message(
                "test-user",
                "test-chat",
                "/custom args",
                vec![],
                Some(MessageCommand::Command {
                    name: "custom".into(),
                    args: "args".into(),
                }),
            )
            .await
            .unwrap();
        let pending = app.messages().find_by_id(&input.id).await.unwrap().unwrap();
        let later = app.input("later input").await;
        // The pending command has not delivered B; a later ordinary input still must.
        assert!(later.inference_content().contains(" / B"));
        let saved = app
            .finalize_command_prompt(&pending, "expanded")
            .await
            .unwrap();
        assert!(saved.inference_content().contains(" / B"));
        assert!(!saved.inference_content().contains(" / A"));
        if let Some(baseline) = baseline {
            assert_eq!(
                app.messages()
                    .find_by_id(&baseline.id)
                    .await
                    .unwrap()
                    .unwrap(),
                baseline
            );
        }
        assert_eq!(
            app.messages().find_by_id(&later.id).await.unwrap().unwrap(),
            later
        );
        assert_eq!(app.history().await.history, app.history().await.history);
    }
}

#[test]
fn processed_content_falls_back_only_when_absent_or_empty() {
    let mut message = Message::builder("chat", MessageRole::User, "original".into()).build();
    assert_eq!(message.inference_content(), "original");
    message.processed_content = Some(String::new());
    assert_eq!(message.inference_content(), "original");
    message.processed_content = Some("frozen".into());
    assert_eq!(message.inference_content(), "frozen");
    assert_eq!(message.content, "original");
}

#[tokio::test]
async fn old_summary_covering_all_rows_stays_unchanged_when_new_turn_adds_context() {
    let app = App::new().await;
    let old = Message::builder("test-chat", MessageRole::User, "old text".into()).build();
    app.messages().create(&old).await.unwrap();
    let now = chrono::Utc::now();
    app.summaries()
        .create(&ChatSummary {
            agent_context_signature: None,
            id: "summary".into(),
            user_id: "test-user".into(),
            chat_id: "test-chat".into(),
            content: "everything so far".into(),
            compacted_until: old.created_at,
            item_count: 1,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let prefix = app.history().await.history;
    assert_eq!(prefix.len(), 1);
    assert!(!serde_json::to_string(&prefix[0]).unwrap().contains(" / A"));
    let summary = app.summaries().find_by_chat_id("test-chat").await.unwrap();
    let next = app.input("next request").await;
    assert!(next.inference_content().contains(" / A"));
    assert_eq!(&app.history().await.history[..prefix.len()], prefix);
    assert_eq!(
        app.summaries().find_by_chat_id("test-chat").await.unwrap(),
        summary
    );
    assert_eq!(
        app.messages().find_by_id(&old.id).await.unwrap().unwrap(),
        old
    );
}

#[tokio::test]
async fn failed_autonomous_message_write_cannot_leave_a_context_row() {
    let app = App::new().await;
    let response = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let before = app.messages().find_by_chat_id("test-chat").await.unwrap();
    let mut duplicate = Message::builder("test-chat", MessageRole::Agent, String::new())
        .agent_id("test-agent".into())
        .status(frona::chat::message::models::MessageStatus::Executing)
        .build();
    duplicate.id = response.id;
    *app.source.text.lock().unwrap() = "B".into();
    assert!(
        app.service
            .persist_inbound_message(&duplicate)
            .await
            .is_err()
    );
    assert_eq!(
        app.messages().find_by_chat_id("test-chat").await.unwrap(),
        before
    );
}

#[tokio::test]
async fn agent_context_state_is_per_chat_persisted_and_independent_of_snapshots() {
    let app = App::new().await;
    let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
    // Older records have no agent_context_state field.
    app.db
        .query("UPDATE chat UNSET agent_context_state")
        .await
        .unwrap()
        .check()
        .unwrap();
    let chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    assert_eq!(
        chat.agent_context_state
            .get::<CaptureState>()
            .unwrap()
            .captures,
        0
    );
    let first = app.input("first").await;
    let second = app.input("second").await;
    assert_eq!(
        first.agent_context_signature,
        second.agent_context_signature
    );
    let chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    assert_eq!(
        chat.agent_context_state
            .get::<CaptureState>()
            .unwrap()
            .captures,
        2
    );

    // Reconstructed services receive the saved state, not process-local counters.
    let restarted = helpers::chat_service_with_context(
        &app.fixture.state,
        AgentContextManager::new(
            vec![
                app.source.clone(),
                Arc::new(TemporalContextSource {
                    default_timezone: app.fixture.state.config.server.timezone.clone(),
                }),
            ],
            SurrealAgentContextSnapshotRepo(app.db.clone()),
        ),
    );
    let resumed = restarted
        .create_stream_user_message("test-user", "test-chat", "restart", vec![], None)
        .await
        .unwrap();
    assert!(
        !app.messages()
            .find_by_id(&resumed.id)
            .await
            .unwrap()
            .unwrap()
            .inference_content()
            .contains("<temporal_context>")
    );
    let chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    assert_eq!(
        chat.agent_context_state
            .get::<CaptureState>()
            .unwrap()
            .captures,
        3
    );
    let rows: Vec<surrealdb::types::Value> = app
        .db
        .query("SELECT * FROM agent_context_snapshot")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows.len(), 1, "state changes must not create snapshots");

    let mut other = helpers::mock_context().chat.unwrap();
    other.id = "other-chat".into();
    chats.create(&other).await.unwrap();
    restarted
        .create_stream_user_message("test-user", "other-chat", "independent", vec![], None)
        .await
        .unwrap();
    assert_eq!(
        chats
            .find_by_id("other-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state
            .get::<CaptureState>()
            .unwrap()
            .captures,
        1
    );

    app.source.fail.store(true, Ordering::SeqCst);
    assert!(
        restarted
            .create_stream_user_message("test-user", "test-chat", "fails", vec![], None)
            .await
            .is_err()
    );
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        chat.agent_context_state
    );
    app.source.fail.store(false, Ordering::SeqCst);
    // A successful collection followed by a failed message write must not advance state.
    let mut duplicate =
        Message::builder("test-chat", MessageRole::User, "duplicate".into()).build();
    duplicate.id = first.id.clone();
    assert!(restarted.persist_inbound_message(&duplicate).await.is_err());
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        chat.agent_context_state
    );
    let before = app.history().await;
    assert_eq!(before.history, app.history().await.history);
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        chat.agent_context_state
    );

    restarted
        .delete_messages_for_chat("test-user", "test-chat")
        .await
        .unwrap();
    assert_eq!(
        chats
            .find_by_id("test-chat")
            .await
            .unwrap()
            .unwrap()
            .agent_context_state,
        AgentContextState::default()
    );
    assert!(
        app.input("after clear")
            .await
            .inference_content()
            .contains("<temporal_context>")
    );
}

#[tokio::test]
async fn nonstream_source_failure_does_not_persist_input_or_advance_state() {
    let app = App::new().await;
    app.source.fail.store(true, Ordering::SeqCst);
    let before = app.service.find_chat("test-chat").await.unwrap().unwrap();
    let error = app
        .service
        .send_message(
            "test-user",
            "test-chat",
            frona::chat::message::models::SendMessageRequest {
                content: "hello".into(),
                attachments: vec![],
                metadata: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Internal(text) if text == "source offline"));
    assert_eq!(app.source.calls.load(Ordering::SeqCst), 1);
    assert!(
        app.messages()
            .find_by_chat_id("test-chat")
            .await
            .unwrap()
            .is_empty()
    );
    let after = app.service.find_chat("test-chat").await.unwrap().unwrap();
    assert_eq!(after.agent_context_state, before.agent_context_state);
}

#[tokio::test]
async fn reply_placeholder_does_not_resolve_objects_when_it_inherits_context() {
    let app = App::new().await;
    let input = app.input("hello").await;
    let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
    let mut chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
    chat.task_id = Some("missing-task".into());
    chats.update(&chat).await.unwrap();

    let reply = app
        .service
        .create_executing_agent_message("test-chat", "test-agent")
        .await
        .unwrap();
    let saved = app.messages().find_by_id(&reply.id).await.unwrap().unwrap();
    assert_eq!(saved.agent_context_signature, input.agent_context_signature);
    assert_eq!(app.source.calls.load(Ordering::SeqCst), 1);
    let after = chats.find_by_id("test-chat").await.unwrap().unwrap();
    assert_eq!(after.agent_context_state, chat.agent_context_state);
}

#[tokio::test]
async fn task_messages_use_the_supplied_task_without_looking_it_up() {
    for (role, status) in [
        (MessageRole::System, None),
        (MessageRole::Agent, None),
        (
            MessageRole::Agent,
            Some(frona::chat::message::models::MessageStatus::Executing),
        ),
    ] {
        let app = App::new().await;
        let users = SurrealRepo::<frona::auth::User>::new(app.db.clone());
        let mut user = users.find_by_id("test-user").await.unwrap().unwrap();
        user.timezone = Some("UTC".into());
        users.update(&user).await.unwrap();
        // The executor has this object; there is deliberately no Task row to reload.
        let task: frona::agent::task::models::Task = serde_json::from_value(serde_json::json!({
            "id": "supplied-task", "user_id": "test-user", "agent_id": "test-agent",
            "title": "Task", "description": "Supplied task", "status": "inprogress",
            "run_at": "2026-01-02T03:30:00Z",
            "created_at": "2026-01-02T03:00:00Z", "updated_at": "2026-01-02T03:00:00Z"
        }))
        .unwrap();
        let chats = frona::db::repo::chats::SurrealChatRepo::new(app.db.clone());
        let mut chat = chats.find_by_id("test-chat").await.unwrap().unwrap();
        chat.task_id = Some(task.id.clone());
        chats.update(&chat).await.unwrap();
        let mut message = Message::builder("test-chat", role, "task instruction".into()).build();
        message.status = status;
        app.service.save_task_message(&task, message).await.unwrap();

        let rows = app.messages().find_by_chat_id("test-chat").await.unwrap();
        assert!(rows.iter().any(|row| {
            let text = row.inference_content();
            text.contains("<task_time>")
                && text.contains("- created_at: 2026-01-02 03:00:00 UTC")
                && text.contains("- scheduled_at: 2026-01-02 03:30:00 UTC")
        }));
        assert_eq!(app.source.calls.load(Ordering::SeqCst), 1);
        // An ID-only caller still reports the genuinely missing task.
        let error = app
            .service
            .create_stream_user_message("test-user", "test-chat", "next", vec![], None)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::NotFound(name) if name == "Task"));
    }
}
