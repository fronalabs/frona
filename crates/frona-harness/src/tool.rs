//! Portable tool registration and invocation contracts. Dispatch policy lives
//! in the shared execution layer, not in the registry or typed helper.

use std::{collections::BTreeMap, future::Future, marker::PhantomData};

use async_trait::async_trait;
use rig_core::completion::request::ToolDefinition;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use rig_core::completion::message::ToolResultContent as ToolContent;

pub type ToolFailure = Box<dyn std::error::Error + Send + Sync>;

/// Control stops must escape tool-error feedback and retry/fallback handling.
/// A storage stop refers to required execution persistence, not an ordinary
/// file operation performed by an application tool.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionStop {
    #[error("execution cancelled: {0}")]
    Cancelled(String),
    #[error("execution budget exhausted: {0}")]
    BudgetExhausted(String),
    #[error("execution storage failed: {0}")]
    Storage(#[source] ToolFailure),
    #[error("execution stopped: {0}")]
    Stopped(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool failed: {0}")]
    Failed(#[source] ToolFailure),
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
                Err(error) => Self::Failed(error),
            },
        }
    }
}

pub type ToolResult = Result<ToolOutput, ToolError>;

/// Call-local information with no persistent identity or services. The shared
/// execution layer will attach its budget/nested executor here in HSDK-07/10;
/// tools must not construct a new budget for nested work.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ToolContext {
    pub call_id: Option<String>,
    pub cancellation: CancellationToken,
}

impl ToolContext {
    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
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
    async fn call(&mut self, arguments: Value, context: &ToolContext) -> ToolResult;
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
    let mut handler = handler;
    tool_with_context(name, description, move |arguments, _context| {
        handler(arguments)
    })
}

/// The context-aware form uses an owned, cheaply cloned call context, so the
/// callback can use it across await without boxing a lending closure.
pub fn tool_with_context<A, O, F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    handler: F,
) -> Result<TypedTool<A, F>, ToolRegistrationError>
where
    A: DeserializeOwned + JsonSchema + Send,
    O: IntoToolOutput,
    F: FnMut(A, ToolContext) -> Fut + Send,
    Fut: Future<Output = Result<O, ToolError>> + Send,
{
    let name = name.into();
    validate_name(&name)?;
    let parameters = schemars::schema_for!(A).to_value();
    let validator = jsonschema::validator_for(&parameters)
        .map_err(|error| ToolRegistrationError::Schema(error.to_string()))?;
    Ok(TypedTool {
        definition: ToolDefinition {
            name,
            description: description.into(),
            parameters,
        },
        validator,
        handler,
        arguments: PhantomData,
    })
}

pub struct TypedTool<A, F> {
    definition: ToolDefinition,
    validator: jsonschema::Validator,
    handler: F,
    arguments: PhantomData<fn(A)>,
}

#[async_trait]
impl<A, O, F, Fut> Tool for TypedTool<A, F>
where
    A: DeserializeOwned + Send,
    O: IntoToolOutput,
    F: FnMut(A, ToolContext) -> Fut + Send,
    Fut: Future<Output = Result<O, ToolError>> + Send,
{
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&mut self, arguments: Value, context: &ToolContext) -> ToolResult {
        self.validator
            .validate(&arguments)
            .map_err(|error| ToolError::failed(error.to_string()))?;
        let input = serde_json::from_value(arguments).map_err(ToolError::failed)?;
        (self.handler)(input, context.clone())
            .await?
            .into_tool_output()
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
    pub fn register(&mut self, tool: impl Tool + 'a) -> Result<(), ToolRegistrationError> {
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
