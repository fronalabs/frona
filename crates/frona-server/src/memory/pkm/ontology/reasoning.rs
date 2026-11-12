use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use super::*;

use frona_pkm::ontology::ReasonPass;

/// The generation blocks stale publication; the mutex coalesces concurrent builds.
#[derive(Default)]
struct UserReasonedGraph {
    generation: AtomicU64,
    build: tokio::sync::Mutex<Option<(u64, Arc<ReasonPass>)>>,
}

#[derive(Default)]
pub(super) struct ReasonedGraphCache {
    users: DashMap<String, Arc<UserReasonedGraph>>,
}

impl ReasonedGraphCache {
    fn user(&self, user_id: &str) -> Arc<UserReasonedGraph> {
        self.users.entry(user_id.to_string()).or_default().clone()
    }

    fn invalidate(&self, user_id: &str) {
        if let Some(user) = self.users.get(user_id) {
            user.generation.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut entry) = user.build.try_lock() {
                *entry = None;
            }
        }
    }

    fn invalidate_all(&self) {
        for user in &self.users {
            user.generation.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut entry) = user.build.try_lock() {
                *entry = None;
            }
        }
    }
}

impl OntologyManager {
    /// Build the user's ABox from the DB, reason over `effective ⊕ delta ⊕ ABox`, and
    /// return the pass (the queryable closure + the source entities/links). Pure read;
    /// nothing is written.
    pub(crate) async fn reason_user(&self, user_id: &str) -> Result<ReasonPass, AppError> {
        let user = self.load(user_id).await?;
        let entities = self
            .repo
            .list_entities(user_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let asserted_links = self
            .repo
            .asserted_links(user_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let px = user.effective_ontology().prefixes();
        let abox = abox::build_abox_triples(&entities, &asserted_links, px);
        let reasoned = user.reason(&abox)?;
        let effective_ontology = user.effective_ontology().clone();
        let delta_ofn = user.delta_ofn().to_string();
        Ok(ReasonPass {
            reasoned,
            entities,
            asserted_links,
            delta_ofn,
            effective_ontology,
        })
    }

    pub(super) async fn cached_reason_user(
        &self,
        user_id: &str,
    ) -> Result<Arc<ReasonPass>, AppError> {
        let cached = self.reasoned_graphs.user(user_id);
        loop {
            let generation = cached.generation.load(Ordering::Acquire);
            let mut entry = cached.build.lock().await;
            if let Some((cached_generation, pass)) = entry.as_ref()
                && *cached_generation == generation
            {
                return Ok(pass.clone());
            }

            let pass = Arc::new(self.reason_user(user_id).await?);
            if cached.generation.load(Ordering::Acquire) == generation {
                *entry = Some((generation, pass.clone()));
                return Ok(pass);
            }

            // Invalidation raced this build; retry without publishing it.
            drop(entry);
        }
    }

    /// Called only after consolidation Cleanup commits the user graph.
    pub(crate) fn publish_consolidated_graph(&self, user_id: &str) {
        self.reasoned_graphs.invalidate(user_id);
    }

    /// Used by terminal lifecycle operations that bypass consolidation Cleanup.
    pub(crate) fn evict_reasoned_graph(&self, user_id: &str) {
        self.reasoned_graphs.invalidate(user_id);
    }

    pub(super) fn invalidate_all_reasoned_graphs_after_catalogue_publish(&self) {
        self.reasoned_graphs.invalidate_all();
    }

    /// The violations for a completed pass (reasoner clashes + facet bounds).
    pub(super) fn validate(&self, pass: &ReasonPass) -> Vec<Violation> {
        frona_pkm::ontology::validation::validate(
            &pass.reasoned,
            pass.effective_ontology.prefixes(),
        )
    }

    /// Reason, rewrite inferred entity links, and return current violations.
    pub async fn materialize(&self, user_id: &str) -> Result<Vec<Violation>, AppError> {
        let pass = self.reason_user(user_id).await?;
        let inferred = abox::extract_inferred(
            &pass.reasoned.store,
            &pass.asserted_links,
            pass.effective_ontology.prefixes(),
        )?;
        self.repo.wipe_inferred_links(user_id).await?;
        self.repo
            .insert_inferred_links(user_id, &inferred.links)
            .await?;
        Ok(self.validate(&pass))
    }

    pub async fn entails_type(
        &self,
        user_id: &str,
        entity_path: &str,
        class: &str,
    ) -> Result<bool, AppError> {
        let pass = self.cached_reason_user(user_id).await?;
        let query = format!(
            "ASK {{ <{}> a <{}> }}",
            individual_iri(entity_path),
            self.prefixes().expand(class),
        );
        Ok(sparql::ask(
            &pass.reasoned.store,
            &query,
            pass.effective_ontology.prefixes(),
        )?)
    }
}
