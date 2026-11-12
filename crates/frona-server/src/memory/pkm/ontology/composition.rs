use super::*;

use frona_pkm::ontology::seed_set;

impl OntologyManager {
    /// The user's delta serialized as OFN.
    pub async fn serialize(&self, user_id: &str) -> Result<String, AppError> {
        Ok(self.load(user_id).await?.delta_ofn().to_string())
    }

    /// Load the user's ontology at its current persisted version (an absent delta
    /// is the empty delta at version 0 - the user's TBox is exactly the catalogue cut
    /// their vault reaches).
    pub(crate) async fn load(&self, user_id: &str) -> Result<UserOntology, AppError> {
        let row = self.repo.ontology_get(user_id).await?;
        let (ofn, version) = row
            .as_ref()
            .map(|o| (o.owl.clone(), o.version))
            .unwrap_or_else(|| (String::new(), 0));

        let catalogue = self
            .catalogue()
            .ok_or_else(|| AppError::Internal("ontology: no catalogue installed yet".into()))?;
        let delta_triples = schema::delta_triples(&ofn)?;
        let seeds = seed_set(
            &catalogue,
            &delta_triples,
            &self.repo.ontology_terms(user_id).await?,
        );
        let fingerprint = catalogue.fingerprint();

        // The stored cut is used verbatim when nothing that determines it has moved.
        // Both halves of that test matter: the seed set covers "the vault started using
        // a new term", the fingerprint covers "the catalogue itself changed", which is
        // how an image upgrade reaches existing users.
        if let Some(stored) = row.as_ref().filter(|r| {
            !r.effective_ontology.is_empty()
                && r.catalog_fingerprint == fingerprint
                && r.seeds == seeds
        }) {
            let effective_ontology = OntologyScope::from_ntriples(
                &stored.effective_ontology,
                seeds,
                stored.sources.clone(),
                catalogue.prefixes().clone(),
            )?;
            return Ok(UserOntology::build(
                catalogue,
                Arc::new(effective_ontology),
                ofn,
                delta_triples,
                version,
            ));
        }

        let effective_ontology = self
            .cut_and_store(user_id, &catalogue, row.as_ref(), seeds, version)
            .await?;
        Ok(UserOntology::build(
            catalogue,
            effective_ontology,
            ofn,
            delta_triples,
            version,
        ))
    }

    /// Re-cut the effective ontology and persist it.
    ///
    /// The cut is what the knowledge base reasons over *now*, so it tracks the vault:
    /// a term nothing references any more leaves. The one exception is a term an entity is
    /// still typed with whose source has left the catalogue - the seed is there but
    /// `project` can supply nothing for it, and dropping it would untype the entity
    /// without anyone having asked. Those are carried forward from the stored copy.
    pub(super) async fn cut_and_store(
        &self,
        user_id: &str,
        catalogue: &Arc<OntologyCatalogue>,
        stored: Option<&crate::memory::pkm::model::KnowledgeOntology>,
        seeds: Vec<String>,
        version: i64,
    ) -> Result<Arc<OntologyScope>, AppError> {
        let fresh = catalogue.project(&seeds);
        // Seeds the catalogue *used to* explain and no longer does - a vocabulary an
        // image upgrade dropped, or a file the user deleted.
        //
        // Both halves of that test are load-bearing, and "the catalogue does not
        // declare it" alone is not the second half. Two kinds of seed have never been
        // in the catalogue at all and so have no axioms to lose:
        //
        //   * a `frona:` mint, which lives in the delta by design;
        //   * the RDF vocabulary the delta's own axioms are *written in* -
        //     `DeclareClass(frona:Foo)` lowers to `frona:Foo rdf:type owl:Class`, and
        //     `referenced_iris` harvests every position, so `rdf:type` and `owl:Class`
        //     become seeds too.
        //
        // Letting those through does not merely log a spurious warning. Adjacency in
        // `carrying_forward` is undirected, and `owl:Class` is the object of the
        // `rdf:type` triple every term in the cut carries - so seeding the walk from it
        // reaches the entire previous cut and *nothing is ever pruned again*. One
        // minted term was enough to make the effective ontology grow monotonically for
        // the rest of that user's life.
        //
        // `describes` is the honest test: a term is stranded only if the previous cut
        // actually held axioms about it.
        let previous = match stored.filter(|r| !r.effective_ontology.is_empty()) {
            None => None,
            Some(prev) => Some(OntologyScope::from_ntriples(
                &prev.effective_ontology,
                prev.seeds.clone(),
                prev.sources.clone(),
                catalogue.prefixes().clone(),
            )?),
        };
        let stranded: Vec<String> = match &previous {
            None => Vec::new(),
            Some(prev) => seeds
                .iter()
                .filter(|s| !catalogue.declares(s) && prev.describes(s))
                .cloned()
                .collect(),
        };
        let effective_ontology = match previous.filter(|_| !stranded.is_empty()) {
            None => fresh,
            Some(previous) => {
                tracing::warn!(
                    user_id,
                    stranded = stranded.len(),
                    example = stranded.first().map(String::as_str).unwrap_or(""),
                    "ontology: terms still in use are no longer in the catalogue; \
                     keeping their last-known axioms"
                );
                Arc::new(fresh.carrying_forward(&previous, &stranded))
            }
        };
        // A `false` here is a racing delta edit: the cut was taken against a delta that
        // has since moved, so it is dropped rather than written over the newer one. The
        // next load re-cuts, which is cheap.
        self.repo
            .ontology_set_effective(
                user_id,
                version,
                &effective_ontology.to_ntriples(),
                effective_ontology.seeds(),
                effective_ontology.sources(),
                catalogue.fingerprint(),
            )
            .await?;
        Ok(effective_ontology)
    }
}
