//! Server provider configuration, prompt overrides, cancellation and usage for portable phases.

use std::sync::Arc;

use crate::agent::harness::Harness;
use crate::core::error::AppError;
use crate::inference::usage::{InferenceKind, UsageContext};
use frona_harness::inference::ModelGroup;
use frona_text::prompt::PromptLoader;
use tokio_util::sync::CancellationToken;

/// Background inference for one consolidation pass, scoped to one user.
#[derive(Clone)]
pub struct ConsolidationInference {
    harness: Arc<Harness>,
    model_group: ModelGroup,
    prompts: PromptLoader,
    /// Carried so usage is attributed without reaching back into the pass scope.
    usage: UsageContext,
    cancel_token: CancellationToken,
}

impl ConsolidationInference {
    #[cfg(test)]
    pub fn new(
        harness: Arc<Harness>,
        model_group: ModelGroup,
        prompts: PromptLoader,
        user_id: String,
    ) -> Self {
        Self::with_cancel_token(
            harness,
            model_group,
            prompts,
            user_id,
            CancellationToken::new(),
        )
    }

    pub fn with_cancel_token(
        harness: Arc<Harness>,
        model_group: ModelGroup,
        prompts: PromptLoader,
        user_id: String,
        cancel_token: CancellationToken,
    ) -> Self {
        let usage = UsageContext::new(InferenceKind::Memory, &user_id, model_group.name.clone());
        Self {
            harness,
            model_group,
            prompts,
            usage,
            cancel_token,
        }
    }

