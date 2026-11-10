mod helpers;
use frona::agent::models::CreateAgentRequest;
use frona::chat::message::models::MessageStatus;
use frona::chat::message::models::SendMessageRequest;
use frona::inference::hitl::HitlResponse;
use frona_harness::config::ProviderModel;
use frona_harness::inference::InferenceError;
use frona_harness::inference::ModelConfig;
use frona_harness::inference::ModelGroup;
use frona_harness::inference::provider::ModelProvider;
use frona_harness::inference::provider::ModelRequestSettings;
use rig_core::completion::{
    AssistantContent, Message,
    message::{CallId, ToolCall, ToolFunction},
    request::Usage,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use surrealdb::{Surreal, engine::local::Db};

#[derive(Clone)]
struct Script {
    turns: Arc<Mutex<VecDeque<Vec<AssistantContent>>>>,
    history: Arc<Mutex<Vec<Vec<Message>>>>,
}
impl Script {
    fn new(turns: Vec<Vec<AssistantContent>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Arc::new(Mutex::new(turns.into())),
            history: Arc::new(Mutex::new(Vec::new())),
        })
    }
}
#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn resolve(
        &self,
        model: &ModelConfig,
    ) -> Result<frona_harness::inference::provider::ResolvedModel, InferenceError> {
        let script = self.clone();
        Ok(frona_harness::inference::provider::ResolvedModel::new(
            helpers::rig::model(model.model_id.clone(), move |request, mode| {
                let content = if matches!(mode, rig_core::wire::Mode::Streaming) {
                    script.history.lock().unwrap().push(
                        request
                            .chat_history
                            .iter()
                            .skip(1)
                            .map(|message| (**message).clone())
                            .collect(),
                    );
                    script
                        .turns
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected inference turn")
                } else if request.tools.iter().any(|tool| tool.name == "submit") {
                    vec![AssistantContent::ToolCall(ToolCall::from_wire(
                        "summary",
                        ToolFunction::new(
                            "submit".try_into().unwrap(),
                            serde_json::json!({"summary":"Earlier work"}),
                        ),
                    ))]
                } else {
                    vec![AssistantContent::text(
                        "Earlier work: created note.txt and asked the user.",
                    )]
                };
                helpers::rig::reply(frona_harness::ModelOutput::new(
                    content,
                    frona_harness::UsageReport::Reported(Usage::default()),
                ))
            }),
        ))
    }
}
fn group(provider: Arc<Script>) -> ModelGroup {
    ModelGroup {
        name: "primary".into(),
        main: ModelConfig {
            catalog_provider: String::new(),
            provider_handle: frona_harness::Handle::try_new("fixture").unwrap(),
            model_id: "fixture".into(),
            provider: ProviderModel::Generic,
            request_settings: ModelRequestSettings::default(),
        },
        fallbacks: vec![],
        max_tokens: Some(1024),
        temperature: None,
        context_window: 128_000,
        retry: frona_harness::config::RetryConfig {
            max_retries: 0,
            ..Default::default()
        },
        inference: Default::default(),
        providers: Arc::new([("fixture".into(), provider as Arc<dyn ModelProvider>)].into()),
    }
}
fn call(name: &str, args: serde_json::Value) -> Vec<AssistantContent> {
    let call = ToolCall::new(
        CallId::from_wire(format!("call-{name}")),
        ToolFunction::new(name.try_into().unwrap(), args),
    );
    vec![AssistantContent::ToolCall(call).with_native(serde_json::json!({"thoughtSignature":"opaque-provider-signature", "functionCall":{"name":name}}))]
}
fn agent() -> CreateAgentRequest {
    serde_json::from_value(serde_json::json!({"name":"Assistant","handle":"assistant","description":"Fixture","model_group":"primary"})).unwrap()
}
fn request(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        content: text.into(),
        attachments: vec![],
        metadata: None,
    }
}
#[tokio::test]
async fn native_tools_human_resume_and_owner_checks_use_the_embedded_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(vec![
        call(
            "write",
            serde_json::json!({"path":"note.txt","content":"persistent note"}),
        ),
        call(
            "ask_user_question",
            serde_json::json!({"question":"Continue?","options":["yes","no"]}),
        ),
        vec![AssistantContent::text("Finished")],
    ]);
    let runtime = fixture(db, dir.path(), group(script.clone()), vec![]).await;
    let mut events = runtime.state.broadcast_service.subscribe_raw();
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    let submitted = submit(
        &runtime,
        &user.id,
        &chat.id,
        request("Write a note and ask me"),
    )
    .await
    .unwrap();
    idle(&runtime).await;
    let message = runtime
        .state
        .chat_service
        .get_message(&user.id, &submitted.id)
        .await
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Paused), "{message:?}");
    let pending = pending_calls(&runtime, &user.id, &chat.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].signature.as_deref(),
        Some("opaque-provider-signature")
    );
    let workspace = dir.path().join("users/alice/agents/assistant/note.txt");
    assert_eq!(
        std::fs::read_to_string(workspace).unwrap(),
        "persistent note"
    );
    let other = create_user(&runtime, frona::handle!("bob"), "Bob")
        .await
        .unwrap();
    assert!(
        runtime
            .state
            .chat_service
            .get_chat(&other.id, &chat.id)
            .await
            .is_err()
    );
    answer(
        &runtime,
        &user.id,
        &pending[0].id,
        HitlResponse::Choice("yes".into()),
    )
    .await
    .unwrap();
    idle(&runtime).await;
    let message = runtime
        .state
        .chat_service
        .get_message(&user.id, &submitted.id)
        .await
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Completed));
    assert_eq!(message.content, "Finished");
    {
        let histories = script.history.lock().unwrap();
        let replay = serde_json::to_string(histories.last().unwrap()).unwrap();
        assert!(replay.contains("opaque-provider-signature"));
        assert!(replay.contains("yes"));
    }
    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let frona::chat::broadcast::BroadcastEventKind::Inference(kind) = event.kind {
            kinds.push(kind);
        }
    }
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, frona::inference::InferenceEventKind::Paused { .. }))
    );
    assert!(matches!(
        kinds.last(),
        Some(frona::inference::InferenceEventKind::Done { .. })
    ));
    shutdown(&runtime).await;
}

