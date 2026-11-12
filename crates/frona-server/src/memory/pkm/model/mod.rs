//! Records for the knowledge memory service.
//!
//! All tables are `knowledge_`-prefixed so they coexist with the basic
//! compaction service's `memory` / `memory_entry` tables (both services
//! live behind the same trait, selected at boot). Every row is `user_id`-scoped.
//!
//! Core model: **memories are canonical; entities are a reconstructable projection
//! of them.** Atomic immutable `KnowledgeMemory` rows are linked to entities via
//! `knowledge_entity_source`; an entity is rebuilt from its linked memories each pass.
//!
//! An entity's identity is its **path** (a unique, indexed
//! field), not the record id - the record id stays an opaque UUID so `/`-bearing
//! paths never hit SurrealDB record-id quoting. Memory ids are opaque UUIDs too,
//! which is what lets `supersedes` be a traversable `RecordId[]`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

use frona_derive::Entity;

use crate::core::error::AppError;

mod consolidation_entity;
mod entity;
mod memory;
mod ontology;

pub use consolidation_entity::*;
pub use entity::*;
pub use memory::*;
pub use ontology::*;

#[cfg(test)]
mod tests;

mod portable;

use frona_pkm::model::{
    derive_resolution_search, derive_search_text, merge_consolidation_attribute_values,
};