    /// Build a portable phase harness while retaining server provider resolution.
    pub fn harness(&self) -> frona_harness::Harness<'static> {
        frona_harness::Harness::new(frona_harness::Model::from_group(self.model_group.clone()))
    }

    /// Attach host usage attribution and cancellation to a phase-owned run.
    pub fn configure<'a, T>(&'a self, run: frona_harness::Run<'a, T>) -> frona_harness::Run<'a, T> {
        run.observer(self.harness.usage_service.scoped(&self.usage))
            .cancellation(self.cancel_token.clone())
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Keep server prompt overrides while using the library's embedded defaults.
    pub fn prompt_source(&self, name: &str, fallback: &str) -> Result<String, AppError> {
        Ok(self
            .prompts
            .try_read_raw(name)?
            .unwrap_or_else(|| fallback.to_owned()))
    }

    pub fn prompt_defaults(&self) -> std::collections::HashMap<String, String> {
        self.prompts.defaults().clone()
    }

    /// Domain writes outside an SDK run must also respect caller cancellation.
    pub(crate) fn check_cancelled(&self) -> Result<(), AppError> {
        if self.cancel_token.is_cancelled() {
            return Err(AppError::Internal("PKM inference cancelled".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::models::User, core::repository::Repository, db::repo::generic::SurrealRepo,
        inference::usage::InferenceUsage,
    };
    use chrono::Utc;
    use frona_harness::{
        Decision, Handle,
        config::{ProviderModel, RetryConfig},
        inference::{
            InferenceError, ModelConfig, Usage,
            provider::{ModelProvider, ModelRequestSettings, group::ModelGroup},
        },
    };
    use frona_text::prompt::PromptTemplate;
    use rig_core::completion::{
        AssistantContent, Message,
        message::{ToolCall, ToolFunction},
        request::ToolDefinition as Definition,
    };
    use serde_json::json;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use surrealdb::{
        Surreal,
        engine::local::{Db, Mem},
    };

    type RecordedRequest = (String, Vec<Message>, Vec<Definition>);

    #[derive(Clone)]
    struct Script {
        outputs: Arc<Mutex<VecDeque<Vec<AssistantContent>>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }
    impl Script {
        fn new(outputs: Vec<Vec<AssistantContent>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Arc::new(Mutex::new(outputs.into())),
                requests: Arc::new(Mutex::new(vec![])),
            })
        }
    }
    #[async_trait::async_trait]
    impl ModelProvider for Script {
        async fn resolve(
            &self,
            model: &ModelConfig,
        ) -> Result<frona_harness::inference::provider::ResolvedModel, InferenceError> {
            let script = self.clone();
            Ok(frona_harness::inference::provider::ResolvedModel::new(
                crate::rig_fixture::model(model.model_id.clone(), move |request, mode| {
                    assert!(matches!(mode, rig_core::wire::Mode::Unary));
                    let mut messages = request
                        .chat_history
                        .iter()
                        .map(|message| (**message).clone())
                        .collect::<Vec<_>>();
                    let system = match messages.remove(0) {
                        Message::System { content } => content,
                        _ => panic!("expected system prompt"),
                    };
                    script.requests.lock().unwrap().push((
                        system,
                        messages,
                        request.tools.iter().map(|tool| (**tool).clone()).collect(),
                    ));
                    let content = script
                        .outputs
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected model call");
                    crate::rig_fixture::reply(frona_harness::ModelOutput::new(
                        content,
                        frona_harness::UsageReport::Reported(
                            Usage::new()
                                .input_tokens(Some(10))
                                .output_tokens(Some(5))
                                .total_tokens(Some(15)),
                        ),
                    ))
                }),
            ))
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
        std::fs::write(
            path.join("input.md"),
            "Inspect {{input}} using {{schema_path}}",
        )
        .unwrap();
        let inference = ConsolidationInference::new(
            app.state.harness.clone(),
            group,
            PromptLoader::new(prompts.path()).with_var("schema_path", "/runtime/schema"),
            "u1".into(),
        );
        Fixture {
            _app: app,
            _prompts: prompts,
            db,
            inference,
        }
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
    async fn configured_runs_use_prompt_overrides_and_attribute_usage_without_chat_rows() {
        let provider = Script::new(vec![submit(2), submit(8)]);
        let f = fixture(provider.clone()).await;
        let system = f
            .inference
            .prompt_source("pkm/sdk-fixture/system.md", "embedded")
            .unwrap();
        let source = f
            .inference
            .prompt_source("pkm/sdk-fixture/input.md", "embedded")
            .unwrap();
        let prompt = PromptTemplate::new("pkm/sdk-fixture/input.md", &["input"])
            .render_source_with_defaults(
                &source,
                &[("input", "facts")],
                &f.inference.prompt_defaults(),
            )
            .unwrap();
        let harness = f.inference.harness();
        let value = f
            .inference
            .configure(harness.run(prompt).instructions(system))
            .structured::<Answer>()
            .inspect(async |answer| {
                Ok::<_, ()>(if answer.value == 8 {
                    Decision::accept(answer.value)
                } else {
                    Decision::revise("try again")
                })
            })
            .await
            .unwrap();
        assert_eq!(value, 8);
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0, "Strict PKM instructions");
            assert!(format!("{:?}", requests[0].1).contains("Inspect facts using /runtime/schema"));
            assert!(format!("{:?}", requests[1].1).contains("try again"));
        }
        let rows: Vec<InferenceUsage> =
            f.db.query("SELECT *, meta::id(id) AS id FROM inference_usage")
                .await
                .unwrap()
                .take(0)
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.user_id == "u1"
            && row.chat_id.is_none()
            && row.kind_tag == "Memory"
            && row.model_group == "prepared"
            && row.total_tokens == 15));
        no_chat_rows(&f).await;
    }

    #[tokio::test]
    async fn cancellation_reaches_the_portable_ingest_phase_before_model_execution() {
        let provider = Script::new(vec![]);
        let f = fixture(provider.clone()).await;
        f.inference.cancel_token.cancel();
        let phase = frona_pkm::ingest::Ingest::new(f.inference.harness());
        let error = phase
            .run_with(
                frona_pkm::ingest::IngestInput {
                    transcript: "[m1] User: Hello".into(),
                    ..Default::default()
                },
                |run| f.inference.configure(run),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, frona_pkm::Error::Cancelled(_)), "{error}");
        assert!(provider.requests.lock().unwrap().is_empty());
        assert!(f.inference.check_cancelled().is_err());
        no_chat_rows(&f).await;
    }

    #[tokio::test]
    async fn prompt_sources_fall_back_only_when_absent() {
        let provider = Script::new(vec![]);
        let f = fixture(provider.clone()).await;
        assert_eq!(
            f.inference
                .prompt_source("absent.md", "embedded source")
                .unwrap(),
            "embedded source"
        );
        std::fs::create_dir(f._prompts.path().join("unreadable.md")).unwrap();
        assert!(
            f.inference
                .prompt_source("unreadable.md", "embedded source")
                .is_err()
        );
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}
