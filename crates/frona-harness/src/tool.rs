//! Portable tool registration and invocation contracts. Dispatch policy lives
//! in the shared execution layer, not in the registry or typed helper.

use std::{collections::BTreeMap, future::Future, marker::PhantomData, pin::Pin};

use async_trait::async_trait;
use rig_core::completion::request::ToolDefinition;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ExecutionControl, ExecutionStop, Model, ModelError, ModelOutput, ModelRequest};
use crate::inference::{InferenceError, observation::Observations, provider::StreamToken};
use tokio::sync::mpsc;

pub use rig_core::completion::message::ToolResultContent as ToolContent;

pub type ToolFailure = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool failed: {0}")]
    Failed(#[source] ToolFailure),
    /// Ordinary unsuccessful output retaining rich model content and metadata.
    #[error("tool returned unsuccessful output")]
    Feedback(ToolOutput),
    #[error(transparent)]
    Stop(#[from] ExecutionStop),
}

impl ToolError {
    /// Wrap an ordinary error without demoting an already typed SDK stop.
    pub fn failed(error: impl Into<ToolFailure>) -> Self {
        match error.into().downcast::<Self>() {
            Ok(error) => *error,
            Err(error) => match error.downcast::<ExecutionStop>() {
                Ok(stop) => Self::Stop(*stop),
                Err(error) => match error.downcast::<ModelError>() {
                    Ok(error) => Self::from(*error),
                    Err(error) => match error.downcast::<InferenceError>() {
                        Ok(error) => Self::from(*error),
                        Err(error) => Self::Failed(error),
                    },
                },
            },
        }
    }
}

impl From<InferenceError> for ToolError {
    fn from(error: InferenceError) -> Self {
        match error.into_execution_stop() {
            Ok(stop) => Self::Stop(stop),
            Err(error) => Self::Failed(Box::new(error)),
        }
    }
}

impl From<ModelError> for ToolError {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::Provider(error) => error.into(),
            error => Self::Failed(Box::new(error)),
        }
    }
}

pub type ToolResult = Result<ToolOutput, ToolError>;

/// Call-local information with no persistent identity or services. Dispatch
/// attaches the caller's budget; nested model work never creates a new allowance.
/// A default context supports direct low-level tools, but nested work requires
/// a dispatcher-supplied execution control.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct ToolContext<'a> {
    pub call_id: Option<String>,
    pub cancellation: CancellationToken,
    execution: Option<ExecutionControl<'a>>,
}

impl std::fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("call_id", &self.call_id)
            .field("cancellation", &self.cancellation)
            .field("has_execution", &self.execution.is_some())
            .finish()
    }
}

impl<'a> ToolContext<'a> {
    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn for_execution(call_id: String, execution: ExecutionControl<'a>) -> Self {
        Self {
            call_id: Some(call_id),
            cancellation: execution.cancellation_token().unwrap_or_default(),
            execution: Some(execution),
        }
    }

    /// Reuse this tool's exact allowance and cancellation for a nested model.
    /// Errors retain typed control stops and can be propagated with `?`.
    pub async fn complete(
        &self,
        model: &Model<'_>,
        request: ModelRequest,
    ) -> Result<ModelOutput, ToolError> {
        self.invoke(model, request, None, Observations::default())
            .await
    }

    pub async fn invoke(
        &self,
        model: &Model<'_>,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
    ) -> Result<ModelOutput, ToolError> {
        model
            .invoke_controlled(request, tokens, observations, self.execution()?)
            .await
            .map_err(Into::into)
    }

    /// Borrow the same controls for other budget-aware SDK operations.
    pub fn execution(&self) -> Result<ExecutionControl<'_>, ToolError> {
        let execution = self.execution.ok_or_else(|| {
            ExecutionStop::Stopped(
                "nested model work requires a dispatcher execution context".into(),
            )
        })?;
        Ok(execution.with_cancellation(&self.cancellation))
    }
}

