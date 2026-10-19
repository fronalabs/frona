//! Provider replay metadata survives persistence without becoming user content.
mod helpers;

use std::sync::Arc;

use frona::{
    chat::message::models::MessageStatus,
    core::repository::Repository,
    db::repo::generic::SurrealRepo,
    inference::{
        ModelGroup,
        config::ModelRegistryConfig,
        conversation::{ConversationBuilder, ConversationContext, DefaultConversationBuilder},
        credential::runtime::RuntimeCredentials,
        hitl::{HitlResponse, ResolveOutcome},
        provider::InferenceCounter,
        tool_call::{ToolCall, ToolCallResponse},
        tool_loop::extract_reasoning,
    },
};
use rig_core::completion::{AssistantContent, Message as RigMessage, message::Reasoning};
use serde_json::{Value, json};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn model_group(state: &frona::core::state::AppState, brand: &str, endpoint: &str) -> ModelGroup {
    let config: frona::core::config::Config = serde_json::from_value(json!({
        "providers": {"fixture": {"provider": brand, "api_key": "fixture-key", "base_url": endpoint}},
        "models": {"test": {"provider": "fixture", "model": "test-model", "max_tokens": 4096, "retry": {"max_retries": 0}}}
    })).unwrap();
    let registry = ModelRegistryConfig {
        providers: config.providers,
        models: config.models,
        skip_auto_discover: true,
    };
    let runtime = RuntimeCredentials::new(
        registry.providers.clone(),
        state.model_provider_service.store.clone(),
        InferenceCounter::new(state.broadcast_service.clone()),
    );
    registry
        .parse_model_groups(&state.config.inference, Arc::new(runtime.providers()))
        .unwrap()
        .remove("test")
        .unwrap()
}

async fn harness(
    state: &frona::core::state::AppState,
    group: ModelGroup,
) -> frona::agent::harness::Harness {
    let models =
        helpers::test_model_service((*group.providers).clone(), [("test".into(), group)].into())
            .await;
    let chat_service = frona::chat::service::ChatService::new(
        SurrealRepo::new(state.db.clone()),
        SurrealRepo::new(state.db.clone()),
        SurrealRepo::new(state.db.clone()),
        state.agent_service.clone(),
        models,
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
    );
    frona::agent::harness::Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        helpers::test_memory_service(state, &state.db),
        state.skill_service.clone(),
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
    )
}

async fn database() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    db
}

#[tokio::test]
async fn gemini_call_signatures_survive_human_approval_and_database_reload() {
    let server = MockServer::start().await;
    let signatures = [Some("opaque-A+/=\nunchanged"), Some("opaque-B+/="), None];
    let parts: Vec<Value> = signatures
        .iter()
        .enumerate()
        .map(|(index, signature)| {
            let mut part = json!({
                "functionCall": {
                    "name": "ask_user_question",
                    "args": {"question": format!("Question {index}?"), "options": ["yes", "no"]}
                }
            });
            if let Some(signature) = signature {
                part["thoughtSignature"] = json!(signature);
            }
            part
        })
        .collect();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let resumed = body["contents"].as_array().unwrap().iter().any(|message| {
                message["parts"].as_array().unwrap().iter()
                    .any(|part| part.get("functionResponse").is_some())
            });
            let parts = if resumed {
                json!([{"text": "Approved and resumed."}])
            } else {
                json!(parts)
            };
            let response = json!({
                "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13}
            });
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream")
        })
        .mount(&server)
        .await;

    let db = database().await;
    let fixture = helpers::app_state::build(&db).await;
    let state = &fixture.state;
    state.policy_service.sync_base_policies().await.unwrap();
    let mut ctx = helpers::mock_context();
    ctx.agent.model_group = "test".into();
    ctx.agent.prompt = Some("You are a helpful assistant.".into());
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
    state
        .chat_service
        .create_stream_user_message(&ctx.user.id, &chat.id, "Ask three questions.", vec![], None)
        .await
        .unwrap();
    let response = state
        .chat_service
        .create_executing_agent_message(&chat.id, &ctx.agent.id)
        .await
        .unwrap();
    let group = model_group(state, "google", &server.uri());
    let initial_harness = harness(state, group.clone()).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        initial_harness.resume(&ctx.user.id, &chat.id, &response.id),
    )
    .await
    .expect("initial turn must settle")
    .unwrap();
    let message = state
        .chat_service
        .find_message(&response.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Paused), "{message:?}");

    // Load real rows, discard the original harness, and resolve the actual pending
    // human questions before resuming. No in-memory provider response is reused.
    drop(initial_harness);
    let calls = state
        .chat_service
        .get_tool_calls_by_message(&response.id)
        .await
        .unwrap();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        let index = (0..3)
            .find(|i| call.arguments["question"] == format!("Question {i}?"))
            .unwrap();
        assert_eq!(call.signature.as_deref(), signatures[index]);
        assert!(call.hitl.is_some());
        assert!(
            serde_json::to_value(ToolCallResponse::from(call.clone()))
                .unwrap()
                .get("signature")
                .is_none()
        );
    }
    let harness = harness(state, group).await;
    for (index, call) in calls.iter().enumerate() {
        let outcome = harness
            .resolve_and_resume(&call.id, HitlResponse::Choice("yes".into()))
            .await
            .unwrap();
        assert!(
            matches!(outcome, ResolveOutcome::Resolved {should_resume, ..} if should_resume == (index == calls.len() - 1)),
            "{outcome:?}"
        );
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        harness.resume(&ctx.user.id, &chat.id, &response.id),
    )
    .await
    .expect("resumed turn must settle")
    .unwrap();
    let message = state
        .chat_service
        .find_message(&response.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        message.status,
        Some(MessageStatus::Completed),
        "{message:?}"
    );
    assert_eq!(message.content, "Approved and resumed.");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body: Value = requests[1].body_json().unwrap();
    let replayed: Vec<&Value> = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part.get("functionCall").is_some())
        .collect();
    assert_eq!(replayed.len(), 3);
    for (index, signature) in signatures.iter().enumerate() {
        let part = replayed
            .iter()
            .find(|part| part["functionCall"]["args"]["question"] == format!("Question {index}?"))
            .unwrap();
        assert_eq!(
            part.get("thoughtSignature").and_then(Value::as_str),
            *signature
        );
    }
}

