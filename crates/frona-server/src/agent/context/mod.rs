//! Immutable agent-context snapshots. Sources are read at turn boundaries;
//! signatures compare content, while messages retain the exact delivered diff.

mod sources;
pub use sources::{CatalogContextSource, MemoryContextSource, TemporalContextSource};

use crate::core::error::AppError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use surrealdb::types::SurrealValue;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, SurrealValue)]
#[serde(transparent)]
#[surreal(crate = "surrealdb::types")]
pub struct AgentContextSignature(pub String);

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, SurrealValue,
)]
#[serde(rename_all = "snake_case")]
#[surreal(crate = "surrealdb::types")]
pub enum AgentContextKind {
    UserProfile,
    ShortMemory,
    UserMemory,
    AgentMemory,
    SpaceContext,
    Skills,
    Agents,
    McpServers,
    Playbooks,
    Temporal,
    TaskTime,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct AgentContextSection {
    pub kind: AgentContextKind,
    /// Delivered with this turn, but excluded from immutable snapshots and signatures.
    #[serde(skip)]
    #[surreal(default)]
    pub transient: bool,
    pub prefix: String,
    pub entries: BTreeMap<String, String>,
    pub suffix: String,
}

impl AgentContextSection {
    pub fn tagged(kind: AgentContextKind, tag: &str, entries: BTreeMap<String, String>) -> Self {
        Self {
            kind,
            transient: false,
            prefix: format!("\n\n<{tag}>\n"),
            entries,
            suffix: format!("</{tag}>"),
        }
    }

    pub fn prose(kind: AgentContextKind, tag: &str, text: String) -> Self {
        Self::tagged(
            kind,
            tag,
            BTreeMap::from([("summary".into(), format!("{text}\n"))]),
        )
    }