#[tokio::test]
async fn persistent_runtime_reopens_a_pending_request_and_resumes_without_server() {
    const SEED_DIRECTORY: &str = "FRONA_RUNTIME_REOPEN_SEED_DIRECTORY";
    if let Some(directory) = std::env::var_os(SEED_DIRECTORY) {
        seed_persistent_pending_request(std::path::Path::new(&directory)).await;
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A full AppState retains service handles through its built-in tools. Use a
    // real process exit as the restart boundary, then reopen the durable DB.
    let seed = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("persistent_runtime_reopens_a_pending_request_and_resumes_without_server")
        .arg("--nocapture")
        .env(SEED_DIRECTORY, dir.path())
        .output()
        .unwrap();
    assert!(seed.status.success(), "seed process failed: {seed:?}");
    let (user_id, chat_id, submission_id, pending_id): (String, String, String, String) =
        serde_json::from_slice(&std::fs::read(dir.path().join("pending.json")).unwrap()).unwrap();
    let path = dir.path().join("database");
    let workspace = dir.path().join("workspace");
    let db = Surreal::new::<surrealdb::engine::local::RocksDb>(path.to_string_lossy().as_ref())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let runtime = fixture(
        db,
        &workspace,
        group(Script::new(vec![vec![AssistantContent::text(
            "Saved after restart",
        )]])),
        vec![],
    )
    .await;
    recover(&runtime).await.unwrap();
    idle(&runtime).await;
    assert_eq!(
        pending_calls(&runtime, &user_id, &chat_id)
            .await
            .unwrap()
            .len(),
        1
    );
    answer(
        &runtime,
        &user_id,
        &pending_id,
        HitlResponse::Choice("yes".into()),
    )
    .await
    .unwrap();
    idle(&runtime).await;
    assert_eq!(
        runtime
            .state
            .chat_service
            .get_message(&user_id, &submission_id)
            .await
            .unwrap()
            .content,
        "Saved after restart"
    );
    shutdown(&runtime).await;
}

async fn seed_persistent_pending_request(directory: &std::path::Path) {
    let path = directory.join("database");
    let workspace = directory.join("workspace");
    let db = Surreal::new::<surrealdb::engine::local::RocksDb>(path.to_string_lossy().as_ref())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let runtime = fixture(
        db,
        &workspace,
        group(Script::new(vec![call(
            "ask_user_question",
            serde_json::json!({"question":"Save?"}),
        )])),
        vec![],
    )
    .await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    let submission = submit(&runtime, &user.id, &chat.id, request("Ask first"))
        .await
        .unwrap();
    idle(&runtime).await;
    let pending = pending_calls(&runtime, &user.id, &chat.id)
        .await
        .unwrap()
        .remove(0);
    std::fs::write(
        directory.join("pending.json"),
        serde_json::to_vec(&(user.id, chat.id, submission.id, pending.id)).unwrap(),
    )
    .unwrap();
    shutdown(&runtime).await;
}

#[tokio::test]
async fn cancelled_human_request_cannot_restart_and_denial_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(vec![
        call(
            "ask_user_question",
            serde_json::json!({"question":"Proceed?"}),
        ),
        call(
            "ask_user_question",
            serde_json::json!({"question":"Proceed again?"}),
        ),
        vec![AssistantContent::text("Denied as requested")],
    ]);
    let runtime = fixture(db, dir.path(), group(script), vec![]).await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    let first = submit(&runtime, &user.id, &chat.id, request("Ask"))
        .await
        .unwrap();
    idle(&runtime).await;
    let call = pending_calls(&runtime, &user.id, &chat.id)
        .await
        .unwrap()
        .remove(0);
    assert!(cancel(&runtime, &user.id, &chat.id).await.unwrap());
    assert_eq!(
        runtime
            .state
            .chat_service
            .get_message(&user.id, &first.id)
            .await
            .unwrap()
            .status,
        Some(MessageStatus::Cancelled)
    );

    assert!(
        !runtime
            .state
            .chat_service
            .mark_message_executing(&call.message_id)
            .await
            .unwrap()
    );
    let second = submit(&runtime, &user.id, &chat.id, request("Ask again"))
        .await
        .unwrap();
    idle(&runtime).await;
    let call = pending_calls(&runtime, &user.id, &chat.id)
        .await
        .unwrap()
        .into_iter()
        .find(|call| call.message_id == second.id)
        .expect("the second message has its own pending question");
    // NotifyHuman questions accept Choice responses. Exercise the server's
    // explicit denial operation rather than inventing an Approval response.
    runtime
        .state
        .chat_service
        .deny_tool_call_with_hitl_response(&call.id, Some("User denied the request.".into()), None)
        .await
        .unwrap();
    let denied = runtime
        .state
        .chat_service
        .get_tool_calls_by_message(&second.id)
        .await
        .unwrap()
        .into_iter()
        .find(|saved| saved.id == call.id)
        .unwrap();
    assert_eq!(
        denied.hitl.unwrap().status,
        frona::inference::tool_call::ToolStatus::Denied
    );
    assert!(
        runtime
            .state
            .chat_service
            .mark_message_executing(&second.id)
            .await
            .unwrap()
    );
    runtime
        .state
        .harness
        .resume(&user.id, &chat.id, &second.id)
        .await
        .unwrap();
    idle(&runtime).await;
    assert_eq!(
        runtime
            .state
            .chat_service
            .get_message(&user.id, &second.id)
            .await
            .unwrap()
            .content,
        "Denied as requested"
    );
    shutdown(&runtime).await;
}

