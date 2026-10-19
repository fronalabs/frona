use axum::body::Body;
use axum::http::{Request, StatusCode};
use frona::core::supervisor::Supervisor;
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn manifests_endpoint_lists_telegram() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "mfest", "mfest@example.com", "password123").await;
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/channels/manifests", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let manifests = body_json(resp).await;
    let arr = manifests.as_array().expect("manifests array");
    let telegram = arr
        .iter()
        .find(|m| m["id"] == "telegram")
        .expect("Telegram manifest registered at startup");
    assert_eq!(telegram["display_name"], "Telegram Bot");
    let fields = telegram["config_fields"]
        .as_array()
        .expect("config_fields array");
    assert!(
        fields.iter().any(|f| f["name"] == "bot_token"),
        "manifest must declare a bot_token field"
    );
}

#[tokio::test]
async fn telegram_webhook_creates_entities_with_metadata() {
    let tg = super::telegram_mock_api().await;
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "tgwh", "tgwh@example.com", "password123").await;
    let agent = create_agent(&state, &token, "TgAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Telegram"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let space = body_json(resp).await;
    let space_id = space["id"].as_str().unwrap();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.to_string(),
        provider: "telegram".into(),
        agent_id: agent_id.to_string(),
        config: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("bot_token".into(), "fake-bot-token-for-test".into());
            m.insert("api_url".into(), tg.uri());
            m
        },
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: Some(frona::chat::channel::models::UserAddress {
            address: Some("@alice".into()),
            pairing_code: None,
            pairing_initiated_at: None,
            paired_at: Some(now),
        }),
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    use frona::core::repository::Repository;
    let channel = frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(
        state.db.clone(),
    )
    .create(&channel)
    .await
    .unwrap();
    let channel_id = channel.id.as_str();
    // Fake bot token → on_connect fails, but the task is registered
    // *before* on_connect, so webhook dispatch still routes.
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let payload = serde_json::json!({
        "update_id": 1001,
        "message": {
            "message_id": 42,
            "chat": {"id": 12345, "type": "private"},
            "from": {
                "id": 12345,
                "first_name": "Alice",
                "username": "alice"
            },
            "text": "hello"
        }
    });
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/webhooks/channels/telegram/{}", channel_id,))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_get(&format!("/api/spaces/{space_id}/chats"), &token))
        .await
        .unwrap();
    let chats_json = if resp.status() == StatusCode::OK {
        body_json(resp).await
    } else {
        let app = build_app(state.clone());
        let resp = app.oneshot(auth_get("/api/chats", &token)).await.unwrap();
        body_json(resp).await
    };
    let chats = chats_json.as_array().expect("chats array");
    let chat = chats
        .iter()
        .find(|c| c["channel_external_id"] == "dm:12345")
        .expect("chat with channel_external_id present");
    assert_eq!(chat["agent_id"], agent_id);
    assert!(
        chat["channel_id"].is_string(),
        "channel_id should be set on channel-bound chat"
    );

    let chat_id = chat["id"].as_str().unwrap();
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{chat_id}/messages"), &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let messages = json["messages"].as_array().expect("messages array");
    let user_msg = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message persisted");
    assert_eq!(user_msg["content"], "hello");
}

#[tokio::test]
async fn telegram_webhook_persists_when_channel_is_signal_mode() {
    let tg = super::telegram_mock_api().await;
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "tgto", "tgto@example.com", "password123").await;
    let agent = create_agent(&state, &token, "TgSignalAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Telegram"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let space = body_json(resp).await;
    let space_id = space["id"].as_str().unwrap();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.to_string(),
        provider: "telegram".into(),
        agent_id: agent_id.to_string(),
        config: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("bot_token".into(), "fake-bot-token-for-test".into());
            m.insert("api_url".into(), tg.uri());
            m
        },
        dispatch_mode: frona::chat::channel::DispatchMode::Signal,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    use frona::core::repository::Repository;
    let channel = frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(
        state.db.clone(),
    )
    .create(&channel)
    .await
    .unwrap();
    let channel_id = channel.id.as_str();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let payload = serde_json::json!({
        "update_id": 7001,
        "message": {
            "message_id": 77,
            "chat": {"id": 77777, "type": "private"},
            "from": {
                "id": 77777,
                "first_name": "Bank",
                "username": "bank2fa"
            },
            "text": "Your code is 482193"
        }
    });
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/webhooks/channels/telegram/{}", channel_id,))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state.clone());
    let resp = app.oneshot(auth_get("/api/chats", &token)).await.unwrap();
    let chats = body_json(resp).await;
    let chat = chats
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel_external_id"] == "dm:77777")
        .expect("chat should exist for signal-mode inbound");
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{chat_id}/messages"), &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let messages = json["messages"].as_array().expect("messages array");
    let user_msg = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("inbound message should persist (receive_signal allowed by default)");
    assert_eq!(user_msg["content"], "Your code is 482193");
}

#[tokio::test]
async fn telegram_webhook_drops_inbound_when_receive_message_forbidden() {
    let tg = super::telegram_mock_api().await;
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "tgblk", "tgblk@example.com", "password123").await;
    let agent = create_agent(&state, &token, "TgBlockedAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    state
        .policy_service
        .create_policy(
            &user_id,
            "@id(\"block-tg-spam-msg\")\nforbid(\n  principal,\n  action == Policy::Action::\"receive_message\",\n  resource in Policy::Channel::\"telegram\"\n)\nwhen { resource.sender.address == \"@spammer\" };",
        )
        .await
        .unwrap();
    // Both gates must deny for a true discard. receive_signal default-permits,
    // so we explicitly forbid it for the same source — otherwise the message
    // would fall through to signal mode and persist.
    state
        .policy_service
        .create_policy(
            &user_id,
            "@id(\"block-tg-spam-signal\")\nforbid(\n  principal,\n  action == Policy::Action::\"receive_signal\",\n  resource in Policy::Channel::\"telegram\"\n)\nwhen { resource.sender.address == \"@spammer\" };",
        )
        .await
        .unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Telegram"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let space = body_json(resp).await;
    let space_id = space["id"].as_str().unwrap();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.to_string(),
        provider: "telegram".into(),
        agent_id: agent_id.to_string(),
        config: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("bot_token".into(), "fake-bot-token-for-test".into());
            m.insert("api_url".into(), tg.uri());
            m
        },
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    use frona::core::repository::Repository;
    let channel = frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(
        state.db.clone(),
    )
    .create(&channel)
    .await
    .unwrap();
    let channel_id = channel.id.as_str();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let payload = serde_json::json!({
        "update_id": 9001,
        "message": {
            "message_id": 99,
            "chat": {"id": 99999, "type": "private"},
            "from": {
                "id": 99999,
                "first_name": "Spam",
                "username": "spammer"
            },
            "text": "buy crypto"
        }
    });
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/webhooks/channels/telegram/{}", channel_id,))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state.clone());
    let resp = app.oneshot(auth_get("/api/chats", &token)).await.unwrap();
    let chats = body_json(resp).await;
    let chat = chats
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["metadata"]["channel:external_id"] == "dm:99999");
    if let Some(chat) = chat {
        let chat_id = chat["id"].as_str().unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(auth_get(&format!("/api/chats/{chat_id}/messages"), &token))
            .await
            .unwrap();
        let json = body_json(resp).await;
        let messages = json["messages"].as_array().expect("messages array");
        let dropped_msg_present = messages.iter().any(|m| m["content"] == "buy crypto");
        assert!(
            !dropped_msg_present,
            "Forbidden inbound message must NOT be persisted",
        );
    }
}

