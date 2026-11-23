//! Verifies the architectural rule that `send_message` is only available in
//! the agent's heartbeat chat. Inside task chats and normal user chats the
//! tool is filtered out at `ChatSessionContext::build` so the model can't
//! satisfy a "send a reminder" instruction via `send_message` and then leave
//! `complete_task.result` empty against a non-nullable schema.

mod helpers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use frona::agent::models::Agent;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::auth::User;
use frona::chat::models::CreateChatRequest;
use frona::chat::service::ChatService;
use frona::chat::session::{CancellationToken, ChatSessionContext};
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db_init;
use frona::db::repo::agents::SurrealAgentRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::conversation::DefaultConversationBuilder;
use frona::storage::StorageService;
use helpers::{MockModelProvider, test_model_group};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

fn workspace_resources() -> PathBuf {
    // Walk up from the test binary's CWD until we find a sibling `resources/prompts`.
    // Mirrors `tool/task_control.rs::tests::tool_with_schema`.
    std::env::current_dir()
        .expect("cwd")
        .ancestors()
        .find(|p| p.join("resources/prompts").exists())
        .expect("workspace resources/ not found from cwd")
        .join("resources")
}

fn test_config(tmp: &tempfile::TempDir) -> Config {
    let base = tmp.path().to_string_lossy().to_string();
    let resources = workspace_resources();
    Config {
        server: frona::core::config::ServerConfig {
            port: 0,
            static_dir: format!("{base}/static"),
            max_concurrent_tasks: 10,
            ..Default::default()
        },
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        database: frona::core::config::DatabaseConfig {
            path: format!("{base}/db"),
        },
        storage: frona::core::config::StorageConfig {
            data_dir: format!("{base}/data"),
            shared_config_dir: resources.to_string_lossy().into_owned(),
            skills_dir: format!("{base}/skills"),
            cache_dir: format!("{base}/cache"),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn build_state() -> (AppState, tempfile::TempDir) {
    build_state_with_provider(Arc::new(MockModelProvider::new(vec![])), false).await
}

async fn build_state_with_provider(
    provider: Arc<MockModelProvider>,
    disable_sandbox: bool,
) -> (AppState, tempfile::TempDir) {
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    db_init::setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut config = test_config(&tmp);
    config.sandbox.disabled = disable_sandbox;
    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::resource_monitor::SystemResourceManager::new(80.0, 80.0, 90.0, 90.0),
    );
    let metrics_handle = frona::core::metrics::setup_metrics_recorder();

    let mut state = AppState::new(
        db.clone(),
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );

    // Replace the default chat_service with one wired to a mock provider so
    // model-group resolution succeeds. We don't run inference here - the tests
    // just inspect the tool registry the session builds - but `session.build`
    // resolves the agent's `model_group` against this registry.
    let provider: Arc<dyn frona_harness::inference::provider::ModelProvider> = provider;
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    let mut groups = HashMap::new();
    groups.insert("primary".to_string(), test_model_group());
    let mock_registry = crate::helpers::test_model_service(providers, groups).await;

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
        mock_registry,
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
        helpers::empty_agent_context_manager(&state.db),
        agent_prompt_builder,
    );
    state.chat_service = chat_service.clone();
    // Rebuild Harness so it sees the new chat_service with the mock registry.
    state.harness = Arc::new(frona::agent::harness::Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        helpers::test_memory_service(&state, &db),
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
    ));
    state.task_executor = Arc::new(frona::agent::task::executor::TaskExecutor::new(
        state.harness.clone(),
    ));
    state.policy_service.sync_base_policies().await.unwrap();
    (state, tmp)
}

