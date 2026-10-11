use frona::chat::message::models::{Message, MessageStatus};
use frona::core::{Principal, repository::Repository};
use frona::credential::vault::models::{CredentialTarget, GrantDuration};
use frona::inference::hitl::{Hitl, HitlRequest, HitlResponse, VaultGrant};
use frona::inference::tool_call::{ToolCall, ToolStatus};
use serde_json::{Value, json};

use super::super::*;

async fn pending_credential_call(state: &AppState, chat_id: &str, agent_id: &str) -> ToolCall {
    let message = state
        .chat_service
        .create_executing_agent_message(chat_id, agent_id)
        .await
        .unwrap();
    let repo: SurrealRepo<Message> = SurrealRepo::new(state.db.clone());
    let mut message = repo.find_by_id(&message.id).await.unwrap().unwrap();
    message.status = Some(MessageStatus::Paused);
    repo.update(&message).await.unwrap();

    let call = ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: chat_id.into(),
        message_id: message.id,
        turn: 0,
        provider_call_id: frona::core::repository::new_id(),
        name: "request_credentials".into(),
        arguments: json!({"query": "TEST", "reason": "test approval"}),
        result: String::new(),
        success: false,
        duration_ms: 0,
        hitl: Some(Hitl {
            prompt: "Allow access?".into(),
            url: format!("/chats/{chat_id}"),
            request: HitlRequest::Credential {
                query: "TEST".into(),
                reason: "test approval".into(),
            },
            status: ToolStatus::Pending,
            response: None,
            delivery: None,
        }),
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        turn_reasoning: None,
        created_at: chrono::Utc::now(),
    };
    SurrealRepo::new(state.db.clone())
        .create(&call)
        .await
        .unwrap();
    call
}

async fn credential_approval(state: &AppState) -> HitlResponse {
    let credential = state
        .model_provider_service
        .store
        .vault()
        .create(
            "static",
            json!({"name": "test approval"}),
            json!({"KEY": "test-secret"}),
        )
        .await
        .unwrap();
    HitlResponse::Vault(VaultGrant::Granted {
        connection_id: "managed".into(),
        vault_item_id: credential.item_id.to_string(),
        grant_duration: GrantDuration::Permanent,
        target: CredentialTarget::Prefix {
            env_var_prefix: "TEST".into(),
        },
    })
}

async fn resolve(
    state: &AppState,
    token: &str,
    chat_id: &str,
    resolution: Value,
) -> axum::response::Response {
    build_app(state.clone())
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/tool-calls/resolve"),
            token,
            json!({"resolutions": [resolution]}),
        ))
        .await
        .unwrap()
}

