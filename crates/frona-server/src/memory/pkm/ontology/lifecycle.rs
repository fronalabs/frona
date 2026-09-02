use super::*;

impl OntologyManager {
    /// Build the manager and load a catalogue if one is already on disk. A missing or
    /// unreadable catalogue is **not** an error: the server starts, and consolidation
    /// skips until [`install_catalogue`](Self::install_catalogue) succeeds.
    pub fn new(roots: Roots, repo: Arc<PkmRepo>) -> Self {
        let catalogue = match roots.load() {
            Ok(c) => {
                tracing::info!(
                    terms = c.terms(),
                    disjoint_pairs = c.disjoint_pairs(),
                    sources = c.sources().len(),
                    "ontology catalogue loaded"
                );
                Some(c)
            }
            // Loud, but not fatal. A malformed file the user dropped in must not stop
            // the server; it stops classification, which is recoverable by removing the
            // file.
            Err(e) => {
                tracing::warn!(error = %e, "no ontology catalogue; consolidation will skip");
                None
            }
        };
        Self {
            catalogue: Arc::new(ArcSwapOption::new(catalogue)),
            repo,
            reasoned_graphs: Arc::new(reasoning::ReasonedGraphCache::default()),
            roots,
        }
    }

    /// Fetch the release if neither the image's copy nor a previous repair verifies,
    /// then load it.
    ///
    /// Deliberately **not** an upgrade path. A release arrives with an image; fetching
    /// one because a newer tag exists upstream would change how the server reasons with
    /// nothing in the deployment having changed, which is indistinguishable from
    /// reasoning that changed because someone edited an entity.
    pub async fn repair(&self) -> Result<(), AppError> {
        if !self.roots.needs_repair() {
            return Ok(());
        }
        let into = crate::memory::pkm::ontology::release::repair_dir(&self.roots.user);
        tracing::warn!(
            release = %self.roots.release.display(),
            "ontology release missing or corrupt; fetching a replacement"
        );
        let tag = crate::memory::pkm::ontology::release::fetch_latest(&into).await?;
        tracing::info!(tag = %tag, into = %into.display(), "ontology release installed");
        self.install_catalogue()
    }

    /// Everything the server can see. Searchable; never reasoned over as a whole.
    /// `None` until a catalogue is installed.
    /// The CURIE ↔ IRI bindings in force, from the catalogue that owns them.
    ///
    /// Global rather than per-user on purpose, and not a per-scope copy: entity kinds,
    /// attribute keys and link relations are *stored* as CURIEs, so a binding has to
    /// expand the same way for every user and forever - two users disagreeing about a
    /// prefix would make one stored string mean two different terms. Falls back to the
    /// bundled set only when no catalogue is installed, which is the same map.
    pub fn prefixes(&self) -> PrefixMap {
        self.catalogue()
            .map(|c| c.prefixes().clone())
            .unwrap_or_default()
    }

    pub(crate) fn catalogue(&self) -> Option<Arc<OntologyCatalogue>> {
        self.catalogue.load_full()
    }

    /// False while no valid catalogue is loaded.
    pub fn is_ready(&self) -> bool {
        self.catalogue.load().is_some()
    }

    /// Re-read both roots and publish the result. Called after the release download
    /// lands, and safe to call at any time: passes in flight hold their own
    /// `OntologyScope` snapshot and are unaffected.
    pub fn install_catalogue(&self) -> Result<(), AppError> {
        let catalogue = self.roots.load()?;
        tracing::info!(
            terms = catalogue.terms(),
            disjoint_pairs = catalogue.disjoint_pairs(),
            sources = catalogue.sources().len(),
            "ontology catalogue installed"
        );
        self.catalogue.store(Some(catalogue));
        self.invalidate_all_reasoned_graphs_after_catalogue_publish();
        Ok(())
    }

    /// What this user reasons over. Use this (never a manager-wide one) whenever
    /// expanding or compacting anything that came out of *their* entities.
    pub(crate) async fn user_effective_ontology(
        &self,
        user_id: &str,
    ) -> Result<Arc<OntologyScope>, AppError> {
        Ok(self.load(user_id).await?.effective_ontology().clone())
    }

    /// Search the **catalogue** for a term to reuse.
    ///
    /// Finding a term brings it into scope, so a hit here always resolves.
    pub(crate) fn search_vocab(&self, term: &str, limit: usize) -> Vec<VocabHit> {
        self.catalogue
            .load()
            .as_ref()
            .map(|catalogue| catalogue.search(term, limit))
            .unwrap_or_default()
    }
}
