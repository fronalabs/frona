use frona_harness::{Model, ModelRequest, ToolContext, ToolRegistry, UsageReport};
use frona_standalone_consumer::{ApplicationClient, LendingTool};
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
