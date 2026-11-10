use super::*;
use crate::{
    CompactionLimits, DefaultBudget, ExecutionControl, Model, ModelAdapter, ModelRequest,
    inference::{InferenceError, observation::Observations, provider::StreamToken},
};

struct Answer;
#[async_trait::async_trait]
impl ModelAdapter for Answer {
    fn capabilities(&self) -> crate::Capabilities {
        crate::Capabilities::default()
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        Ok(output("answer"))
    }
}
fn output(text: &str) -> ModelOutput {
    ModelOutput::new(
        vec![AssistantContent::text(text)],
        crate::UsageReport::Unavailable,
    )
}

#[tokio::test]
async fn projected_input_preserves_compacted_positions_and_never_delivers_summary_or_changed_feedback()
 {
    let budget = DefaultBudget::default();
    let report = ReportHandle::default();
    let execution = ExecutionControl::new(&budget).with_report(&report);
    let mut conversation = Conversation::from_execution(execution);
    conversation
        .push_user(vec![UserContent::text("old input ".repeat(100))])
        .unwrap();
    conversation
        .record_response(output(&"old answer ".repeat(100)), vec![])
        .unwrap();
    conversation
        .push_user(vec![UserContent::text("current input")])
        .unwrap();
    let compacted = conversation
        .model_view()
        .unwrap()
        .plan(
            "",
            &[],
            CompactionLimits {
                available_input_tokens: 400,
                target_input_tokens: 60,
            },
        )
        .unwrap()
        .unwrap()
        .finish("summary", None)
        .unwrap();
    conversation
        .commit_compaction(compacted, "summary model".into(), output("summary"))
        .unwrap();
    let model = Model::from_adapter("fake", "projection", Answer).unwrap();
    let literal = conversation.request_messages().unwrap();
    assert_eq!(literal.len(), 2);
    for changed in [true, false] {
        let mut projection = literal.clone();
        if changed {
            projection[1] = Message::user("transformed current input");
        }
        let input = conversation.projected_input(&projection).unwrap();
        let mut request = ModelRequest::new("");
        request.messages = projection;
        model
            .invoke_controlled(
                request,
                None,
                Observations::default(),
                input.execution(execution).unwrap(),
            )
            .await
            .unwrap();
    }
    let snapshot = report.snapshot();
    assert!(
        snapshot.requests[0]
            .input
            .as_ref()
            .unwrap()
            .messages
            .is_empty()
    );
    assert_eq!(snapshot.requests[1].input.as_ref().unwrap().messages, [2]);
    assert!(
        snapshot
            .feedback
            .iter()
            .find(|feedback| feedback.message == Some(0))
            .unwrap()
            .issued_in
            .is_empty()
    );
    assert_eq!(
        snapshot
            .feedback
            .iter()
            .find(|feedback| feedback.message == Some(2))
            .unwrap()
            .issued_in,
        [crate::AttemptId(1)]
    );
    assert_eq!(conversation.messages().len(), 3);
}

#[test]
fn projected_input_rejects_foreign_scope_prefix_and_roles_and_pending_proposals() {
    let budget = DefaultBudget::default();
    let report = ReportHandle::default();
    let execution = ExecutionControl::new(&budget).with_report(&report);
    let mut conversation = Conversation::from_execution(execution);
    conversation
        .push_user(vec![UserContent::text("input")])
        .unwrap();
    assert!(
        conversation
            .projected_input(&[Message::user("prefix"), Message::user("input")])
            .is_err()
    );
    assert!(
        conversation
            .projected_input(&[Message::assistant("foreign role")])
            .is_err()
    );
    let input = conversation
        .projected_input(conversation.messages())
        .unwrap();
    let foreign = ReportHandle::default();
    assert!(input.execution(execution.with_report(&foreign)).is_err());
    let call = ToolCall::from_wire(
        "id",
        rig_core::completion::message::ToolFunction::new(
            "tool".try_into().unwrap(),
            serde_json::json!({}),
        ),
    );
    conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(call)],
                crate::UsageReport::Unavailable,
            ),
            vec![],
        )
        .unwrap();
    assert!(matches!(
        conversation.projected_input(conversation.messages()),
        Err(ConversationError::PendingActions(1))
    ));
}
