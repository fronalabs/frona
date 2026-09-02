use std::collections::{BTreeSet, HashMap, HashSet};

use oxrdf::NamedOrBlankNode;

use crate::core::error::AppError;
use crate::memory::pkm::ontology::OntologyManager;
use crate::memory::pkm::ontology::catalogue::OntologyCatalogue;
use crate::memory::pkm::ontology::prefixes::{KB_NAMESPACE, PrefixMap};
use crate::memory::pkm::ontology::schema::{self, SchemaEdit};

mod model;
mod overlay;

pub use model::{
    OntologyExport, OntologyPropertyInspection, OntologySearchHit, OntologyTermInspection,
    OntologyTermRelation,
};
use overlay::SchemaOverlay;

const CHILD_LIMIT: usize = 25;
const CATALOGUE_SEARCH_MULTIPLIER: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedClassTerm {
    pub(super) iri: String,
    pub(super) term: String,
    pub(super) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExactClassResolution {
    None,
    Resolved(Vec<ResolvedClassTerm>),
    Ambiguous(Vec<ResolvedClassTerm>),
}

struct OntologySchemaView {
    catalogue: std::sync::Arc<OntologyCatalogue>,
    overlay: SchemaOverlay,
    effective_overlay: Option<SchemaOverlay>,
    proposed: HashSet<String>,
}

impl OntologySchemaView {
    fn term(&self, iri: &str, active: &HashSet<String>) -> OntologyTermInspection {
        let base = self.catalogue.term(iri, CHILD_LIMIT + 1);
        let local = self.overlay.terms.get(iri);
        let effective = self
            .effective_overlay
            .as_ref()
            .and_then(|overlay| overlay.terms.get(iri));
        let exists = base.is_some() || local.and_then(|term| term.kind.as_ref()).is_some();
        let compact = |value: &str| {
            self.catalogue
                .prefixes()
                .compact(value)
                .unwrap_or_else(|| value.to_string())
        };
        let merged = |base: Vec<String>,
                      local: Option<&BTreeSet<String>>,
                      effective: Option<&BTreeSet<String>>| {
            if self.effective_overlay.is_some() {
                return effective.into_iter().flatten().cloned().collect::<Vec<_>>();
            }
            let mut values: BTreeSet<String> = base.into_iter().collect();
            if let Some(local) = local {
                values.extend(local.iter().cloned());
            }
            values.into_iter().collect::<Vec<_>>()
        };
        let direct_parents = merged(
            base.as_ref()
                .map(|term| term.direct_parents.clone())
                .unwrap_or_default(),
            local.map(|term| &term.direct_parents),
            effective.map(|term| &term.direct_parents),
        );
        let mut direct_children = merged(
            base.as_ref()
                .map(|term| term.direct_children.clone())
                .unwrap_or_default(),
            local.map(|term| &term.direct_children),
            effective.map(|term| &term.direct_children),
        );
        let children_truncated = if self.effective_overlay.is_some() {
            direct_children.len() > CHILD_LIMIT
        } else {
            base.as_ref().is_some_and(|term| term.children_truncated)
                || direct_children.len() > CHILD_LIMIT
        };
        direct_children.truncate(CHILD_LIMIT);
        let equivalents = merged(
            base.as_ref()
                .map(|term| term.equivalents.clone())
                .unwrap_or_default(),
            local.map(|term| &term.equivalents),
            effective.map(|term| &term.equivalents),
        );
        let disjoint_with = merged(
            base.as_ref()
                .map(|term| term.disjoint_with.clone())
                .unwrap_or_default(),
            local.map(|term| &term.disjoint_with),
            effective.map(|term| &term.disjoint_with),
        );
        let domain = merged(
            base.as_ref()
                .map(|term| term.domain.clone())
                .unwrap_or_default(),
            local.map(|term| &term.domain),
            effective.map(|term| &term.domain),
        );
        let range = merged(
            base.as_ref()
                .map(|term| term.range.clone())
                .unwrap_or_default(),
            local.map(|term| &term.range),
            effective.map(|term| &term.range),
        );
        let inverse = merged(
            base.as_ref()
                .map(|term| term.inverse.clone())
                .unwrap_or_default(),
            local.map(|term| &term.inverse),
            effective.map(|term| &term.inverse),
        );
        let kind = local
            .and_then(|term| term.kind.clone())
            .or_else(|| base.as_ref().map(|term| term.kind.clone()));
        let property =
            kind.as_deref()
                .filter(|kind| *kind != "class")
                .map(|_| OntologyPropertyInspection {
                    domain: domain.iter().map(|value| compact(value)).collect(),
                    range: range.iter().map(|value| compact(value)).collect(),
                    inverse: inverse.iter().map(|value| compact(value)).collect(),
                });
        OntologyTermInspection {
            term: compact(iri),
            exists,
            kind,
            label: local
                .and_then(|term| term.label.clone())
                .or_else(|| base.as_ref().and_then(|term| term.label.clone())),
            definition: local
                .and_then(|term| term.definition.clone())
                .or_else(|| base.as_ref().and_then(|term| term.definition.clone())),
            origin: if local.and_then(|term| term.kind.as_ref()).is_some() {
                Some("user".to_string())
            } else {
                base.as_ref().and_then(|term| term.source.clone())
            },
            user_relevance: self.relevance(iri, active).to_string(),
            direct_parents: direct_parents.iter().map(|value| compact(value)).collect(),
            ancestors: self
                .ancestors(iri)
                .iter()
                .map(|value| compact(value))
                .collect(),
            direct_children: direct_children.iter().map(|value| compact(value)).collect(),
            children_truncated,
            equivalents: equivalents.iter().map(|value| compact(value)).collect(),
            disjoint_with: disjoint_with.iter().map(|value| compact(value)).collect(),
            property,
        }
    }

    fn parents(&self, iri: &str) -> Vec<String> {
        if let Some(effective) = &self.effective_overlay {
            return effective
                .terms
                .get(iri)
                .map(|term| term.direct_parents.iter().cloned().collect())
                .unwrap_or_default();
        }
        let mut values = BTreeSet::new();
        values.extend(self.catalogue.direct_parents(iri));
        if let Some(term) = self.overlay.terms.get(iri) {
            values.extend(term.direct_parents.iter().cloned());
        }
        values.into_iter().collect()
    }

    fn equivalents(&self, iri: &str) -> Vec<String> {
        if let Some(effective) = &self.effective_overlay {
            return effective
                .terms
                .get(iri)
                .map(|term| term.equivalents.iter().cloned().collect())
                .unwrap_or_default();
        }
        let mut values = BTreeSet::new();
        values.extend(self.catalogue.equivalents(iri));
        if let Some(term) = self.overlay.terms.get(iri) {
            values.extend(term.equivalents.iter().cloned());
        }
        values.into_iter().collect()
    }

    fn ancestors(&self, iri: &str) -> BTreeSet<String> {
        let mut ancestors = walk_ancestors(
            iri,
            |term| self.parents(term),
            |term| self.equivalents(term),
        );
        let mut equivalent_queue = self.equivalents(iri);
        let mut equivalents = HashSet::new();
        while let Some(equivalent) = equivalent_queue.pop() {
            if equivalents.insert(equivalent.clone()) {
                equivalent_queue.extend(self.equivalents(&equivalent));
            }
        }
        for equivalent in equivalents {
            ancestors.remove(&equivalent);
        }
        ancestors
    }

    fn equivalent(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let mut visited = HashSet::from([a.to_string()]);
        let mut queue = vec![a.to_string()];
        while let Some(current) = queue.pop() {
            for next in self.equivalents(&current) {
                if next == b {
                    return true;
                }
                if visited.insert(next.clone()) {
                    queue.push(next);
                }
            }
        }
        false
    }

    fn disjoint(&self, a: &str, b: &str) -> bool {
        let mut left = self.ancestors(a);
        let mut right = self.ancestors(b);
        left.insert(a.to_string());
        right.insert(b.to_string());
        for term in &left {
            let disjoint = if let Some(effective) = &self.effective_overlay {
                effective
                    .terms
                    .get(term)
                    .map(|term| term.disjoint_with.clone())
                    .unwrap_or_default()
            } else {
                let mut disjoint = BTreeSet::new();
                disjoint.extend(self.catalogue.disjoint_with(term));
                if let Some(local) = self.overlay.terms.get(term) {
                    disjoint.extend(local.disjoint_with.iter().cloned());
                }
                disjoint
            };
            if disjoint.iter().any(|candidate| right.contains(candidate)) {
                return true;
            }
        }
        false
    }

    fn relation(&self, a: &str, b: &str) -> &'static str {
        if a == b {
            "same"
        } else if self.equivalent(a, b) {
            "equivalent"
        } else if self.ancestors(a).contains(b) {
            "subclass"
        } else if self.ancestors(b).contains(a) {
            "superclass"
        } else if self.disjoint(a, b) {
            "disjoint"
        } else {
            "unrelated"
        }
    }

    fn relevance(&self, iri: &str, active: &HashSet<String>) -> &'static str {
        if self.proposed.contains(iri) {
            "proposed"
        } else if active.contains(iri) {
            "directly_used"
        } else if self
            .overlay
            .terms
            .get(iri)
            .and_then(|term| term.kind.as_ref())
            .is_some()
        {
            "user_defined"
        } else {
            "catalogue"
        }
    }
}