async fn seed_user_and_agent(state: &AppState) {
    let now = Utc::now();
    let _ = state
        .user_service
        .create(&User {
            id: "user-1".into(),
            handle: frona::handle!("user-1"),
            email: "user-1@test.com".into(),
            name: "Test User".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await;

    let repo = SurrealAgentRepo::new(state.db.clone());
    let _ = repo
        .create(&Agent {
            id: "agent-1".into(),
            user_id: "user-1".into(),
            handle: frona::handle!("agent-1"),
            name: "Test Agent".into(),
            description: String::new(),
            model_group: "primary".into(),
            enabled: true,
            skills: None,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: Default::default(),
            prompt: Some("You are a test agent.".into()),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        })
        .await;
}

async fn build_session_for_chat(state: &AppState, chat_id: &str) -> ChatSessionContext {
    let chat = state
        .chat_service
        .find_chat(chat_id)
        .await
        .unwrap()
        .expect("chat exists");
    let builder = Arc::new(DefaultConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
        agent_service: state.agent_service.clone(),
    });
    ChatSessionContext::build(
        &state.harness,
        "user-1",
        chat,
        None,
        CancellationToken::new(),
        builder,
        &[],
        Default::default(),
    )
    .await
    .expect("session builds")
}

fn registry_has_tool(session: &ChatSessionContext, tool_id: &str) -> bool {
    session
        .tool_registry
        .definitions()
        .iter()
        .any(|d| d.id == tool_id)
}

/// Heartbeat path: the agent's `heartbeat_chat_id` points at the current chat,
/// so `send_message` IS registered - this is the one context where the agent
/// has no other channel to reach the user.
#[tokio::test]
async fn send_message_registered_in_heartbeat_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Heartbeat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    state
        .agent_service
        .update_heartbeat_chat("agent-1", &chat.id)
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        registry_has_tool(&session, "send_message"),
        "send_message must be available in the heartbeat chat (the only context where autonomous outreach has no other channel)"
    );
}

/// Task-execution path: the chat has `task_id` set, so `send_message` is
/// filtered out. The agent must deliver via `complete_task.result`. This is
/// the regression guard for the trace bug.
#[tokio::test]
async fn send_message_filtered_in_task_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    // Persist a Task so the chat row's `task_id` is referentially honest.
    let task_repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    let now = Utc::now();
    let task = Task {
        id: frona::core::repository::new_id(),
        user_id: "user-1".into(),
        agent_id: "agent-1".into(),
        space_id: None,
        chat_id: None,
        title: "Send reminder".into(),
        description: "Send a friendly reminder to drink water.".into(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Direct {
            source_chat_id: None,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: Some(serde_json::json!({"type": "string"})),
        result_description: None,
        created_at: now,
        updated_at: now,
    };
    task_repo.create(&task).await.unwrap();

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: Some(task.id.clone()),
                agent_id: "agent-1".to_string(),
                title: Some("Task chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must be hidden inside a task chat — `complete_task.result` is the delivery channel"
    );
}

/// Normal user chat (no task, no heartbeat): `send_message` is filtered out
/// under the Strict rule. The agent already replies by streaming text into
/// the current chat; `send_message` would be redundant.
#[tokio::test]
async fn send_message_filtered_in_normal_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Plain chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must be hidden in a normal user chat under the Strict rule"
    );
}

/// Sanity check the heartbeat allowance is keyed on identity, not presence -
/// a second chat owned by the same agent (not its `heartbeat_chat_id`) should
/// still be filtered. Prevents accidentally widening the rule to "agent has
/// heartbeat configured anywhere".
#[tokio::test]
async fn send_message_filtered_in_non_heartbeat_chat_of_heartbeat_agent() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let hb_chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Heartbeat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();
    state
        .agent_service
        .update_heartbeat_chat("agent-1", &hb_chat.id)
        .await
        .unwrap();

    let other_chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Other".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &other_chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must remain hidden in non-heartbeat chats even when the agent has a heartbeat elsewhere"
    );
}

