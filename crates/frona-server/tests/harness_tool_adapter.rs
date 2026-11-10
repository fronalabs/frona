use async_trait::async_trait;
use frona_harness::{ExecutionStop, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};

#[tokio::test]
async fn runtime_adapter_requires_explicit_context_and_preserves_output_for_mapping() {
    use frona::{
        inference::request::InferenceContext,
        tool::{AgentTool, sdk_adapter::AgentToolAdapter},
    };
    struct Legacy;
    #[async_trait]
    impl AgentTool for Legacy {
        fn name(&self) -> &str {
            "legacy"
        }
        fn definitions(&self) -> Vec<frona::tool::ToolDefinition> {
            vec![frona::tool::ToolDefinition {
                id: "legacy_read".into(),
                provider_id: "legacy".into(),
                description: "read".into(),
                parameters: json!({"type":"object"}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _: Value,
            context: &InferenceContext,
        ) -> Result<frona::tool::ToolOutput, frona::core::error::AppError> {
            assert_eq!(name, "legacy_read");
            assert_eq!(context.user.id, "runtime-user");
            Ok(frona::tool::ToolOutput::mixed(
                "legacy text",
                vec![frona_harness::tools::files::image::ImageData {
                    bytes: vec![1, 2],
                    media_type: "image/png".into(),
                }],
            )
            .with_attachment(frona::storage::Attachment {
                filename: "result.png".into(),
                content_type: "image/png".into(),
                size_bytes: 2,
                owner: "runtime-user".into(),
                path: "result.png".into(),
                url: None,
            })
            .with_system_prompt("runtime update")
            .as_pending_external())
        }
    }
    // Real persisted model types remain entirely on this explicit runtime side.
    let user = serde_json::from_value(json!({
        "id":"runtime-user","handle":"user","email":"user@example.test","name":"User","password_hash":"unused",
        "created_at":"2026-11-09T00:00:00Z","updated_at":"2026-11-09T00:00:00Z"
    })).unwrap();
    let agent = serde_json::from_value(json!({
        "id":"runtime-agent","user_id":"runtime-user","handle":"agent","name":"Agent","description":"test","model_group":"test","enabled":true,
        "created_at":"2026-11-09T00:00:00Z","updated_at":"2026-11-09T00:00:00Z"
    })).unwrap();
    let context =
        InferenceContext::new_detached(user, agent, Default::default(), Default::default());
    let mut mapped = false;
    {
        let mut adapter = AgentToolAdapter::new(
            &Legacy,
            &context,
            "legacy_read",
            |output: frona::tool::ToolOutput| {
                assert!(output.is_pending_external());
                assert_eq!(output.system_prompt(), Some("runtime update"));
                assert_eq!(output.attachments()[0].owner, "runtime-user");
                assert_eq!(output.images()[0].bytes, [1, 2]);
                mapped = true;
                // The integration chooses how to handle runtime effects. There is
                // intentionally no blanket conversion that would discard them.
                Err(ExecutionStop::Stopped("runtime pending work".into()).into())
            },
        )
        .unwrap();
        assert!(matches!(
            adapter.call(json!({}), &ToolContext::default()).await,
            Err(ToolError::Stop(ExecutionStop::Stopped(_)))
        ));
    }
    assert!(mapped);
    assert!(
        AgentToolAdapter::new(
            &Legacy,
            &context,
            "not_advertised",
            |_: frona::tool::ToolOutput| Ok(ToolOutput::text("unused"))
        )
        .is_err()
    );
}