/// Walk a taxonomy composed from any parent and equivalence indexes. Inspection and
/// type normalization use this one rule so their subsumption answers cannot drift.
pub(super) fn walk_ancestors(
    iri: &str,
    mut parents: impl FnMut(&str) -> Vec<String>,
    mut equivalents: impl FnMut(&str) -> Vec<String>,
) -> BTreeSet<String> {
    let mut ancestors = BTreeSet::new();
    let mut visited = HashSet::from([iri.to_string()]);
    let mut queue = vec![iri.to_string()];
    while let Some(current) = queue.pop() {
        for equivalent in equivalents(&current) {
            ancestors.insert(equivalent.clone());
            if visited.insert(equivalent.clone()) {
                queue.push(equivalent);
            }
        }
        for parent in parents(&current) {
            ancestors.insert(parent.clone());
            if visited.insert(parent.clone()) {
                queue.push(parent);
            }
        }
    }
    ancestors.remove(iri);
    ancestors
}

impl OntologyManager {
    pub async fn catalog(&self, user_id: &str) -> Result<super::Catalog, AppError> {
        self.load(user_id).await?.catalog()
    }

    /// The axioms this user's delta has in force that Assemble could loosen.
    pub(crate) async fn retractable(
        &self,
        user_id: &str,
    ) -> Result<Vec<super::OverrideTarget>, AppError> {
        self.load(user_id).await?.retractable()
    }

