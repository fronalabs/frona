use super::super::*;
use frona_harness::{
    BudgetLimits, Capabilities, ModelAdapter, ModelOutput, ModelRequest, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
};
use frona_text::prompt::PromptLoader;
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction, UserContent},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Requests(Mutex<Vec<ModelRequest>>);
struct Script {
    responses: Mutex<VecDeque<Vec<AssistantContent>>>,
    requests: Arc<Requests>,
    tokens: u64,
}
#[async_trait::async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests.0.lock().unwrap().push(request);
        Ok(ModelOutput::new(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model call"),
            UsageReport::Reported(Usage::new().total_tokens(Some(self.tokens))),
        ))
    }
}
fn fixture(
    responses: Vec<Vec<AssistantContent>>,
    tokens: u64,
) -> (frona_harness::Harness<'static>, Arc<Requests>) {
    let requests = Arc::new(Requests::default());
    let model = frona_harness::Model::from_adapter(
        "fixture",
        "author",
        Script {
            responses: Mutex::new(responses.into()),
            requests: requests.clone(),
            tokens,
        },
    )
    .unwrap();
    (frona_harness::Harness::new(model), requests)
}
fn call(id: &str, name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.try_into().unwrap(), args),
    ))
}
fn valid() -> Value {
    json!({"name":"Restart", "description":"Restart safely", "body":"Run the command.", "related_playbooks":[]})
}
fn prompts() -> PromptLoader {
    PromptLoader::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
    )
}
fn research(calls: Arc<AtomicUsize>) -> impl frona_harness::Tool + 'static {
    frona_harness::tool(
        "read_transcript",
        "Read selected source context",
        move |_: Value| {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Ok::<_, frona_harness::ToolError>("recorded source") }
        },
    )
    .unwrap()
}

#[tokio::test]
async fn missing_content_is_paired_before_explicit_acceptance() {
    let (harness, requests) = fixture(
        vec![
            vec![call(
                "bad",
                "submit",
                json!({"name":" ", "description":"", "body":"\n"}),
            )],
            vec![call("good", "submit", valid())],
        ],
        1,
    );
    let prompts = prompts();
    let run = harness
        .run("author")
        .structured::<AuthoredPlaybook>()
        .submission_attempts(2);
    let report = run.report();
    let accepted = run
        .inspect(async |candidate| inspect_submission(candidate, &prompts))
        .await
        .unwrap();
    assert_eq!(accepted.body, "Run the command.");
    let requests = requests.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let paired = requests[1].messages.iter().any(|message| match message {
        Message::User { content } => content.iter().any(|item| match item {
            UserContent::ToolResult(result) if result.call.to_string() == "bad" => {
                let feedback = format!("{:?}", result.content);
                ["name", "description", "body"]
                    .iter()
                    .all(|field| feedback.contains(field))
            }
            _ => false,
        }),
        _ => false,
    });
    assert!(
        paired,
        "required content feedback must answer the rejected submit call"
    );
    assert!(
        report
            .snapshot()
            .feedback
            .iter()
            .any(|feedback| !feedback.issued_in.is_empty())
    );
}

#[tokio::test]
async fn ending_exploration_rejects_unoffered_calls_before_acceptance() {
    let (harness, requests) = fixture(
        vec![
            vec![call("read1", "read_transcript", json!({}))],
            vec![
                call("read2", "read_transcript", json!({})),
                call("shell", "shell", json!({})),
            ],
            vec![call("accepted", "submit", valid())],
        ],
        1,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let prompts = prompts();
    let run = harness
        .run("author")
        .tool(research(calls.clone()))
        .exploration_turns(1)
        .structured::<AuthoredPlaybook>()
        .submission_attempts(2);
    let report = run.report();
    run.inspect(async |candidate| inspect_submission(candidate, &prompts))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let requests = requests.0.lock().unwrap();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "read_transcript")
    );
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.tools.len() == 1 && request.tools[0].name == "submit")
    );
    assert_eq!(
        report
            .snapshot()
            .actions
            .iter()
            .filter(|action| action.outcome == Some(frona_harness::ActionOutcome::Rejected))
            .count(),
        2
    );
}

#[tokio::test]
async fn exploration_and_submission_phases_cannot_bypass_shared_admission() {
    for (limits, expected_tools) in [
        (
            BudgetLimits {
                model_attempts: Some(1),
                ..Default::default()
            },
            1,
        ),
        (
            BudgetLimits {
                tool_calls: Some(0),
                ..Default::default()
            },
            0,
        ),
        (
            BudgetLimits {
                tokens: Some(1),
                ..Default::default()
            },
            0,
        ),
    ] {
        let (harness, requests) =
            fixture(vec![vec![call("read", "read_transcript", json!({}))]], 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let prompts = prompts();
        let incomplete = harness
            .run("author")
            .tool(research(calls.clone()))
            .budget(frona_harness::DefaultBudget::new(limits))
            .exploration_turns(1)
            .structured::<AuthoredPlaybook>()
            .submission_attempts(2)
            .inspect(async |candidate| inspect_submission(candidate, &prompts))
            .await
            .unwrap_err();
        assert!(matches!(
            incomplete.report.outcome,
            Some(RunOutcome::Incomplete {
                kind: FailureKind::Budget,
                ..
            })
        ));
        assert!(incomplete.retained.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), expected_tools);
        assert_eq!(requests.0.lock().unwrap().len(), 1);
    }
}
