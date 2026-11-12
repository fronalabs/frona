mod concepts;
mod ingest;
mod playbook;
pub use concepts::ConsolidationWorkState;
pub use playbook::PlaybookResolveState;
mod record;
mod stage;

#[cfg(test)]
mod tests;

pub use ingest::IngestState;
pub use record::{ConsolidationFailure, KnowledgeConsolidationRecord};
pub use stage::ConsolidationStageState;
