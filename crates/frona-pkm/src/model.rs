//! Portable knowledge snapshots and domain values copied from PKM records.
use crate::Error;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
mod consolidation_entity;
mod entity;
mod memory;
pub use crate::ingest::PendingPlaybookCandidate;
pub use consolidation_entity::*;
pub use entity::*;
pub use memory::*;
pub(crate) fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

mod ontology;
pub use ontology::*;