#[tokio::test]
async fn pairing_round_trip_flips_channel_to_connected() {
    use frona::core::repository::Repository;

    let tg = super::telegram_mock_api().await;
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "pair", "pair@example.com", "password123").await;
    let agent = create_agent(&state, &token, "PairAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Pair Space"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel_id = frona::core::repository::new_id();
    let channel = frona::chat::channel::Channel {
        id: channel_id.clone(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "telegram".into(),
        agent_id: agent_id.into(),
        config: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("bot_token".into(), "fake-bot-token".into());
            m.insert("api_url".into(), tg.uri());
            m
        },
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/channels/{channel_id}/pair"),
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let code = body["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 6, "code should be 6 chars: {code}");

    let mid = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    // Pairing is an overlay (pending code), not a connection status.
    assert_eq!(
        mid.user_address
            .as_ref()
            .and_then(|ua| ua.pairing_code.as_deref()),
        Some(code.as_str()),
    );
    assert!(
        mid.user_address
            .as_ref()
            .and_then(|ua| ua.address.as_deref())
            .is_none()
    );

    let payload = serde_json::json!({
        "update_id": 42,
        "message": {
            "message_id": 1,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "first_name": "Op", "username": "operator"},
            "text": code,
        }
    });
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/webhooks/channels/telegram/{}", &channel_id,))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Pipeline runs async (mpsc → process_inbound). Poll until the
    // redemption shows up in DB (max 2s).
    // Redemption clears the pending pairing_code overlay (connection status is
    // unaffected — it stays Connected throughout).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut after = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    while tokio::time::Instant::now() < deadline
        && after
            .user_address
            .as_ref()
            .and_then(|ua| ua.pairing_code.as_ref())
            .is_some()
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        after = state
            .channel_service
            .find_owned(&user_id, &channel_id)
            .await
            .unwrap();
    }
    let ua = after.user_address.as_ref().expect("user_address set");
    assert_eq!(ua.address.as_deref(), Some("@operator"));
    assert!(ua.pairing_code.is_none());
    assert!(ua.pairing_initiated_at.is_none());
    assert!(ua.paired_at.is_some());
}