#[tokio::test]
async fn delegated_task_persists_completion_and_parent_result() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(vec![call(
        "complete_task",
        serde_json::json!({"result":"Delegated result"}),
    )]);
    let runtime = fixture(db, dir.path(), group(script), vec![]).await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    let task=runtime.state.task_service.create(&user.id,serde_json::from_value(serde_json::json!({"agent_id":agent.id,"title":"Delegated task","description":"Produce the result","source_agent_id":agent.id,"source_chat_id":chat.id,"resume_parent":false,"result_description":"The completed result"})).unwrap()).await.unwrap();
    run_task(&runtime, &user.id, &task.id).await.unwrap();
    idle(&runtime).await;
    let task = runtime
        .state
        .task_service
        .find_by_id(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        task.status,
        frona::agent::task::models::TaskStatus::Completed,
        "{task:?}"
    );
    assert_eq!(task.result_summary.as_deref(), Some("Delegated result"));
    use frona::chat::message::repository::MessageRepository;
    let messages = frona::db::repo::messages::SurrealMessageRepo::new(runtime.state.db.clone())
        .find_by_chat_id(&chat.id)
        .await
        .unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m.content.contains("Delegated result")),
        "{messages:?}"
    );
    shutdown(&runtime).await;
}

#[tokio::test]
async fn crash_interrupted_chat_is_recovered_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let runtime = fixture(db.clone(), dir.path(), group(Script::new(vec![])), vec![]).await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    runtime
        .state
        .chat_service
        .create_stream_user_message(&user.id, &chat.id, "Interrupted", vec![], None)
        .await
        .unwrap();
    let message = runtime
        .state
        .chat_service
        .create_executing_agent_message(&chat.id, &agent.id)
        .await
        .unwrap();
    shutdown(&runtime).await;
    drop(runtime);
    let script = Script::new(vec![vec![AssistantContent::text("Recovered")]]);
    let runtime = fixture(db, dir.path(), group(script.clone()), vec![]).await;
    recover(&runtime).await.unwrap();
    recover(&runtime).await.unwrap();
    idle(&runtime).await;
    assert_eq!(
        runtime
            .state
            .chat_service
            .get_message(&user.id, &message.id)
            .await
            .unwrap()
            .content,
        "Recovered"
    );
    assert_eq!(script.history.lock().unwrap().len(), 1);
    shutdown(&runtime).await;
}

