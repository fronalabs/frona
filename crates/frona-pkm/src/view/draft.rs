use std::collections::BTreeMap;

use crate::Error;
use crate::model::{ConsolidationEntityLifecycle, KnowledgeConsolidationEntity};

pub(crate) enum DraftEntityLookup {
    Found(Box<KnowledgeConsolidationEntity>),
    Missing,
}

#[derive(Debug, Clone, Default)]
pub struct EntityDraft {
    rows: BTreeMap<String, KnowledgeConsolidationEntity>,
}

impl EntityDraft {
    pub fn from_rows(rows: impl IntoIterator<Item = KnowledgeConsolidationEntity>) -> Self {
        Self {
            rows: rows
                .into_iter()
                .map(|row| (row.path.clone(), row))
                .collect(),
        }
    }

    pub fn rows(&self) -> impl Iterator<Item = &KnowledgeConsolidationEntity> {
        self.rows.values()
    }

    pub(crate) fn resolve(&self, path: &str) -> Result<DraftEntityLookup, Error> {
        let path = super::resolve_path(path, |path| {
            let Some(row) = self.rows.get(path) else {
                return Ok(None);
            };
            if row.lifecycle == ConsolidationEntityLifecycle::Coalesced {
                row.canonical_path.as_deref().map(Some).ok_or_else(|| {
                    Error::Internal(format!(
                        "entity view draft: coalesced row `{path}` has no canonical path"
                    ))
                })
            } else {
                Ok(None)
            }
        })?;
        match self.rows.get(path) {
            Some(row)
                if matches!(
                    row.lifecycle,
                    ConsolidationEntityLifecycle::Pending | ConsolidationEntityLifecycle::Active
                ) =>
            {
                Ok(DraftEntityLookup::Found(Box::new(row.clone())))
            }
            _ => Ok(DraftEntityLookup::Missing),
        }
    }
}
