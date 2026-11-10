//! Conversion of annotated function items into the existing typed tool adapter.

use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::typed::{TypedHandler, typed_tool};
use super::{
    Tool, ToolContext, ToolFuture, ToolOutput, ToolRegistrationError, ToolResult, TypedTool,
};

/// A tool implementation or an annotated function accepted by registration.
/// The marker is inferred; callers never need to name it.
pub trait IntoTool<Marker> {
    type Tool: Tool;
    fn into_tool(self) -> Result<Self::Tool, ToolRegistrationError>;
}

#[doc(hidden)]
pub struct ExistingTool;

impl<T: Tool> IntoTool<ExistingTool> for T {
    type Tool = T;
    fn into_tool(self) -> Result<T, ToolRegistrationError> {
        Ok(self)
    }
}

/// Implementation detail of `#[tool]`. Carries metadata in a genuine function's
/// return type without changing its awaited output. Each call boxes one future.
#[doc(hidden)]
pub struct AnnotatedFuture<'a, R, M> {
    future: Pin<Box<dyn Future<Output = R> + Send + 'a>>,
    metadata: PhantomData<fn() -> M>,
}

impl<'a, R, M> AnnotatedFuture<'a, R, M> {
    pub fn new(future: impl Future<Output = R> + Send + 'a) -> Self {
        Self {
            future: Box::pin(future),
            metadata: PhantomData,
        }
    }
}

impl<R, M> Future for AnnotatedFuture<'_, R, M> {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<R> {
        self.future.as_mut().poll(context)
    }
}

#[doc(hidden)]
pub trait AnnotatedTool<F> {
    type Arguments: DeserializeOwned + JsonSchema + Send;
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    fn invoke<'call>(
        function: &'call mut F,
        arguments: Self::Arguments,
        context: &ToolContext<'call>,
    ) -> ToolFuture<'call, ToolOutput>;
}

#[doc(hidden)]
pub struct FunctionTool<A, R, M>(PhantomData<fn(A) -> (R, M)>);

#[doc(hidden)]
pub struct AnnotatedHandler<F, M> {
    function: F,
    metadata: PhantomData<fn() -> M>,
}

impl<F: Send, M: AnnotatedTool<F>> TypedHandler<M::Arguments> for AnnotatedHandler<F, M> {
    async fn call(&mut self, arguments: M::Arguments, context: &ToolContext<'_>) -> ToolResult {
        M::invoke(&mut self.function, arguments, context).await
    }
}

// Function arguments are used only to infer metadata from the return type. The
// generated adapter invokes the supplied callable at the actual context lifetime,
// so this 'static probe neither retains nor manufactures execution controls.
macro_rules! function_tools {
    ($(($($argument:ident),*)),* $(,)?) => {$(
        impl<F, R, M, $($argument,)*> IntoTool<FunctionTool<($($argument,)*), R, M>> for F
        where
            F: FnMut($($argument),*) -> AnnotatedFuture<'static, R, M> + Send,
            M: AnnotatedTool<F>,
        {
            type Tool = TypedTool<M::Arguments, AnnotatedHandler<F, M>>;
            fn into_tool(self) -> Result<Self::Tool, ToolRegistrationError> {
                typed_tool(M::NAME.into(), M::DESCRIPTION.into(), AnnotatedHandler {
                    function: self, metadata: PhantomData,
                })
            }
        }
    )*};
}

function_tools!(
    (),
    (A),
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, G),
    (A, B, C, D, E, G, H),
    (A, B, C, D, E, G, H, I),
    (A, B, C, D, E, G, H, I, J),
    (A, B, C, D, E, G, H, I, J, K),
    (A, B, C, D, E, G, H, I, J, K, L),
    (A, B, C, D, E, G, H, I, J, K, L, N),
    (A, B, C, D, E, G, H, I, J, K, L, N, P),
    (A, B, C, D, E, G, H, I, J, K, L, N, P, Q),
    (A, B, C, D, E, G, H, I, J, K, L, N, P, Q, S),
    (A, B, C, D, E, G, H, I, J, K, L, N, P, Q, S, T),
);