struct Checkpoint(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait::async_trait]
impl frona::tool::AgentTool for Checkpoint {
    fn name(&self) -> &str {
        "custom_review"
    }
    fn definitions(&self) -> Vec<frona::tool::ToolDefinition> {
        vec![frona::tool::ToolDefinition {
            id: "custom_review".into(),
            provider_id: "custom".into(),
            description: "Ask the host to review the work".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        }]
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &frona::tool::InferenceContext,
    ) -> Result<frona::tool::ToolOutput, frona::core::error::AppError> {
        use frona::inference::hitl::Hitl;
        use frona::inference::hitl::HitlRequest;
        use frona::inference::tool_call::ToolStatus;
        Ok(frona::tool::ToolOutput::text("").with_hitl(Hitl {
            prompt: "Review?".into(),
            url: String::new(),
            request: HitlRequest::Question { options: vec![] },
            status: ToolStatus::Pending,
            response: None,
            delivery: None,
        }))
    }
    async fn on_resume(
        &self,
        _: &str,
        _: &frona::inference::hitl::HitlRequest,
        response: HitlResponse,
        _: &frona::tool::InferenceContext,
    ) -> Result<frona::inference::hitl::HitlOutcome, frona::core::error::AppError> {
        assert!(matches!(response, HitlResponse::Choice(_)));
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(frona::inference::hitl::HitlOutcome::Resolved(
            "Host reviewed".into(),
        ))
    }
}
#[tokio::test]
async fn user_registered_tool_resumes_through_its_host_handler() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(vec![
        call("custom_review", serde_json::json!({})),
        vec![AssistantContent::text("Review complete")],
    ]);
    let runtime = fixture(db, dir.path(), group(script), vec![]).await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    register_user_tool(&runtime, &user.id, Arc::new(Checkpoint(resumed.clone())))
        .await
        .unwrap();
    let submission = submit(&runtime, &user.id, &chat.id, request("Review"))
        .await
        .unwrap();
    idle(&runtime).await;
    let pending = pending_calls(&runtime, &user.id, &chat.id)
        .await
        .unwrap()
        .remove(0);
    answer(
        &runtime,
        &user.id,
        &pending.id,
        HitlResponse::Choice("yes".into()),
    )
    .await
    .unwrap();
    idle(&runtime).await;
    assert!(matches!(
        answer(
            &runtime,
            &user.id,
            &pending.id,
            HitlResponse::Choice("yes".into())
        )
        .await
        .unwrap(),
        frona::inference::hitl::ResolveOutcome::AlreadyResolved
    ));
    assert_eq!(resumed.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .state
            .chat_service
            .get_message(&user.id, &submission.id)
            .await
            .unwrap()
            .content,
        "Review complete"
    );
    shutdown(&runtime).await;
}

#[tokio::test]
async fn long_conversation_compacts_without_deleting_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(
        (0..8)
            .map(|_| vec![AssistantContent::text("Acknowledged")])
            .collect(),
    );
    let mut models = group(script);
    // The full server tool catalog and prompts need more room than the source
    // Runtime fixture. Eight large turns still force compaction within this bound.
    models.context_window = 48_000;
    models.main.request_settings.max_tokens = Some(1024);
    let runtime = fixture(db, dir.path(), models, vec![]).await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
    for turn in 0..8 {
        let submission = submit(
            &runtime,
            &user.id,
            &chat.id,
            request(&format!(
                "Turn {turn}: {}",
                "some durable fact ".repeat(1500)
            )),
        )
        .await
        .unwrap();
        idle(&runtime).await;
        let message = runtime
            .state
            .chat_service
            .get_message(&user.id, &submission.id)
            .await
            .unwrap();
        assert_eq!(
            message.status,
            Some(MessageStatus::Completed),
            "{message:?}"
        );
    }
    let loaded = runtime
        .state
        .chat_service
        .compactor()
        .load_conversation(&chat.id)
        .await
        .unwrap();
    assert!(loaded.summary.is_some());
    use frona::chat::message::repository::MessageRepository;
    let persisted = frona::db::repo::messages::SurrealMessageRepo::new(runtime.state.db.clone())
        .find_by_chat_id(&chat.id)
        .await
        .unwrap();
    assert_eq!(persisted.len(), 16);
    assert!(loaded.messages.len() < persisted.len());
    shutdown(&runtime).await;
}

#[tokio::test]
async fn cancelling_a_paused_task_prevents_human_input_from_restarting_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let runtime = fixture(
        db,
        dir.path(),
        group(Script::new(vec![call(
            "ask_user_question",
            serde_json::json!({"question":"Continue task?","options":["yes"]}),
        )])),
        vec![],
    )
    .await;
    let user = create_user(&runtime, frona::handle!("alice"), "Alice")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let task=runtime.state.task_service.create(&user.id,serde_json::from_value(serde_json::json!({"agent_id":agent.id,"title":"Pause for input","result_description":"A result"})).unwrap()).await.unwrap();
    run_task(&runtime, &user.id, &task.id).await.unwrap();
    idle(&runtime).await;
    let stored = runtime
        .state
        .task_service
        .find_by_id(&task.id)
        .await
        .unwrap()
        .unwrap();
    let chat = stored.chat_id.unwrap();
    let pending = pending_calls(&runtime, &user.id, &chat)
        .await
        .unwrap()
        .remove(0);
    assert!(cancel(&runtime, &user.id, &chat).await.unwrap());

    assert!(
        !runtime
            .state
            .chat_service
            .mark_message_executing(&pending.message_id)
            .await
            .unwrap()
    );
    run_task(&runtime, &user.id, &task.id).await.unwrap();
    assert_eq!(
        runtime
            .state
            .task_service
            .find_by_id(&task.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        frona::agent::task::models::TaskStatus::Cancelled
    );
    shutdown(&runtime).await;
}