/// Typed protocol content is retained unchanged, including image fields and
/// explicit JSON. Metadata is application-owned and is not model-facing text.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
    pub metadata: Value,
}

impl ToolOutput {
    pub fn content(content: Vec<ToolContent>) -> Self {
        Self {
            content,
            metadata: Value::Null,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::content(vec![ToolContent::Text(text.into().into())])
    }

    pub fn json(value: impl Serialize) -> Result<Self, ToolError> {
        let value = serde_json::to_value(value).map_err(ToolError::failed)?;
        Ok(Self::content(vec![ToolContent::Json { value }]))
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Ordinary serializable results become explicit JSON; a rich ToolOutput keeps
/// its native content and metadata instead of being serialized a second time.
pub trait IntoToolOutput {
    fn into_tool_output(self) -> ToolResult;
}

impl<T: Serialize> IntoToolOutput for T {
    fn into_tool_output(self) -> ToolResult {
        ToolOutput::json(self)
    }
}

impl IntoToolOutput for ToolOutput {
    fn into_tool_output(self) -> ToolResult {
        Ok(self)
    }
}

/// A tool may borrow mutable application state. Invocation lends a Send future
/// from &mut self; neither Sync nor static state is required.
#[async_trait]
pub trait Tool: Send {
    fn definition(&self) -> &ToolDefinition;
    async fn call(&mut self, arguments: Value, context: &ToolContext<'_>) -> ToolResult;
}

#[derive(Debug, thiserror::Error)]
pub enum ToolRegistrationError {
    #[error("tool name must not be blank or contain surrounding whitespace")]
    InvalidName,
    #[error("tool name '{0}' is reserved for structured submission")]
    ReservedName(String),
    #[error("tool '{0}' is already registered")]
    Duplicate(String),
    #[error("invalid tool argument schema: {0}")]
    Schema(String),
}

fn validate_name(name: &str) -> Result<(), ToolRegistrationError> {
    if name.trim().is_empty() || name.trim() != name {
        return Err(ToolRegistrationError::InvalidName);
    }
    if name == crate::inference::provider::SUBMIT_TOOL_NAME {
        return Err(ToolRegistrationError::ReservedName(name.into()));
    }
    Ok(())
}

/// Create a typed tool without a database or runtime context. The callback's
/// future may capture borrowed application references, but cannot lend a mutable
/// capture from FnMut across await. Implement Tool for that lending shape.
pub fn tool<A, O, F, Fut>(
    name: &str,
    description: &str,
    handler: F,
) -> Result<impl Tool + use<A, O, F, Fut>, ToolRegistrationError>
where
    A: DeserializeOwned + JsonSchema + Send,
    O: IntoToolOutput,
    F: FnMut(A) -> Fut + Send,
    Fut: Future<Output = Result<O, ToolError>> + Send,
{
    typed_tool(name.into(), description.into(), PlainHandler(handler))
}

/// A context-aware callback may keep borrowed execution controls across await.
/// Box only this future (`Box::pin(async move { ... })`); ordinary `tool` callbacks
/// remain unboxed. Implement `Tool` for callbacks lending mutable captured state.
pub fn tool_with_context<A, O, F>(
    name: &str,
    description: &str,
    handler: F,
) -> Result<impl Tool + use<A, O, F>, ToolRegistrationError>
where
    A: DeserializeOwned + JsonSchema + Send,
    O: IntoToolOutput,
    F: for<'ctx> FnMut(A, ToolContext<'ctx>) -> ToolFuture<'ctx, O> + Send,
{
    typed_tool(name.into(), description.into(), ContextHandler(handler))
}

pub type ToolFuture<'a, O> = Pin<Box<dyn Future<Output = Result<O, ToolError>> + Send + 'a>>;

pub(super) fn typed_tool<A: JsonSchema, H>(
    name: String,
    description: String,
    handler: H,
) -> Result<TypedTool<A, H>, ToolRegistrationError> {
    validate_name(&name)?;
    let parameters = schemars::schema_for!(A).to_value();
    let validator = jsonschema::validator_for(&parameters)
        .map_err(|error| ToolRegistrationError::Schema(error.to_string()))?;
    Ok(TypedTool {
        definition: ToolDefinition {
            name,
            description,
            parameters,
        },
        validator,
        handler,
        arguments: PhantomData,
    })
}

pub struct TypedTool<A, H> {
    definition: ToolDefinition,
    validator: jsonschema::Validator,
    handler: H,
    arguments: PhantomData<fn(A)>,
}

pub(super) trait TypedHandler<A>: Send {
    fn call(
        &mut self,
        arguments: A,
        context: &ToolContext<'_>,
    ) -> impl Future<Output = ToolResult> + Send;
}

struct PlainHandler<F>(F);
struct ContextHandler<F>(F);

impl<A, O, F, Fut> TypedHandler<A> for PlainHandler<F>
where
    A: Send,
    O: IntoToolOutput,
    F: FnMut(A) -> Fut + Send,
    Fut: Future<Output = Result<O, ToolError>> + Send,
{
    async fn call(&mut self, arguments: A, _context: &ToolContext<'_>) -> ToolResult {
        (self.0)(arguments).await?.into_tool_output()
    }
}

impl<A, O, F> TypedHandler<A> for ContextHandler<F>
where
    A: Send,
    O: IntoToolOutput,
    F: for<'ctx> FnMut(A, ToolContext<'ctx>) -> ToolFuture<'ctx, O> + Send,
{
    async fn call(&mut self, arguments: A, context: &ToolContext<'_>) -> ToolResult {
        (self.0)(arguments, context.clone())
            .await?
            .into_tool_output()
    }
}

#[async_trait]
impl<A, H> Tool for TypedTool<A, H>
where
    A: DeserializeOwned + Send,
    H: TypedHandler<A>,
{
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&mut self, arguments: Value, context: &ToolContext<'_>) -> ToolResult {
        self.validator
            .validate(&arguments)
            .map_err(|error| ToolError::failed(error.to_string()))?;
        let input = serde_json::from_value(arguments).map_err(ToolError::failed)?;
        self.handler.call(input, context).await
    }
}

struct RegisteredTool<'a> {
    definition: ToolDefinition,
    tool: Box<dyn Tool + 'a>,
}

