//! Optional application decisions before ordinary tool dispatch.

use std::future::Future;

use async_trait::async_trait;
use rig_core::completion::message::ToolCall;

use super::ToolFailure;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Execute,
    Reject(String),
    Stop(String),
}

/// A sequential hook may borrow application state and lend mutable state across
/// await. It need not be Sync or static. Its future is Send, matching the run's
/// execution contract. Failures stop the run; they are never model feedback.
#[async_trait]
pub trait ToolCallHook: Send {
    async fn decide(&mut self, call: &ToolCall) -> Result<ToolDecision, ToolFailure>;
}

pub(crate) struct CallbackHook<F>(pub F);

#[async_trait]
impl<F, Fut, E> ToolCallHook for CallbackHook<F>
where
    F: FnMut(ToolCall) -> Fut + Send,
    Fut: Future<Output = Result<ToolDecision, E>> + Send,
    E: std::fmt::Debug,
{
    async fn decide(&mut self, call: &ToolCall) -> Result<ToolDecision, ToolFailure> {
        (self.0)(call.clone())
            .await
            .map_err(|error| format!("{error:?}").into())
    }
}
