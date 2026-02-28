use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use frona::core::metrics::{self, InferenceMetricsContext};
use frona::inference::config::{ModelGroup, RetryConfig};
use frona::inference::error::InferenceError;
use frona::inference::provider::{ModelProvider, ModelRef};
use frona::inference::registry::ModelProviderRegistry;
use frona::inference::Usage;
use frona::tool::{AgentTool, ToolContext, ToolDefinition, ToolOutput, ToolType};
use rig::completion::request::ToolDefinition as RigToolDefinition;
use rig::completion::{AssistantContent, Message as RigMessage};
use rig::completion::message::{ToolCall, ToolFunction};
use serde_json::Value;
use tokio::sync::mpsc;

pub enum MockResponse {
    Text(String),
    ToolCalls(Vec<(String, String, Value)>),
    Error(InferenceError),
}

pub struct MockModelProvider {
    responses: Mutex<Vec<MockResponse>>,
    pub call_count: Mutex<usize>,
}

impl MockModelProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: Mutex::new(0),
        }
    }

    fn next_response(&self) -> MockResponse {
        let mut responses = self.responses.lock().unwrap();
        *self.call_count.lock().unwrap() += 1;
        if responses.is_empty() {
            MockResponse::Text("default response".into())
        } else {
            responses.remove(0)
        }
    }

    pub fn calls(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

#[async_trait]
impl ModelProvider for MockModelProvider {
    async fn inference(
        &self,
        _model_id: &str,
        _system_prompt: &str,
        _chat_history: Vec<RigMessage>,
        _tools: Vec<RigToolDefinition>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<(Vec<AssistantContent>, Usage), InferenceError> {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_input_tokens: 0,
        };
        match self.next_response() {
            MockResponse::Text(t) => Ok((vec![AssistantContent::text(&t)], usage)),
            MockResponse::ToolCalls(calls) => {
                let contents = calls
                    .into_iter()
                    .map(|(id, name, args)| {
                        AssistantContent::ToolCall(ToolCall::new(id, ToolFunction::new(name, args)))
                    })
                    .collect();
                Ok((contents, usage))
            }
            MockResponse::Error(e) => Err(e),
        }
    }

    async fn stream_inference(
        &self,
        _model_id: &str,
        _system_prompt: &str,
        _chat_history: Vec<RigMessage>,
        _tools: Vec<RigToolDefinition>,
        token_tx: mpsc::Sender<String>,
        _max_tokens: Option<u64>,
        _temperature: Option<f64>,
    ) -> Result<Vec<AssistantContent>, InferenceError> {
        match self.next_response() {
            MockResponse::Text(t) => {
                let _ = token_tx.send(t.clone()).await;
                Ok(vec![AssistantContent::text(t)])
            }
            MockResponse::ToolCalls(calls) => {
                let contents = calls
                    .into_iter()
                    .map(|(id, name, args)| {
                        AssistantContent::ToolCall(ToolCall::new(id, ToolFunction::new(name, args)))
                    })
                    .collect();
                Ok(contents)
            }
            MockResponse::Error(e) => Err(e),
        }
    }
}

pub struct MockInternalTool {
    pub tool_name: String,
    responses: Mutex<Vec<String>>,
}

impl MockInternalTool {
    pub fn new(name: &str, responses: Vec<String>) -> Self {
        Self {
            tool_name: name.to_string(),
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl AgentTool for MockInternalTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.clone(),
            description: format!("Mock tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        let mut responses = self.responses.lock().unwrap();
        let text = if responses.is_empty() {
            "mock result".to_string()
        } else {
            responses.remove(0)
        };
        Ok(ToolOutput::text(text))
    }
}

pub struct MockExternalTool {
    pub tool_name: String,
}

impl MockExternalTool {
    pub fn new(name: &str) -> Self {
        Self {
            tool_name: name.to_string(),
        }
    }
}

#[async_trait]
impl AgentTool for MockExternalTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.clone(),
            description: format!("External tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    fn tool_type(&self, _tool_name: &str) -> ToolType {
        ToolType::External
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Ok(ToolOutput::text("external result"))
    }
}

pub struct MockFailingTool {
    pub tool_name: String,
}

impl MockFailingTool {
    pub fn new(name: &str) -> Self {
        Self {
            tool_name: name.to_string(),
        }
    }
}

#[async_trait]
impl AgentTool for MockFailingTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.clone(),
            description: format!("Failing tool {}", self.tool_name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Err(frona::core::error::AppError::Tool("tool failed".into()))
    }
}

pub fn mock_context() -> ToolContext {
    let (tx, _rx) = mpsc::channel(100);
    ToolContext {
        user: frona::core::models::user::User {
            id: "test-user".into(),
            username: "testuser".into(),
            email: "test@test.com".into(),
            name: "Test".into(),
            password_hash: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        agent: frona::agent::models::Agent {
            id: "test-agent".into(),
            user_id: Some("test-user".into()),
            name: "Test Agent".into(),
            description: String::new(),
            model_group: "primary".into(),
            enabled: true,
            tools: vec![],
            sandbox_config: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: Default::default(),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        chat: frona::chat::models::Chat {
            id: "test-chat".into(),
            user_id: "test-user".into(),
            space_id: None,
            task_id: None,
            agent_id: "test-agent".into(),
            title: None,
            archived_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        event_tx: tx,
    }
}

pub fn test_model_group() -> ModelGroup {
    ModelGroup {
        name: "test".into(),
        main: ModelRef { provider: "mock".into(), model_id: "test-model".into() },
        fallbacks: vec![],
        max_tokens: Some(4096),
        temperature: None,
        context_window: Some(128_000),
        retry: RetryConfig {
            max_retries: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            max_backoff_ms: 10,
        },
        inference: Default::default(),
    }
}

pub fn test_model_group_with_fallback(fallback_provider: &str, fallback_model: &str) -> ModelGroup {
    let mut group = test_model_group();
    group.fallbacks.push(ModelRef {
        provider: fallback_provider.into(),
        model_id: fallback_model.into(),
    });
    group
}

pub fn test_metrics_ctx() -> InferenceMetricsContext {
    InferenceMetricsContext {
        user_id: "test-user".into(),
        agent_id: "test-agent".into(),
        model_group: "test".into(),
    }
}

pub fn test_registry_with_provider(
    name: &str,
    provider: Arc<dyn ModelProvider>,
) -> ModelProviderRegistry {
    let mut providers = HashMap::new();
    providers.insert(name.to_string(), provider);
    let model_groups = HashMap::new();
    ModelProviderRegistry::for_testing(providers, model_groups)
}

pub fn init_metrics() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        metrics::setup_metrics_recorder();
    });
}