#[tokio::test]
async fn portable_send_message_resolver_uses_shared_model_budget_before_sending() {
    use frona::tool::InferenceContext;
    use frona_harness::{
        BudgetLimits, Conversation, DefaultBudget, ExecutionControl, ExecutionStop, ModelOutput,
        ToolDispatchError, UsageReport,
    };
    use rig_core::completion::{
        AssistantContent,
        message::{ToolCall, ToolFunction},
    };
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;
    // A real candidate forces the routing-model branch (no candidate would
    // simply create a chat). The provider has no responses and must not be called.
    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                agent_id: "agent-1".into(),
                space_id: None,
                task_id: None,
                title: Some("Candidate".into()),
                metadata: None,
            },
        )
        .await
        .unwrap();
    let user = state
        .user_service
        .find_by_id("user-1")
        .await
        .unwrap()
        .unwrap();
    let agent = state
        .agent_service
        .find_by_id("agent-1")
        .await
        .unwrap()
        .unwrap();
    let ctx = InferenceContext::new_detached(
        user,
        agent,
        CancellationToken::new(),
        CancellationToken::new(),
    );
    let tool = frona::tool::send_message::SendMessageTool::new(
        state.chat_service.clone(),
        state.notification_service.clone(),
        state.broadcast_service.clone(),
        state.agent_service.clone(),
        state.task_service.clone(),
        state.prompts.clone(),
    );
    state
        .tool_manager
        .register_user_tool(&ctx.user.id, Arc::new(tool))
        .await;
    state.policy_service.create_policy(&ctx.user.id, "@id(\"allow-nested\") permit(principal, action == Policy::Action::\"invoke_tool\", resource);").await.unwrap();
    let legacy = state
        .tool_manager
        .build_agent_registry(&ctx.user.id, &ctx.agent, &state.policy_service, None)
        .await;
    let mut tools = legacy
        .portable(&ctx, |output| {
            Ok(frona_harness::ToolOutput::text(output.text_content()))
        })
        .unwrap();
    let mut conversation = Conversation::default();
    let actions = conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "send",
                    ToolFunction::new(
                        "send_message".try_into().unwrap(),
                        serde_json::json!({"content":"Do not send this"}),
                    ),
                ))],
                UsageReport::Unavailable,
            ),
            tools.definitions().cloned().collect(),
        )
        .await
        .unwrap();
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    let error = conversation
        .execute_tool(
            &actions.actions[0],
            &mut tools,
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_))
        ),
        "{error:?}"
    );
    assert!(
        state
            .chat_service
            .list_messages("user-1", &chat.id)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn create_test_chat(state: &AppState) -> String {
    state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                agent_id: "agent-1".into(),
                space_id: None,
                task_id: None,
                title: Some("Isolated run".into()),
                metadata: None,
            },
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn interrupted_chat_run_is_excluded_from_turn_resume() {
    use frona::chat::message::models::MessageStatus;
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;
    let chat_id = create_test_chat(&state).await;
    let run = state
        .harness
        .prepare_for_chat("user-1", &chat_id)
        .await
        .unwrap()
        .run("later")
        .start()
        .await
        .unwrap();
    drop(run);
    assert!(
        state
            .chat_service
            .find_executing_chat_messages()
            .await
            .is_empty()
    );
    state
        .chat_service
        .interrupt_application_runs()
        .await
        .unwrap();
    let messages = state
        .chat_service
        .get_stored_messages(&chat_id)
        .await
        .unwrap();
    let response = messages
        .iter()
        .find(|m| m.metadata.get("execution_owner").is_some())
        .unwrap();
    assert_eq!(response.status, Some(MessageStatus::Failed));
    assert!(
        response
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("interrupted")
    );
}

async fn ordinary_run(
    state: &AppState,
    chat_id: &str,
    message_id: &str,
) -> Result<frona::inference::tool_loop::ToolLoopOutcome, frona::core::error::AppError> {
    let session = build_session_for_chat(state, chat_id).await;
    frona::inference::tool_loop::run_tool_loop(
        &session.model_group,
        &session.system_prompt,
        session.rig_history,
        &session.tool_registry,
        session.tool_ctx.event_tx.clone(),
        session.cancel_token,
        &session.tool_ctx,
        &state.usage_service,
        &state.chat_service,
        message_id,
        None,
    )
    .await
}

#[tokio::test]
async fn ordinary_chat_history_writer_preserves_hitl_answer_across_resume() {
    use frona::chat::message::models::MessageStatus;
    use frona::inference::{
        hitl::HitlResponse,
        tool_call::ToolStatus,
        tool_loop::{PauseReason, ToolLoopOutcome},
    };
    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::ToolCalls(vec![(
            "question-call".into(),
            "ask_user_question".into(),
            serde_json::json!({"question":"Region?","options":["eu","us"]}),
        )]),
        helpers::MockResponse::Text("Using eu".into()),
    ]));
    let (state, _tmp) = build_state_with_provider(provider.clone(), false).await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    state
        .chat_service
        .save_system_message("user-1", None, &chat, "Choose a region".into())
        .await
        .unwrap();
    let message = state
        .chat_service
        .create_executing_agent_message(&chat, "agent-1")
        .await
        .unwrap();
    let outcome = ordinary_run(&state, &chat, &message.id).await.unwrap();
    let ToolLoopOutcome::ExternalToolPending { tool_calls, .. } = outcome else {
        panic!("expected pending question")
    };
    assert_eq!(tool_calls.len(), 1);
    let id = tool_calls[0].id.clone();
    assert_eq!(
        tool_calls[0].hitl.as_ref().unwrap().status,
        ToolStatus::Pending
    );
    let before = state
        .chat_service
        .get_message("user-1", &message.id)
        .await
        .unwrap();
    assert_eq!(before.provider_turns.len(), 1);
    assert!(
        serde_json::to_string(&before.provider_turns)
            .unwrap()
            .contains("Region?")
    );
    state
        .chat_service
        .pause_agent_message(before, PauseReason::Hitl, tool_calls)
        .await
        .unwrap();
    state
        .harness
        .resolve_and_resume(&id, HitlResponse::Choice("eu".into()))
        .await
        .unwrap();
    let answered = state
        .chat_service
        .get_tool_call(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answered.hitl.as_ref().unwrap().status, ToolStatus::Resolved);
    assert!(answered.result.contains("eu"));
    assert_eq!(
        state
            .chat_service
            .get_message("user-1", &message.id)
            .await
            .unwrap()
            .status,
        Some(MessageStatus::Executing)
    );
    let outcome = ordinary_run(&state, &chat, &message.id).await.unwrap();
    let ToolLoopOutcome::Completed { text, .. } = outcome else {
        panic!("expected completed resume")
    };
    assert_eq!(text, "Using eu");
    assert!(format!("{:?}", provider.last_history()).contains("eu"));
    assert_eq!(
        state
            .chat_service
            .get_tool_calls_by_message(&message.id)
            .await
            .unwrap()
            .len(),
        1,
        "resume must not re-execute the question"
    );
    // Finalize a snapshot captured before inference, as the ordinary harness does.
    let mut stale = state
        .chat_service
        .get_message("user-1", &message.id)
        .await
        .unwrap();
    stale.provider_turns.clear();
    stale.content = text;
    let public = state
        .chat_service
        .complete_agent_message(stale)
        .await
        .unwrap();
    assert!(!public.metadata.contains_key("harness_history"));
    let saved = state
        .chat_service
        .get_message("user-1", &message.id)
        .await
        .unwrap();
    assert_eq!(saved.provider_turns.len(), 2);
    assert!(!saved.metadata.contains_key("harness_history"));
}

