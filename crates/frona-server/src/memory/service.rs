//! The memory-system seam.
//!
//! A [`MemoryService`] is the abstraction over *how memory works* - the
//! foreground-facing surface only. Exactly one service is selected at boot from
//! config (`basic` or `pkm`); switching requires a restart, so the
//! trait is deliberately not designed for hot-swap.
//!
//! Foreground collection returns immutable, structured contributions. The chat
//! layer owns their persistence and replay; sources never mutate mapped history.
//! Background maintenance remains owned by each memory implementation.

use std::sync::Arc;

use async_trait::async_trait;

use crate::core::error::AppError;
use crate::tool::AgentTool;

/// The active memory system. One implementation is chosen at boot.
#[async_trait]
pub trait MemoryService: Send + Sync {
    /// Tools this service contributes to the agent. Folded into the builtin
    /// tool set and Cedar-gated like any other tool.
    fn tools(&self) -> Vec<Arc<dyn AgentTool>>;

    /// Backend usage/navigation guidance for the standing system prompt.
    /// This is configuration-scoped text, outside snapshot hashing and diffs.
    async fn instructions(&self, user: &crate::auth::User) -> Result<String, AppError>;

    /// Collect selected memory with stable entry identities and exact rendered text.
    /// Failure aborts context preparation; an empty result is a successful read.
    async fn collect(
        &self,
        scope: &crate::agent::context::AgentContextScope<'_>,
    ) -> Result<Vec<crate::agent::context::AgentContextSection>, AppError>;

    /// Register background-maintenance jobs with the scheduler. A registration
    /// lifecycle hook (not a business method): each service registers whatever
    /// periodic upkeep it needs via `scheduler.register_periodic(...)`. Default
    /// no-op (e.g. an event-driven service that maintains itself elsewhere).
    /// Called once at `Scheduler::start()`.
    fn register_maintenance(&self, _scheduler: &crate::scheduler::Scheduler) {}
}
