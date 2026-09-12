//! `InferenceKind` is a tagged enum so wrong-combo states (e.g. ToolTurn
//! without a turn_index) are unrepresentable. The persisted `InferenceUsage`
//! row is the flat materialised projection of that kind.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use frona_derive::Entity;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "inference_usage")]
pub struct InferenceUsage {
    pub id: String,

    /// Identity columns are denormalised from `InferenceKind` at write time
    /// so rollup queries hit indices defined in `db/init.rs`.
    pub user_id: String,
    pub agent_id: Option<String>,
    pub chat_id: Option<String>,
    pub space_id: Option<String>,
    pub message_id: Option<String>,
    pub turn_index: Option<u32>,
    pub kind_tag: String,

    pub model_group: String,
    pub provider: String,
    pub model_id: String,
    pub model_ref: String,

    // Usage. `cached_input_tokens` is rig's collapsed view - see plan
    // "Cache fidelity" section for why we can't split Anthropic
    // cache_creation vs cache_read on the streaming path.
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub fallback_index: u8,
    pub duration_ms: u64,

    // Latency breakdown. `ttft_ms` is the time to the first streamed chunk and
    // is None on the non-streaming path. `output_tokens_per_second` is
    // pre-computed as `output_tokens / ((duration_ms - ttft_ms) / 1000)` so
    // percentile queries are a single-column scan. `retry_overhead_ms` /
    // `retry_count` are the wall time + count of failed attempts within THIS
    // recorded model (cross-model fallback is captured by `fallback_index`).
    pub ttft_ms: Option<u64>,
    pub output_tokens_per_second: Option<f64>,
    pub retry_overhead_ms: u64,
    pub retry_count: u32,

    // Pricing snapshot. cost_usd is None if pricing lookup missed for this
    // model_ref; pricing_version always reflects the table version that was
    // loaded when the row was written.
    pub cost_usd: Option<f64>,
    pub pricing_version: String,

    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct UsageRollup {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, PartialEq, Eq)]
#[surreal(crate = "surrealdb::types")]
pub enum InferenceKind {
    /// A text-only reply - the no-tool fast path. One row per user turn
    /// (the agent has no tools available). Tool-using agents produce
    /// `ToolTurn` rows instead (even for the final user-visible reply
    /// turn that stops calling tools).
    Text {
        agent_id: String,
        chat_id: String,
        message_id: String,
    },
    ToolTurn {
        agent_id: String,
        chat_id: String,
        message_id: String,
        /// Matches the `turn: u32` field on the N `ToolCall` rows produced by
        /// this LLM call (one inference → many tool calls per turn).
        turn_index: u32,
    },
    Title {
        agent_id: String,
        chat_id: String,
    },
    Compaction {
        target: CompactionTarget,
    },
    /// Background PKM consolidation - turning a transcript (or a User Vault note)
    /// into KB structure. Attributed to the **user** only (top-level `user_id`);

    /// it carries no agent/chat because it runs async, detached from any turn, and
    /// may have no originating chat at all (sync ingest). Distinct from
    /// `Compaction` (context-window distillation). Mirrors `CompactionTarget::User`.
    Memory,
    Signal {
        agent_id: String,
        chat_id: String,
        message_id: String,
    },
    Router {
        agent_id: String,
        chat_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, PartialEq, Eq)]
#[surreal(crate = "surrealdb::types")]
pub enum CompactionTarget {
    /// User-level memory distillation. `user_id` is on the top-level row.
    User,
    Chat {
        agent_id: String,
        chat_id: String,
    },
    Agent {
        agent_id: String,
    },
    Space {
        space_id: String,
    },
}

impl InferenceKind {
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Text { agent_id, .. }
            | Self::ToolTurn { agent_id, .. }
            | Self::Title { agent_id, .. }
            | Self::Signal { agent_id, .. }
            | Self::Router { agent_id, .. } => Some(agent_id),
            Self::Memory => None,
            Self::Compaction { target } => match target {
                CompactionTarget::Chat { agent_id, .. } | CompactionTarget::Agent { agent_id } => {
                    Some(agent_id)
                }
                CompactionTarget::User | CompactionTarget::Space { .. } => None,
            },
        }
    }

    pub fn chat_id(&self) -> Option<&str> {
        match self {
            Self::Text { chat_id, .. }
            | Self::ToolTurn { chat_id, .. }
            | Self::Title { chat_id, .. }
            | Self::Signal { chat_id, .. } => Some(chat_id),
            Self::Router { chat_id, .. } => chat_id.as_deref(),
            Self::Memory => None,
            Self::Compaction {
                target: CompactionTarget::Chat { chat_id, .. },
            } => Some(chat_id),
            Self::Compaction { .. } => None,
        }
    }

    pub fn space_id(&self) -> Option<&str> {
        match self {
            Self::Compaction {
                target: CompactionTarget::Space { space_id },
            } => Some(space_id),
            _ => None,
        }
    }

    pub fn message_id(&self) -> Option<&str> {
        match self {
            Self::Text { message_id, .. }
            | Self::ToolTurn { message_id, .. }
            | Self::Signal { message_id, .. } => Some(message_id),
            _ => None,
        }
    }

    pub fn turn_index(&self) -> Option<u32> {
        match self {
            Self::ToolTurn { turn_index, .. } => Some(*turn_index),
            _ => None,
        }
    }

    pub fn tag(&self) -> &'static str {
        match self {
            Self::Text { .. } => "Text",
            Self::ToolTurn { .. } => "ToolTurn",
            Self::Title { .. } => "Title",
            Self::Compaction { .. } => "Compaction",
            Self::Memory => "Memory",
            Self::Signal { .. } => "Signal",
            Self::Router { .. } => "Router",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UsageContext {
    pub kind: InferenceKind,
    pub user_id: String,
    pub model_group: String,
}

impl UsageContext {
    pub fn new(
        kind: InferenceKind,
        user_id: impl Into<String>,
        model_group: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            user_id: user_id.into(),
            model_group: model_group.into(),
        }
    }
}