#[tokio::test]
async fn pairing_cancel_reverts_to_disconnected() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "pcancel", "pcancel@example.com", "password123").await;
    let agent = create_agent(&state, &token, "CancelAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Cancel Space"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel_id = frona::core::repository::new_id();
    let channel = frona::chat::channel::Channel {
        id: channel_id.clone(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id,
        provider: "telegram".into(),
        agent_id: agent_id.into(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    state
        .channel_service
        .initiate_pairing(&user_id, &channel_id)
        .await
        .unwrap();
    state
        .channel_service
        .cancel_pairing(&user_id, &channel_id)
        .await
        .unwrap();

    let after = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    assert_eq!(format!("{:?}", after.status), "Disconnected");
    assert!(after.user_address.is_none(), "no prior address → cleared");
}

#[tokio::test]
async fn restart_clears_orphaned_pairing() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (_token, user_id) =
        register_user(&state, "rstart", "rstart@example.com", "password123").await;

    let now = chrono::Utc::now();
    let channel_id = frona::core::repository::new_id();
    let channel = frona::chat::channel::Channel {
        id: channel_id.clone(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: "space-x".into(),
        provider: "telegram".into(),
        agent_id: "agent-x".into(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_service
        .initiate_pairing(&user_id, &channel_id)
        .await
        .unwrap();

    let count = state
        .channel_service
        .revert_orphaned_pairings()
        .await
        .unwrap();
    assert_eq!(count, 1);

    let after = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    assert_eq!(format!("{:?}", after.status), "Disconnected");
    assert!(after.user_address.is_none());
}

// This is the seam where the SMS-not-sent bug lived (broadcast missing on
// completion); regressions here would silently re-break outbound delivery.

use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct CapturedSend {
    msg_id: String,
    chat_id: String,
    content: String,
}

#[derive(Default)]
#[allow(dead_code)] // msg_id is recorded for diagnostic purposes; only some tests assert on it.
struct CapturedToolCall {
    msg_id: String,
    tool_call_id: String,
    turn_text: Option<String>,
}

#[derive(Default, Clone)]
#[allow(dead_code)]
struct CapturedPendingHitlBatch {
    msg_id: String,
    tool_call_ids: Vec<String>,
}

#[derive(Default)]
struct StubConfig {
    render_tool_segments: bool,
    fail_on_tool: Option<(String, String)>,
    fail_on_send: Option<String>,
    /// Fail the first N connect attempts by emitting a `Disconnected` signal
    /// (instead of `connected()`). Attempt number = the shared `create_count`.
    fail_connect_attempts: usize,
    /// When failing connect, emit a *terminal* (not transient) Disconnected.
    connect_fail_terminal: bool,
}

/// Mirrors what a real adapter's classify_<provider> does: maps the test
/// injected free-form string into a typed ChannelError. Tests use it via
/// `fail_on_tool`/`fail_on_send`.
fn stub_classify_error(msg: &str) -> frona::chat::channel::ChannelError {
    use frona::chat::channel::{ChannelError, ChannelErrorKind};
    let lower = msg.to_ascii_lowercase();
    if lower.contains("blocked") || lower.contains("forbidden") || lower.contains("kicked") {
        ChannelError::terminal(msg.to_string(), ChannelErrorKind::Forbidden)
    } else if lower.contains("chat not found") || lower.contains("user not found") {
        ChannelError::terminal(msg.to_string(), ChannelErrorKind::NotFound)
    } else {
        ChannelError::transient(msg.to_string())
    }
}

struct StubAdapter {
    captured: std::sync::Arc<StdMutex<Vec<CapturedSend>>>,
    tool_calls: std::sync::Arc<StdMutex<Vec<CapturedToolCall>>>,
    pending_hitls: std::sync::Arc<StdMutex<Vec<CapturedPendingHitlBatch>>>,
    inference_start_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    config: std::sync::Arc<StdMutex<StubConfig>>,
    disconnect_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    create_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl frona::chat::channel::ChannelAdapter for StubAdapter {
    async fn on_connect(
        &self,
        ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::core::error::AppError> {
        // Attempt index = number of creates so far (incremented in factory.create
        // before on_connect). Fail the first N attempts if configured, else report
        // readiness so the supervisor reaches Connected.
        let attempt = self.create_count.load(std::sync::atomic::Ordering::SeqCst);
        let (fail_n, terminal) = {
            let c = self.config.lock().unwrap();
            (c.fail_connect_attempts, c.connect_fail_terminal)
        };
        if attempt <= fail_n {
            if terminal {
                ctx.signals
                    .disconnected_terminal(format!("stub terminal fail (attempt {attempt})"));
            } else {
                ctx.signals
                    .disconnected_transient(format!("stub transient fail (attempt {attempt})"));
            }
        } else {
            ctx.signals.connected();
        }
        Ok(())
    }
    async fn on_disconnect(
        &self,
        _ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::core::error::AppError> {
        self.disconnect_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn on_tool(
        &self,
        tc: &frona::inference::tool_call::ToolCall,
        msg: &frona::chat::message::models::Message,
        _chat: &frona::chat::models::Chat,
        _ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::chat::channel::ChannelError> {
        let injected = {
            let mut cfg = self.config.lock().unwrap();
            if let Some((id, _)) = &cfg.fail_on_tool {
                if id == &tc.id {
                    cfg.fail_on_tool.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some((_, err)) = injected {
            return Err(stub_classify_error(&err));
        }
        let render = self.config.lock().unwrap().render_tool_segments;
        if !render {
            return Ok(());
        }
        self.tool_calls.lock().unwrap().push(CapturedToolCall {
            msg_id: msg.id.clone(),
            tool_call_id: tc.id.clone(),
            turn_text: tc.turn_text.clone(),
        });
        Ok(())
    }
    async fn on_send(
        &self,
        msg: &frona::chat::message::models::Message,
        _tool_calls: &[frona::inference::tool_call::ToolCall],
        chat: &frona::chat::models::Chat,
        _ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::chat::channel::ChannelError> {
        let injected = self.config.lock().unwrap().fail_on_send.take();
        if let Some(err) = injected {
            return Err(stub_classify_error(&err));
        }
        if msg.content.trim().is_empty() {
            return Ok(());
        }
        self.captured.lock().unwrap().push(CapturedSend {
            msg_id: msg.id.clone(),
            chat_id: chat.id.clone(),
            content: msg.content.clone(),
        });
        Ok(())
    }
    async fn on_inference_start(
        &self,
        _chat: &frona::chat::models::Chat,
        _ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::chat::channel::ChannelError> {
        self.inference_start_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn on_pending_hitl(
        &self,
        batch: &[frona::inference::tool_call::ToolCall],
        msg: &frona::chat::message::models::Message,
        _chat: &frona::chat::models::Chat,
        ctx: &frona::chat::channel::ChannelCtx,
    ) -> Result<Vec<frona::inference::hitl::HitlDelivery>, frona::chat::channel::ChannelError> {
        self.pending_hitls
            .lock()
            .unwrap()
            .push(CapturedPendingHitlBatch {
                msg_id: msg.id.clone(),
                tool_call_ids: batch.iter().map(|tc| tc.id.clone()).collect(),
            });
        // Return a successful delivery for each — simulates the adapter
        // rendering all prompts and reporting back.
        let now = chrono::Utc::now();
        Ok(batch
            .iter()
            .enumerate()
            .map(|(i, _)| frona::inference::hitl::HitlDelivery {
                channel_id: ctx.channel.id.clone(),
                external_message_id: format!("stub-msg-{i}"),
                delivered_at: now,
            })
            .collect())
    }
    async fn on_webhook(
        &self,
        ctx: &frona::chat::channel::ChannelCtx,
        request: axum::http::Request<axum::body::Bytes>,
    ) -> Result<axum::response::Response, frona::chat::channel::ChannelError> {
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(request.body())
                .into_owned()
                .collect();
        let from = params.get("from").cloned().unwrap_or_default();
        let text = params.get("text").cloned().unwrap_or_default();
        let event = frona::chat::channel::models::ExternalMessage {
            external_chat_id: format!("test:{from}"),
            sender_address: from.clone(),
            sender_external_id: Some(from.clone()),
            sender_display_name: Some(from),
            content: text,
            attachments: vec![],
        };
        ctx.emit
            .send(event)
            .await
            .map_err(|e| frona::chat::channel::ChannelError::transient(format!("emit: {e}")))?;
        use axum::response::IntoResponse;
        Ok((axum::http::StatusCode::OK, "ok").into_response())
    }
}

struct StubFactory {
    captured: std::sync::Arc<StdMutex<Vec<CapturedSend>>>,
    tool_calls: std::sync::Arc<StdMutex<Vec<CapturedToolCall>>>,
    pending_hitls: std::sync::Arc<StdMutex<Vec<CapturedPendingHitlBatch>>>,
    inference_start_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    config: std::sync::Arc<StdMutex<StubConfig>>,
    disconnect_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    create_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StubFactory {
    fn new(captured: std::sync::Arc<StdMutex<Vec<CapturedSend>>>) -> Self {
        Self {
            captured,
            tool_calls: std::sync::Arc::new(StdMutex::new(Vec::new())),
            pending_hitls: std::sync::Arc::new(StdMutex::new(Vec::new())),
            inference_start_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            config: std::sync::Arc::new(StdMutex::new(StubConfig::default())),
            disconnect_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            create_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn disconnect_count(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.disconnect_count.clone()
    }
}

impl frona::chat::channel::ChannelFactory for StubFactory {
    fn manifest(&self) -> frona::chat::channel::ChannelManifest {
        frona::chat::channel::ChannelManifest {
            id: "test".into(),
            display_name: "Test".into(),
            description: "stub for e2e tests".into(),
            config_fields: vec![],
            webhook_url_visible: false,
            setup_instructions: None,
            external_links: vec![],
        }
    }
    fn create(
        &self,
        _config: serde_json::Value,
    ) -> Result<Box<dyn frona::chat::channel::ChannelAdapter>, frona::core::error::AppError> {
        self.create_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(StubAdapter {
            captured: self.captured.clone(),
            tool_calls: self.tool_calls.clone(),
            pending_hitls: self.pending_hitls.clone(),
            inference_start_count: self.inference_start_count.clone(),
            config: self.config.clone(),
            disconnect_count: self.disconnect_count.clone(),
            create_count: self.create_count.clone(),
        }))
    }
}

async fn poll_until<F, Fut>(label: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("timeout waiting for: {label}");
}

#[tokio::test]
async fn inbound_webhook_persists_message_via_stub_adapter() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "e2ein", "e2ein@example.com", "password123").await;
    let agent = create_agent(&state, &token, "E2eAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    state
        .channel_registry
        .register_factory(std::sync::Arc::new(StubFactory::new(captured.clone())));

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "E2E"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.into(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/webhooks/channels/test/{}", &channel.id,))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("from=%2B15551234567&text=hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // process_inbound runs async (mpsc → pipeline). Poll the user's chats.
    let svc = state.chat_service.clone();
    let user_for_poll = user_id.clone();
    poll_until("chat upserted", || {
        let svc = svc.clone();
        let uid = user_for_poll.clone();
        async move {
            svc.list_chats(&uid)
                .await
                .ok()
                .map(|chats| {
                    chats
                        .iter()
                        .any(|c| c.channel_external_id.as_deref() == Some("test:+15551234567"))
                })
                .unwrap_or(false)
        }
    })
    .await;
}

#[tokio::test]
async fn delete_channel_cancels_spawned_task_and_invokes_on_disconnect() {
    use frona::core::repository::Repository;
    use std::sync::atomic::Ordering;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "delch", "delch@example.com", "password123").await;
    let agent = create_agent(&state, &token, "DelChAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    let factory = StubFactory::new(captured.clone());
    let disconnect_count = factory.disconnect_count();
    state
        .channel_registry
        .register_factory(std::sync::Arc::new(factory));

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "DelCh"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.into(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    let repo = SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone());
    repo.create(&channel).await.unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    // Sanity: task is running before delete.
    assert!(
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some(),
        "spawned task should be live after start_channel",
    );
    assert_eq!(
        disconnect_count.load(Ordering::SeqCst),
        0,
        "on_disconnect should not have run yet",
    );

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(
            &format!("/api/channels/{}", &channel.id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Cancellation + on_disconnect run asynchronously off the request thread.
    let mgr = state.channel_supervisor.clone();
    let id_for_poll = channel.id.clone();
    poll_until("spawned task removed from manager", || {
        let mgr = mgr.clone();
        let id = id_for_poll.clone();
        async move { mgr.running_adapter(&id).await.is_none() }
    })
    .await;

    let dc = disconnect_count.clone();
    poll_until("adapter.on_disconnect invoked exactly once", || {
        let dc = dc.clone();
        async move { dc.load(Ordering::SeqCst) >= 1 }
    })
    .await;
    assert_eq!(
        disconnect_count.load(Ordering::SeqCst),
        1,
        "on_disconnect must run exactly once per stop",
    );

    // Row is gone from the DB.
    assert!(
        repo.find_by_id(&channel.id).await.unwrap().is_none(),
        "channel row should be deleted",
    );
}

#[tokio::test]
async fn agent_message_completion_dispatches_to_outbound_adapter() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "e2eout", "e2eout@example.com", "password123").await;
    let agent = create_agent(&state, &token, "E2eOutAgent").await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    state
        .channel_registry
        .register_factory(std::sync::Arc::new(StubFactory::new(captured.clone())));

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "E2E Out"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.clone(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let chat = state
        .chat_service
        .upsert_channel_chat(
            &user_id,
            &space_id,
            &agent_id,
            &channel.id,
            "test:+15551234567",
            None,
        )
        .await
        .unwrap();

    let executing = state
        .chat_service
        .create_executing_agent_message(&chat.id, &agent_id)
        .await
        .unwrap();
    let mut msg = state
        .chat_service
        .get_message(&user_id, &executing.id)
        .await
        .unwrap();
    msg.content = "hello back".into();
    state
        .chat_service
        .complete_agent_message(msg)
        .await
        .unwrap();

    let captured_for_poll = captured.clone();
    poll_until("on_send invoked", || {
        let c = captured_for_poll.clone();
        async move { !c.lock().unwrap().is_empty() }
    })
    .await;

    {
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one outbound dispatch");
        assert_eq!(calls[0].chat_id, chat.id);
        assert_eq!(calls[0].content, "hello back");
        assert_eq!(calls[0].msg_id, executing.id);
    }

    let msg = state
        .chat_service
        .get_message(&user_id, &executing.id)
        .await
        .unwrap();
    assert_eq!(
        msg.delivery.as_ref().map(|d| d.state),
        Some(frona::chat::message::models::DeliveryState::Sent),
    );
}

/// Regression: a `save_agent_message` row (status=None - the `send_message`
/// tool path) must reach the channel adapter. Previously `attempt_send`'s
/// inner status filter required `Some(Completed)`, dropping these silently.
#[tokio::test]
async fn fire_and_forget_agent_message_dispatches_to_outbound_adapter() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "e2eff", "e2eff@example.com", "password123").await;
    let agent = create_agent(&state, &token, "E2eFfAgent").await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    state
        .channel_registry
        .register_factory(std::sync::Arc::new(StubFactory::new(captured.clone())));

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "Fire-and-forget"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.clone(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let chat = state
        .chat_service
        .upsert_channel_chat(
            &user_id,
            &space_id,
            &agent_id,
            &channel.id,
            "test:+15559876543",
            None,
        )
        .await
        .unwrap();

    let response = state
        .chat_service
        .save_agent_message(
            &user_id,
            Some(&space_id),
            &chat.id,
            &agent_id,
            "💧 Time to drink water!".to_string(),
            None,
        )
        .await
        .unwrap();

    let captured_for_poll = captured.clone();
    poll_until("on_send invoked for fire-and-forget", || {
        let c = captured_for_poll.clone();
        async move { !c.lock().unwrap().is_empty() }
    })
    .await;

    {
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one outbound dispatch");
        assert_eq!(calls[0].chat_id, chat.id);
        assert_eq!(calls[0].content, "💧 Time to drink water!");
        assert_eq!(calls[0].msg_id, response.id);
    }

    let msg = state
        .chat_service
        .get_message(&user_id, &response.id)
        .await
        .unwrap();
    assert_eq!(
        msg.delivery.as_ref().map(|d| d.state),
        Some(frona::chat::message::models::DeliveryState::Sent),
        "delivery must be marked Sent after dispatch",
    );
}

#[tokio::test]
async fn empty_agent_message_skips_adapter_and_marks_sent() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "e2eempty", "e2eempty@example.com", "password123").await;
    let agent = create_agent(&state, &token, "E2eEmptyAgent").await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    state
        .channel_registry
        .register_factory(std::sync::Arc::new(StubFactory::new(captured.clone())));

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "E2E Empty"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.clone(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let chat = state
        .chat_service
        .upsert_channel_chat(
            &user_id,
            &space_id,
            &agent_id,
            &channel.id,
            "test:+15559999999",
            None,
        )
        .await
        .unwrap();

    let executing = state
        .chat_service
        .create_executing_agent_message(&chat.id, &agent_id)
        .await
        .unwrap();
    let msg = state
        .chat_service
        .get_message(&user_id, &executing.id)
        .await
        .unwrap();
    state
        .chat_service
        .complete_agent_message(msg)
        .await
        .unwrap();

    let svc = state.chat_service.clone();
    let user_for_poll = user_id.clone();
    let msg_id = executing.id.clone();
    poll_until("delivery state settled to Sent", || {
        let svc = svc.clone();
        let uid = user_for_poll.clone();
        let id = msg_id.clone();
        async move {
            svc.get_message(&uid, &id)
                .await
                .ok()
                .and_then(|m| m.delivery.map(|d| d.state))
                == Some(frona::chat::message::models::DeliveryState::Sent)
        }
    })
    .await;

    assert!(
        captured.lock().unwrap().is_empty(),
        "adapter.on_send must NOT be called for an empty agent message",
    );
}

struct SegmentTestSetup {
    state: frona::core::state::AppState,
    chat: frona::chat::models::Chat,
    agent_id: String,
    captured: std::sync::Arc<StdMutex<Vec<CapturedSend>>>,
    tool_calls_recorder: std::sync::Arc<StdMutex<Vec<CapturedToolCall>>>,
    pending_hitls_recorder: std::sync::Arc<StdMutex<Vec<CapturedPendingHitlBatch>>>,
    inference_start_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    config: std::sync::Arc<StdMutex<StubConfig>>,
    _tmp: tempfile::TempDir,
}

async fn setup_segment_test(prefix: &str) -> SegmentTestSetup {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(
        &state,
        prefix,
        &format!("{prefix}@example.com"),
        "password123",
    )
    .await;
    let agent = create_agent(&state, &token, &format!("{prefix}_agent")).await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    let factory = std::sync::Arc::new(StubFactory::new(captured.clone()));
    let tool_calls_recorder = factory.tool_calls.clone();
    let pending_hitls_recorder = factory.pending_hitls.clone();
    let inference_start_count = factory.inference_start_count.clone();
    let config = factory.config.clone();
    state.channel_registry.register_factory(factory);

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": format!("{prefix} Space")}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.clone(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    state
        .channel_supervisor
        .clone()
        .start(&channel.id)
        .await
        .unwrap();
    poll_until("adapter registered", || async {
        state
            .channel_supervisor
            .running_adapter(&channel.id)
            .await
            .is_some()
    })
    .await;

    let chat = state
        .chat_service
        .upsert_channel_chat(
            &user_id,
            &space_id,
            &agent_id,
            &channel.id,
            &format!("test:{prefix}"),
            None,
        )
        .await
        .unwrap();
    let chat = state
        .chat_service
        .find_chat(&chat.id)
        .await
        .unwrap()
        .unwrap();

    SegmentTestSetup {
        state,
        chat,
        agent_id,
        captured,
        tool_calls_recorder,
        pending_hitls_recorder,
        inference_start_count,
        config,
        _tmp,
    }
}

/// Build a real `OutboundDeliveryService` over the test DB — the same type + deps prod
/// wires in `AppState::new` — so tests drive the production delivery engine directly
/// rather than a test-only shim on `ChannelSupervisor`.
fn outbound_engine(
    state: &frona::core::state::AppState,
) -> frona::chat::channel::OutboundDeliveryService {
    let message_repo: std::sync::Arc<dyn frona::chat::message::repository::MessageRepository> =
        std::sync::Arc::new(frona::db::repo::generic::SurrealRepo::<
            frona::chat::message::models::Message,
        >::new(state.db.clone()));
    frona::chat::channel::OutboundDeliveryService::new(message_repo, state.chat_service.clone())
}

async fn create_executing_msg(
    state: &frona::core::state::AppState,
    chat_id: &str,
    agent_id: &str,
) -> frona::chat::message::models::Message {
    let resp = state
        .chat_service
        .create_executing_agent_message(chat_id, agent_id)
        .await
        .unwrap();
    // Production stamps Pending lazily on first dispatch; tests want it
    // set up front so the segment state machine has something to update.
    outbound_engine(state)
        .ensure_pending_delivery(&resp.id)
        .await
        .unwrap();
    state
        .chat_service
        .find_message(&resp.id)
        .await
        .unwrap()
        .expect("message just created")
}

async fn insert_tool_call(
    state: &frona::core::state::AppState,
    chat_id: &str,
    msg_id: &str,
    turn: u32,
    turn_text: Option<&str>,
) -> frona::inference::tool_call::ToolCall {
    let id = frona::core::repository::new_id();
    state
        .chat_service
        .begin_tool_call(
            &id,
            chat_id,
            msg_id,
            turn,
            &format!("provider-{id}"),
            None,
            "stub_tool",
            &serde_json::json!({}),
            None,
            turn_text.map(String::from),
            None,
        )
        .await
        .unwrap()
}

async fn complete_msg(state: &frona::core::state::AppState, msg_id: &str, content: &str) {
    // The terminal-write API now takes an owned Message rather than re-fetching by id.
    // Tests don't always have user_id in scope; pull the row directly via the repo.
    use frona::core::repository::Repository;
    let repo = frona::db::repo::messages::SurrealMessageRepo::new(state.db.clone());
    let mut msg = repo.find_by_id(msg_id).await.unwrap().unwrap();
    msg.content = content.to_string();
    state
        .chat_service
        .complete_agent_message(msg)
        .await
        .unwrap();
}

async fn reload_msg(
    state: &frona::core::state::AppState,
    msg_id: &str,
) -> frona::chat::message::models::Message {
    state
        .chat_service
        .find_message(msg_id)
        .await
        .unwrap()
        .expect("message must exist")
}

async fn poll_delivery_state(
    state: &frona::core::state::AppState,
    msg_id: &str,
    target: frona::chat::message::models::DeliveryState,
) {
    let svc = state.chat_service.clone();
    let id = msg_id.to_string();
    poll_until(
        &format!("delivery state == {target:?} for {msg_id}"),
        || {
            let svc = svc.clone();
            let id = id.clone();
            async move {
                svc.find_message(&id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|m| m.delivery.map(|d| d.state))
                    == Some(target)
            }
        },
    )
    .await;
}

#[tokio::test]
async fn segments_happy_path_tools_then_trailing() {
    let setup = setup_segment_test("seg_happy").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    let tc0 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("first")).await;
    let tc1 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, Some("second")).await;

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    {
        let tool_calls_seen = setup.tool_calls_recorder.lock().unwrap();
        assert_eq!(
            tool_calls_seen.len(),
            2,
            "on_tool fires once per tool segment"
        );
        assert_eq!(tool_calls_seen[0].tool_call_id, tc0.id);
        assert_eq!(tool_calls_seen[0].turn_text.as_deref(), Some("first"));
        assert_eq!(tool_calls_seen[1].tool_call_id, tc1.id);
    }
    {
        let sends = setup.captured.lock().unwrap();
        assert_eq!(sends.len(), 1, "on_send fires once for trailing");
        assert_eq!(sends[0].content, "tail");
    }

    let final_msg = reload_msg(&setup.state, &msg.id).await;
    assert_eq!(
        final_msg.delivery.unwrap().tool_index,
        2,
        "cursor at final_index after trailing sent"
    );
}

#[tokio::test]
async fn segments_skip_empty_turn_text_at_manager() {
    let setup = setup_segment_test("seg_skip").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("")).await;
    insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, None).await;
    let tc2 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 2, Some("real")).await;

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    {
        let seen = setup.tool_calls_recorder.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "on_tool only invoked for non-empty turn_text"
        );
        assert_eq!(seen[0].tool_call_id, tc2.id);
    }

    let final_msg = reload_msg(&setup.state, &msg.id).await;
    assert_eq!(
        final_msg.delivery.unwrap().tool_index,
        3,
        "cursor at final_index after walking 3 tools"
    );
}

#[tokio::test]
async fn segments_empty_trailing_drains_to_sent() {
    let setup = setup_segment_test("seg_drain").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("only this")).await;

    complete_msg(&setup.state, &msg.id, "").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    assert_eq!(setup.tool_calls_recorder.lock().unwrap().len(), 1);
    assert!(
        setup.captured.lock().unwrap().is_empty(),
        "on_send returns None for empty trailing → no captured send",
    );
}

#[tokio::test]
async fn segments_transient_failure_backs_off_and_resumes() {
    let setup = setup_segment_test("seg_transient").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    let tc0 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("first")).await;
    let tc1 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, Some("second")).await;

    setup.config.lock().unwrap().fail_on_tool = Some((tc1.id.clone(), "transient blip".into()));

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Failed,
    )
    .await;

    {
        let seen = setup.tool_calls_recorder.lock().unwrap();
        assert_eq!(seen.len(), 1, "only segment 0 made it through");
        assert_eq!(seen[0].tool_call_id, tc0.id);
    }
    {
        let mid = reload_msg(&setup.state, &msg.id).await.delivery.unwrap();
        assert_eq!(mid.tool_index, 1, "cursor halted at the failed segment");
        assert!(
            mid.next_attempt_at.is_some(),
            "transient → backoff scheduled"
        );
        assert_eq!(mid.attempts, 1);
    }

    {
        use frona::core::repository::Repository;
        let repo =
            SurrealRepo::<frona::chat::message::models::Message>::new(setup.state.db.clone());
        let mut m = setup
            .state
            .chat_service
            .find_message(&msg.id)
            .await
            .unwrap()
            .unwrap();
        m.delivery.as_mut().unwrap().next_attempt_at = Some(chrono::Utc::now());
        repo.update(&m).await.unwrap();
    }
    let _ = setup
        .state
        .channel_supervisor
        .retry_due_deliveries()
        .await
        .unwrap();
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    let seen_after = setup.tool_calls_recorder.lock().unwrap();
    assert_eq!(seen_after.len(), 2, "retry sent segment 1");
    assert_eq!(seen_after[1].tool_call_id, tc1.id);
    drop(seen_after);
    assert_eq!(
        setup.captured.lock().unwrap().len(),
        1,
        "trailing sent once"
    );
}

#[tokio::test]
async fn segments_permanent_failure_halts() {
    let setup = setup_segment_test("seg_permanent").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("first")).await;
    let tc1 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, Some("second")).await;

    setup.config.lock().unwrap().fail_on_tool =
        Some((tc1.id.clone(), "Forbidden: bot was blocked".into()));

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Failed,
    )
    .await;

    let delivery = reload_msg(&setup.state, &msg.id).await.delivery.unwrap();
    assert_eq!(delivery.tool_index, 1);
    assert!(
        delivery.next_attempt_at.is_none(),
        "permanent error must drain the retry queue (next_attempt_at=None)",
    );
}

#[tokio::test]
async fn segments_resume_after_partial_delivery() {
    let setup = setup_segment_test("seg_resume").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    let _tc0 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("a")).await;
    let _tc1 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, Some("b")).await;
    let tc2 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 2, Some("c")).await;

    outbound_engine(&setup.state)
        .record_segment_progress(&msg.id)
        .await
        .unwrap();
    outbound_engine(&setup.state)
        .record_segment_progress(&msg.id)
        .await
        .unwrap();
    assert_eq!(
        reload_msg(&setup.state, &msg.id)
            .await
            .delivery
            .unwrap()
            .tool_index,
        2
    );

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    let seen = setup.tool_calls_recorder.lock().unwrap();
    assert_eq!(seen.len(), 1, "only segment 2 was sent on resume");
    assert_eq!(seen[0].tool_call_id, tc2.id);
    drop(seen);
    assert_eq!(
        setup.captured.lock().unwrap().len(),
        1,
        "trailing also sent"
    );
}

#[tokio::test]
async fn segments_executing_excluded_from_retry_then_completed_walks_full_list() {
    let setup = setup_segment_test("seg_exec").await;
    setup.config.lock().unwrap().render_tool_segments = true;

    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    let tc0 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("a")).await;
    let _tc1 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 1, Some("b")).await;

    let retried = setup
        .state
        .channel_supervisor
        .retry_due_deliveries()
        .await
        .unwrap();
    assert_eq!(retried, 0, "Executing must not surface in retry queue");
    assert!(setup.tool_calls_recorder.lock().unwrap().is_empty());
    assert!(setup.captured.lock().unwrap().is_empty());

    let _tc2 = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 2, Some("c")).await;

    complete_msg(&setup.state, &msg.id, "tail").await;
    poll_delivery_state(
        &setup.state,
        &msg.id,
        frona::chat::message::models::DeliveryState::Sent,
    )
    .await;

    let seen = setup.tool_calls_recorder.lock().unwrap();
    assert_eq!(
        seen.len(),
        3,
        "all 3 tool segments delivered, including the late-appended one"
    );
    assert_eq!(seen[0].tool_call_id, tc0.id);
    drop(seen);
    assert_eq!(setup.captured.lock().unwrap().len(), 1);
}

