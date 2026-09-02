//! Lowering a user's entities + asserted links into RDF **ABox** triples for
//! reasoning, and reading inferred entity-graph edges back out of the closure.
//!
//!   - each **Concept** entity → `rdf:type <kind>` on its individual
//!   - each attribute → a typed-literal datatype-property assertion
//!   - each asserted link → an object-property assertion between individuals
//!
//! Playbook entities are procedures, not ontology individuals, so they are skipped.

use std::collections::{HashMap, HashSet};

use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use oxrdf::{Literal, NamedNode, NamedOrBlankNode, Term, Triple};

use crate::core::error::AppError;
use crate::memory::pkm::model::{
    ENTITY_NAME_PROPERTY_IRI, ENTITY_PATH_PROPERTY_IRI, EntityCategory, KnowledgeEntity,
    KnowledgeEntityLink,
};

use super::prefixes::{KB_NAMESPACE, PrefixMap, individual_iri, path_from_individual};
use super::sparql;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

fn nn(iri: &str) -> NamedNode {
    NamedNode::new_unchecked(iri.to_string())
}
fn subj(iri: &str) -> NamedOrBlankNode {
    NamedOrBlankNode::NamedNode(nn(iri))
}

pub(super) fn entity_is_eligible(entity: &KnowledgeEntity) -> bool {
    entity.category == EntityCategory::Concept
}

pub(super) fn eligible_entity_paths(entities: &[KnowledgeEntity]) -> HashSet<&str> {
    entities
        .iter()
        .filter(|entity| entity_is_eligible(entity))
        .map(|entity| entity.path.as_str())
        .collect()
}

pub(super) fn link_is_eligible(link: &KnowledgeEntityLink, eligible_paths: &HashSet<&str>) -> bool {
    eligible_paths.contains(link.from_entity_path.as_str())
        && eligible_paths.contains(link.to_entity_path.as_str())
}

pub fn build_abox_triples(
    entities: &[KnowledgeEntity],
    links: &[KnowledgeEntityLink],
    prefixes: &PrefixMap,
) -> Vec<Triple> {
    let mut triples = Vec::new();
    // The set of individuals we actually type - links are only lifted between two
    // of these (a link to a playbook / missing entity has no individual to reason on).
    let concepts = eligible_entity_paths(entities);

    for entity in entities {
        if !entity_is_eligible(entity) {
            continue;
        }
        let iri = individual_iri(&entity.path);
        triples.push(Triple::new(
            subj(&iri),
            nn(ENTITY_NAME_PROPERTY_IRI),
            Term::Literal(Literal::new_simple_literal(&entity.name)),
        ));
        triples.push(Triple::new(
            subj(&iri),
            nn(ENTITY_PATH_PROPERTY_IRI),
            Term::Literal(Literal::new_simple_literal(&entity.path)),
        ));
        // One `rdf:type` per class - an entity is genuinely several things at once, and
        // the reasoner derives the union of what they entail.
        //
        // `kinds` are stored as IRIs, but they still go through `expand` (a no-op on an
        // absolute IRI) and `valid_iri`: an attribute key or relation that nothing has
        // typed yet can be free text like "works for", which expands to something with
        // a space in it. Skipping the entry beats poisoning the whole pass - it reasons
        // once the Classify stage maps it.
        for kind in &entity.kinds {
            if !kind.trim().is_empty()
                && let Some(kind_iri) = valid_iri(&prefixes.expand(kind))
            {
                triples.push(Triple::new(
                    subj(&iri),
                    nn(RDF_TYPE),
                    Term::NamedNode(nn(&kind_iri)),
                ));
            }
        }
        if let Some(map) = entity.attributes.as_object() {
            for (key, val) in map {
                if let (Some(key_iri), Some(term)) =
                    (valid_iri(&prefixes.expand(key)), literal_term(val))
                {
                    if key_iri == ENTITY_NAME_PROPERTY_IRI || key_iri == ENTITY_PATH_PROPERTY_IRI {
                        continue;
                    }
                    triples.push(Triple::new(subj(&iri), nn(&key_iri), term));
                }
            }
        }
    }

    for link in links {
        if link_is_eligible(link, &concepts)
            && let Some(rel_iri) = valid_iri(&prefixes.expand(&link.relation))
        {
            triples.push(Triple::new(
                subj(&individual_iri(&link.from_entity_path)),
                nn(&rel_iri),
                Term::NamedNode(nn(&individual_iri(&link.to_entity_path))),
            ));
        }
    }
    triples
}

