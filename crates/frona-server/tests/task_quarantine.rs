mod helpers;

use std::path::PathBuf;
use std::sync::Arc;

use frona::agent::task::models::Task;
use frona::agent::task::schema::ResultSpec;
use frona::agent::task::service::TaskService;
use frona::core::error::AppError;
use frona::db::repo::generic::SurrealRepo;
use frona::tool::AgentTool;
use frona::tool::task_control::TaskControlTool;
use frona_text::prompt::PromptLoader;
use helpers::mock_context;
use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

fn workspace_root() -> PathBuf {
    std::env::current_dir()
        .unwrap()
        .ancestors()
        .find(|p| p.join("resources/prompts").exists())
        .expect("workspace root with resources/prompts")
        .to_path_buf()
}

fn prompts() -> PromptLoader {
    PromptLoader::new(workspace_root().join("resources/prompts"))
}

fn ctx_with_task(quarantined: bool, result_schema: Option<Value>) -> frona::tool::InferenceContext {
    let mut ctx = mock_context();
    let now = chrono::Utc::now();
    ctx.task = Some(Task {
        id: "task-1".into(),
        user_id: "test-user".into(),
        agent_id: "test-agent".into(),
        space_id: None,
        chat_id: Some("test-chat".into()),
        title: "test".into(),
        description: "wait for code".into(),
        status: frona::agent::task::models::TaskStatus::InProgress,
        kind: frona::agent::task::models::TaskKind::Signal {
            source_chat_id: "parent-chat".into(),
            resume_parent: true,
            mode: frona::agent::task::models::SignalMode::Once,
            expected_categories: vec!["verification_code".into()],
            expected_channels: vec![],
            expected_contacts: vec![],
            expires_at: None,
            max_evaluations: 50,
            evaluation_count: 0,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined,
        result_schema,
        result_description: None,
        created_at: now,
        updated_at: now,
    });
    ctx
}

fn tool(schema: Option<Value>) -> TaskControlTool {
    let spec = schema.map(|s| Arc::new(ResultSpec::new(s).expect("schema compiles")));
    let storage = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: workspace_root().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    });
    TaskControlTool::new(storage, prompts(), spec)
}