#[tokio::test]
async fn ordinary_chat_provider_turn_write_failure_prevents_tool_dispatch() {
    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::ToolCalls(vec![(
            "question".into(),
            "ask_user_question".into(),
            serde_json::json!({"question":"Region?", "options":["eu","us"]}),
        )]),
    ]));
    let (state, _tmp) = build_state_with_provider(provider.clone(), false).await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    let message = state
        .chat_service
        .create_executing_agent_message(&chat, "agent-1")
        .await
        .unwrap();
    // Build first: preparation is independently persisted by the existing chat loader.
    let session = build_session_for_chat(&state, &chat).await;
    state.db.query("DEFINE EVENT refuse_history ON TABLE message WHEN $event = 'UPDATE' AND $after.provider_turns != $before.provider_turns THEN { THROW 'history unavailable'; };").await.unwrap().check().unwrap();
    let result = frona::inference::tool_loop::run_tool_loop(
        &session.model_group,
        &session.system_prompt,
        session.rig_history,
        &session.tool_registry,
        session.tool_ctx.event_tx.clone(),
        session.cancel_token,
        &session.tool_ctx,
        &state.usage_service,
        &state.chat_service,
        &message.id,
        None,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(provider.histories().len(), 1);
    assert!(
        state
            .chat_service
            .get_tool_calls_by_message(&message.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn caller_owned_run_records_hitl_before_stopping_without_continuation() {
    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::ToolCalls(vec![(
            "question-call".into(),
            "ask_user_question".into(),
            serde_json::json!({"question":"Region?","options":["eu","us"]}),
        )]),
    ]));
    let (state, _tmp) = build_state_with_provider(provider.clone(), false).await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = hook_calls.clone();
    let result = state
        .harness
        .prepare_for_chat_with_options(
            "user-1",
            &chat,
            frona::agent::harness::ChatRunOptions {
                tools: &["ask_user_question"],
                kind: frona::core::execution::ExecutionKind::Inference,
                action: "Ask",
                related_chat_ids: &[],
            },
        )
        .await
        .unwrap()
        .run("ask")
        .start()
        .await
        .unwrap()
        .before_tool(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Ok::<_, ()>(frona_harness::ToolDecision::Execute) }
        })
        .await;
    assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        result.unwrap_err().reason,
        frona_harness::ManualError::Stop(frona_harness::ExecutionStop::Stopped(_))
    ));
    assert_eq!(provider.histories().len(), 1);
    let rows = state.chat_service.get_tool_calls(&chat).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].hitl.as_ref().unwrap().status,
        frona::inference::tool_call::ToolStatus::Pending
    );
    let message = state
        .chat_service
        .get_message("user-1", &rows[0].message_id)
        .await
        .unwrap();
    assert_eq!(
        message.status,
        Some(frona::chat::message::models::MessageStatus::Failed)
    );
    assert_eq!(message.provider_turns.len(), 1);
    assert!(!message.metadata.contains_key("harness_history"));
}

