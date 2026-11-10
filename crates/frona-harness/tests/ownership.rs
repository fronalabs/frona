#[path = "helpers/ownership.rs"]
mod ownership;

use frona_harness::{Model, ModelRequest, ToolContext, ToolRegistry, UsageReport};
use ownership::{ApplicationClient, LendingTool};
use serde_json::json;

#[tokio::test]
async fn borrowed_dependencies_leave_application_state_reusable() {
    let mut response = String::from("application response");
    let mut observations = Vec::new();
    {
        let model = Model::from_adapter("local", "fixture", ApplicationClient(&response)).unwrap();
        let output = model.complete(ModelRequest::new("hello")).await.unwrap();
        assert!(matches!(output.usage, UsageReport::Unavailable));
        let mut tools = ToolRegistry::default();
        tools.register(LendingTool::new(&mut observations)).unwrap();
        tools
            .get_mut("observe")
            .unwrap()
            .call(json!({"result": 1}), &ToolContext::default())
            .await
            .unwrap();
    }
    response.push_str(" reused");
    assert_eq!(observations, ["{\"result\":1}"]);
    observations.push(response);
    assert_eq!(observations.len(), 2);
}

#[tokio::test]
async fn application_owned_task_can_lend_its_local_state() {
    let observations = tokio::spawn(async {
        let response = String::from("application response");
        let mut observations = Vec::new();
        {
            let model =
                Model::from_adapter("local", "fixture", ApplicationClient(&response)).unwrap();
            model.complete(ModelRequest::new("hello")).await.unwrap();
            let mut tools = ToolRegistry::default();
            tools.register(LendingTool::new(&mut observations)).unwrap();
            tools
                .get_mut("observe")
                .unwrap()
                .call(json!({}), &ToolContext::default())
                .await
                .unwrap();
        }
        observations
    })
    .await
    .unwrap();
    assert_eq!(observations, ["{}"]);
}

#[tokio::test]
async fn one_configuration_supports_text_typed_and_manual_execution() {
    use ownership::{Answer, configured_run, model};

    let model = model("shared answer");
    let mut automatic_observations = Vec::new();
    let automatic = configured_run(model.clone(), &mut automatic_observations)
        .await
        .unwrap();
    let mut typed_observations = Vec::new();
    let typed: Answer = configured_run(model.clone(), &mut typed_observations)
        .structured()
        .await
        .unwrap();
    let mut manual_observations = Vec::new();
    let manual = {
        let mut session = configured_run(model, &mut manual_observations).manual();
        loop {
            let turn = session.call_model().await.unwrap();
            if turn.actions.is_empty() {
                break session.finish().unwrap();
            }
            // The owned turn/actions retain no borrow of the mutable session.
            for action in turn.actions {
                session.execute(action).await.unwrap();
            }
        }
    };
    assert_eq!(automatic, "shared answer");
    assert_eq!(typed.value, automatic);
    assert_eq!(manual, automatic);
    assert_eq!(automatic_observations, ["{}"]);
    assert_eq!(typed_observations, automatic_observations);
    assert_eq!(manual_observations, automatic_observations);
}

#[tokio::test]
async fn borrowed_model_tool_and_inspector_transform_the_accepted_type() {
    use ownership::{Accepted, Answer, Inspector, configured_run, model};

    let mut response = String::from("submitted");
    let mut observations = Vec::new();
    let mut inspector = Inspector::default();
    let accepted: Accepted = configured_run(model(&response), &mut observations)
        .structured::<Answer>()
        .inspect(async |answer| inspector.inspect(answer).await)
        .await
        .unwrap();
    assert_eq!(accepted, Accepted("SUBMITTED".into()));
    assert_eq!(inspector.seen, ["submitted", "submitted"]);
    assert_eq!(observations, ["{}"]);
    response.push_str(" reused");
    observations.push(response);
    inspector.seen.clear();
    assert_eq!(observations[1], "submitted reused");
}

#[tokio::test]
async fn local_inspection_accepts_non_send_state_and_non_send_output() {
    use frona_harness::{Decision, Harness};
    use ownership::{Answer, model};
    use std::{cell::Cell, rc::Rc};

    let inspections = Rc::new(Cell::new(0));
    let accepted: Rc<String> = Harness::new(model("local"))
        .run("inspect locally")
        .structured::<Answer>()
        .inspect(async |answer| {
            tokio::task::yield_now().await;
            inspections.set(inspections.get() + 1);
            Ok::<_, ()>(Decision::accept(Rc::new(answer.value)))
        })
        .await
        .unwrap();
    assert_eq!(accepted.as_str(), "local");
    assert_eq!(inspections.get(), 1);
    inspections.set(0);
}

#[tokio::test]
async fn owned_run_and_stateful_inspector_are_spawn_safe() {
    use frona_harness::{Harness, tool};
    use ownership::{Accepted, Answer, Inspector, model};
    use std::sync::{Arc, Mutex};

    let observations = Arc::new(Mutex::new(Vec::new()));
    let recorded = observations.clone();
    let run = Harness::new(model("owned"))
        .run("inspect owned dependencies")
        .tool(
            tool("observe", "Observe", move |arguments: serde_json::Value| {
                recorded.lock().unwrap().push(arguments.to_string());
                async { Ok("recorded") }
            })
            .unwrap(),
        )
        .structured::<Answer>();
    let mut inspector = Inspector::default();
    let result = tokio::spawn(async move {
        run.inspect(async move |answer| inspector.inspect(answer).await)
            .await
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap(), Accepted("OWNED".into()));
    assert_eq!(*observations.lock().unwrap(), ["{}"]);
}