struct ShutdownTool;
#[async_trait::async_trait]
impl frona::tool::AgentTool for ShutdownTool {
    fn name(&self) -> &str {
        "shutdown_test"
    }
    fn definitions(&self) -> Vec<frona::tool::ToolDefinition> {
        vec![frona::tool::ToolDefinition {
            id: self.name().into(),
            provider_id: String::new(),
            description: "Stop this runtime after saving the tool result".into(),
            parameters: serde_json::json!({"type":"object"}),
        }]
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        context: &frona::tool::InferenceContext,
    ) -> Result<frona::tool::ToolOutput, frona::core::error::AppError> {
        context.shutdown_token.cancel();
        Ok(frona::tool::ToolOutput::text("saved before shutdown"))
    }
}

fn task_request(agent_id: &str) -> frona::agent::task::models::CreateTaskRequest {
    serde_json::from_value(serde_json::json!({"agent_id":agent_id,"title":"Finish explicit work","description":"Return the requested result"})).unwrap()
}

#[tokio::test]
async fn task_shutdown_retains_partial_work_and_recovery_requires_a_valid_lifecycle_result() {
    use frona::agent::task::models::TaskStatus;
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let mut first_turn = vec![AssistantContent::text("Partial task progress")];
    first_turn.extend(call("shutdown_test", serde_json::json!({})));
    let script = Script::new(vec![first_turn]);
    let runtime = fixture(
        db.clone(),
        dir.path(),
        group(script.clone()),
        vec![Arc::new(ShutdownTool)],
    )
    .await;
    let mut events = runtime.state.broadcast_service.subscribe_raw();
    let user = create_user(&runtime, frona::handle!("worker"), "Worker")
        .await
        .unwrap();
    let mut agent_request = agent();
    agent_request.tools = Some(vec!["shutdown_test".into(), "complete_task".into()]);
    let agent = create_agent(&runtime, &user.id, agent_request)
        .await
        .unwrap();
    let mut request = task_request(&agent.id);
    request.result_schema = Some(serde_json::json!({"type":"integer"}));
    let task = runtime
        .state
        .task_service
        .create(&user.id, request)
        .await
        .unwrap();
    run_task(&runtime, &user.id, &task.id).await.unwrap();
    idle(&runtime).await;
    let saved = runtime
        .state
        .task_service
        .find_by_id(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.status,
        TaskStatus::InProgress,
        "saved task: {saved:?}"
    );
    assert!(saved.result_summary.is_none());
    let chat_id = saved.chat_id.unwrap();
    let messages = runtime
        .state
        .chat_service
        .get_stored_messages(&chat_id)
        .await
        .unwrap();
    let partial = messages
        .iter()
        .find(|message| message.status == Some(MessageStatus::Executing))
        .unwrap();
    assert_eq!(partial.content, "Partial task progress");
    let calls = runtime
        .state
        .chat_service
        .get_tool_calls_by_message(&partial.id)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].result, "saved before shutdown");
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event.kind,
                frona::chat::broadcast::BroadcastEventKind::Inference(
                    frona::inference::InferenceEventKind::Done { .. }
                )
            ),
            "shutdown must not publish completed inference"
        );
    }
    shutdown(&runtime).await;
    drop(runtime);

    let resumed = Script::new(vec![
        call("complete_task", serde_json::json!({"result":"invalid"})),
        call("complete_task", serde_json::json!({"result":7})),
    ]);
    let runtime = fixture(
        db,
        dir.path(),
        group(resumed.clone()),
        vec![Arc::new(ShutdownTool)],
    )
    .await;
    recover(&runtime).await.unwrap();
    idle(&runtime).await;
    let saved = runtime
        .state
        .task_service
        .find_by_id(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Completed, "{saved:?}");
    assert_eq!(saved.result_summary.as_deref(), Some("7"));
    let histories = resumed.history.lock().unwrap();
    assert_eq!(histories.len(), 2);
    assert!(
        serde_json::to_string(&histories[0])
            .unwrap()
            .contains("saved before shutdown")
    );
    assert!(
        serde_json::to_string(&histories[1])
            .unwrap()
            .contains("does not match")
    );
    drop(histories);
    shutdown(&runtime).await;
}

#[tokio::test]
async fn task_continuation_exhaustion_cannot_auto_complete() {
    use frona::agent::task::models::TaskStatus;
    let dir = tempfile::tempdir().unwrap();
    let db = Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    let script = Script::new(
        (0..10)
            .map(|_| vec![AssistantContent::text("still working")])
            .collect(),
    );
    let runtime = fixture(db, dir.path(), group(script.clone()), vec![]).await;
    let user = create_user(&runtime, frona::handle!("worker"), "Worker")
        .await
        .unwrap();
    let agent = create_agent(&runtime, &user.id, agent()).await.unwrap();
    let task = runtime
        .state
        .task_service
        .create(&user.id, task_request(&agent.id))
        .await
        .unwrap();
    run_task(&runtime, &user.id, &task.id).await.unwrap();
    idle(&runtime).await;
    let saved = runtime
        .state
        .task_service
        .find_by_id(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Failed, "{saved:?}");
    assert!(saved.result_summary.is_none());
    assert!(
        saved
            .error_message
            .as_deref()
            .unwrap()
            .contains("maximum task continuation")
    );
    assert_eq!(script.history.lock().unwrap().len(), 10);
    shutdown(&runtime).await;
}

