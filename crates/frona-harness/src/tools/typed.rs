//! Typed callbacks share argument validation and output conversion.

use std::{future::Future, marker::PhantomData, pin::Pin};

use async_trait::async_trait;
use rig_core::completion::request::ToolDefinition;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::registry::validate_name;
use super::{IntoToolOutput, Tool, ToolContext, ToolError, ToolRegistrationError, ToolResult};

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