/// The IRI back, if it is a valid absolute IRI (an un-typed free-text term expands to
/// an invalid one - e.g. a space - which must not enter the reasoner's N-Triples round-trip).
fn valid_iri(iri: &str) -> Option<String> {
    NamedNode::new(iri).ok().map(|_| iri.to_string())
}

/// A JSON attribute value → a typed RDF literal (scalars only; arrays/objects/null
/// are not lifted).
fn literal_term(val: &serde_json::Value) -> Option<Term> {
    let lit = match val {
        serde_json::Value::String(s) => Literal::new_simple_literal(s),
        serde_json::Value::Bool(b) => Literal::new_typed_literal(b.to_string(), nn(XSD_BOOLEAN)),
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => {
            Literal::new_typed_literal(n.to_string(), nn(XSD_INTEGER))
        }
        serde_json::Value::Number(n) => Literal::new_typed_literal(n.to_string(), nn(XSD_DECIMAL)),
        _ => return None,
    };
    Some(Term::Literal(lit))
}

/// `owl:sameAs` - identity, not a navigable entity edge.
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";

/// What a reasoning pass read back out of the closure: the entity-graph edges to
/// persist, and the identity the reasoner concluded.
///
/// The two are split because they are consumed by different things and on different
/// terms. `links` is rewritten wholesale on every pass (wipe + reinsert). `same_as`
/// is never persisted as an edge at all - it is a *merge candidate*, handed to
/// resolve for a verdict, because acting on it deletes an entity.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InferredGraph {
    /// Derived entity-graph edges that were neither asserted nor a mirror of one.
    /// `(from_entity_path, to_entity_path, relation_curie)`, sorted and deduped so the write-back
    /// is idempotent.
    pub links: Vec<(String, String, String)>,
    /// Entity pairs the reasoner concluded are the same entity, `(a, b)` with `a < b`
    /// so each pair appears once regardless of which direction was derived.
    pub same_as: Vec<(String, String)>,
}

/// Read the **inferred** entity graph out of a reasoned closure: object-property
/// assertions between two KB individuals that are not already asserted.
///
/// # Why the mirror filter exists
///
/// OWL RL's `eq-rep-s`/`eq-rep-o` copy every triple across an `owl:sameAs` pair, so
/// the moment the reasoner identifies two entities, each one's edges reappear on the
/// other. Those copies are not new knowledge - they are the same edge seen from the
/// twin - but they are not *asserted* either, so a plain "derived and not asserted"
/// test persists all of them. Measured: two identified entities with two edges each
/// yield eight edges in the closure, four of them mirrors.
///
/// So an edge is dropped when it matches an asserted one *after* both endpoints are
/// mapped to their identity-class representative. That removes exactly the mirrors
/// and nothing else: a genuinely new edge - a `prp-symp` reverse, a `prp-trp`
/// shortcut - has endpoints that are not `sameAs` anything, so canonicalizing leaves
/// it untouched and it survives.
pub fn extract_inferred(
    store: &Store,
    asserted: &[KnowledgeEntityLink],
    prefixes: &PrefixMap,
) -> Result<InferredGraph, AppError> {
    let query = format!(
        "SELECT ?s ?p ?o WHERE {{ ?s ?p ?o . FILTER(isIRI(?o)) \
         FILTER(STRSTARTS(STR(?s), \"{KB_NAMESPACE}\")) \
         FILTER(STRSTARTS(STR(?o), \"{KB_NAMESPACE}\")) }}"
    );

    // One pass over the solutions, splitting identity from edges. Edges are held
    // unfiltered until the identity classes are complete - a `sameAs` may be read
    // after an edge it has to be applied to.
    let mut same_as: Vec<(String, String)> = Vec::new();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    if let QueryResults::Solutions(sols) = sparql::query(store, &query, prefixes)? {
        for sol in sols {
            let sol =
                sol.map_err(|e| AppError::Internal(format!("ontology: inferred sol: {e}")))?;
            let (Some(Term::NamedNode(s)), Some(Term::NamedNode(p)), Some(Term::NamedNode(o))) =
                (sol.get("s"), sol.get("p"), sol.get("o"))
            else {
                continue;
            };
            let (Some(from), Some(to)) = (
                path_from_individual(s.as_str()),
                path_from_individual(o.as_str()),
            ) else {
                continue;
            };
            // Self-loops are never useful entity edges, and `eq-ref` makes one of these
            // for every individual in the graph.
            if from == to {
                continue;
            }
            if p.as_str() == OWL_SAME_AS {
                let pair = if from < to { (from, to) } else { (to, from) };
                same_as.push(pair);
            } else {
                edges.push((from, to, p.as_str().to_string()));
            }
        }
    }
    same_as.sort();
    same_as.dedup();

    let identity = IdentityClasses::new(&same_as);

    // Key asserted edges by expanded relation IRI so a CURIE/bare mismatch never
    // re-reports an asserted edge as inferred, and by canonical endpoint so a
    // mirrored copy keys to the same triple as the edge it mirrors.
    let asserted_set: HashSet<(&str, &str, String)> = asserted
        .iter()
        .map(|l| {
            (
                identity.canonical(&l.from_entity_path),
                identity.canonical(&l.to_entity_path),
                prefixes.expand(&l.relation),
            )
        })
        .collect();

    let mut links: Vec<(String, String, String)> = Vec::new();
    for (from, to, rel_iri) in edges {
        let (cfrom, cto) = (identity.canonical(&from), identity.canonical(&to));
        // A loop once the twins are collapsed: the edge relates an entity to itself.
        if cfrom == cto {
            continue;
        }
        if asserted_set.contains(&(cfrom, cto, rel_iri.clone())) {
            continue;
        }
        let rel_curie = prefixes.compact(&rel_iri).unwrap_or(rel_iri);
        links.push((from, to, rel_curie));
    }
    links.sort();
    links.dedup();

    Ok(InferredGraph { links, same_as })
}