#[tokio::test]
async fn cross_chat_resolutions_cannot_mutate_tools_or_resume_messages() {
    let (state, _tmp) = test_app_state().await;
    let (owner_token, owner_id) =
        register_user(&state, "owner", "owner@example.com", "password123").await;
    let (other_token, _) = register_user(&state, "other", "other@example.com", "password123").await;
    let agent = create_agent(&state, &owner_token, "OwnerAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let target = create_chat(&state, &owner_token, agent_id, None).await;
    let target_id = target["id"].as_str().unwrap();
    let own_other_chat = create_chat(&state, &owner_token, agent_id, None).await;
    let other_agent = create_agent(&state, &other_token, "OtherAgent").await;
    let other_chat = create_chat(
        &state,
        &other_token,
        other_agent["id"].as_str().unwrap(),
        None,
    )
    .await;
    let call = pending_credential_call(&state, target_id, agent_id).await;
    let before = state
        .chat_service
        .find_message(&call.message_id)
        .await
        .unwrap()
        .unwrap();
    let approval = credential_approval(&state).await;

    // Both URLs are authorized for their caller. Neither authorizes the tool
    // call from the target chat, even when the caller owns both chats.
    for (token, chat_id) in [
        (&other_token, other_chat["id"].as_str().unwrap()),
        (&owner_token, own_other_chat["id"].as_str().unwrap()),
    ] {
        for resolution in [
            json!({"tool_call_id": call.id, "response": "yes"}),
            json!({"tool_call_id": call.id, "action": "fail", "response": "no"}),
            json!({"tool_call_id": call.id, "hitl_response": approval}),
            json!({"tool_call_id": call.id, "hitl_response": HitlResponse::Vault(VaultGrant::Denied)}),
        ] {
            let resp = resolve(&state, token, chat_id, resolution).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            let stored = state
                .chat_service
                .get_tool_call(&call.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::to_value(stored).unwrap(),
                serde_json::to_value(&call).unwrap()
            );
            let message = state
                .chat_service
                .find_message(&call.message_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                message, before,
                "foreign message must remain paused and unchanged"
            );
            assert!(
                state
                    .vault_service
                    .list_grants(&owner_id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                state
                    .vault_service
                    .list_bindings_for_principal(&owner_id, &Principal::agent(agent_id))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                state
                    .vault_service
                    .find_existing_access(target_id, "TEST", None)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    // The original ownership check still applies to the actual target URL.
    let resp = resolve(
        &state,
        &other_token,
        target_id,
        json!({"tool_call_id": call.id, "response": "yes"}),
    )
    .await;
    assert!(matches!(
        resp.status(),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
}

#[tokio::test]
async fn same_chat_ordinary_resolutions_preserve_repeats_and_conflicts() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "ordinary", "ordinary@example.com", "password123").await;
    let agent = create_agent(&state, &token, "OrdinaryAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, None).await;
    let chat_id = chat["id"].as_str().unwrap();
    for (action, expected, opposite) in [
        ("success", ToolStatus::Resolved, "fail"),
        ("fail", ToolStatus::Denied, "success"),
    ] {
        let call = pending_credential_call(&state, chat_id, agent_id).await;
        let resolution = json!({"tool_call_id": call.id, "action": action, "response": "answer"});
        for _ in 0..2 {
            let resp = resolve(&state, &token, chat_id, resolution.clone()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(body_json(resp).await["id"], call.message_id);
        }
        let stored = state
            .chat_service
            .get_tool_call(&call.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.hitl.unwrap().status, expected);
        assert_eq!(stored.result, "answer");
        let resp = resolve(
            &state,
            &token,
            chat_id,
            json!({"tool_call_id": call.id, "action": opposite, "response": "answer"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}

#[tokio::test]
async fn same_chat_typed_resolutions_apply_effects_once() {
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "typed", "typed@example.com", "password123").await;
    let agent = create_agent(&state, &token, "TypedAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, None).await;
    let chat_id = chat["id"].as_str().unwrap();
    let approval = credential_approval(&state).await;
    for (response, expected) in [
        (approval, ToolStatus::Resolved),
        (HitlResponse::Vault(VaultGrant::Denied), ToolStatus::Denied),
    ] {
        let call = pending_credential_call(&state, chat_id, agent_id).await;
        // Leave another HITL pending on the same message so this test isolates
        // resolution effects. handlers::batched_resolve_resumes_agent_loop
        // covers successful dispatch after the final pending HITL clears.
        let mut barrier = call.clone();
        barrier.id = frona::core::repository::new_id();
        barrier.provider_call_id = frona::core::repository::new_id();
        SurrealRepo::new(state.db.clone())
            .create(&barrier)
            .await
            .unwrap();
        let resolution = json!({"tool_call_id": call.id, "hitl_response": response});
        for _ in 0..2 {
            let resp = resolve(&state, &token, chat_id, resolution.clone()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(body_json(resp).await["id"], call.message_id);
            assert_eq!(
                state
                    .vault_service
                    .list_grants(&user_id)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                state
                    .vault_service
                    .list_bindings_for_principal(&user_id, &Principal::agent(agent_id))
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
        let stored = state
            .chat_service
            .get_tool_call(&call.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.hitl.unwrap().status, expected);
    }
    assert!(
        state
            .vault_service
            .find_existing_access(chat_id, "TEST", None)
            .await
            .unwrap()
            .is_some()
    );
}