struct CallbackShutdownTool;
#[async_trait::async_trait]
impl frona::tool::AgentTool for CallbackShutdownTool {
    fn name(&self) -> &str {
        "callback_test"
    }
    fn definitions(&self) -> Vec<frona::tool::ToolDefinition> {
        vec![frona::tool::ToolDefinition {
            id: self.name().into(),
            provider_id: String::new(),
            description: "Wait for a host callback".into(),
            parameters: serde_json::json!({"type":"object"}),
        }]
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        context: &frona::tool::InferenceContext,
    ) -> Result<frona::tool::ToolOutput, frona::core::error::AppError> {
        context.shutdown_token.cancel();
        Ok(frona::tool::ToolOutput::text("waiting for callback").as_pending_external())
    }
}

#[tokio::test]
async fn pending_results_survive_shutdown_and_recovery_until_explicit_readiness() {
    use frona::agent::task::models::TaskStatus;
    use frona::core::repository::Repository;
    use frona::db::repo::generic::SurrealRepo;
    for mode in ["chat-hitl", "task-hitl", "task-callback"] {
        let task_mode = mode != "chat-hitl";
        let callback = mode == "task-callback";
        let dir = tempfile::tempdir().unwrap();
        let db = Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        db.use_ns("frona").use_db("frona").await.unwrap();
        let first = if callback {
            call("callback_test", serde_json::json!({}))
        } else {
            let mut content = call(
                "ask_user_question",
                serde_json::json!({"question":"Continue?","options":["yes","no"]}),
            );
            content.extend(call("shutdown_test", serde_json::json!({})));
            content
        };
        let first_script = Script::new(vec![first]);
        let runtime = fixture(
            db.clone(),
            dir.path(),
            group(first_script),
            vec![Arc::new(ShutdownTool), Arc::new(CallbackShutdownTool)],
        )
        .await;
        let user = create_user(&runtime, frona::handle!("waiter"), "Waiter")
            .await
            .unwrap();
        let mut agent_request = agent();
        agent_request.tools = Some(vec![
            "shutdown_test".into(),
            "callback_test".into(),
            "ask_user_question".into(),
            "complete_task".into(),
        ]);
        let agent = create_agent(&runtime, &user.id, agent_request)
            .await
            .unwrap();
        let task_id = if task_mode {
            let task = runtime
                .state
                .task_service
                .create(&user.id, task_request(&agent.id))
                .await
                .unwrap();
            run_task(&runtime, &user.id, &task.id).await.unwrap();
            Some(task.id)
        } else {
            None
        };
        let chat_id = if let Some(task_id) = &task_id {
            idle(&runtime).await;
            let task = runtime
                .state
                .task_service
                .find_by_id(task_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task.status, TaskStatus::InProgress);
            task.chat_id.unwrap()
        } else {
            let chat = create_chat(&runtime, &user.id, &agent.id).await.unwrap();
            submit(&runtime, &user.id, &chat.id, request("Ask me first"))
                .await
                .unwrap();
            idle(&runtime).await;
            chat.id
        };
        let mut paused = runtime
            .state
            .chat_service
            .find_paused_message_for_chat(&chat_id)
            .await
            .unwrap()
            .expect("Pending wins over shutdown");
        let calls = runtime
            .state
            .chat_service
            .get_tool_calls_by_message(&paused.id)
            .await
            .unwrap();
        let waiting = calls
            .iter()
            .find(|call| {
                call.name
                    == if callback {
                        "callback_test"
                    } else {
                        "ask_user_question"
                    }
            })
            .unwrap();
        let waiting_id = waiting.id.clone();
        if !task_mode {
            // Crash after the pending tool record, before the Paused message write.
            paused.status = Some(MessageStatus::Executing);
            SurrealRepo::new(db.clone()).update(&paused).await.unwrap();
        }
        shutdown(&runtime).await;
        drop(runtime);
        let next = if task_mode {
            call(
                "complete_task",
                serde_json::json!({"result":"after answer"}),
            )
        } else {
            vec![AssistantContent::text("after answer")]
        };
        let script = Script::new(vec![next]);
        let runtime = fixture(
            db,
            dir.path(),
            group(script.clone()),
            vec![Arc::new(ShutdownTool), Arc::new(CallbackShutdownTool)],
        )
        .await;
        let mut events = runtime.state.broadcast_service.subscribe_raw();
        recover(&runtime).await.unwrap();
        idle(&runtime).await;
        assert!(
            script.history.lock().unwrap().is_empty(),
            "recovery cannot feed temporary output: {mode}"
        );
        assert_eq!(
            runtime
                .state
                .chat_service
                .get_message(&user.id, &paused.id)
                .await
                .unwrap()
                .status,
            Some(MessageStatus::Paused)
        );
        let mut pause_events = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event.kind,
                frona::chat::broadcast::BroadcastEventKind::Inference(
                    frona::inference::InferenceEventKind::Paused { .. }
                )
            ) {
                pause_events += 1;
            }
        }
        assert_eq!(
            pause_events,
            usize::from(!task_mode),
            "repair should publish one pause, not two"
        );
        if let Some(task_id) = &task_id {
            if callback {
                runtime
                    .state
                    .chat_service
                    .finish_tool_call(&waiting_id, "callback complete".into(), true, 0, None)
                    .await
                    .unwrap();
            } else {
                runtime
                    .state
                    .chat_service
                    .resolve_tool_call_with_hitl_response(
                        &waiting_id,
                        Some("yes".into()),
                        Some(HitlResponse::Choice("yes".into())),
                    )
                    .await
                    .unwrap();
            }
            run_task(&runtime, &user.id, task_id).await.unwrap();
            idle(&runtime).await;
            assert!(
                script.history.lock().unwrap().is_empty(),
                "Paused requires explicit host readiness, including resolved HITL rows"
            );
            assert!(
                runtime
                    .state
                    .chat_service
                    .mark_message_executing(&paused.id)
                    .await
                    .unwrap()
            );
            run_task(&runtime, &user.id, task_id).await.unwrap();
        } else {
            answer(
                &runtime,
                &user.id,
                &waiting_id,
                HitlResponse::Choice("yes".into()),
            )
            .await
            .unwrap();
        }
        idle(&runtime).await;
        assert_eq!(script.history.lock().unwrap().len(), 1);
        if let Some(task_id) = &task_id {
            assert_eq!(
                runtime
                    .state
                    .task_service
                    .find_by_id(task_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                TaskStatus::Completed
            );
        } else {
            assert_eq!(
                runtime
                    .state
                    .chat_service
                    .get_message(&user.id, &paused.id)
                    .await
                    .unwrap()
                    .content,
                "after answer"
            );
        }
        shutdown(&runtime).await;
    }
}

