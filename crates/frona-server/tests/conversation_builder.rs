use std::sync::Arc;

use chrono::Utc;
use frona::agent::service::AgentService;
use frona::auth::UserService;
use frona::chat::message::models::{Message, MessageRole, MessageStatus, Reasoning};
use frona::db::init as db;
use frona::db::repo::agents::SurrealAgentRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::conversation::{
    ConversationBuilder, ConversationContext, DefaultConversationBuilder,
};
use frona::inference::provider::ModelConfig;
use frona::inference::tool_call::ToolCall;
use frona::policy::service::PolicyService;
use frona::storage::StorageService;
use frona::tool::sandbox::driver::resource_monitor::SystemResourceManager;
use rig_core::completion::message::UserContent;
use rig_core::completion::{AssistantContent, Message as RigMessage};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

async fn test_builder(db: &Surreal<Db>) -> DefaultConversationBuilder {
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
    let user_service = UserService::new(SurrealRepo::new(db.clone()), &config.cache);
    let storage_service = StorageService::new(&config);
    let policy_repo: Arc<dyn frona::policy::repository::PolicyRepository> = Arc::new(
        SurrealRepo::<frona::policy::models::Policy>::new(db.clone()),
    );
    let tool_fixture = helpers::app_state::build(db).await;
    let policy_service = PolicyService::new(
        policy_repo,
        frona::policy::schema::build_schema(),
        tool_fixture.state.tool_manager.clone(),
        storage_service.clone(),
        user_service.clone(),
    );
    let agent_service = AgentService::new(
        SurrealAgentRepo::new(db.clone()),
        &config.cache,
        Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0)),
        policy_service,
        user_service.clone(),
    );
    DefaultConversationBuilder {
        user_service,
        storage_service,
        agent_service,
    }
}

fn test_ctx() -> ConversationContext {
    ConversationContext {
        agent_id: "test-agent".into(),
        model_config: ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: frona::core::Handle::const_validated("mock"),
            provider: "mock".into(),
            model_id: "test-model".into(),
        },
        user_id: "test-user".into(),
    }
}

fn user_message(chat_id: &str, content: &str) -> Message {
    Message::builder(chat_id, MessageRole::User, content.to_string()).build()
}

fn agent_message(chat_id: &str, content: &str, status: Option<MessageStatus>) -> Message {
    let mut msg = Message::builder(chat_id, MessageRole::Agent, content.to_string())
        .agent_id("test-agent".to_string())
        .build();
    msg.status = status;
    msg
}

fn tool_call(chat_id: &str, message_id: &str, turn: u32, name: &str) -> ToolCall {
    ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: chat_id.to_string(),
        message_id: message_id.to_string(),
        turn,
        provider_call_id: format!("call-{}", frona::core::repository::new_id()),
        name: name.to_string(),
        arguments: serde_json::json!({"query": "test"}),
        result: "tool output".to_string(),
        success: true,
        duration_ms: 100,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn agent_with_tool_calls_single_turn() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message(
        "chat-1",
        "Here's the answer",
        Some(MessageStatus::Completed),
    );
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "Search for Rust"), agent_msg];

    let te1 = tool_call("chat-1", &agent_msg_id, 0, "search_web");
    let te2 = tool_call("chat-1", &agent_msg_id, 0, "browse_page");
    let tool_calls = vec![te1, te2];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    assert_eq!(result.len(), 4);
    assert!(matches!(&result[0], RigMessage::User { .. }));

    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|c| matches!(c, AssistantContent::ToolCall(_)))
        );
    } else {
        panic!("Expected assistant message with tool calls");
    }

    if let RigMessage::User { content } = &result[2] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|c| matches!(c, UserContent::ToolResult(_)))
        );
    } else {
        panic!("Expected user message with tool results");
    }

    if let RigMessage::Assistant { content, .. } = &result[3] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], AssistantContent::Text(t) if t.text == "Here's the answer"));
    } else {
        panic!("Expected assistant message with final text");
    }
}

#[tokio::test]
async fn agent_with_tool_calls_multi_turn() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "Final answer", Some(MessageStatus::Completed));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "Help me"), agent_msg];

    let te_turn0 = tool_call("chat-1", &agent_msg_id, 0, "search");
    let te_turn1 = tool_call("chat-1", &agent_msg_id, 1, "browse");
    let tool_calls = vec![te_turn0, te_turn1];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    assert_eq!(result.len(), 6);
}

#[tokio::test]
async fn agent_executing_status_no_final_text() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "", Some(MessageStatus::Executing));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "Do something"), agent_msg];

    let te = tool_call("chat-1", &agent_msg_id, 0, "web_search");
    let tool_calls = vec![te];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    assert_eq!(result.len(), 3);
    assert!(matches!(&result[0], RigMessage::User { .. }));
    assert!(matches!(&result[1], RigMessage::Assistant { .. }));
    assert!(matches!(&result[2], RigMessage::User { .. }));
}

#[tokio::test]
async fn agent_without_tool_calls_unchanged() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let messages = vec![
        user_message("chat-1", "Hello"),
        agent_message("chat-1", "Hi there!", None),
    ];
    let tool_calls = vec![];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    assert_eq!(result.len(), 2);
    assert!(matches!(&result[0], RigMessage::User { .. }));
    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], AssistantContent::Text(t) if t.text == "Hi there!"));
    } else {
        panic!("Expected assistant message");
    }
}

