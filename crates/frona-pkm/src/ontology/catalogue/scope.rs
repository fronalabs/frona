use std::collections::{HashMap, HashSet};

use oxigraph::io::{RdfFormat, RdfParser};
use oxrdf::{NamedOrBlankNode, Term, Triple};
use serde::Serialize;

use crate::Error;
use crate::ontology::PrefixMap;
use crate::ontology::catalogue::roots::Root;

/// Provenance for one source in the catalogue.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    /// The file stem (`kbpedia`, `schema-org`).
    pub name: String,
    /// The `owl:Ontology` this file declares itself as - the source's *identity*, as
    /// opposed to where it happens to sit on disk. `None` for a file that declares no
    /// header, which is legal and simply means it cannot collide with anything.
    pub iri: Option<String>,
    pub root: Root,
    /// Terms this source was the first to declare.
    pub terms: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VocabHit {
    pub curie: String,
    pub label: Option<String>,
    /// `"class"` or `"property"`.
    pub kind: &'static str,
}

/// The catalogue facts needed to inspect one named ontology term without lowering
/// the catalogue to triples or materialising a reasoner store. All relationships use
/// full IRIs so a caller can compose them with a user's schema delta before compacting
/// the final result for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueTerm {
    pub iri: String,
    pub label: Option<String>,
    pub definition: Option<String>,
    pub kind: String,
    pub source: Option<String>,
    pub direct_parents: Vec<String>,
    pub direct_children: Vec<String>,
    pub children_truncated: bool,
    pub equivalents: Vec<String>,
    pub disjoint_with: Vec<String>,
    pub domain: Vec<String>,
    pub range: Vec<String>,
    pub inverse: Vec<String>,
}

/// Two types that cannot both hold, and the axiom that says so.
///
/// `via` is the disjointness pair reached from the two type chains - the ancestors,
/// not the types themselves, because that is where the axiom almost always sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clash {
    pub a: String,
    pub b: String,
    pub via: (String, String),
}

/// The cut a reasoning pass runs over: the seeds, closed upward, lowered to triples.
///
/// It gives the reasoner and tools a bounded `triples()` and `prefixes()` view without
/// materializing the complete catalogue.
pub struct OntologyScope {
    pub(super) triples: Vec<Triple>,
    pub(super) prefixes: PrefixMap,
    pub(super) seeds: Vec<String>,
    pub(super) sources: Vec<String>,
    pub(super) terms: usize,
}

impl OntologyScope {
    pub fn triples(&self) -> &[Triple] {
        &self.triples
    }

    /// The canonical output prefix map (CURIE frontmatter, JSON-LD `@context`,
    /// LLM CURIE I/O). Derived from the catalogue, not from this cut, so a term
    /// entering scope later never changes how an already-stored CURIE expands.
    pub fn prefixes(&self) -> &PrefixMap {
        &self.prefixes
    }

    /// Does this cut carry axioms *about* `iri` - is it the subject of any triple?
    ///
    /// Subject position specifically, which is what separates a term the cut explains
    /// from one it merely mentions. Every `<term> rdf:type owl:Class` names `rdf:type`
    /// and `owl:Class`, but the cut says nothing about either of them; treating a
    /// mention as description makes the whole RDF vocabulary look like it belongs to
    /// this knowledge base.
    pub fn describes(&self, iri: &str) -> bool {
        self.triples.iter().any(|t| match &t.subject {
            NamedOrBlankNode::NamedNode(s) => s.as_str() == iri,
            _ => false,
        })
    }

    /// The IRIs this cut was seeded from - every term the vault referenced.
    pub fn seeds(&self) -> &[String] {
        &self.seeds
    }

    /// Which catalogue sources the cut spans. A cut confined to one source means the
    /// artifacts carry no cross-vocabulary links reaching out of it.
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// How many terms are in scope (not how many triples they lower to).
    pub fn terms(&self) -> usize {
        self.terms
    }

    pub fn is_empty(&self) -> bool {
        self.terms == 0
    }

    pub fn to_ntriples(&self) -> String {
        let mut out = String::with_capacity(self.triples.len() * 96);
        for t in &self.triples {
            use std::fmt::Write;
            let _ = writeln!(out, "{} {} {} .", t.subject, t.predicate, t.object);
        }
        out
    }

