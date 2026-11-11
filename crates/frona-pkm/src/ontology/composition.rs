use super::reasoning::Reasoned;
use super::*;

/// Every IRI a triple set names, in any position. Schema and staged ABox references both
/// contribute seeds to the effective ontology.
fn referenced_iris(triples: &[Triple]) -> Vec<String> {
    let mut out = Vec::with_capacity(triples.len() * 2);
    for triple in triples {
        if let NamedOrBlankNode::NamedNode(subject) = &triple.subject {
            out.push(subject.as_str().to_string());
        }
        out.push(triple.predicate.as_str().to_string());
        if let Term::NamedNode(object) = &triple.object {
            out.push(object.as_str().to_string());
        }
    }
    out
}

pub fn seed_set(
    catalogue: &OntologyCatalogue,
    delta_triples: &[Triple],
    referenced: &[String],
) -> Vec<String> {
    let prefixes = catalogue.prefixes();
    let mut all: Vec<String> = referenced
        .iter()
        .map(|term| prefixes.expand(term))
        .chain(referenced_iris(delta_triples))
        .collect();
    all.sort();
    all.dedup();
    all
}

/// A user's ontology: the cut of the catalogue their vault reaches, plus this user's
/// `frona:` delta, resolved to a version, with the delta pre-lowered to triples.
pub struct UserOntology {
    /// Held so a pass can widen its own effective ontology mid-flight - see
    /// [`effective_ontology_admitting_all`](Self::effective_ontology_admitting_all).
    catalogue: Arc<OntologyCatalogue>,
    effective_ontology: Arc<OntologyScope>,
    delta_ofn: String,
    delta_triples: Vec<Triple>,
    version: i64,
}

impl UserOntology {
    pub fn build(
        catalogue: Arc<OntologyCatalogue>,
        effective_ontology: Arc<OntologyScope>,
        delta_ofn: String,
        delta_triples: Vec<Triple>,
        version: i64,
    ) -> Self {
        Self {
            catalogue,
            effective_ontology,
            delta_ofn,
            delta_triples,
            version,
        }
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    pub fn delta_ofn(&self) -> &str {
        &self.delta_ofn
    }

    /// The delta lowered to triples (base ⊕ these ⊕ abox is the reasoning input).
    pub fn delta_triples(&self) -> &[Triple] {
        &self.delta_triples
    }

    pub fn prefixes(&self) -> &PrefixMap {
        self.effective_ontology.prefixes()
    }

    /// The cut of the catalogue this user reasons over. A stable snapshot for the
    /// life of the pass.
    pub fn effective_ontology(&self) -> &Arc<OntologyScope> {
        &self.effective_ontology
    }

    /// Widen the effective ontology for every term referenced by several graph layers.
    /// A staged A-Box is as much a source of required T-Box terms as a schema edit: the
    /// assertion cannot be judged before its class or property definition is in scope.
    pub fn effective_ontology_admitting_all(&self, extras: &[&[Triple]]) -> Arc<OntologyScope> {
        let mut seeds: Vec<String> = self.effective_ontology.seeds().to_vec();
        let before = seeds.len();
        for extra in extras {
            seeds.extend(
                referenced_iris(extra)
                    .into_iter()
                    .filter(|iri| self.catalogue.declares(iri)),
            );
        }
        seeds.sort();
        seeds.dedup();
        if seeds.len() == before {
            return self.effective_ontology.clone();
        }
        self.catalogue.project(&seeds)
    }

    /// The `frona:` terms minted so far (delta only) - for the Classify prompt.
    pub fn catalog(&self) -> Result<Catalog, Error> {
        schema::catalog(&self.delta_ofn, self.effective_ontology.prefixes())
    }

    /// The delta's own constraint/derivation axioms, as the targets an `Amend` can name.
    pub fn retractable(&self) -> Result<Vec<OverrideTarget>, Error> {
        schema::retractable(&self.delta_ofn, self.effective_ontology.prefixes())
    }

    /// Reason over `base ⊕ delta ⊕ abox` → a queryable materialized closure.
    pub fn reason(&self, abox: &[Triple]) -> Result<Reasoned, Error> {
        reasoning::materialize(self.effective_ontology.triples(), &self.delta_triples, abox)
    }
}

/// A per-pass composition: the shared registry (via the user's `base`) ⊕ the user's
/// committed delta ⊕ an optional layer of this pass's **proposed** schema edits.
///
/// The proposed layer is what lets classify and resolve reason over edits the pass has
/// put forward but Assemble has not committed - so entity N is judged against the terms
/// entities 1..N-1 introduced, not against the stored schema alone. Nothing is persisted:
/// the layer simply ceases to exist when the pass ends. Composition is a union of
/// full-IRI triples.
pub struct ComposedOntology {
    /// `delta ⊕ proposed`, lowered to triples once.
    schema_triples: Vec<Triple>,
    /// The user's effective ontology, widened to admit whatever catalogue terms the
    /// proposals reference. Frozen for the pass.
    effective_ontology: Arc<OntologyScope>,
}

impl ComposedOntology {
    /// Compose the user's committed delta with this pass's `proposed_edits`. With none,
    /// this is exactly the committed schema.
    pub fn with_proposed(
        user: &UserOntology,
        px: &PrefixMap,
        proposed_edits: &[SchemaEdit],
        abox: &[Triple],
    ) -> Result<Self, Error> {
        let schema_triples = if proposed_edits.is_empty() {
            user.delta_triples().to_vec()
        } else {
            let scratch_ofn = schema::apply_edits(user.delta_ofn(), proposed_edits, px)?;
            schema::delta_triples(&scratch_ofn)?
        };
        // A proposal is free to reference a catalogue term the vault has never used -
        // that is what adopting a standard term *is*. Judge it against that term's real
        // ancestors and disjointness, not against a cut that predates it.
        let effective_ontology = user.effective_ontology_admitting_all(&[&schema_triples, abox]);
        Ok(Self {
            schema_triples,
            effective_ontology,
        })
    }