/// **Regression**: when the agent pauses mid-turn on a HITL while running in
/// a channel-bound chat, the channel adapter MUST be invoked via
/// `on_pending_hitl` with the pending tool_calls so the user sees the prompt
/// in their channel (e.g. Telegram inline keyboard, SMS prompt, etc.).
///
/// Today this test fails — the broadcast dispatcher routes `Inference(Paused)`
/// to `on_inference_done` (stops typing) but never reaches `on_pending_hitl`.
/// The HITL infrastructure exists (`on_pending_hitl`, `HitlDelivery`, the
/// segment state machine) but the wiring from the typed Paused event to the
/// rendering is missing.
#[tokio::test]
async fn channel_hitl_pause_renders_pending_hitls() {
    let setup = setup_segment_test("hitl_pause").await;

    // 1. Executing agent message — what the loop is producing into.
    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;

    // 2. A tool call whose execution emitted a pending HITL. Mirrors what
    //    `notify_human::ask_user_question` would persist mid-turn.
    let tc = insert_tool_call(&setup.state, &setup.chat.id, &msg.id, 0, Some("ask user")).await;
    let hitl = frona::inference::hitl::Hitl {
        prompt: "Which one?".into(),
        url: format!("/chats/{}", setup.chat.id),
        request: frona::inference::hitl::HitlRequest::Question {
            options: vec!["yes".into(), "no".into()],
        },
        status: frona::inference::tool_call::ToolStatus::Pending,
        response: None,
        delivery: None,
    };
    setup
        .state
        .chat_service
        .set_hitl(&tc.id, hitl)
        .await
        .unwrap();

    // 3. Trigger the pause broadcast — same call the agent loop makes when
    //    it returns ExternalToolPending.
    let tcs: Vec<frona::inference::tool_call::ToolCallResponse> = setup
        .state
        .chat_service
        .get_tool_calls_by_message(&msg.id)
        .await
        .unwrap()
        .into_iter()
        .map(Into::into)
        .collect();
    {
        use frona::core::repository::Repository;
        let repo = frona::db::repo::messages::SurrealMessageRepo::new(setup.state.db.clone());
        let placeholder = repo.find_by_id(&msg.id).await.unwrap().unwrap();
        setup
            .state
            .chat_service
            .pause_agent_message(
                placeholder,
                frona::inference::tool_loop::PauseReason::Hitl,
                tcs,
            )
            .await
            .unwrap();
    }

    // 4. Adapter must see the pending HITL.
    let pending_hitls_recorder = setup.pending_hitls_recorder.clone();
    let expected_tool_call_id = tc.id.clone();
    poll_until(
        "adapter on_pending_hitl called with the pending HITL",
        || {
            let rec = pending_hitls_recorder.clone();
            let expected = expected_tool_call_id.clone();
            async move {
                rec.lock()
                    .unwrap()
                    .iter()
                    .any(|b| b.tool_call_ids.iter().any(|id| id == &expected))
            }
        },
    )
    .await;

    let captured = setup.pending_hitls_recorder.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "exactly one HITL render batch");
    assert_eq!(captured[0].msg_id, msg.id);
    assert_eq!(captured[0].tool_call_ids, vec![tc.id.clone()]);

    // Pause flips the persisted message status from Executing → Paused so the
    // startup-resume sweep can safely skip un-resolved HITLs across restarts.
    let reloaded = setup
        .state
        .chat_service
        .find_message(&msg.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.status,
        Some(frona::chat::message::models::MessageStatus::Paused),
        "pause_agent_message must flip status to Paused"
    );

    // The Paused message is discoverable via the paused-specific lookup but
    // NOT via the strict executing lookup.
    let paused_found = setup
        .state
        .chat_service
        .find_paused_message_for_chat(&setup.chat.id)
        .await
        .unwrap();
    assert!(
        paused_found.is_some(),
        "find_paused_message_for_chat finds it"
    );
    let executing_found = setup
        .state
        .chat_service
        .find_executing_message_for_chat(&setup.chat.id)
        .await
        .unwrap();
    assert!(
        executing_found.is_none(),
        "find_executing_message_for_chat must NOT find paused message"
    );

    // Startup sweep must skip Paused messages — otherwise the loop would
    // resume with empty HITL answers.
    let sweep = setup
        .state
        .chat_service
        .find_executing_chat_messages()
        .await;
    assert!(
        sweep.iter().all(|m| m.id != msg.id),
        "find_executing_chat_messages must skip Paused messages"
    );
}