/// Registration only. The execution layer resolves proposed calls, checks
/// admission, and invokes the selected tool. An empty registry stays empty.
#[derive(Default)]
pub struct ToolRegistry<'a> {
    tools: BTreeMap<String, RegisteredTool<'a>>,
}

impl<'a> ToolRegistry<'a> {
    pub fn register<M>(
        &mut self,
        tool: impl super::IntoTool<M, Tool: 'a>,
    ) -> Result<(), ToolRegistrationError> {
        let tool = tool.into_tool()?;
        let definition = tool.definition().clone();
        validate_name(&definition.name)?;
        if self.tools.contains_key(&definition.name) {
            return Err(ToolRegistrationError::Duplicate(definition.name));
        }
        jsonschema::validator_for(&definition.parameters)
            .map_err(|error| ToolRegistrationError::Schema(error.to_string()))?;
        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                tool: Box::new(tool),
            },
        );
        Ok(())
    }

    pub(super) fn extend(&mut self, other: Self) -> Result<(), ToolRegistrationError> {
        for (name, entry) in other.tools {
            if self.tools.contains_key(&name) {
                return Err(ToolRegistrationError::Duplicate(name));
            }
            self.tools.insert(name, entry);
        }
        Ok(())
    }

    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values().map(|entry| &entry.definition)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut (dyn Tool + 'a)> {
        match self.tools.get_mut(name) {
            Some(entry) => Some(entry.tool.as_mut()),
            None => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
