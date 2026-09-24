mod helpers;

use frona::{
    chat::service::ChatService, core::repository::Repository, db::repo::generic::SurrealRepo,
};
use helpers::{MockModelProvider, MockResponse, mock_context, test_model_group};
use std::sync::Arc;

#[tokio::test]
async fn title_override_and_fallback_defaults_reach_the_provider_transport() {
    use serde_json::{Value, json};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &wiremock::Request| {
        let body: Value = request.body_json().unwrap();
        if body["model"] == "main" { return ResponseTemplate::new(502); }
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"fixture","object":"chat.completion","created":0,"model":"backup",
            "choices":[{"index":0,"message":{"role":"assistant","content":"{\"title\":\"Wire title\"}"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}
        }))
    }).mount(&server).await;
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let fixture = helpers::app_state::build(&db).await;
    let state = &fixture.state;
    let ctx = mock_context();
    SurrealRepo::new(db.clone())
        .create(&ctx.user)
        .await
        .unwrap();
    SurrealRepo::new(db.clone())
        .create(&ctx.agent)
        .await
        .unwrap();
    SurrealRepo::new(db.clone())
        .create(ctx.chat.as_ref().unwrap())
        .await
        .unwrap();
    state
        .storage_service
        .agent_workspace(&ctx.user.handle, &ctx.agent.handle)
        .write("TITLE.md", "---\n---\nGive this conversation a title.")
        .unwrap();
    let config: frona::core::config::Config = serde_json::from_value(json!({
        "providers":{"work-openai":{"provider":"openai","base_url":server.uri(),"api_key":"fixture"}},
        "models":{"primary":{"provider":"work-openai","model":"main","api":"completions","max_tokens":8192,"temperature":0.7,
            "retry":{"max_retries":0},"fallbacks":[{"provider":"work-openai","model":"backup","api":"completions","max_tokens":2048,"temperature":0.3}]}}
    })).unwrap();
    let runtime = frona::inference::credential::runtime::RuntimeCredentials::new(
        config.providers.clone(),
        state.model_provider_service.store.clone(),
        frona::inference::provider::InferenceCounter::new(state.broadcast_service.clone()),
    );
    let groups = frona::inference::config::ModelRegistryConfig {
        providers: config.providers,
        models: config.models,
        skip_auto_discover: true,
    }
    .parse_model_groups(&config.inference, Arc::new(runtime.providers()))
    .unwrap();
    let service = helpers::test_model_service(runtime.providers(), groups).await;
    let chat = ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        state.agent_service.clone(),
        service.clone(),
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
    );
    assert_eq!(
        chat.generate_title(&ctx.chat.as_ref().unwrap().id, &ctx.agent.id, "hello")
            .await
            .unwrap(),
        "Wire title"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["max_completion_tokens"], 1024);
    }
    let group = service
        .resolve(&frona::inference::ModelRef::PRIMARY)
        .unwrap();
    let usage_ctx = helpers::test_usage_ctx();
    group
        .inference(frona::inference::ModelRequest {
            system_prompt: "fixture",
            history: vec![rig_core::completion::Message::user("hi")],
            tools: vec![],
            usage_service: &state.usage_service,
            usage_context: &usage_ctx,
            overrides: Default::default(),
        })
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[2..] {
        let body: Value = request.body_json().unwrap();
        let (max, temperature) = if body["model"] == "main" {
            (8192, 0.7)
        } else {
            (2048, 0.3)
        };
        assert_eq!(body["max_completion_tokens"], max);
        assert_eq!(body["temperature"], temperature);
    }
}

#[tokio::test]
async fn title_selection_is_explicit_and_missing_title_alone_uses_primary() {
    for (groups, metadata, expected, unavailable_title) in [
        (vec!["primary", "title"], "", Some("title"), false),
        (vec!["primary"], "", Some("primary"), false),
        (vec![], "", None, false),
        (
            vec!["primary", "title"],
            "model: primary\n",
            Some("primary"),
            false,
        ),
        (vec!["primary", "title"], "model: typo\n", None, false),
        (vec!["primary", "title"], "model: Primary\n", None, false),
        (
            vec!["primary", "title"],
            "model: mock/test-model\n",
            None,
            false,
        ),
        (vec!["primary"], "model: ''\n", Some("primary"), false),
        (vec!["primary", "title"], "", None, true),
    ] {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let fixture = helpers::app_state::build(&db).await;
        let state = &fixture.state;
        let ctx = mock_context();
        SurrealRepo::new(db.clone())
            .create(&ctx.user)
            .await
            .unwrap();
        SurrealRepo::new(db.clone())
            .create(&ctx.agent)
            .await
            .unwrap();
        SurrealRepo::new(db.clone())
            .create(ctx.chat.as_ref().unwrap())
            .await
            .unwrap();
        state
            .storage_service
            .agent_workspace(&ctx.user.handle, &ctx.agent.handle)
            .write(
                "TITLE.md",
                &format!("---\n{metadata}---\nGive this conversation a title."),
            )
            .unwrap();
        let providers = groups
            .iter()
            .map(|name| {
                let response = if unavailable_title && *name == "title" {
                    MockResponse::Error(
                        frona::inference::error::InferenceError::ProviderNotConfigured(
                            "title".into(),
                        ),
                    )
                } else {
                    MockResponse::Text(format!(r#"{{"title":"{name}"}}"#))
                };
                let provider = Arc::new(MockModelProvider::new(vec![response]));
                (
                    (*name).to_string(),
                    provider as Arc<dyn frona::inference::provider::ModelProvider>,
                )
            })
            .collect();
        let settings = groups
            .iter()
            .map(|name| {
                let mut group = test_model_group();
                group.name = (*name).into();
                group.main.provider_handle = frona::core::Handle::try_new(name).unwrap();
                ((*name).to_string(), group)
            })
            .collect();
        let service = helpers::test_model_service(providers, settings).await;
        let chat = ChatService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            state.agent_service.clone(),
            service,
            state.storage_service.clone(),
            state.user_service.clone(),
            state.prompts.clone(),
            state.broadcast_service.clone(),
            state.presign_service.clone(),
            state.usage_service.clone(),
        );
        let result = chat
            .generate_title(&ctx.chat.as_ref().unwrap().id, &ctx.agent.id, "hello")
            .await;
        match expected {
            Some(title) => assert_eq!(result.unwrap(), title, "{metadata}"),
            None => assert!(result.is_err(), "{metadata}"),
        }
    }
}