/// **Regression**: when a channel adapter resolves a HITL (e.g. Telegram
/// `callback_query` from a button tap), the inference loop must:
/// 1. Mark the tool_call's `hitl.status` as `Resolved`
/// 2. Populate `tool_call.result` with the synthesized response text
/// 3. Spawn the resume dispatch (`harness.resume` for user chats,
///    `task_executor.run_task_by_id` for task chats) → trigger a fresh
///    inference turn (observable via `on_inference_start` on the adapter)
///
/// Exercises the same `ChannelSupervisor::resolve_hitl` entry point that the
/// real Telegram adapter calls from its `on_webhook` callback_query handler.
#[tokio::test]
async fn channel_button_resolution_resumes_inference() {
    let setup = setup_segment_test("hitl_resolve").await;

    // 1. Paused agent message + a pending HITL — what the loop persisted
    //    when it hit `notify_human::ask_user_question` and called
    //    `pause_agent_message`. (Tests realistic post-Phase 1 state where
    //    pause flips status to Paused.)
    let msg = create_executing_msg(&setup.state, &setup.chat.id, &setup.agent_id).await;
    {
        use frona::core::repository::Repository;
        let mut reloaded = frona::db::repo::generic::SurrealRepo::<
            frona::chat::message::models::Message,
        >::new(setup.state.db.clone())
        .find_by_id(&msg.id)
        .await
        .unwrap()
        .unwrap();
        reloaded.status = Some(frona::chat::message::models::MessageStatus::Paused);
        frona::db::repo::generic::SurrealRepo::<frona::chat::message::models::Message>::new(
            setup.state.db.clone(),
        )
        .update(&reloaded)
        .await
        .unwrap();
    }
    let tc = frona::inference::tool_call::ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: setup.chat.id.clone(),
        message_id: msg.id.clone(),
        turn: 0,
        provider_call_id: "tc-1".into(),
        name: "ask_user_question".into(),
        arguments: serde_json::json!({"question": "Continue?"}),
        result: String::new(),
        success: false,
        duration_ms: 0,
        hitl: Some(frona::inference::hitl::Hitl {
            prompt: "Continue?".into(),
            url: format!("/chats/{}", setup.chat.id),
            request: frona::inference::hitl::HitlRequest::Question {
                options: vec!["yes".into(), "no".into()],
            },
            status: frona::inference::tool_call::ToolStatus::Pending,
            response: None,
            delivery: None,
        }),
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: chrono::Utc::now(),
    };
    use frona::core::repository::Repository;
    frona::db::repo::generic::SurrealRepo::<frona::inference::tool_call::ToolCall>::new(
        setup.state.db.clone(),
    )
    .create(&tc)
    .await
    .unwrap();

    // 2. Simulate the Telegram callback_query: button tap → adapter parses
    //    `r:{tcid}:c:0` → builds HitlResponse::Choice("yes") → `ctx.hitl.resolve`.
    //    We drive the exact production path an adapter uses: grab the live channel's
    //    ctx via `running_adapter`, then call `HitlDeliveryService::resolve`.
    let channel_id = setup
        .chat
        .channel_id
        .clone()
        .expect("segment-test chat is channel-bound");
    let (_adapter, ctx) = setup
        .state
        .channel_supervisor
        .running_adapter(&channel_id)
        .await
        .expect("channel should be running");
    let outcome = ctx
        .hitl
        .resolve(
            &tc.id,
            frona::inference::hitl::HitlResponse::Choice("yes".to_string()),
        )
        .await
        .expect("resolve should succeed");
    assert!(matches!(
        outcome,
        frona::inference::hitl::ResolveOutcome::Resolved { .. }
    ));

    // 3. Tool call's HITL status must flip to Resolved, with the response
    //    persisted and `result` populated (notify_human's on_resume synthesizes
    //    the answer text into the tool result).
    let chat_service = setup.state.chat_service.clone();
    let tc_id = tc.id.clone();
    poll_until("tool_call.hitl.status == Resolved", || {
        let svc = chat_service.clone();
        let id = tc_id.clone();
        async move {
            let Ok(Some(reloaded)) = svc.get_tool_call(&id).await else {
                return false;
            };
            reloaded
                .hitl
                .as_ref()
                .is_some_and(|h| h.status == frona::inference::tool_call::ToolStatus::Resolved)
        }
    })
    .await;

    let reloaded = setup
        .state
        .chat_service
        .get_tool_call(&tc.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.result, "yes",
        "tool result carries the user's choice"
    );

    // 4. Resume kicked off — the channel-resolve handler `tokio::spawn`ed
    //    `harness.resume(...)`, which triggered `inference()`, which emits
    //    `Inference(Start)`. The dispatcher routes Start to
    //    `adapter.on_inference_start`. (Inference will fail because the test
    //    app has no model providers, but the START signal is enough to prove
    //    the resume kicked off.)
    let counter = setup.inference_start_count.clone();
    poll_until(
        "adapter on_inference_start fired (resume kicked off)",
        || {
            let counter = counter.clone();
            async move { counter.load(std::sync::atomic::Ordering::SeqCst) > 0 }
        },
    )
    .await;

    // 5. The Paused → Executing status flip happened before resume — the
    //    startup sweep should pick it up if the server crashes mid-resume.
    //    (Resume may downstream-flip status further depending on inference
    //    outcome; we only assert it left Paused.)
    let chat_service = setup.state.chat_service.clone();
    let msg_id = msg.id.clone();
    poll_until("message status flipped out of Paused on resume", || {
        let svc = chat_service.clone();
        let id = msg_id.clone();
        async move {
            let Ok(Some(reloaded)) = svc.find_message(&id).await else {
                return false;
            };
            !matches!(
                reloaded.status,
                Some(frona::chat::message::models::MessageStatus::Paused)
            )
        }
    })
    .await;
}