fn unwrap_validation_err(result: Result<frona::tool::ToolOutput, AppError>) -> String {
    match result {
        Ok(_) => panic!("expected Validation error, got Ok"),
        Err(AppError::Validation(msg)) => msg,
        Err(other) => panic!("expected Validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_task_accepts_conformant_six_digit_code() {
    let schema = json!({"type": "string", "pattern": "^[0-9]{6}$"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    t.execute("complete_task", json!({"result": "482910"}), &ctx)
        .await
        .map_err(|e| format!("six-digit code should pass: {e:?}"))
        .ok();
}

#[tokio::test]
async fn complete_task_rejects_too_short_code() {
    let schema = json!({"type": "string", "pattern": "^[0-9]{6}$"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    let msg = unwrap_validation_err(
        t.execute("complete_task", json!({"result": "12345"}), &ctx)
            .await,
    );
    assert!(
        msg.contains("schema"),
        "error should reference schema: {msg}"
    );
}

#[tokio::test]
async fn complete_task_rejects_dashed_attacker_payload() {
    let schema = json!({"type": "string", "pattern": "^[0-9]{6}$"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    let _ = unwrap_validation_err(
        t.execute("complete_task", json!({"result": "4-8-2-9-1-0"}), &ctx)
            .await,
    );
}

#[tokio::test]
async fn complete_task_enum_only_accepts_listed_values() {
    let schema = json!({"type": "string", "enum": ["yes", "no", "cancelled"]});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    assert!(
        t.execute("complete_task", json!({"result": "yes"}), &ctx)
            .await
            .is_ok()
    );
    let _ = unwrap_validation_err(
        t.execute("complete_task", json!({"result": "maybe"}), &ctx)
            .await,
    );
}

#[tokio::test]
async fn complete_task_object_schema_accepts_valid_json_payload() {
    let schema = json!({
        "type": "object",
        "properties": {
            "is_important": {"type": "string", "enum": ["yes", "no"]},
            "category": {"type": "string", "enum": ["dismissal","schedule","fundraiser","other"]},
            "evidence_quote": {"type": "string", "maxLength": 300}
        },
        "required": ["is_important", "category"],
        "additionalProperties": false
    });
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    let body = r#"{"is_important":"yes","category":"dismissal","evidence_quote":"Early dismissal at 1pm Friday."}"#;
    assert!(
        t.execute("complete_task", json!({"result": body}), &ctx)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn complete_task_object_schema_rejects_missing_required_field() {
    let schema = json!({
        "type": "object",
        "properties": {
            "is_important": {"type": "string"},
            "category": {"type": "string"}
        },
        "required": ["is_important", "category"]
    });
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    let body = r#"{"is_important":"yes"}"#;
    let msg = unwrap_validation_err(
        t.execute("complete_task", json!({"result": body}), &ctx)
            .await,
    );
    assert!(
        msg.contains("category"),
        "error should name the missing field: {msg}"
    );
}

#[tokio::test]
async fn complete_task_object_schema_rejects_malformed_json() {
    let schema = json!({"type": "object"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(true, Some(schema));

    let _ = unwrap_validation_err(
        t.execute("complete_task", json!({"result": "not-json"}), &ctx)
            .await,
    );
}

/// Regression: agent passes `result` as a JSON number (not a quoted string).
/// Pre-fix the tool read via `as_str()` → returned None → schema validation
/// skipped → summary persisted as None → empty body in source chat ("nothing
/// delivered to the parent chat").
#[tokio::test]
async fn complete_task_accepts_numeric_result_against_number_schema() {
    let schema = json!({"type": "number", "description": "random number"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let out = t
        .execute("complete_task", json!({"result": 554669}), &ctx)
        .await
        .expect("numeric result against number schema must validate");

    match out.task_event() {
        Some(frona::inference::tool_call::TaskEvent::Completion { summary, .. }) => {
            assert_eq!(summary.as_deref(), Some("554669"));
        }
        other => panic!("expected TaskCompletion tool_data, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_task_accepts_boolean_result_against_boolean_schema() {
    let schema = json!({"type": "boolean"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let out = t
        .execute("complete_task", json!({"result": true}), &ctx)
        .await
        .expect("boolean result must validate");
    match out.task_event() {
        Some(frona::inference::tool_call::TaskEvent::Completion { summary, .. }) => {
            assert_eq!(summary.as_deref(), Some("true"));
        }
        _ => panic!("expected TaskCompletion"),
    }
}

#[tokio::test]
async fn complete_task_accepts_object_result_as_actual_object() {
    let schema = json!({
        "type": "object",
        "properties": {
            "symbol": {"type": "string"},
            "price": {"type": "number"}
        },
        "required": ["symbol", "price"]
    });
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let out = t
        .execute(
            "complete_task",
            json!({"result": {"symbol": "AAPL", "price": 234}}),
            &ctx,
        )
        .await
        .expect("object result must validate");
    match out.task_event() {
        Some(frona::inference::tool_call::TaskEvent::Completion { summary, .. }) => {
            // Stored as JSON-encoded so ResultSpec::parse can roundtrip it.
            let s = summary.as_deref().expect("summary should be set");
            let parsed: serde_json::Value = serde_json::from_str(s).unwrap();
            assert_eq!(parsed, json!({"symbol": "AAPL", "price": 234}));
        }
        _ => panic!("expected TaskCompletion"),
    }
}

#[tokio::test]
async fn complete_task_rejects_missing_result_against_required_schema() {
    let schema = json!({"type": "string"});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let msg = unwrap_validation_err(t.execute("complete_task", json!({}), &ctx).await);
    assert!(
        msg.contains("schema"),
        "missing result must surface schema error: {msg}"
    );
}

#[tokio::test]
async fn complete_task_accepts_null_against_nullable_schema() {
    let schema = json!({"type": ["string", "null"]});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let out = t
        .execute("complete_task", json!({"result": null}), &ctx)
        .await
        .expect("null against nullable schema must validate");
    match out.task_event() {
        Some(frona::inference::tool_call::TaskEvent::Completion { summary, .. }) => {
            assert_eq!(summary.as_deref(), Some("null"));
        }
        _ => panic!("expected TaskCompletion"),
    }
}

#[tokio::test]
async fn complete_task_accepts_missing_against_nullable_schema_as_silent() {
    // Agents often call complete_task() with no args. Against a nullable
    // schema this should be treated as null (silent close), not a validation
    // error.
    let schema = json!({"type": ["string", "null"]});
    let t = tool(Some(schema.clone()));
    let ctx = ctx_with_task(false, Some(schema));

    let out = t
        .execute("complete_task", json!({}), &ctx)
        .await
        .expect("missing result against nullable schema must validate");
    match out.task_event() {
        Some(frona::inference::tool_call::TaskEvent::Completion { summary, .. }) => {
            assert_eq!(summary.as_deref(), None);
        }
        _ => panic!("expected TaskCompletion"),
    }
}

#[tokio::test]
async fn complete_task_without_schema_accepts_any_string() {
    let t = tool(None);
    let ctx = ctx_with_task(false, None);

    assert!(
        t.execute("complete_task", json!({"result": "anything goes"}), &ctx)
            .await
            .is_ok()
    );
    assert!(
        t.execute(
            "complete_task",
            json!({"result": "Your code is forty-eight-two-nine-ten."}),
            &ctx,
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn create_signal_always_sets_quarantined_true() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let task_service = TaskService::new(
        SurrealRepo::new(db.clone()),
        frona::chat::broadcast::BroadcastService::new(),
    );

    let schema = json!({"type": "string", "pattern": "^[0-9]{6}$"});
    let task = task_service
        .create_signal(
            "user-1",
            "agent-1".into(),
            "chat-A".into(),
            "Bank 2FA".into(),
            "Wait for code".into(),
            true,
            frona::agent::task::models::SignalMode::Once,
            vec!["verification_code".into()],
            vec!["sms".into()],
            vec![],
            None,
            50,
            Some(schema.clone()),
        )
        .await
        .unwrap();

    assert!(task.quarantined, "Signal tasks must always be quarantined");
    assert_eq!(task.result_schema.as_ref(), Some(&schema));
}

#[tokio::test]
async fn create_signal_rejects_malformed_schema_at_submission() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let task_service = TaskService::new(
        SurrealRepo::new(db.clone()),
        frona::chat::broadcast::BroadcastService::new(),
    );

    let bad_schema = json!({"type": "string", "pattern": "[unterminated"});
    let err = task_service
        .create_signal(
            "user-1",
            "agent-1".into(),
            "chat-A".into(),
            "Bad".into(),
            "Bad".into(),
            true,
            frona::agent::task::models::SignalMode::Once,
            vec!["x".into()],
            vec![],
            vec![],
            None,
            50,
            Some(bad_schema),
        )
        .await
        .expect_err("malformed schema should reject");
    matches!(err, AppError::Validation(_));

    assert!(
        task_service
            .list_pending_signal_tasks()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn create_signal_with_no_schema_still_quarantines() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let task_service = TaskService::new(
        SurrealRepo::new(db.clone()),
        frona::chat::broadcast::BroadcastService::new(),
    );

    let task = task_service
        .create_signal(
            "user-1",
            "agent-1".into(),
            "chat-A".into(),
            "No schema".into(),
            "test".into(),
            true,
            frona::agent::task::models::SignalMode::Once,
            vec!["x".into()],
            vec![],
            vec![],
            None,
            50,
            None,
        )
        .await
        .unwrap();

    assert!(task.quarantined);
    assert!(task.result_schema.is_none());
}