    /// Build the stable semantic export without exposing loaded ontology state or ABox
    /// lowering to binary callers.
    pub async fn export(&self, user_id: &str) -> Result<OntologyExport, AppError> {
        let ontology = self.load(user_id).await?;
        let mut tbox = ontology.effective_ontology().triples().to_vec();
        tbox.extend_from_slice(ontology.delta_triples());
        let entities = self.repo.list_entities(user_id).await?;
        let links = self.repo.asserted_links(user_id).await?;
        let abox = super::abox::build_abox_triples(&entities, &links, ontology.prefixes());
        Ok(OntologyExport {
            tbox,
            abox,
            entity_count: entities.len(),
            asserted_link_count: links.len(),
        })
    }

    async fn schema_view(
        &self,
        user_id: &str,
        proposed_edits: &[SchemaEdit],
    ) -> Result<OntologySchemaView, AppError> {
        let ofn = self
            .repo
            .ontology_get(user_id)
            .await?
            .map(|ontology| ontology.owl)
            .unwrap_or_default();
        self.schema_view_from_ofn(&ofn, proposed_edits)
    }

    fn schema_view_from_ofn(
        &self,
        ofn: &str,
        proposed_edits: &[SchemaEdit],
    ) -> Result<OntologySchemaView, AppError> {
        let catalogue = self
            .catalogue()
            .ok_or_else(|| AppError::Internal("ontology: no catalogue installed yet".into()))?;
        let combined = if proposed_edits.is_empty() {
            ofn.to_string()
        } else {
            schema::apply_edits(ofn, proposed_edits, catalogue.prefixes())?
        };
        let triples = schema::delta_triples(&combined)?;
        let proposed = proposed_frona_terms(proposed_edits, catalogue.prefixes());
        Ok(OntologySchemaView {
            catalogue,
            overlay: SchemaOverlay::from_triples(&triples),
            effective_overlay: None,
            proposed,
        })
    }