#[tokio::test]
async fn slack_manifest_is_registered_with_required_secret_tokens() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "slmf", "slmf@example.com", "password123").await;
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/channels/manifests", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let manifests = body_json(resp).await;
    let slack = manifests
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "slack")
        .expect("Slack manifest registered at startup");
    assert_eq!(slack["display_name"], "Slack");
    let fields = slack["config_fields"].as_array().unwrap();
    for name in ["bot_token", "app_token"] {
        let f = fields
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("manifest must declare {name}"));
        assert_eq!(f["is_required"], true, "{name} must be required");
        assert_eq!(f["is_secret"], true, "{name} must be marked secret");
    }
}

#[tokio::test]
async fn slack_pairing_binds_slack_user_id_into_user_address() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (_token, user_id) =
        register_user(&state, "slpair", "slpair@example.com", "password123").await;

    let now = chrono::Utc::now();
    let channel_id = frona::core::repository::new_id();
    let channel = frona::chat::channel::Channel {
        id: channel_id.clone(),
        user_id: user_id.clone(),
        handle: frona::handle!("telegram"),
        space_id: "space-x".into(),
        provider: "slack".into(),
        agent_id: "agent-x".into(),
        config: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("bot_token".into(), "xoxb-fake".into());
            m.insert("app_token".into(), "xapp-fake".into());
            m
        },
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    frona::db::repo::generic::SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    let code = state
        .channel_service
        .initiate_pairing(&user_id, &channel_id)
        .await
        .unwrap();
    let pairing = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    // Pairing is an overlay on user_address.pairing_code, not a status.
    assert_eq!(
        pairing
            .user_address
            .as_ref()
            .and_then(|ua| ua.pairing_code.as_deref()),
        Some(code.as_str()),
    );

    let redeemed = state
        .channel_service
        .try_redeem_pairing(&channel_id, "U07AB12C", &code)
        .await
        .unwrap();
    assert!(redeemed, "matching code should redeem");

    let after = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    let ua = after
        .user_address
        .expect("user_address populated by redeem");
    assert!(ua.pairing_code.is_none(), "redeem clears the pending code");
    assert_eq!(ua.address.as_deref(), Some("U07AB12C"));
    assert!(ua.paired_at.is_some());
    assert!(ua.pairing_code.is_none(), "code cleared after redeem");

    // Replays after redemption are no-ops — status is no longer Pairing.
    let again = state
        .channel_service
        .try_redeem_pairing(&channel_id, "U99XYZ45", &code)
        .await
        .unwrap();
    assert!(!again, "second redeem after Connected returns false");
    let still = state
        .channel_service
        .find_owned(&user_id, &channel_id)
        .await
        .unwrap();
    assert_eq!(
        still.user_address.and_then(|ua| ua.address).as_deref(),
        Some("U07AB12C"),
        "second attempt does not overwrite the paired address",
    );
}