    /// Rebuild a stored cut. The seeds and source list are stored alongside rather than
    /// re-derived: a term whose source has since left the catalogue is exactly the case
    /// this exists to survive, and re-deriving would drop it.
    pub fn from_ntriples(
        nt: &str,
        seeds: Vec<String>,
        sources: Vec<String>,
        prefixes: PrefixMap,
    ) -> Result<Self, Error> {
        let mut triples = Vec::new();
        let mut subjects: HashSet<String> = HashSet::new();
        for q in RdfParser::from_format(RdfFormat::NTriples).for_reader(nt.as_bytes()) {
            let q = q.map_err(|e| {
                Error::Internal(format!("ontology: parse stored effective ontology: {e}"))
            })?;
            if let NamedOrBlankNode::NamedNode(s) = &q.subject {
                subjects.insert(s.as_str().to_string());
            }
            triples.push(Triple::new(q.subject, q.predicate, q.object));
        }
        Ok(Self {
            triples,
            prefixes,
            seeds,
            sources,
            terms: subjects.len(),
        })
    }

    /// Carry forward the part of a previous cut that the catalogue can no longer
    /// supply, for terms the vault still references.
    ///
    /// This is **not** a general merge, and the distinction is the whole point. A cut
    /// is the *effective ontology* - what this knowledge base reasons over right now -
    /// so a term that nothing references any more has to leave. Unioning every cut ever
    /// taken would make the stored copy grow without bound and stop describing the
    /// knowledge base at all.
    ///
    /// Exactly one case justifies keeping something the fresh cut lacks: an entity is
    /// still typed with a term whose *source* has left the catalogue - an image that
    /// dropped a vocabulary, or a file the user deleted. The seed is still there, but
    /// `project` can supply nothing for it, so the entity would stay typed in the
    /// database while reasoning quietly inferred nothing about it and the gate stopped
    /// firing. Nobody asked for that: the entity did not change, and a packaging change
    /// must not become a data change.
    ///
    /// `stranded` are those seeds. Everything reachable from them in the previous cut
    /// comes across - the ancestors are what the reasoning actually needs, not just the
    /// term itself.
    pub fn carrying_forward(&self, previous: &OntologyScope, stranded: &[String]) -> Self {
        if stranded.is_empty() {
            return self.clone_shallow();
        }
        // Adjacency over the previous cut, **undirected**. A directed walk from the
        // stranded term is not enough: symmetric axioms are stated once, on whichever
        // end sorts first, so `CreativeWork ⊥ Person` has `CreativeWork` as its subject
        // and a forward walk from `Person` never reaches it. Losing exactly the
        // disjointness is the worst possible subset to lose - the gate would stop
        // firing while the taxonomy still looked intact.
        let mut adjacent: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut by_subject: HashMap<&str, Vec<&Triple>> = HashMap::new();
        for t in &previous.triples {
            let NamedOrBlankNode::NamedNode(s) = &t.subject else {
                continue;
            };
            by_subject.entry(s.as_str()).or_default().push(t);
            if let Term::NamedNode(o) = &t.object {
                adjacent.entry(s.as_str()).or_default().push(o.as_str());
                adjacent.entry(o.as_str()).or_default().push(s.as_str());
            }
        }

        // Everything in the stranded terms' connected component - the axioms *about*
        // them, which is what reasoning over them needs.
        let mut walked: HashSet<&str> = HashSet::new();
        let mut queue: Vec<&str> = Vec::new();
        for s in stranded {
            if let Some((term, _)) = adjacent.get_key_value(s.as_str())
                && walked.insert(term)
            {
                queue.push(term);
            }
        }
        while let Some(term) = queue.pop() {
            for &next in adjacent.get(term).map(|v| &v[..]).unwrap_or(&[]) {
                if walked.insert(next) {
                    queue.push(next);
                }
            }
        }

        let key = |t: &Triple| format!("{} {} {}", t.subject, t.predicate, t.object);
        let mut have: HashSet<String> = self.triples.iter().map(key).collect();
        let mut triples = self.triples.clone();
        for term in &walked {
            for t in by_subject.get(term).map(|v| &v[..]).unwrap_or(&[]) {
                if have.insert(key(t)) {
                    triples.push((*t).clone());
                }
            }
        }

        let subjects: HashSet<&str> = triples
            .iter()
            .filter_map(|t| match &t.subject {
                NamedOrBlankNode::NamedNode(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let mut sources = self.sources.clone();
        sources.extend(previous.sources.iter().cloned());
        sources.sort();
        sources.dedup();
        Self {
            terms: subjects.len(),
            triples,
            prefixes: self.prefixes.clone(),
            seeds: self.seeds.clone(),
            sources,
        }
    }

    fn clone_shallow(&self) -> Self {
        Self {
            triples: self.triples.clone(),
            prefixes: self.prefixes.clone(),
            seeds: self.seeds.clone(),
            sources: self.sources.clone(),
            terms: self.terms,
        }
    }
}