    fn published_schema_view(
        &self,
        pass: &super::reasoning::ReasonPass,
    ) -> Result<OntologySchemaView, AppError> {
        let mut view = self.schema_view_from_ofn(&pass.delta_ofn, &[])?;
        let mut triples = pass.effective_ontology.triples().to_vec();
        triples.extend(schema::delta_triples(&pass.delta_ofn)?);
        view.effective_overlay = Some(SchemaOverlay::from_triples(&triples));
        Ok(view)
    }

    /// Equivalent exact matches form one interpretation; unrelated matches are ambiguous.
    pub(super) fn resolve_published_class_query(
        &self,
        pass: &super::reasoning::ReasonPass,
        query: &str,
    ) -> Result<ExactClassResolution, AppError> {
        let view = self.published_schema_view(pass)?;
        let effective_terms = published_effective_terms(pass)?;
        let prefixes = view.catalogue.prefixes();

        let details = |iri: &str| {
            let base = view.catalogue.term(iri, 0);
            let local = view.overlay.terms.get(iri);
            let effective = view
                .effective_overlay
                .as_ref()
                .and_then(|overlay| overlay.terms.get(iri));
            let kind = effective
                .and_then(|term| term.kind.clone())
                .or_else(|| local.and_then(|term| term.kind.clone()))
                .or_else(|| base.as_ref().map(|term| term.kind.clone()));
            let label = effective
                .and_then(|term| term.label.clone())
                .or_else(|| local.and_then(|term| term.label.clone()))
                .or_else(|| base.as_ref().and_then(|term| term.label.clone()));
            (kind, label)
        };
        let display = |iri: &str, label: Option<String>| ResolvedClassTerm {
            iri: iri.to_string(),
            term: prefixes.compact(iri).unwrap_or_else(|| iri.to_string()),
            label,
        };

        let trimmed = query.trim();
        let expanded = prefixes.expand(trimmed);
        let explicit = effective_terms.contains(&expanded)
            && details(&expanded).0.as_deref() == Some("class")
            && (expanded != trimmed
                || trimmed.starts_with("http://")
                || trimmed.starts_with("https://"));

        let mut exact = Vec::new();
        if explicit {
            exact.push(expanded);
        } else {
            for iri in &effective_terms {
                let (kind, label) = details(iri);
                if kind.as_deref() == Some("class")
                    && view
                        .catalogue
                        .exactly_matches_term(trimmed, iri, label.as_deref())
                {
                    exact.push(iri.clone());
                }
            }
        }
        exact.sort();
        exact.dedup();
        let Some(first) = exact.first().cloned() else {
            return Ok(ExactClassResolution::None);
        };
        if exact
            .iter()
            .skip(1)
            .any(|term| !view.equivalent(&first, term))
        {
            return Ok(ExactClassResolution::Ambiguous(
                exact
                    .into_iter()
                    .map(|iri| {
                        let label = details(&iri).1;
                        display(&iri, label)
                    })
                    .collect(),
            ));
        }

        let mut group: BTreeSet<String> = exact.into_iter().collect();
        let mut queue: Vec<String> = group.iter().cloned().collect();
        while let Some(current) = queue.pop() {
            for equivalent in view.equivalents(&current) {
                if effective_terms.contains(&equivalent)
                    && details(&equivalent).0.as_deref() == Some("class")
                    && group.insert(equivalent.clone())
                {
                    queue.push(equivalent);
                }
            }
        }
        Ok(ExactClassResolution::Resolved(
            group
                .into_iter()
                .map(|iri| {
                    let label = details(&iri).1;
                    display(&iri, label)
                })
                .collect(),
        ))
    }