// Regression: a single channel_service.start() once spawned the adapter
// twice — direct start_with_retry plus a second one from the watcher
// catching its own mark_status(Connecting) broadcast — producing duplicate
// gateways (inbound dup) and duplicate run_outbound subscribers (outbound dup).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_service_start_spawns_adapter_exactly_once() {
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(&state, "racr", "racr@example.com", "password123").await;
    let agent = create_agent(&state, &token, "RaceAgent").await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    let captured = std::sync::Arc::new(StdMutex::new(Vec::<CapturedSend>::new()));
    let factory = std::sync::Arc::new(StubFactory::new(captured.clone()));
    let create_count = factory.create_count.clone();
    state.channel_registry.register_factory(factory);

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "RaceSpace"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    // Direct write keeps the row Disconnected AND disabled so manager.start()
    // below doesn't auto-iterate it via find_active (enabled=false).
    let now = chrono::Utc::now();
    let channel = frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.clone(),
        handle: frona::handle!("test"),
        space_id: space_id.clone(),
        provider: "test".into(),
        agent_id: agent_id.clone(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: false,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    state.channel_supervisor.clone().boot().await.unwrap();

    let before = create_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        before, 0,
        "factory.create must not run during manager.start() for a Disconnected channel \
         (got {before}) — test invariant broken",
    );

    state
        .channel_service
        .start(&state, &user_id, &channel.id)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let count = create_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        count, 1,
        "factory.create should run exactly once per channel start; got {count}",
    );
}