#[tokio::test]
async fn chat_run_text_matches_sdk_and_rejects_unsupported_checkpoint() {
    use frona::chat::message::models::{MessageRole, MessageStatus};
    use rig_core::completion::AssistantContent;

    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::Content(vec![
            AssistantContent::text("first "),
            AssistantContent::text("second"),
        ]),
    ]));
    let (state, _tmp) = build_state_with_provider(provider, false).await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    let mut session = state
        .harness
        .prepare_for_chat("user-1", &chat)
        .await
        .unwrap()
        .run("answer")
        .start()
        .await
        .unwrap()
        .manual();
    session.call_model().await.unwrap();
    let answer = session.finish().await.unwrap();
    assert_eq!(answer, "first second");
    let error = session.save_checkpoint("cursor", 1, &42).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support application checkpoints"),
        "{error}"
    );
    assert!(session.conversation().checkpoint().is_none());
    let message = state
        .chat_service
        .get_stored_messages(&chat)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.role == MessageRole::Agent)
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Completed));
    assert_eq!(message.content, answer);
}

#[tokio::test]
async fn structured_chat_run_preserves_description_field() {
    use frona::chat::message::models::MessageRole;

    #[derive(schemars::JsonSchema, serde::Deserialize)]
    struct Answer {
        description: String,
    }

    let arguments = serde_json::json!({"description":"fixed"});
    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::ToolCalls(vec![(
            "submission".into(),
            "submit".into(),
            arguments.clone(),
        )]),
    ]));
    let (state, _tmp) = build_state_with_provider(provider, false).await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    let answer = state
        .harness
        .prepare_for_chat("user-1", &chat)
        .await
        .unwrap()
        .run("submit")
        .start()
        .await
        .unwrap()
        .structured::<Answer>()
        .await
        .unwrap();
    assert_eq!(answer.description, "fixed");
    let message = state
        .chat_service
        .get_stored_messages(&chat)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.role == MessageRole::Agent)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&message.content).unwrap(),
        arguments
    );
    let calls = state
        .chat_service
        .get_tool_calls_by_message(&message.id)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, arguments);
}