/// The identity classes an `owl:sameAs` set induces over entity paths, as a map from
/// each path to its class representative (the lowest path in the class).
///
/// Transitive closure matters even though `eq-trans` already materializes it: the
/// pairs arrive from a SPARQL scan with no ordering guarantee, so folding them in
/// one pass would leave `a→b, c→a` pointing at two different representatives.
struct IdentityClasses<'a> {
    rep: HashMap<&'a str, &'a str>,
}

impl<'a> IdentityClasses<'a> {
    fn new(pairs: &'a [(String, String)]) -> Self {
        let mut rep: HashMap<&str, &str> = HashMap::new();
        for (a, b) in pairs {
            let (a, b) = (a.as_str(), b.as_str());
            let ra = *rep.get(a).unwrap_or(&a);
            let rb = *rep.get(b).unwrap_or(&b);
            let (keep, drop) = if ra <= rb { (ra, rb) } else { (rb, ra) };
            rep.insert(a, keep);
            rep.insert(b, keep);
            // Re-point everything that pointed at the losing representative, so the
            // map stays flat and `canonical` is a single lookup.
            if keep != drop {
                for r in rep.values_mut() {
                    if *r == drop {
                        *r = keep;
                    }
                }
            }
        }
        Self { rep }
    }

    /// The representative of `path`'s identity class - itself when it is in none.
    fn canonical<'b: 'a>(&self, path: &'b str) -> &'a str {
        self.rep.get(path).copied().unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use oxrdf::GraphName;

    const SAME_AS: &str = OWL_SAME_AS;
    const WORKS_FOR: &str = "urn:frona:worksFor";
    const KNOWS: &str = "urn:frona:knows";

    fn closure(edges: &[(&str, &str, &str)]) -> Store {
        let store = Store::new().unwrap();
        store
            .extend(edges.iter().map(|(s, p, o)| {
                Triple::new(
                    subj(&individual_iri(s)),
                    nn(p),
                    Term::NamedNode(nn(&individual_iri(o))),
                )
                .in_graph(GraphName::DefaultGraph)
            }))
            .unwrap();
        store
    }

    fn asserted(edges: &[(&str, &str, &str)]) -> Vec<KnowledgeEntityLink> {
        edges
            .iter()
            .map(|(from, rel, to)| KnowledgeEntityLink {
                id: format!("{from}|{rel}|{to}"),
                user_id: "u1".into(),
                from_entity_path: (*from).into(),
                to_entity_path: (*to).into(),
                relation: (*rel).into(),
                source_memory_ids: Vec::new(),
                origin: crate::memory::pkm::model::LinkOrigin::Asserted,
                created_at: Utc::now(),
            })
            .collect()
    }

    fn extract(
        closure_edges: &[(&str, &str, &str)],
        asserted_edges: &[(&str, &str, &str)],
    ) -> InferredGraph {
        extract_inferred(
            &closure(closure_edges),
            &asserted(asserted_edges),
            &PrefixMap::standard(),
        )
        .expect("extract")
    }

    /// The `eq-rep` mirrors are the whole reason the filter exists: once two entities are
    /// identified, each one's asserted edges reappear on the other. Those copies say
    /// nothing new and must not be persisted as inferred links.
    #[test]
    fn mirrored_copy_of_an_asserted_edge_is_not_an_inferred_link() {
        let g = extract(
            &[
                ("people/sarah", SAME_AS, "people/sarah-2"),
                ("people/sarah", WORKS_FOR, "orgs/acme"),
                ("people/sarah-2", WORKS_FOR, "orgs/acme"),
            ],
            &[("people/sarah", WORKS_FOR, "orgs/acme")],
        );
        assert!(g.links.is_empty(), "the mirror adds nothing: {:?}", g.links);
        assert_eq!(
            g.same_as,
            [("people/sarah".to_string(), "people/sarah-2".to_string())]
        );
    }

    /// A genuinely derived edge survives. `knows` declared symmetric puts the reverse
    /// edge in the closure, and neither endpoint is `sameAs` anything, so nothing
    /// about the identity filter should touch it.
    #[test]
    fn derived_reverse_edge_is_kept() {
        let g = extract(
            &[
                ("people/alice", KNOWS, "people/bob"),
                ("people/bob", KNOWS, "people/alice"),
            ],
            &[("people/alice", KNOWS, "people/bob")],
        );
        assert_eq!(
            g.links,
            [(
                "people/bob".to_string(),
                "people/alice".to_string(),
                "frona:knows".to_string()
            )],
            "the symmetric reverse is new knowledge"
        );
    }

    /// `owl:sameAs` is identity, not a navigable edge - it leaves through `same_as`
    /// and must never be written into the entity graph. Direction is normalized so the
    /// pair is the same however the reasoner happened to derive it.
    #[test]
    fn same_as_is_split_out_and_normalized() {
        let g = extract(
            &[
                ("people/sarah-2", SAME_AS, "people/sarah"),
                ("people/sarah", SAME_AS, "people/sarah-2"),
            ],
            &[],
        );
        assert!(
            g.links.is_empty(),
            "sameAs is not an entity edge: {:?}",
            g.links
        );
        assert_eq!(
            g.same_as,
            [("people/sarah".to_string(), "people/sarah-2".to_string())],
            "one pair, lowest path first, however it was derived"
        );
    }

    /// An edge between two entities that turn out to be the same entity is a self-loop, and
    /// self-loops are never useful entity edges.
    #[test]
    fn edge_within_one_identity_class_is_dropped() {
        let g = extract(
            &[
                ("people/sarah", SAME_AS, "people/sarah-2"),
                ("people/sarah", KNOWS, "people/sarah-2"),
            ],
            &[],
        );
        assert!(g.links.is_empty(), "sarah knows herself: {:?}", g.links);
    }

    /// The classes have to close transitively regardless of the order the pairs are
    /// read in - the SPARQL scan gives no ordering guarantee, and folding naively
    /// would leave `a` and `c` pointing at different representatives.
    #[test]
    fn identity_classes_close_transitively_whatever_the_pair_order() {
        let pairs = vec![
            ("b".to_string(), "c".to_string()),
            ("a".to_string(), "b".to_string()),
        ];
        let ids = IdentityClasses::new(&pairs);
        assert_eq!(ids.canonical("a"), "a");
        assert_eq!(ids.canonical("b"), "a");
        assert_eq!(ids.canonical("c"), "a", "c reaches a through b");
        assert_eq!(
            ids.canonical("z"),
            "z",
            "an entity in no class is its own representative"
        );
    }
}