    /// Reason over `effective ⊕ (delta ⊕ proposed) ⊕ abox` → the materialized graph. The
    /// `abox` may itself carry proposed individual types (entities under a proposed,
    /// not-yet-stamped kind), so resolve can type-filter over proposals.
    pub fn reason(&self, abox: &[Triple]) -> Result<Reasoned, Error> {
        reasoning::materialize(
            self.effective_ontology.triples(),
            &self.schema_triples,
            abox,
        )
    }

    /// What this composition reasons under - the user's, widened.
    pub fn effective_ontology(&self) -> &Arc<OntologyScope> {
        &self.effective_ontology
    }
}

impl Ontology {
    /// The user's delta serialized as OFN.
    pub async fn serialize(&self, user_id: &str) -> Result<String, Error> {
        Ok(self.load(user_id).await?.delta_ofn().to_string())
    }

    /// Load the user's ontology at its current persisted version (an absent delta
    /// is the empty delta at version 0 - the user's TBox is exactly the catalogue cut
    /// their vault reaches).
    pub async fn load(&self, user_id: &str) -> Result<UserOntology, Error> {
        self.ensure_scope(user_id)?;
        let row = Some(self.state.clone());
        let (ofn, version) = row
            .as_ref()
            .map(|o| (o.owl.clone(), o.version))
            .unwrap_or_else(|| (String::new(), 0));

        let catalogue = self
            .catalogue()
            .ok_or_else(|| Error::Internal("ontology: no catalogue installed yet".into()))?;
        let delta_triples = schema::delta_triples(&ofn)?;
        let seeds = seed_set(&catalogue, &delta_triples, &self.referenced_terms());
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
            .cut_effective_ontology(user_id, &catalogue, row.as_ref(), seeds, version)
            .await?;
        Ok(UserOntology::build(
            catalogue,
            effective_ontology,
            ofn,
            delta_triples,
            version,
        ))
    }

    /// Re-cut the effective ontology from the supplied snapshot.
    ///
    /// The cut is what the knowledge base reasons over *now*, so it tracks the vault:
    /// a term nothing references any more leaves. The one exception is a term an entity is
    /// still typed with whose source has left the catalogue - the seed is there but
    /// `project` can supply nothing for it, and dropping it would untype the entity
    /// without anyone having asked. Those are carried forward from the stored copy.
    pub(super) async fn cut_effective_ontology(
        &self,
        user_id: &str,
        catalogue: &Arc<OntologyCatalogue>,
        stored: Option<&crate::model::KnowledgeOntology>,
        seeds: Vec<String>,
        _version: i64,
    ) -> Result<Arc<OntologyScope>, Error> {
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
        Ok(effective_ontology)
    }
}
