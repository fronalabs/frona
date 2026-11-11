use crate::model::{Disposition, MemoryKind};
/// One validated memory change from a human page edit. The sync layer removes invalid
/// model output before it gives the plan to the repository.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PageEditMemoryOp {
    Add {
        kind: MemoryKind,
        content: String,
    },
    Supersede {
        older_id: String,
        kind: MemoryKind,
        content: String,
        note: String,
    },
    SetDisposition {
        memory_id: String,
        disposition: Disposition,
    },
}