    pub(crate) async fn inspect_ontology_terms(
        &self,
        user_id: &str,
        proposed_edits: &[SchemaEdit],
        active_terms: &HashSet<String>,
        terms: &[String],
    ) -> Result<(Vec<OntologyTermInspection>, Vec<OntologyTermRelation>), AppError> {
        let view = self.schema_view(user_id, proposed_edits).await?;
        Ok(Self::inspect_terms(&view, active_terms, terms))
    }

    fn inspect_terms(
        view: &OntologySchemaView,
        active_terms: &HashSet<String>,
        terms: &[String],
    ) -> (Vec<OntologyTermInspection>, Vec<OntologyTermRelation>) {
        let expanded: Vec<String> = terms
            .iter()
            .map(|term| view.catalogue.prefixes().expand(term))
            .collect();
        let inspections = expanded
            .iter()
            .map(|term| view.term(term, active_terms))
            .collect();
        let mut relations = Vec::new();
        for (index, a) in expanded.iter().enumerate() {
            for b in &expanded[index + 1..] {
                relations.push(OntologyTermRelation {
                    a: view
                        .catalogue
                        .prefixes()
                        .compact(a)
                        .unwrap_or_else(|| a.clone()),
                    b: view
                        .catalogue
                        .prefixes()
                        .compact(b)
                        .unwrap_or_else(|| b.clone()),
                    relation: view.relation(a, b).to_string(),
                });
            }
        }
        (inspections, relations)
    }

