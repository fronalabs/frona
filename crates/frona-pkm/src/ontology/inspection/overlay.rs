use std::collections::{BTreeSet, HashMap};

use frona_ontologies::rdf::{
    C_ANN_PROP, C_DATA_PROP, C_OBJ_PROP, C_OWL_CLASS, C_RDF_PROPERTY, P_COMMENT, P_DISJOINT,
    P_DOMAIN, P_EQ_CLASS, P_EQ_PROP, P_INVERSE, P_LABEL, P_RANGE, P_SUBCLASS, P_SUBPROP, P_TYPE,
};
use oxrdf::{NamedOrBlankNode, Term, Triple};

#[derive(Default)]
pub(super) struct OverlayTerm {
    pub(super) kind: Option<String>,
    pub(super) label: Option<String>,
    pub(super) definition: Option<String>,
    pub(super) direct_parents: BTreeSet<String>,
    pub(super) direct_children: BTreeSet<String>,
    pub(super) equivalents: BTreeSet<String>,
    pub(super) disjoint_with: BTreeSet<String>,
    pub(super) domain: BTreeSet<String>,
    pub(super) range: BTreeSet<String>,
    pub(super) inverse: BTreeSet<String>,
}

#[derive(Default)]
pub(super) struct SchemaOverlay {
    pub(super) terms: HashMap<String, OverlayTerm>,
}

impl SchemaOverlay {
    pub(super) fn from_triples(triples: &[Triple]) -> Self {
        let mut overlay = Self::default();
        for triple in triples {
            let NamedOrBlankNode::NamedNode(subject) = &triple.subject else {
                continue;
            };
            let subject = subject.as_str().to_string();
            let predicate = triple.predicate.as_str();
            match (&triple.object, predicate) {
                (Term::NamedNode(object), P_TYPE) => {
                    let kind = match object.as_str() {
                        C_OWL_CLASS => Some("class"),
                        C_OBJ_PROP => Some("object_property"),
                        C_DATA_PROP => Some("data_property"),
                        C_RDF_PROPERTY => Some("property"),
                        C_ANN_PROP => Some("annotation_property"),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        overlay.terms.entry(subject).or_default().kind = Some(kind.to_string());
                    }
                }
                (Term::NamedNode(object), P_SUBCLASS | P_SUBPROP) => {
                    overlay.add_parent(&subject, object.as_str());
                }
                (Term::NamedNode(object), P_EQ_CLASS | P_EQ_PROP) => {
                    overlay.add_symmetric(&subject, object.as_str(), RelationKind::Equivalent);
                }
                (Term::NamedNode(object), P_DISJOINT) => {
                    overlay.add_symmetric(&subject, object.as_str(), RelationKind::Disjoint);
                }
                (Term::NamedNode(object), P_INVERSE) => {
                    overlay.add_symmetric(&subject, object.as_str(), RelationKind::Inverse);
                }
                (Term::NamedNode(object), P_DOMAIN) => {
                    overlay
                        .terms
                        .entry(subject)
                        .or_default()
                        .domain
                        .insert(object.as_str().to_string());
                }
                (Term::NamedNode(object), P_RANGE) => {
                    overlay
                        .terms
                        .entry(subject)
                        .or_default()
                        .range
                        .insert(object.as_str().to_string());
                }
                (Term::Literal(value), P_LABEL) => {
                    overlay.terms.entry(subject).or_default().label =
                        Some(value.value().to_string());
                }
                (Term::Literal(value), P_COMMENT) => {
                    overlay.terms.entry(subject).or_default().definition =
                        Some(value.value().to_string());
                }
                _ => {}
            }
        }
        overlay
    }

    fn add_parent(&mut self, child: &str, parent: &str) {
        self.terms
            .entry(child.to_string())
            .or_default()
            .direct_parents
            .insert(parent.to_string());
        self.terms
            .entry(parent.to_string())
            .or_default()
            .direct_children
            .insert(child.to_string());
    }

    fn add_symmetric(&mut self, a: &str, b: &str, kind: RelationKind) {
        for (left, right) in [(a, b), (b, a)] {
            let term = self.terms.entry(left.to_string()).or_default();
            match kind {
                RelationKind::Equivalent => &mut term.equivalents,
                RelationKind::Disjoint => &mut term.disjoint_with,
                RelationKind::Inverse => &mut term.inverse,
            }
            .insert(right.to_string());
        }
    }
}

#[derive(Clone, Copy)]
enum RelationKind {
    Equivalent,
    Disjoint,
    Inverse,
}