#[tokio::test]
async fn tracked_sdk_run_observes_chat_cancellation_and_shutdown() {
    use frona::agent::harness::ChatRunOptions;
    use frona::chat::message::models::{MessageRole, MessageStatus};
    use frona::core::execution::ExecutionKind;
    for shutdown in [false, true] {
        let started = Arc::new(tokio::sync::Barrier::new(2));
        let provider = Arc::new(MockModelProvider::new(vec![
            helpers::MockResponse::Barrier(
                started.clone(),
                Box::new(helpers::MockResponse::Pending),
            ),
        ]));
        let (state, _tmp) = build_state_with_provider(provider, false).await;
        seed_user_and_agent(&state).await;
        let chat = create_test_chat(&state).await;
        let run = state
            .harness
            .prepare_for_chat_with_options(
                "user-1",
                &chat,
                ChatRunOptions {
                    tools: &[],
                    kind: ExecutionKind::App,
                    action: "Checking",
                    related_chat_ids: &[],
                },
            )
            .await
            .unwrap()
            .run("work");
        assert!(
            state
                .chat_service
                .get_stored_messages(&chat)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.active_sessions.count().await, 0);
        assert!(
            state
                .execution_registry
                .snapshot("user-1")
                .executions
                .is_empty()
        );
        let cancel = async {
            started.wait().await;
            assert_eq!(state.active_sessions.count().await, 1);
            let activity = state.execution_registry.snapshot("user-1");
            assert_eq!(activity.executions.len(), 1);
            assert_eq!(
                activity.executions[0].source.as_ref().unwrap().id.as_ref(),
                Some(&chat)
            );
            if shutdown {
                state.shutdown_token.cancel();
            } else {
                assert!(state.active_sessions.cancel(&chat).await);
            }
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(async { run.await }, cancel)
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(state.active_sessions.count().await, 0);
        assert!(
            state
                .execution_registry
                .snapshot("user-1")
                .executions
                .is_empty()
        );
        let messages = state.chat_service.get_stored_messages(&chat).await.unwrap();
        assert_eq!(
            messages
                .iter()
                .find(|m| m.role == MessageRole::Agent)
                .unwrap()
                .status,
            Some(MessageStatus::Cancelled)
        );
    }
}

#[tokio::test]
async fn prepare_rejects_unavailable_tools_before_writing_to_chat() {
    use frona::agent::harness::ChatRunOptions;
    use frona::core::{error::AppError, execution::ExecutionKind};

    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;
    let chat = create_test_chat(&state).await;
    let outcome = state
        .harness
        .prepare_for_chat_with_options(
            "user-1",
            &chat,
            ChatRunOptions {
                tools: &["unavailable-tool"],
                kind: ExecutionKind::App,
                action: "Checking",
                related_chat_ids: &[],
            },
        )
        .await;
    assert!(matches!(outcome, Err(AppError::Forbidden(_))));
    assert!(
        state
            .chat_service
            .get_stored_messages(&chat)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.active_sessions.count().await, 0);
}

#[tokio::test]
async fn factory_creates_a_chat_only_when_an_explicit_new_chat_run_is_driven() {
    let provider = Arc::new(MockModelProvider::new(vec![
        helpers::MockResponse::Text("memory".into()),
        helpers::MockResponse::Text("saved".into()),
    ]));
    let (state, _tmp) = build_state_with_provider(provider, false).await;
    seed_user_and_agent(&state).await;
    let before = state.chat_service.list_chats("user-1").await.unwrap().len();

    let memory = state.harness.prepare("user-1", "agent-1").await.unwrap();
    assert_eq!(memory.user_id(), "user-1");
    assert_eq!(memory.chat_id(), None);
    assert_eq!(memory.run("work").await.unwrap(), "memory");
    assert_eq!(
        state.chat_service.list_chats("user-1").await.unwrap().len(),
        before
    );

    let unused = state
        .harness
        .prepare_with_new_chat("user-1", "agent-1", "A new run")
        .await
        .unwrap();
    let unused_id = unused.chat_id().unwrap().to_owned();
    drop(unused.run("never polled"));
    assert!(
        state
            .chat_service
            .find_chat(&unused_id)
            .await
            .unwrap()
            .is_none()
    );

    let harness = state
        .harness
        .prepare_with_new_chat("user-1", "agent-1", "A new run")
        .await
        .unwrap();
    let id = harness.chat_id().unwrap().to_owned();
    let run = harness.run("start");
    assert!(state.chat_service.find_chat(&id).await.unwrap().is_none());
    assert_eq!(run.await.unwrap(), "saved");
    let chat = state.chat_service.get_chat("user-1", &id).await.unwrap();
    assert_eq!(chat.agent_id, "agent-1");
    assert_eq!(chat.title.as_deref(), Some("A new run"));
    assert_eq!(
        state.chat_service.list_chats("user-1").await.unwrap().len(),
        before + 1
    );
    use frona::chat::message::models::{MessageRole, MessageStatus};
    let rows = state.chat_service.get_stored_messages(&id).await.unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.role == MessageRole::System && row.content == "start")
            .count(),
        1
    );
    let replies: Vec<_> = rows
        .iter()
        .filter(|row| row.role == MessageRole::Agent)
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].status, Some(MessageStatus::Completed));
    assert_eq!(replies[0].content, "saved");
}

#[tokio::test]
async fn prepare_checks_the_existing_chats_user_and_agent() {
    use frona::core::error::AppError;
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;
    let id = create_test_chat(&state).await;
    let mut chat = state.chat_service.get_chat("user-1", &id).await.unwrap();
    let previously_authorized = state.harness.prepare_for_chat("user-1", &id).await.unwrap();
    chat.agent_id = "other-agent".into();
    state.chat_service.save_chat(&chat).await.unwrap();
    assert!(matches!(
        previously_authorized.run("stale").await,
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(
        state.harness.prepare_for_chat("user-1", &id).await,
        Err(AppError::NotFound(_))
    ));
    chat.agent_id = "agent-1".into();
    chat.user_id = "other-user".into();
    state.chat_service.save_chat(&chat).await.unwrap();
    assert!(matches!(
        state.harness.prepare_for_chat("user-1", &id).await,
        Err(AppError::Forbidden(_))
    ));
    assert!(
        state
            .chat_service
            .get_stored_messages(&id)
            .await
            .unwrap()
            .is_empty()
    );
}
