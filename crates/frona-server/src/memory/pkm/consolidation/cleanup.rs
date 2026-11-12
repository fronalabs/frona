//! Post-pass cleanup: repair dangling entity links, remove projection-empty entities,
//! and decay stale short memories by recency score.

use std::sync::Arc;

use chrono::Utc;
use tracing::warn;

use crate::core::error::AppError;
use crate::memory::pkm::model::{EntityCategory, classify_memories, decay_score};
use frona_pkm::page::projection::{MarkdownPage, compose_page};

use super::CleanupOutcome;
use super::context::ConsolidationContext;

pub(super) struct Cleanup {
    pub ctx: Arc<ConsolidationContext>,
    pub prefixes: frona_pkm::ontology::PrefixMap,
    /// Short-memory recency-decay half-life (seconds).
    pub half_life_secs: f32,
    /// Decay score below which a short memory is dropped.
    pub demote_threshold: f32,
    /// How many finished consolidation passes to keep as a log.
    pub keep_records: usize,
}

impl Cleanup {
    pub(super) async fn run(&self) -> Result<CleanupOutcome, AppError> {
        let mut out = CleanupOutcome::default();
        self.cleanup(&mut out).await?;
        Ok(out)
    }

    /// Re-home memories whose entity never materialised.
    async fn repair_dangling(&self, stats: &mut CleanupOutcome) -> Result<(), AppError> {
        let user_id = &self.ctx.scope.user_id;
        for path in self.ctx.repo.dangling_memory_paths(user_id).await? {
            let memories = self.ctx.repo.memories_for_entity(user_id, &path).await?;
            let (current, history) = classify_memories(&memories);
            if current.is_empty() && history.is_empty() {
                continue;
            }
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            match self
                .ctx
                .repo
                .upsert_entity_skeleton(
                    user_id,
                    &path,
                    EntityCategory::Concept,
                    &[],
                    &name,
                    "",
                    &[],
                )
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        entity = %path,
                        facts = current.len() + history.len(),
                        "pkm cleanup: re-homed memories whose entity was missing"
                    );
                    stats.entities_rehomed += 1;
                }
                Err(e) => warn!(error = %e, entity = %path, "pkm cleanup: re-home failed"),
            }
        }
        Ok(())
    }

    async fn cleanup(&self, stats: &mut CleanupOutcome) -> Result<(), AppError> {
        self.repair_dangling(stats).await?;

        // Entity GC - the symmetric case to orphan-memory GC: an entity whose memories
        // were all retired as erroneous (e.g. the user deleted it) has nothing to
        // project, so drop its record + file. The memories stay (erroneous,
        // canonical, for re-learn suppression); only the projection node goes.
        for path in self
            .ctx
            .repo
            .entities_with_no_valid_memories(&self.ctx.scope.user_id)
            .await?
        {
            if let Err(e) = self
                .ctx
                .repo
                .delete_entity(&self.ctx.scope.user_id, &path)
                .await
            {
                warn!(error = %e, path = %path, "pkm cleanup: entity GC failed");
                continue;
            }
            if let Err(e) = self.ctx.storage.delete_page(&self.ctx.scope.vault, &path) {
                warn!(error = %e, path = %path, "pkm cleanup: entity file GC failed");
            }
            stats.entities_gced += 1;
        }

        // Unreferenced extraction memories remain repair input when an entity is discarded
        // or a model exhausts its budget without assigning every memory.

        // Dead-weight GC: memories retired via `Duplicate`/`Absorbed` are invisible to
        // every projection (their content lives in the survivor), so delete the rows +
        // their links. `Replace`/`Outdated` (History) and `Erroneous` (suppression) stay.
        for memory_id in self
            .ctx
            .repo
            .dropped_memory_ids(&self.ctx.scope.user_id)
            .await?
        {
            if let Err(e) = self
                .ctx
                .repo
                .delete_memory(&self.ctx.scope.user_id, &memory_id)
                .await
            {
                warn!(error = %e, memory = %memory_id, "pkm cleanup: dropped-memory GC failed");
            } else {
                stats.dropped_gced += 1;
            }
        }

        // Retention for the pass log. Finished records accumulate one per pass per user
        // and are never read by the pipeline, only by whoever is debugging it.
        match self
            .ctx
            .repo
            .prune_consolidation_records(&self.ctx.scope.user_id, self.keep_records)
            .await
        {
            Ok(n) if n > 0 => tracing::debug!(dropped = n, "pkm cleanup: pruned pass records"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "pkm cleanup: pruning pass records failed"),
        }

        self.decay_sweep(stats).await?;
        self.reconcile_entity_files().await
    }

    /// Two-pass filesystem projection. Materialize and verify every live entity first;
    /// only a completely successful materialization permits the obsolete-file sweep.
    async fn reconcile_entity_files(&self) -> Result<(), AppError> {
        let entities = self.ctx.repo.list_entities(&self.ctx.scope.user_id).await?;
        let authored = entities
            .iter()
            .filter(|entity| entity.rev.is_some())
            .collect::<Vec<_>>();
        let canonical = authored
            .iter()
            .map(|entity| entity.path.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for entity in authored {
            let links = self
                .ctx
                .repo
                .links_from_entity(&self.ctx.scope.user_id, &entity.path)
                .await
                .unwrap_or_default();
            let article = MarkdownPage::parse(&entity.body);
            let file = compose_page(
                &entity.clone().into(),
                &article,
                &entity.attributes,
                &links.into_iter().map(Into::into).collect::<Vec<_>>(),
                &self.prefixes,
                &frona_pkm::page::PageLayout::new(self.ctx.scope.vault.directory())?,
            );
            self.ctx.write_page_and_rev(&entity.path, &file).await?;
        }
        for (path, _) in self.ctx.storage.list_page_files(&self.ctx.scope.vault) {
            if !canonical.contains(&path) {
                self.ctx.storage.delete_page(&self.ctx.scope.vault, &path)?;
            }
        }
        self.ctx
            .storage
            .remove_empty_page_directories(&self.ctx.scope.vault)
    }

    async fn decay_sweep(&self, stats: &mut CleanupOutcome) -> Result<(), AppError> {
        let rows = self
            .ctx
            .repo
            .list_short_memory(&self.ctx.scope.user_id)
            .await?;
        let now = Utc::now();
        for row in rows {
            let age = (now - row.last_accessed_at).num_seconds().max(0) as f32;
            let score = decay_score(age, self.half_life_secs);
            if score < self.demote_threshold {
                if let Err(e) = self.ctx.repo.delete_short_memory(&row.id).await {
                    warn!(error = %e, "pkm cleanup: decay delete failed");
                } else {
                    stats.short_memory_dropped += 1;
                }
            }
        }
        Ok(())
    }
}