    pub(crate) async fn search_ontology_terms(
        &self,
        user_id: &str,
        proposed_edits: &[SchemaEdit],
        active_terms: &HashSet<String>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<OntologySearchHit>, AppError> {
        let view = self.schema_view(user_id, proposed_edits).await?;
        Ok(Self::search_terms(&view, active_terms, None, query, limit))
    }

    fn search_terms(
        view: &OntologySchemaView,
        active_terms: &HashSet<String>,
        allowed_terms: Option<&HashSet<String>>,
        query: &str,
        limit: usize,
    ) -> Vec<OntologySearchHit> {
        let catalogue_limit = limit.saturating_mul(CATALOGUE_SEARCH_MULTIPLIER).max(limit);
        let mut candidates: HashMap<String, (u8, usize, String, String, Option<String>)> =
            HashMap::new();

        if let Some(allowed) = allowed_terms {
            for iri in allowed {
                let Some(term) = view.catalogue.term(iri, 0) else {
                    continue;
                };
                let Some((rank, brevity)) =
                    view.catalogue.match_term(query, iri, term.label.as_deref())
                else {
                    continue;
                };
                candidates.insert(
                    iri.clone(),
                    (
                        rank,
                        brevity,
                        term.kind,
                        term.source.unwrap_or_else(|| "catalogue".to_string()),
                        term.label,
                    ),
                );
            }
        } else {
            for (rank, brevity, hit) in view.catalogue.search_ranked(query, catalogue_limit) {
                let iri = view.catalogue.prefixes().expand(&hit.curie);
                let source = view
                    .catalogue
                    .term(&iri, 0)
                    .and_then(|term| term.source)
                    .unwrap_or_else(|| "catalogue".to_string());
                candidates.insert(
                    iri,
                    (rank, brevity, hit.kind.to_string(), source, hit.label),
                );
            }
        }
        for iri in active_terms.iter().chain(view.overlay.terms.keys()) {
            if allowed_terms.is_some_and(|allowed| !allowed.contains(iri)) {
                continue;
            }
            let base = view.catalogue.term(iri, 0);
            let local = view.overlay.terms.get(iri);
            let label = local
                .and_then(|term| term.label.clone())
                .or_else(|| base.as_ref().and_then(|term| term.label.clone()));
            let kind = local
                .and_then(|term| term.kind.clone())
                .or_else(|| base.as_ref().map(|term| term.kind.clone()));
            let Some(kind) = kind else { continue };
            let Some((rank, brevity)) = view.catalogue.match_term(query, iri, label.as_deref())
            else {
                continue;
            };
            let origin = if local.and_then(|term| term.kind.as_ref()).is_some() {
                "user".to_string()
            } else {
                base.and_then(|term| term.source)
                    .unwrap_or_else(|| "catalogue".to_string())
            };
            candidates.insert(iri.clone(), (rank, brevity, kind, origin, label));
        }
        let relevance_rank = |iri: &str| match view.relevance(iri, active_terms) {
            "proposed" => 0,
            "directly_used" => 1,
            "user_defined" => 2,
            _ => 3,
        };
        let mut ranked: Vec<_> = candidates.into_iter().collect();
        ranked.sort_by(|a, b| {
            let (a_iri, (a_rank, a_brevity, ..)) = a;
            let (b_iri, (b_rank, b_brevity, ..)) = b;
            (*a_rank, relevance_rank(a_iri), *a_brevity, a_iri).cmp(&(
                *b_rank,
                relevance_rank(b_iri),
                *b_brevity,
                b_iri,
            ))
        });
        ranked.truncate(limit);
        ranked
            .into_iter()
            .map(|(iri, (_, _, kind, origin, label))| OntologySearchHit {
                term: view
                    .catalogue
                    .prefixes()
                    .compact(&iri)
                    .unwrap_or(iri.clone()),
                kind,
                label,
                origin,
                user_relevance: view.relevance(&iri, active_terms).to_string(),
            })
            .collect()
    }

    /// Count entities and asserted links that use one ontology term.
    pub(crate) async fn usage_impact(
        &self,
        user_id: &str,
        term: &str,
    ) -> Result<(usize, usize), AppError> {
        let user = self.load(user_id).await?;
        let prefixes = user.prefixes();
        let target = prefixes.expand(term);
        let entity_count = self
            .repo
            .list_entities(user_id)
            .await?
            .iter()
            .filter(|entity| {
                entity
                    .kinds
                    .iter()
                    .any(|kind| prefixes.expand(kind) == target)
            })
            .count();
        let link_count = self
            .repo
            .asserted_links(user_id)
            .await?
            .iter()
            .filter(|link| prefixes.expand(&link.relation) == target)
            .count();
        Ok((entity_count, link_count))
    }
}

fn published_effective_terms(
    pass: &super::reasoning::ReasonPass,
) -> Result<HashSet<String>, AppError> {
    let delta = schema::delta_triples(&pass.delta_ofn)?;
    let mut terms = HashSet::new();
    for triple in pass.effective_ontology.triples().iter().chain(delta.iter()) {
        if let NamedOrBlankNode::NamedNode(subject) = &triple.subject {
            terms.insert(subject.as_str().to_string());
        }
    }
    Ok(terms)
}

fn proposed_frona_terms(edits: &[SchemaEdit], prefixes: &PrefixMap) -> HashSet<String> {
    let mut terms = HashSet::new();
    let mut add = |term: &str| {
        let iri = prefixes.expand(term);
        if iri.starts_with("urn:frona:") && !iri.starts_with(KB_NAMESPACE) {
            terms.insert(iri);
        }
    };
    for edit in edits {
        match edit {
            SchemaEdit::AnnotateComment { term, .. } => add(term),
            SchemaEdit::DeclareClass { class } => add(class),
            SchemaEdit::SubClassOf { sub, sup } => {
                add(sub);
                add(sup);
            }
            SchemaEdit::EquivalentClasses { a, b }
            | SchemaEdit::DisjointClasses { a, b }
            | SchemaEdit::EquivalentProperties { a, b }
            | SchemaEdit::InverseProperties { a, b } => {
                add(a);
                add(b);
            }
            SchemaEdit::DeclareObjectProperty { property }
            | SchemaEdit::DeclareDataProperty { property }
            | SchemaEdit::PropertyCharacteristic { property, .. } => add(property),
            SchemaEdit::SubPropertyOf { sub, sup } => {
                add(sub);
                add(sup);
            }
            SchemaEdit::ObjectPropertyDomain { property, class }
            | SchemaEdit::ObjectPropertyRange { property, class } => {
                add(property);
                add(class);
            }
            SchemaEdit::RestrictDatatype { property, .. } => add(property),
            SchemaEdit::Align { frona, .. } => add(frona),
            SchemaEdit::AmendOverride { .. } => {}
        }
    }
    terms
}