// ---- Supervisor reconnection tests (directly exercise the new lifecycle) ----

fn stub_channel(
    user_id: &str,
    handle: &str,
    space_id: &str,
    agent_id: &str,
    status: frona::chat::channel::ChannelStatus,
) -> frona::chat::channel::Channel {
    let now = chrono::Utc::now();
    frona::chat::channel::Channel {
        id: frona::core::repository::new_id(),
        user_id: user_id.to_string(),
        handle: frona::core::Handle::try_new(handle).unwrap(),
        space_id: space_id.to_string(),
        provider: "test".into(),
        agent_id: agent_id.to_string(),
        config: Default::default(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    }
}

async fn channel_status(
    state: &frona::core::state::AppState,
    id: &str,
) -> frona::chat::channel::ChannelStatus {
    state.channel_service.find_by_id(id).await.unwrap().status
}

/// register_user + agent + space; returns (user_id, agent_id, space_id).
async fn rc_setup(state: &frona::core::state::AppState, slug: &str) -> (String, String, String) {
    let (token, user_id) =
        register_user(state, slug, &format!("{slug}@example.com"), "password123").await;
    let agent = create_agent(state, &token, "RcAgent").await;
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            &token,
            serde_json::json!({"name": "RcSpace"}),
        ))
        .await
        .unwrap();
    let space_id = body_json(resp).await["id"].as_str().unwrap().to_string();
    (user_id, agent_id, space_id)
}

/// Regression for the lost-wakeup wedge: a connect that fails right after the
/// adapter is registered must recover on its own (old code dropped the wakeup
/// and left the channel Failed forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_reconnects_after_transient_connect_failure() {
    use frona::core::repository::Repository;
    let (state, _tmp) = test_app_state().await;
    let (user_id, agent_id, space_id) = rc_setup(&state, "rctrans").await;

    let factory = std::sync::Arc::new(StubFactory::new(std::sync::Arc::new(StdMutex::new(
        Vec::new(),
    ))));
    let config = factory.config.clone();
    let create_count = factory.create_count.clone();
    state.channel_registry.register_factory(factory);
    config.lock().unwrap().fail_connect_attempts = 1; // fail the first attempt (transient)

    let channel = stub_channel(
        &user_id,
        "test",
        &space_id,
        &agent_id,
        frona::chat::channel::ChannelStatus::Disconnected,
    );
    let id = channel.id.clone();
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    state.channel_supervisor.start(&id).await.unwrap();

    poll_until("reconnects to Connected", || {
        let state = state.clone();
        let id = id.clone();
        async move {
            matches!(
                channel_status(&state, &id).await,
                frona::chat::channel::ChannelStatus::Connected
            )
        }
    })
    .await;
    assert!(
        create_count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "supervisor must retry the connect after a transient failure",
    );
}

/// A terminal connect failure marks Failed and does NOT retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_does_not_retry_terminal_connect_failure() {
    use frona::core::repository::Repository;
    let (state, _tmp) = test_app_state().await;
    let (user_id, agent_id, space_id) = rc_setup(&state, "rcterm").await;

    let factory = std::sync::Arc::new(StubFactory::new(std::sync::Arc::new(StdMutex::new(
        Vec::new(),
    ))));
    let config = factory.config.clone();
    let create_count = factory.create_count.clone();
    state.channel_registry.register_factory(factory);
    {
        let mut c = config.lock().unwrap();
        c.fail_connect_attempts = 999;
        c.connect_fail_terminal = true;
    }

    let channel = stub_channel(
        &user_id,
        "test",
        &space_id,
        &agent_id,
        frona::chat::channel::ChannelStatus::Disconnected,
    );
    let id = channel.id.clone();
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    state.channel_supervisor.start(&id).await.unwrap();

    poll_until("marks Failed", || {
        let state = state.clone();
        let id = id.clone();
        async move {
            matches!(
                channel_status(&state, &id).await,
                frona::chat::channel::ChannelStatus::Failed
            )
        }
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        create_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "terminal failure must not retry",
    );
    assert!(
        state
            .channel_supervisor
            .running_adapter(&id)
            .await
            .is_none(),
        "terminally-failed channel must not stay registered",
    );
}

/// The level-triggered reconcile sweep re-arms an enabled, non-Failed channel
/// with no live supervisor, and leaves a terminally-Failed one alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_rearms_supervisorless_channel_but_not_failed() {
    use frona::core::repository::Repository;
    let (state, _tmp) = test_app_state().await;
    let (user_id, agent_id, space_id) = rc_setup(&state, "rcrecon").await;

    let factory = std::sync::Arc::new(StubFactory::new(std::sync::Arc::new(StdMutex::new(
        Vec::new(),
    ))));
    let create_count = factory.create_count.clone();
    state.channel_registry.register_factory(factory);
    let repo = SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone());

    // A Reconnecting row with no supervisor (e.g. survived a crash).
    let reconnecting = stub_channel(
        &user_id,
        "test",
        &space_id,
        &agent_id,
        frona::chat::channel::ChannelStatus::Reconnecting,
    );
    let recon_id = reconnecting.id.clone();
    repo.create(&reconnecting).await.unwrap();
    // A terminally-Failed row (also enabled) that must stay untouched. Distinct
    // space id (one-channel-per-space unique index); never dereferenced since
    // find_active excludes Failed.
    let failed = stub_channel(
        &user_id,
        "test-failed",
        &frona::core::repository::new_id(),
        &agent_id,
        frona::chat::channel::ChannelStatus::Failed,
    );
    let failed_id = failed.id.clone();
    repo.create(&failed).await.unwrap();

    assert_eq!(create_count.load(std::sync::atomic::Ordering::SeqCst), 0);

    for dead_id in state.channel_supervisor.find_dead().await.unwrap() {
        state.channel_supervisor.start(&dead_id).await.unwrap();
    }

    poll_until("reconnecting channel re-armed to Connected", || {
        let state = state.clone();
        let id = recon_id.clone();
        async move {
            matches!(
                channel_status(&state, &id).await,
                frona::chat::channel::ChannelStatus::Connected
            )
        }
    })
    .await;
    // The Failed channel was never started (excluded by find_active).
    assert!(
        state
            .channel_supervisor
            .running_adapter(&failed_id)
            .await
            .is_none(),
        "Failed channel must not be re-armed by reconcile",
    );
    assert!(
        matches!(
            channel_status(&state, &failed_id).await,
            frona::chat::channel::ChannelStatus::Failed
        ),
        "Failed channel stays Failed",
    );
}