#[tokio::test]
async fn reasoning_metadata_replays_through_native_anthropic_and_openai_requests() {
    for (brand, reasoning) in [
        (
            "anthropic",
            Reasoning::new_with_signature("Private reasoning", Some("signed-thinking+/=".into())),
        ),
        (
            "openai",
            Reasoning::encrypted("encrypted-reasoning+/=").with_id("rs_original".into()),
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let db = database().await;
        let fixture = helpers::app_state::build(&db).await;
        let state = &fixture.state;
        let ctx = helpers::mock_context();
        let chat = ctx.chat.as_ref().unwrap();
        let response = state
            .chat_service
            .create_executing_agent_message(&chat.id, &ctx.agent.id)
            .await
            .unwrap();
        let stored = extract_reasoning(&[AssistantContent::Reasoning(reasoning)]).unwrap();
        state
            .chat_service
            .begin_tool_call(
                "replay-call",
                &chat.id,
                &response.id,
                0,
                "call_original",
                None,
                "lookup",
                &json!({}),
                None,
                None,
                Some(stored),
            )
            .await
            .unwrap();
        state
            .chat_service
            .finish_tool_call("replay-call", "Result".into(), true, 1, None)
            .await
            .unwrap();
        let message = state
            .chat_service
            .find_message(&response.id)
            .await
            .unwrap()
            .unwrap();
        let calls = state
            .chat_service
            .get_tool_calls_by_message(&response.id)
            .await
            .unwrap();
        let group = model_group(state, brand, &server.uri());
        let model = group.main;
        let builder = DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
        };
        let mut history = vec![RigMessage::user("Look up the value.")];
        history.extend(
            builder
                .build(
                    &[message],
                    &calls,
                    &ConversationContext {
                        user_id: ctx.user.id,
                        agent_id: ctx.agent.id,
                        model_config: model.clone(),
                    },
                    None,
                )
                .await
                .history,
        );
        // The HTTP error is deliberate: capture the native outbound body without
        // requiring an unrelated response fixture for each provider dialect.
        assert!(
            group.providers["fixture"]
                .inference(&model, "system", history, vec![], Some(4096), None)
                .await
                .is_err()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "{brand}");
        let body: Value = requests[0].body_json().unwrap();
        if brand == "anthropic" {
            let thinking = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().unwrap())
                .find(|block| block["type"] == "thinking")
                .unwrap();
            assert_eq!(thinking["thinking"], "Private reasoning");
            assert_eq!(thinking["signature"], "signed-thinking+/=");
        } else {
            let reasoning = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "reasoning")
                .unwrap();
            assert_eq!(reasoning["id"], "rs_original");
            assert_eq!(reasoning["encrypted_content"], "encrypted-reasoning+/=");
        }
    }
}

#[tokio::test]
async fn old_tool_calls_without_signature_load_from_json_and_database() {
    let db = database().await;
    let fixture = helpers::app_state::build(&db).await;
    let call = fixture
        .state
        .chat_service
        .begin_tool_call(
            "old-call",
            "chat",
            "message",
            0,
            "provider-call",
            Some("removed-to-match-an-old-row".into()),
            "lookup",
            &json!({}),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let mut json = serde_json::to_value(&call).unwrap();
    json.as_object_mut().unwrap().remove("signature");
    assert!(
        serde_json::from_value::<ToolCall>(json)
            .unwrap()
            .signature
            .is_none()
    );
    db.query("UPDATE type::record('tool_call', $id) UNSET signature")
        .bind(("id", call.id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);
    let loaded = repo.find_by_id(&call.id).await.unwrap().unwrap();
    assert!(loaded.signature.is_none());
    assert_eq!(loaded.provider_call_id, "provider-call");
}

#[test]
fn separate_reasoning_items_currently_keep_only_the_first_item() {
    // Characterize the existing storage limitation separately from call signatures.
    // Preserving all items and their ordering requires a different storage change.
    let first = Reasoning::encrypted("first-ciphertext").with_id("rs_first".into());
    let second = Reasoning::encrypted("second-ciphertext").with_id("rs_second".into());
    let stored = extract_reasoning(&[
        AssistantContent::Reasoning(first.clone()),
        AssistantContent::text("Between reasoning items"),
        AssistantContent::Reasoning(second),
    ])
    .unwrap();
    let retained: Reasoning = serde_json::from_value(stored.raw.unwrap()).unwrap();
    assert_eq!(retained, first);
}