#[tokio::test]
async fn turn_text_appears_in_reconstructed_history() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "Done", Some(MessageStatus::Completed));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "Search for Rust"), agent_msg];

    let mut te = tool_call("chat-1", &agent_msg_id, 0, "search_web");
    te.turn_text = Some("Here's what I found:".into());
    let tool_calls = vec![te];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    assert_eq!(result.len(), 4);

    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], AssistantContent::Text(t) if t.text == "Here's what I found:"));
        assert!(matches!(items[1], AssistantContent::ToolCall(_)));
    } else {
        panic!("Expected assistant message with turn text and tool call");
    }
}

#[tokio::test]
async fn turn_text_empty_string_omitted() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "Done", Some(MessageStatus::Completed));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "Do it"), agent_msg];

    let mut te = tool_call("chat-1", &agent_msg_id, 0, "search_web");
    te.turn_text = Some(String::new());
    let tool_calls = vec![te];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], AssistantContent::ToolCall(_)));
    } else {
        panic!("Expected assistant message with tool call only");
    }
}

/// Per-turn reasoning stamped on the first tool_call of a turn must surface
/// as `AssistantContent::Reasoning` in that turn's Assistant block - load-
/// bearing for thinking-mode providers (DeepSeek, Anthropic) which reject
/// chat requests if previously-emitted reasoning_content isn't replayed.
#[tokio::test]
async fn agent_with_tool_calls_includes_per_turn_reasoning() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "", Some(MessageStatus::Executing));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "ask me"), agent_msg];

    let mut te = tool_call("chat-1", &agent_msg_id, 0, "ask_user_question");
    te.turn_text = Some("Let me ask you something.".into());
    te.turn_reasoning = Some(Reasoning {
        id: Some("r-0".into()),
        content: "I should engage with a question.".into(),
        signature: Some("sig-0".into()),
        raw: None,
    });

    let result = builder.build(&messages, &[te], &ctx, None).await;

    assert_eq!(result.len(), 3);

    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 3, "expected reasoning + text + tool_call");
        let AssistantContent::Reasoning(r) = items[0] else {
            panic!("first item must be Reasoning, got {:?}", items[0]);
        };
        assert_eq!(r.display_text(), "I should engage with a question.");
        assert!(
            matches!(items[1], AssistantContent::Text(t) if t.text == "Let me ask you something.")
        );
        assert!(matches!(items[2], AssistantContent::ToolCall(_)));
    } else {
        panic!("Expected assistant message at index 1");
    }
}

#[tokio::test]
async fn agent_with_tool_calls_omits_reasoning_when_absent() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    let agent_msg = agent_message("chat-1", "done", Some(MessageStatus::Completed));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "go"), agent_msg];

    let mut te = tool_call("chat-1", &agent_msg_id, 0, "search_web");
    te.turn_text = Some("looking".into());
    let tool_calls = vec![te];

    let result = builder.build(&messages, &tool_calls, &ctx, None).await;

    if let RigMessage::Assistant { content, .. } = &result[1] {
        let items: Vec<_> = content.iter().collect();
        assert_eq!(items.len(), 2, "expected text + tool_call only");
        assert!(matches!(items[0], AssistantContent::Text(_)));
        assert!(matches!(items[1], AssistantContent::ToolCall(_)));
    } else {
        panic!("Expected assistant message at index 1");
    }
}

#[tokio::test]
async fn agent_with_tool_calls_attaches_reasoning_per_turn() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();

    // Multi-turn paused message - each turn has its OWN reasoning that must
    // surface in its OWN Assistant block (DeepSeek requires per-turn replay).
    let agent_msg = agent_message("chat-1", "", Some(MessageStatus::Executing));
    let agent_msg_id = agent_msg.id.clone();

    let messages = vec![user_message("chat-1", "go"), agent_msg];

    let mut te0 = tool_call("chat-1", &agent_msg_id, 0, "tool_a");
    te0.turn_reasoning = Some(Reasoning {
        id: None,
        content: "thinking for turn 0".into(),
        signature: None,
        raw: None,
    });

    let mut te1 = tool_call("chat-1", &agent_msg_id, 1, "tool_b");
    te1.turn_reasoning = Some(Reasoning {
        id: None,
        content: "thinking for turn 1".into(),
        signature: None,
        raw: None,
    });

    let result = builder.build(&messages, &[te0, te1], &ctx, None).await;

    assert_eq!(result.len(), 5);

    for (idx, expected) in [(1usize, "thinking for turn 0"), (3, "thinking for turn 1")] {
        let RigMessage::Assistant { content, .. } = &result[idx] else {
            panic!("expected assistant at {idx}");
        };
        let items: Vec<_> = content.iter().collect();
        let AssistantContent::Reasoning(r) = items[0] else {
            panic!("first item at {idx} must be Reasoning");
        };
        assert_eq!(r.display_text(), expected);
    }
}

#[tokio::test]
async fn prepends_conversation_summary() {
    let db = test_db().await;
    let builder = test_builder(&db).await;
    let ctx = test_ctx();
    let messages = vec![user_message("chat-1", "hello")];

    let without = builder.build(&messages, &[], &ctx, None).await;
    let with = builder
        .build(&messages, &[], &ctx, Some("earlier conversation"))
        .await;

    assert_eq!(
        with.len(),
        without.len() + 1,
        "summary adds one leading message"
    );
    let RigMessage::User { .. } = &with[0] else {
        panic!("leading message must be a user message");
    };
    assert!(
        format!("{:?}", with[0]).contains("conversation_summary"),
        "leading message must be the summary block"
    );
}

mod helpers;
