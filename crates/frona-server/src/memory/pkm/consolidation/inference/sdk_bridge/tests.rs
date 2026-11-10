use super::*;
use crate::{
    agent::models::Agent,
    auth::models::User,
    core::repository::Repository,
    db::repo::generic::SurrealRepo,
    inference::usage::InferenceUsage,
    tool::{AgentTool, InferenceContext, ToolDefinition, registry::ToolFilter},
};
use chrono::Utc;
use frona_harness::{
    BudgetLimits, Decision, DefaultBudget, FailureKind, Handle, ManualError, RunOutcome, ToolError,
    config::{ProviderModel, RetryConfig},
    inference::{
        InferenceError, ModelConfig, Usage,
        provider::{
            InferenceOutput, ModelProvider, ModelRequestSettings, StreamToken, group::ModelGroup,
        },
    },
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction},
    request::ToolDefinition as Definition,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

struct Script {
    outputs: Mutex<VecDeque<Vec<AssistantContent>>>,
    requests: Mutex<Vec<(String, Vec<Message>, Vec<Definition>)>>,
}
impl Script {
    fn new(outputs: Vec<Vec<AssistantContent>>) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn inference(
        &self,
        _: &ModelConfig,
        system: &str,
        messages: Vec<Message>,
        tools: Vec<Definition>,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.requests
            .lock()
            .unwrap()
            .push((system.into(), messages, tools));
        Ok(InferenceOutput::new(
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model call"),
            Usage::default()
                .input_tokens(Some(10))
                .output_tokens(Some(5))
                .total_tokens(Some(15)),
        ))
    }
    async fn stream_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: Vec<Definition>,
        _: tokio::sync::mpsc::Sender<StreamToken>,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        unreachable!()
    }
    async fn structured_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: serde_json::Value,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        unreachable!()
    }
}
struct Read(AtomicUsize);
#[async_trait::async_trait]
impl AgentTool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            id: "read".into(),
            provider_id: "fixture".into(),
            description: "read evidence".into(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        assert!(crate::inference::request::active_chat(ctx).is_err());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("evidence"))
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.try_into().unwrap(), args),
    ))
}
fn submit(value: u32) -> Vec<AssistantContent> {
    vec![call(
        &format!("submit-{value}"),
        "submit",
        json!({"value":value}),
    )]
}
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Answer {
    value: u32,
}
const PROMPT: PromptSpec = PromptSpec {
    dir: "sdk-fixture",
    vars: &["input"],
    reject_vars: None,
    advisory_vars: None,
    bad_term_vars: None,
};
struct Fixture {
    _app: crate::app_state_fixture::Fixture,
    _prompts: tempfile::TempDir,
    db: Surreal<Db>,
    inference: ConsolidationInference,
}
async fn fixture(provider: Arc<Script>) -> Fixture {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let app = crate::app_state_fixture::build(&db).await;
    SurrealRepo::<User>::new(db.clone())
        .create(&User {
            id: "u1".into(),
            handle: crate::handle!("owner"),
            email: "owner@example.test".into(),
            name: "Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    SurrealRepo::<Agent>::new(db.clone())
        .create(&Agent {
            id: "a1".into(),
            user_id: "u1".into(),
            handle: crate::handle!("agent"),
            name: "Agent".into(),
            description: String::new(),
            model_group: "prepared".into(),
            enabled: true,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            skills: None,
            avatar: None,
            identity: Default::default(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    let group = ModelGroup {
        name: "prepared".into(),
        main: ModelConfig {
            catalog_provider: String::new(),
            provider_handle: Handle::const_validated("fixture"),
            model_id: "pkm".into(),
            provider: ProviderModel::Generic,
            request_settings: ModelRequestSettings {
                max_tokens: Some(100),
                ..Default::default()
            },
        },
        fallbacks: vec![],
        max_tokens: Some(100),
        temperature: None,
        context_window: 4096,
        retry: RetryConfig {
            max_retries: 0,
            ..Default::default()
        },
        inference: Default::default(),
        providers: Arc::new([("fixture".into(), provider as Arc<dyn ModelProvider>)].into()),
    };
    let prompts = tempfile::tempdir().unwrap();
    let path = prompts.path().join("pkm/sdk-fixture");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("system.md"), "Strict PKM instructions").unwrap();
    std::fs::write(path.join("input.md"), "Inspect {{input}}").unwrap();
    let inference = ConsolidationInference::new(
        app.state.harness.clone(),
        group,
        PromptLoader::new(prompts.path()),
        "u1".into(),
    );
    Fixture {
        _app: app,
        _prompts: prompts,
        db,
        inference,
    }
}
async fn prepare(f: &Fixture, tools: &[Arc<dyn AgentTool>]) -> PkmHarness {
    f.inference
        .sdk("a1", &[ToolFilter::AllowList(&[])], tools)
        .await
        .unwrap()
}
async fn no_chat_rows(f: &Fixture) {
    let mut response =
        f.db.query("SELECT * FROM chat; SELECT * FROM message;")
            .await
            .unwrap();
    for i in 0..2 {
        let rows: Vec<surrealdb::types::Value> = response.take(i).unwrap();
        assert!(
            rows.is_empty(),
            "in-memory loops must not create conversation rows: {rows:?}"
        );
    }
}
#[tokio::test]
async fn strict_prompt_detached_tools_async_inspection_and_attributed_usage() {
    let provider = Script::new(vec![
        vec![call("read-1", "read", json!({}))],
        submit(2),
        submit(8),
    ]);
    let f = fixture(provider.clone()).await;
    let read = Arc::new(Read(AtomicUsize::new(0)));
    let bridge = prepare(&f, &[read.clone()]).await;
    assert!(bridge.run(PROMPT, &[]).is_err());
    assert!(
        bridge
            .run(
                PromptSpec {
                    dir: "missing",
                    ..PROMPT
                },
                &[("input", "facts")]
            )
            .is_err()
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    let run = bridge
        .run(PROMPT, &[("input", "facts")])
        .unwrap()
        .structured::<Answer>()
        .exploration_turns(1);
    let report = run.report();
    let mut seen = vec![];
    let value = run
        .inspect(async |answer| {
            seen.push(answer.value);
            tokio::task::yield_now().await;
            Ok::<_, ()>(if answer.value == 8 {
                Decision::accept(format!("validated:{}", answer.value))
            } else {
                Decision::revise("try again").retain(format!("candidate:{}", answer.value))
            })
        })
        .await
        .unwrap();
    assert_eq!(value, "validated:8");
    assert_eq!(seen, [2, 8]);
    assert_eq!(read.0.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].0, "Strict PKM instructions");
    assert!(requests[0].2.iter().any(|t| t.name == "read"));
    assert!(
        requests[1..]
            .iter()
            .all(|r| r.2.len() == 1 && r.2[0].name == "submit")
    );
    assert!(format!("{:?}", requests[2].1).contains("try again"));
    drop(requests);
    assert_eq!(report.snapshot().requests.len(), 3);
    assert_eq!(report.snapshot().attempts.len(), 3);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
    let rows: Vec<InferenceUsage> =
        f.db.query("SELECT *, meta::id(id) AS id FROM inference_usage")
            .await
            .unwrap()
            .take(0)
            .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.user_id == "u1"
        && r.chat_id.is_none()
        && r.kind_tag == "Memory"
        && r.model_group == "prepared"
        && r.total_tokens == 15));
    no_chat_rows(&f).await;
    let other = ConsolidationInference {
        user_id: "other-user".into(),
        ..f.inference.clone()
    };
    assert!(matches!(
        other.sdk("a1", &[], &[]).await,
        Err(AppError::Forbidden(_))
    ));
}
#[tokio::test]
async fn retained_progress_never_becomes_accepted_on_stop_abandon_error_or_budget() {
    for mode in 0..4 {
        let provider = Script::new(vec![submit(3), submit(4)]);
        let f = fixture(provider.clone()).await;
        let bridge = prepare(&f, &[]).await;
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(if mode == 3 { 1 } else { 2 }),
            ..Default::default()
        });
        let run = bridge
            .run(PROMPT, &[("input", "facts")])
            .unwrap()
            .structured::<Answer>()
            .budget(&budget);
        let report = run.report();
        let incomplete = run
            .inspect(async |answer| {
                if answer.value == 3 {
                    return Ok(Decision::revise("again").retain(answer.value));
                }
                match mode {
                    0 => Ok(Decision::stop("enough")),
                    1 => Ok(Decision::abandon("not actionable")),
                    _ => Err("inspector failed"),
                }
            })
            .await
            .unwrap_err();
        assert_eq!(incomplete.retained, Some(3));
        match mode {
            0 => assert!(matches!(
                incomplete.reason,
                ManualError::Stop(ExecutionStop::Stopped(_))
            )),
            1 => assert!(matches!(incomplete.reason, ManualError::Abandoned(_))),
            2 => assert!(matches!(incomplete.reason, ManualError::Inspector(_))),
            _ => assert!(matches!(
                incomplete.reason,
                ManualError::Stop(ExecutionStop::BudgetExhausted(_))
            )),
        }
        assert!(
            matches!(report.snapshot().outcome,Some(RunOutcome::Incomplete { kind,.. }) if kind==match mode { 0|1=>FailureKind::ApplicationStop,2=>FailureKind::Inspector,_=>FailureKind::Budget })
        );
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if mode == 3 { 1 } else { 2 }
        );
        no_chat_rows(&f).await;
    }
}
#[tokio::test]
async fn exploration_is_offering_policy_and_shared_budget_can_refuse_the_final_submission() {
    let provider = Script::new(vec![vec![call("read-1", "read", json!({}))], submit(5)]);
    let f = fixture(provider.clone()).await;
    let read = Arc::new(Read(AtomicUsize::new(0)));
    let bridge = prepare(&f, &[read.clone()]).await;
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(1),
        ..Default::default()
    });
    let incomplete = bridge
        .run(PROMPT, &[("input", "facts")])
        .unwrap()
        .structured::<Answer>()
        .exploration_turns(1)
        .budget(&budget)
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(read.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    // Reusing the object cannot reset the shared allowance.
    let result = bridge
        .run(PROMPT, &[("input", "another loop")])
        .unwrap()
        .budget(&budget)
        .await;
    assert!(result.is_err());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let run = bridge.run(PROMPT, &[("input", "unpolled")]).unwrap();
    let report = run.report();
    drop(run);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Abandoned));
    let token = f.inference.cancel_token.clone();
    token.cancel();
    let run = bridge.run(PROMPT, &[("input", "cancelled")]).unwrap();
    let report = run.report();
    assert!(matches!(
        run.await.unwrap_err().reason,
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert!(matches!(
        report.snapshot().outcome,
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Cancelled,
            ..
        })
    ));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    no_chat_rows(&f).await;
}
#[test]
fn complete_tool_outputs_and_explicit_host_effect_refusal() {
    let result = map_output(ToolOutput::error("retry").with_images(vec![
        frona_harness::tools::files::image::ImageData {
            bytes: vec![1, 2],
            media_type: "image/png".into(),
        },
    ]))
    .unwrap_err();
    let ToolError::Feedback(output) = result else {
        panic!("ordinary failure remains feedback")
    };
    assert_eq!(output.content.len(), 2);
    for output in [
        ToolOutput::text("pending").as_pending_external(),
        ToolOutput::text("changed").with_system_prompt("hidden instructions"),
    ] {
        assert!(matches!(
            map_output(output),
            Err(ToolError::Stop(ExecutionStop::Stopped(_)))
        ));
    }
}

#[tokio::test]
async fn input_suffix_preserves_strict_rendering_and_reaches_the_first_request() {
    let provider = Script::new(vec![submit(3)]);
    let f = fixture(provider.clone()).await;
    let bridge = prepare(&f, &[]).await;
    assert!(
        bridge
            .run_with_input_suffix(PROMPT, &[], "saved proposal rejected")
            .is_err()
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    bridge
        .run_with_input_suffix(
            PROMPT,
            &[("input", "facts")],
            "\nSaved proposal rejected: live graph changed",
        )
        .unwrap()
        .structured::<Answer>()
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let input = format!("{:?}", requests[0].1);
    assert!(input.contains("facts"));
    assert!(input.contains("Saved proposal rejected: live graph changed"));
    assert_eq!(requests[0].0, "Strict PKM instructions");
}
