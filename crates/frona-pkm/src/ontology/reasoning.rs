use oxigraph::store::Store;
use oxrdf::GraphName;
use reasonable::reasoner::Reasoner;

use super::*;

const FALSE_POSITIVE_RULE: &str = "rdfs-datatype-range";

pub const CLASH_RULES: &[&str] = &["cax-dw", "cls-nothing2", "prp-pdw", "prp-asyp", "prp-irp"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub rule: String,
    pub severity: String,
    pub message: String,
}

impl Diagnostic {
    pub fn is_clash(&self) -> bool {
        CLASH_RULES.contains(&self.rule.as_str())
    }
}

pub struct Reasoned {
    pub store: Store,
    pub(super) diagnostics: Vec<Diagnostic>,
}

impl Reasoned {
    pub fn clashes(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_clash())
    }
}

/// Reason over `base ⊕ delta ⊕ abox` and return the queryable closure.
pub(super) fn materialize(
    base: &[Triple],
    delta: &[Triple],
    abox: &[Triple],
) -> Result<Reasoned, Error> {
    let mut all = Vec::with_capacity(base.len() + delta.len() + abox.len());
    all.extend_from_slice(base);
    all.extend_from_slice(delta);
    all.extend_from_slice(abox);

    let mut reasoner = Reasoner::new();
    reasoner.load_triples(all);
    reasoner.reason();

    let diagnostics = reasoner
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.rule() != FALSE_POSITIVE_RULE)
        .map(|diagnostic| Diagnostic {
            rule: diagnostic.rule().to_string(),
            severity: diagnostic.severity().to_string(),
            message: diagnostic.message().to_string(),
        })
        .collect();

    let closure = reasoner.view_output();
    let store = Store::new()
        .map_err(|error| Error::Internal(format!("ontology: reasoned store: {error}")))?;
    store
        .extend(
            closure
                .iter()
                .map(|triple| triple.clone().in_graph(GraphName::DefaultGraph)),
        )
        .map_err(|error| Error::Internal(format!("ontology: load closure: {error}")))?;

    Ok(Reasoned { store, diagnostics })
}

/// One completed reasoning pass over a user's graph: the materialized closure
/// (query surface) plus the source rows it was built from (needed to compute the
/// inferred-link write-back and to validate facet bounds).
pub struct ReasonPass {
    pub reasoned: Reasoned,
    pub(super) entities: Vec<KnowledgeEntity>,
    pub(super) asserted_links: Vec<KnowledgeEntityLink>,
    pub(super) delta_ofn: String,
    /// What this pass reasoned under. Carried so downstream steps
    /// (`validate`, the inferred-link write-back) compact IRIs through the *same*
    /// prefix map the ABox was built with, rather than the bundled one.
    pub(super) effective_ontology: Arc<OntologyScope>,
}

impl ReasonPass {
    /// Return derived edges and identity candidates for host-owned persistence or
    /// a subsequent Resolve invocation. This never writes or applies a merge.
    pub fn inferred_graph(&self) -> Result<super::InferredGraph, Error> {
        abox::extract_inferred(
            &self.reasoned.store,
            &self.asserted_links,
            self.effective_ontology.prefixes(),
        )
    }
}

impl Ontology {
    pub fn assertion_graph(
        &self,
        entities: &[KnowledgeEntity],
        links: &[KnowledgeEntityLink],
    ) -> Vec<Triple> {
        abox::build_abox_triples(entities, links, &self.prefixes())
    }

    /// Build the user's ABox from the supplied snapshot, reason over `effective ⊕ delta ⊕ ABox`, and
    /// return the pass (the queryable closure + the source entities/links). Pure read;
    /// nothing is written.
    pub async fn reason_user(&self, user_id: &str) -> Result<ReasonPass, Error> {
        let user = self.load(user_id).await?;
        let entities = self.graph.knowledge_entities();
        let asserted_links = self.asserted_links();
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

    pub(super) async fn reason_snapshot(&self, user_id: &str) -> Result<Arc<ReasonPass>, Error> {
        Ok(Arc::new(self.reason_user(user_id).await?))
    }

    pub async fn reason_user_with_proposed(
        &self,
        user_id: &str,
        proposed_edits: &[SchemaEdit],
        extra_abox: &[Triple],
    ) -> Result<ReasonPass, Error> {
        let user = self.load(user_id).await?;
        let entities = self.graph.knowledge_entities();
        let asserted_links = self.asserted_links();
        let px = user.effective_ontology().prefixes();
        let mut abox = abox::build_abox_triples(&entities, &asserted_links, px);
        abox.extend_from_slice(extra_abox);
        let composed = ComposedOntology::with_proposed(&user, px, proposed_edits, &abox)?;
        let reasoned = composed.reason(&abox)?;
        let effective_ontology = composed.effective_ontology().clone();
        let delta_ofn = user.delta_ofn().to_string();
        Ok(ReasonPass {
            reasoned,
            entities,
            asserted_links,
            delta_ofn,
            effective_ontology,
        })
    }

    /// Query the reasoned snapshot without persisting inferred links or other state.
    pub async fn sparql(&self, user_id: &str, query: &str) -> Result<QueryResults<'static>, Error> {
        let pass = self.reason_snapshot(user_id).await?;
        sparql::query(
            &pass.reasoned.store,
            query,
            pass.effective_ontology.prefixes(),
        )
    }

    pub async fn entails_type(
        &self,
        user_id: &str,
        entity_path: &str,
        class: &str,
    ) -> Result<bool, Error> {
        let pass = self.reason_snapshot(user_id).await?;
        let query = format!(
            "ASK {{ <{}> a <{}> }}",
            individual_iri(entity_path),
            self.prefixes().expand(class),
        );
        sparql::ask(
            &pass.reasoned.store,
            &query,
            pass.effective_ontology.prefixes(),
        )
    }
}
