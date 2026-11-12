use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;
/// Ingest - the chats mined so far. A mined chat is finished because its memories and
/// watermark are already committed, so the position is all that is needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct IngestState {
    pub mined: BTreeSet<String>,
}