    pub fn render(&self) -> String {
        format!(
            "{}{}{}",
            self.prefix,
            self.entries.values().cloned().collect::<String>(),
            self.suffix
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct AgentContextSnapshot {
    pub signature: AgentContextSignature,
    pub sections: Vec<AgentContextSection>,
}

impl AgentContextSnapshot {
    pub fn new(mut sections: Vec<AgentContextSection>) -> Result<Self, AppError> {
        sections.retain(|section| !section.transient);
        sections.sort_by_key(|s| s.kind);
        if sections.windows(2).any(|s| s[0].kind == s[1].kind) {
            return Err(AppError::Internal("Duplicate agent context section".into()));
        }
        let bytes = serde_json::to_vec(&sections).map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(Self {
            signature: AgentContextSignature(hex::encode(Sha256::digest(bytes))),
            sections,
        })
    }

    pub fn render(&self) -> String {
        self.sections
            .iter()
            .map(AgentContextSection::render)
            .collect()
    }

    pub fn diff(&self, previous: &Self) -> Option<String> {
        let mut text = String::new();
        for section in &self.sections {
            let old = previous.sections.iter().find(|s| s.kind == section.kind);
            if old.is_none_or(|o| o.prefix != section.prefix || o.suffix != section.suffix) {
                text.push_str(&section.render());
                continue;
            }
            let entries: BTreeMap<_, _> = section
                .entries
                .iter()
                .filter(|(key, value)| old.and_then(|o| o.entries.get(*key)) != Some(*value))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !entries.is_empty() {
                text.push_str(
                    &AgentContextSection {
                        entries,
                        ..section.clone()
                    }
                    .render(),
                );
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

/// Borrowed session data used while collecting a turn's agent context.
#[derive(Clone, Copy, Debug)]
pub struct AgentContextScope<'a> {
    pub user: &'a crate::auth::User,
    pub agent: &'a crate::agent::models::Agent,
    pub space_id: Option<&'a str>,
    pub task: Option<&'a crate::agent::task::models::Task>,
}

/// Per-chat source bookkeeping. Values are addressed by the full Rust type name.
/// Renaming or moving a state type starts it from its default; this is not replay data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(transparent)]
#[surreal(crate = "surrealdb::types")]
pub struct AgentContextState(BTreeMap<String, serde_json::Value>);

impl AgentContextState {
    pub fn get<T: serde::de::DeserializeOwned + Default>(&self) -> Result<T, AppError> {
        let key = std::any::type_name::<T>();
        match self.0.get(key) {
            Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                AppError::Database(format!("Invalid source state {key}: {error}"))
            }),
            None => Ok(T::default()),
        }
    }

    pub fn set<T: Serialize>(&mut self, value: T) -> Result<(), AppError> {
        let key = std::any::type_name::<T>();
        let value = serde_json::to_value(value).map_err(|error| {
            AppError::Internal(format!("Serialize source state {key}: {error}"))
        })?;
        self.0.insert(key.to_owned(), value);
        Ok(())
    }
}

#[async_trait]
pub trait AgentContextSource: Send + Sync {
    /// Return complete current sections, including any transient text for this turn.
    /// The caller persists candidate state only after the associated context is saved.
    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError>;
}

#[derive(Clone)]
pub struct AgentContextManager {
    sources: Vec<Arc<dyn AgentContextSource>>,
    repository: crate::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo,
}

pub struct PreparedAgentContext {
    pub snapshot: AgentContextSnapshot,
    pub update: Option<String>,
}

impl AgentContextManager {
    pub fn new(
        sources: Vec<Arc<dyn AgentContextSource>>,
        repository: crate::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo,
    ) -> Self {
        Self {
            sources,
            repository,
        }
    }

    pub async fn current(
        &self,
        scope: &AgentContextScope<'_>,
        state: &mut AgentContextState,
    ) -> Result<AgentContextSnapshot, AppError> {
        AgentContextSnapshot::new(self.collect(scope, state).await?)
    }

    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        let mut sections = Vec::new();
        for source in &self.sources {
            sections.extend(source.collect(scope, state).await?);
        }
        sections.sort_by_key(|section| section.kind);
        if sections.windows(2).any(|s| s[0].kind == s[1].kind) {
            return Err(AppError::Internal("Duplicate agent context section".into()));
        }
        Ok(sections)
    }

    pub async fn load(
        &self,
        signature: &AgentContextSignature,
    ) -> Result<AgentContextSnapshot, AppError> {
        let snapshot = self.repository.find(signature).await?.ok_or_else(|| {
            AppError::Database(format!("Missing agent context snapshot {}", signature.0))
        })?;
        if snapshot.signature != *signature
            || AgentContextSnapshot::new(snapshot.sections.clone())?.signature != *signature
        {
            return Err(AppError::Database("Invalid agent context snapshot".into()));
        }
        Ok(snapshot)
    }

    pub async fn prepare(
        &self,
        scope: &AgentContextScope<'_>,
        previous: Option<&AgentContextSignature>,
        state: &mut AgentContextState,
    ) -> Result<PreparedAgentContext, AppError> {
        let sections = self.collect(scope, state).await?;
        let transient: String = sections
            .iter()
            .filter(|section| section.transient)
            .map(AgentContextSection::render)
            .collect();
        let snapshot = AgentContextSnapshot::new(sections)?;
        let previous = match previous {
            Some(signature) => Some(self.load(signature).await?),
            None => None,
        };
        let mut text = match &previous {
            Some(previous) => snapshot.diff(previous).unwrap_or_default(),
            None => snapshot.render(),
        };
        if previous
            .as_ref()
            .is_none_or(|old| old.signature != snapshot.signature)
        {
            self.repository
                .ensure_stored(&snapshot)
                .await
                .map_err(crate::inference::controls::required_write_error)?;
        }
        text.push_str(&transient);
        Ok(PreparedAgentContext {
            snapshot,
            update: (!text.is_empty()).then_some(text),
        })
    }
}

#[cfg(test)]
mod tests;