// These fixtures wire the production server services directly. Scenario helpers
// sequence actual service calls; no standalone Runtime facade is reconstructed.
struct ServerFixture {
    state: frona::core::state::AppState,
    _fixture: helpers::app_state::Fixture,
    tools: Vec<Arc<dyn frona::tool::AgentTool>>,
}

async fn fixture(
    db: Surreal<Db>,
    directory: &std::path::Path,
    model: ModelGroup,
    tools: Vec<Arc<dyn frona::tool::AgentTool>>,
) -> ServerFixture {
    frona::db::init::setup_schema(&db).await.unwrap();
    let fixture = helpers::app_state::build_configured(&db, |config| {
        config.storage.data_dir = directory.to_string_lossy().into_owned();
        config.sandbox.disabled = true;
    })
    .await;
    let mut state = fixture.state.clone();
    // Match production startup after AppState constructs its services.
    state.agent_service.sync_agent_limits().await.unwrap();
    state.policy_service.sync_base_policies().await.unwrap();
    let providers = model.providers.as_ref().clone();
    state.model_provider_service =
        helpers::test_model_service(providers, [(model.name.clone(), model)].into()).await;
    state.chat_service =
        helpers::chat_service_with_context(&state, helpers::empty_agent_context_manager(&db));
    state.harness = Arc::new(frona::agent::harness::Harness::new(
        state.chat_service.clone(),
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        helpers::empty_memory_service(),
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
    for user in state.user_service.list_all(true).await.unwrap() {
        for tool in &tools {
            state
                .tool_manager
                .register_user_tool(&user.id, tool.clone())
                .await;
        }
    }
    ServerFixture {
        state,
        _fixture: fixture,
        tools,
    }
}

async fn create_user(
    app: &ServerFixture,
    handle: frona::core::Handle,
    name: &str,
) -> Result<frona::auth::User, frona::core::error::AppError> {
    let now = chrono::Utc::now();
    let id = frona::core::repository::new_id();
    let user = app
        .state
        .user_service
        .create(&frona::auth::User {
            id: id.clone(),
            handle,
            email: format!("{id}@fixture.invalid"),
            name: name.into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await?;
    for tool in &app.tools {
        app.state
            .tool_manager
            .register_user_tool(&user.id, tool.clone())
            .await;
    }
    Ok(user)
}

async fn create_agent(
    app: &ServerFixture,
    user: &str,
    request: CreateAgentRequest,
) -> Result<frona::agent::models::Agent, frona::core::error::AppError> {
    // Real server agents supply their own prompt; Runtime's bundled fallback was
    // deliberately omitted from the portable crate and this fixture.
    let selected = request.tools.clone();
    let agent = app.state.agent_service.create(user, request).await?;
    let agent = app
        .state
        .agent_service
        .update(
            user,
            &agent.id,
            frona::agent::models::UpdateAgentRequest {
                prompt: Some("You are a helpful assistant.".into()),
                ..Default::default()
            },
        )
        .await?;
    if let Some(tools) = selected {
        app.state
            .policy_service
            .reconcile_agent_tools(user, &agent.id, &tools)
            .await?;
    }
    Ok(agent)
}

async fn create_chat(
    app: &ServerFixture,
    user: &str,
    agent: &str,
) -> Result<frona::chat::models::Chat, frona::core::error::AppError> {
    let chat = app
        .state
        .chat_service
        .create_chat(
            user,
            frona::chat::models::CreateChatRequest {
                agent_id: agent.into(),
                space_id: None,
                task_id: None,
                title: None,
                metadata: None,
            },
        )
        .await?;
    app.state.chat_service.get_chat(user, &chat.id).await
}

async fn submit(
    app: &ServerFixture,
    user: &str,
    chat_id: &str,
    request: SendMessageRequest,
) -> Result<frona::chat::message::models::Message, frona::core::error::AppError> {
    let chat = app.state.chat_service.get_chat(user, chat_id).await?;
    app.state
        .chat_service
        .create_stream_user_message(user, chat_id, &request.content, request.attachments, None)
        .await?;
    let response = app
        .state
        .chat_service
        .create_executing_agent_message(chat_id, &chat.agent_id)
        .await?;
    let token = app.state.active_sessions.register(chat_id).await;
    app.state
        .harness
        .run_turn(
            user,
            chat_id,
            &response.id,
            token,
            Box::new(frona::inference::conversation::DefaultConversationBuilder {
                user_service: app.state.user_service.clone(),
                storage_service: app.state.storage_service.clone(),
                agent_service: app.state.agent_service.clone(),
            }),
            &[],
            None,
        )
        .await;
    app.state.chat_service.get_message(user, &response.id).await
}

async fn pending_calls(
    app: &ServerFixture,
    user: &str,
    chat: &str,
) -> Result<Vec<frona::inference::tool_call::ToolCall>, frona::core::error::AppError> {
    app.state.chat_service.get_chat(user, chat).await?;
    Ok(app
        .state
        .chat_service
        .get_tool_calls(chat)
        .await?
        .into_iter()
        .filter(|call| {
            call.hitl
                .as_ref()
                .is_some_and(|h| h.status == frona::inference::tool_call::ToolStatus::Pending)
        })
        .collect())
}

async fn answer(
    app: &ServerFixture,
    _user: &str,
    call: &str,
    response: HitlResponse,
) -> Result<frona::inference::hitl::ResolveOutcome, frona::core::error::AppError> {
    use frona::inference::hitl::ResolveOutcome;
    let outcome = app.state.harness.resolve_and_resume(call, response).await?;
    if let ResolveOutcome::Resolved {
        should_resume: true,
        user_id,
        chat_id,
        message_id,
        task_id,
    } = &outcome
    {
        if let Some(task) = task_id {
            app.state.task_executor.resume_task_by_id(task).await?;
        } else {
            app.state
                .harness
                .resume(user_id, chat_id, message_id)
                .await?;
        }
    }
    Ok(outcome)
}

async fn cancel(
    app: &ServerFixture,
    user: &str,
    chat: &str,
) -> Result<bool, frona::core::error::AppError> {
    let chat = app.state.chat_service.get_chat(user, chat).await?;
    let signalled = if let Some(task) = &chat.task_id {
        app.state.task_executor.cancel_task(task).await
    } else {
        app.state.active_sessions.cancel(&chat.id).await
    };
    if let Some(message) = app
        .state
        .chat_service
        .find_paused_message_for_chat(&chat.id)
        .await?
    {
        app.state.chat_service.cancel_agent_message(message).await?;
        return Ok(true);
    }
    Ok(signalled)
}

async fn run_task(
    app: &ServerFixture,
    _user: &str,
    task: &str,
) -> Result<(), frona::core::error::AppError> {
    let executor = app.state.task_executor.clone();
    let task = task.to_owned();
    tokio::spawn(async move { executor.run_task_by_id(&task).await })
        .await
        .expect("task worker panicked")
}

async fn recover(app: &ServerFixture) -> Result<(), frona::core::error::AppError> {
    for task in app.state.task_service.find_resumable().await? {
        app.state.task_executor.run_task(task).await?;
    }
    for message in app.state.chat_service.find_executing_chat_messages().await {
        let chat = app
            .state
            .chat_service
            .find_chat(&message.chat_id)
            .await?
            .unwrap();

        app.state
            .harness
            .resume(&chat.user_id, &chat.id, &message.id)
            .await?;
    }
    Ok(())
}

async fn register_user_tool(
    app: &ServerFixture,
    user: &str,
    tool: Arc<dyn frona::tool::AgentTool>,
) -> Result<(), frona::core::error::AppError> {
    app.state.tool_manager.register_user_tool(user, tool).await;
    app.state.policy_service.invalidate_cache(user).await;
    Ok(())
}

async fn idle(app: &ServerFixture) {
    // TaskExecutor releases its active entry in a spawned Drop cleanup task.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while app.state.task_executor.active_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server task executor becomes idle");
}

async fn shutdown(app: &ServerFixture) {
    app.state.shutdown_token.cancel();
    idle(app).await;
}
