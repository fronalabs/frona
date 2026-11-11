use super::{PageEditMemoryOp, submission::WriteBackOps};
use crate::model::{Disposition, MemoryKind};
use std::collections::HashSet;
/// Convert model output to the small repository plan. Unknown memory ids, unknown
/// operations, and empty payloads are dropped before the transaction starts.
pub(super) fn validated_writeback_ops(
    clean_path: &str,
    ops: WriteBackOps,
    valid_ids: &HashSet<String>,
) -> Vec<PageEditMemoryOp> {
    let mut planned = Vec::new();
    for op in ops.ops {
        match op.op.trim() {
            "add" => {
                let content = op.content.trim();
                if let Some(kind) = MemoryKind::parse(&op.kind).filter(|_| !content.is_empty()) {
                    planned.push(PageEditMemoryOp::Add {
                        kind,
                        content: content.to_string(),
                    });
                }
            }
            "supersede" => {
                let older = op.memory_id.trim();
                let content = op.content.trim();
                if valid_ids.contains(older)
                    && !content.is_empty()
                    && let Some(kind) = MemoryKind::parse(&op.kind)
                {
                    planned.push(PageEditMemoryOp::Supersede {
                        older_id: older.to_string(),
                        kind,
                        content: content.to_string(),
                        note: op.note.trim().to_string(),
                    });
                }
            }
            "outdated" => {
                let id = op.memory_id.trim();
                if valid_ids.contains(id) {
                    planned.push(PageEditMemoryOp::SetDisposition {
                        memory_id: id.to_string(),
                        disposition: Disposition::Outdated,
                    });
                }
            }
            "wrong" => {
                let id = op.memory_id.trim();
                if valid_ids.contains(id) {
                    planned.push(PageEditMemoryOp::SetDisposition {
                        memory_id: id.to_string(),
                        disposition: Disposition::Erroneous,
                    });
                }
            }
            other => {
                tracing::warn!(op = %other, path = %clean_path, "pkm writeback: unknown op, ignored")
            }
        }
    }
    planned
}
