use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use frona_ontologies::graph::{Graph, Id, Kind};
use frona_ontologies::rdf::{
    P_DISJOINT, P_DOMAIN, P_EQ_CLASS, P_EQ_PROP, P_INVERSE, P_LABEL, P_RANGE, P_SUBCLASS,
    P_SUBPROP, P_TYPE,
};
use oxrdf::{Literal, NamedNode, NamedOrBlankNode, Term, Triple};
use sha2::{Digest, Sha256};

use crate::core::error::AppError;
use crate::memory::pkm::ontology::PrefixMap;
use crate::memory::pkm::ontology::catalogue::loading::{
    absorb, ontology_files, scan_file, source_name,
};
use crate::memory::pkm::ontology::catalogue::roots::Root;
use crate::memory::pkm::ontology::catalogue::scope::{
    CatalogueTerm, Clash, OntologyScope, SourceInfo, VocabHit,
};
use crate::memory::pkm::ontology::catalogue::search::{
    decamel, local_name, match_rank, normalize, squash, squashed_match,
};

/// Everything the server can see, interned once and shared.
///
/// Immutable after load apart from the projection cache. Cheap to hold as
/// `Arc<OntologyCatalogue>`; ~37 MB for the shipped release.
pub struct OntologyCatalogue {
    graph: Graph,
    /// Term id → index into `sources`, assigned on **first sight** during absorb.
    /// Deriving it afterwards would mean re-reading every artifact just to answer
    /// "which file was this from".
    source_of: HashMap<Id, usize>,
    sources: Vec<SourceInfo>,
    /// Hoisted once. `ancestor_closure` rebuilds the equivalence index per call,
    /// which is free for a single projection and quadratic over a sweep - rebuilding
    /// it per term took a full-catalogue walk from 137 ms to 1,814 ms upstream.
    eq: HashMap<Id, Vec<Id>>,
    dj: HashMap<Id, Vec<Id>>,
    children: HashMap<Id, Vec<Id>>,
    inverse: HashMap<Id, Vec<Id>>,
    prefixes: PrefixMap,
    fingerprint: String,
    /// Projections keyed on their seed set. A pass re-derives the same seeds from an
    /// unchanged vault every time, so this turns every `load()` after the first into
    /// a map lookup. Keyed on the seeds themselves rather than a hash: a collision
    /// here would silently serve a different user's cut.
    cache: RwLock<HashMap<Vec<String>, Arc<OntologyScope>>>,
}

impl OntologyCatalogue {
    /// Absorb every ontology under each root into one interned graph.
    ///
    /// Roots are scanned in order and the result is a **union**, not an override. There
    /// is no precedence between them: a term both roots mention keeps both sets of
    /// edges, because an axiom is a claim about the term, not a definition of it that a
    /// later file could replace. What is refused instead is two files claiming to *be*
    /// the same ontology - see below.
    ///
    /// `Release` is passed before `User` so a vocabulary the release declares stays
    /// attributed to the release: "gone because a newer release replaced it" has to
    /// remain distinguishable from "the user deleted it".
    ///
    /// A missing root directory is not an error - the user root legitimately does not
    /// exist until someone drops a file in it. A root that exists but yields nothing
    /// parseable across *all* roots is, since the PKM backend needs a catalogue.
    pub fn load(roots: &[(Root, &Path)]) -> Result<Arc<Self>, AppError> {
        let mut graph = Graph::default();
        let mut source_of: HashMap<Id, usize> = HashMap::new();
        let mut sources: Vec<SourceInfo> = Vec::new();
        let mut declared_by: HashMap<String, String> = HashMap::new();
        let mut hasher = Sha256::new();

        for &(root, dir) in roots {
            for path in ontology_files(dir)? {
                let bytes = std::fs::read(&path).map_err(|e| {
                    AppError::Internal(format!("ontology: read {}: {e}", path.display()))
                })?;
                let name = source_name(&path);
                hasher.update(name.as_bytes());
                hasher.update(Sha256::digest(&bytes));

                let scan = scan_file(&bytes, &path)?;

                // Identity is the `owl:Ontology` header, not the filename. Two files
                // claiming to be the same ontology is a packaging mistake - a stale copy
                // left in the user root, an artifact unpacked twice - and merging them
                // would double every axiom while looking like it worked.
                if let Some(iri) = &scan.iri {
                    if let Some(first) = declared_by.get(iri) {
                        return Err(AppError::Internal(format!(
                            "ontology: {first} and {name} both identify as <{iri}>"
                        )));
                    }
                    declared_by.insert(iri.clone(), name.clone());
                }

                if !scan.anonymous.is_empty() {
                    return Err(AppError::Internal(format!(
                        "ontology: {} states axioms against anonymous class expressions on \
                         {} term(s) (e.g. {}). Subsumption here is answered by graph \
                         reachability, which is exact only while class expressions are \
                         absent — loading this would return thin answers with nothing \
                         reporting a problem. State the axioms over named classes instead.",
                        path.display(),
                        scan.anonymous.len(),
                        scan.anonymous
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", "),
                    )));
                }

                absorb(&mut graph, &bytes, &path)?;

                let idx = sources.len();
                let mut terms = 0usize;
                for id in graph.declared() {
                    if let std::collections::hash_map::Entry::Vacant(slot) = source_of.entry(id) {
                        slot.insert(idx);
                        terms += 1;
                    }
                }
                sources.push(SourceInfo {
                    name,
                    iri: scan.iri,
                    root,
                    terms,
                });
            }
        }

        if sources.is_empty() {
            return Err(AppError::Internal(format!(
                "ontology: no sources found under {}",
                roots
                    .iter()
                    .map(|(_, p)| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        // `absorb` parks disjointness in scaffolding until this runs. Skipping it
        // leaves `disjoint` empty, every clash check silently passes, and the gate is
        // ineffective.
        graph.decompose_disjointness();

        let eq = graph.equivalence_index();
        let dj = graph.disjointness_index();
        let children = graph.children_index();
        let mut inverse: HashMap<Id, Vec<Id>> = HashMap::new();
        for &(a, b) in &graph.inverse {
            inverse.entry(a).or_default().push(b);
            inverse.entry(b).or_default().push(a);
        }

        Ok(Arc::new(Self {
            graph,
            source_of,
            sources,
            eq,
            dj,
            children,
            inverse,
            prefixes: PrefixMap::standard(),
            fingerprint: hex::encode(hasher.finalize()),
            cache: RwLock::new(HashMap::new()),
        }))
    }

    /// Identifies the catalogue's *contents*: the name and bytes of every file
    /// absorbed. A stored projection cut against a different fingerprint has to be
    /// re-cut.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn sources(&self) -> &[SourceInfo] {
        &self.sources
    }

    pub fn terms(&self) -> usize {
        self.graph.declared().count()
    }

    /// Disjointness pairs after decomposition. The load gate requires a non-empty set
    /// because an empty set makes contradiction checks pass vacuously.
    pub fn disjoint_pairs(&self) -> usize {
        self.graph.disjoint.len()
    }

    /// The display prefix map. Not per-scope: a CURIE written into a stored entity has
    /// to expand the same way forever, so this cannot depend on what is in scope.
    pub fn prefixes(&self) -> &PrefixMap {
        &self.prefixes
    }

    pub fn declares(&self, iri: &str) -> bool {
        self.graph
            .id_of(iri)
            .is_some_and(|id| self.graph.kind[id as usize].is_some())
    }

    /// Return the indexed facts about one named term. This is the catalogue inspection
    /// path: bounded graph reads only, with no projection, triples, or reasoner store.
    pub(crate) fn term(&self, iri: &str, child_limit: usize) -> Option<CatalogueTerm> {
        let id = self.graph.id_of(iri)?;
        let kind = self.graph.kind[id as usize]?;
        let iris = |ids: &[Id]| {
            let mut values: Vec<String> = ids
                .iter()
                .map(|&item| self.graph.iri(item).to_string())
                .collect();
            values.sort();
            values.dedup();
            values
        };
        let kind = match kind {
            Kind::Class => "class",
            Kind::ObjectProperty => "object_property",
            Kind::DataProperty => "data_property",
            Kind::Property => "property",
            Kind::AnnotationProperty => "annotation_property",
        };
        let all_children = self.children.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let children_truncated = all_children.len() > child_limit;
        let mut child_ids = all_children.to_vec();
        child_ids.sort_by_key(|&child| self.graph.iri(child));
        child_ids.truncate(child_limit);
        let direct_children = iris(&child_ids);
        Some(CatalogueTerm {
            iri: self.graph.iri(id).to_string(),
            label: self.graph.label[id as usize]
                .as_ref()
                .map(|value| value.to_string()),
            definition: self.graph.definition[id as usize]
                .as_ref()
                .map(|value| value.to_string()),
            kind: kind.to_string(),
            source: self
                .source_of
                .get(&id)
                .and_then(|&source| self.sources.get(source))
                .map(|source| source.name.clone()),
            direct_parents: iris(&self.graph.sup[id as usize]),
            direct_children,
            children_truncated,
            equivalents: iris(self.eq.get(&id).map(Vec::as_slice).unwrap_or(&[])),
            disjoint_with: iris(self.dj.get(&id).map(Vec::as_slice).unwrap_or(&[])),
            domain: iris(&self.graph.domain[id as usize]),
            range: iris(&self.graph.range[id as usize]),
            inverse: iris(self.inverse.get(&id).map(Vec::as_slice).unwrap_or(&[])),
        })
    }

    pub(crate) fn direct_parents(&self, iri: &str) -> Vec<String> {
        let Some(id) = self.graph.id_of(iri) else {
            return Vec::new();
        };
        let mut values: Vec<String> = self.graph.sup[id as usize]
            .iter()
            .map(|&parent| self.graph.iri(parent).to_string())
            .collect();
        values.sort();
        values.dedup();
        values
    }

    pub(crate) fn equivalents(&self, iri: &str) -> Vec<String> {
        let Some(id) = self.graph.id_of(iri) else {
            return Vec::new();
        };
        let mut values: Vec<String> = self
            .eq
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|&equivalent| self.graph.iri(equivalent).to_string())
            .collect();
        values.sort();
        values.dedup();
        values
    }

    pub(crate) fn disjoint_with(&self, iri: &str) -> Vec<String> {
        let Some(id) = self.graph.id_of(iri) else {
            return Vec::new();
        };
        let mut values: Vec<String> = self
            .dj
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|&other| self.graph.iri(other).to_string())
            .collect();
        values.sort();
        values.dedup();
        values
    }

    /// Every class (or property) above `iri`, following `subClassOf` **and**
    /// equivalence. `iri` itself is not included.
    ///
    /// Equivalence is load-bearing, not a nicety: `scm-eqc1` makes
    /// `owl:equivalentClass` subsumption both ways, and walking only `subClassOf`
    /// missed 26,544 subsumptions against a materialised closure of the catalogue.
    pub fn ancestors(&self, iri: &str) -> Vec<String> {
        let Some(id) = self.graph.id_of(iri) else {
            return Vec::new();
        };
        let mut out: Vec<String> = self
            .graph
            .ancestor_closure_with(&HashSet::from([id]), &self.eq)
            .into_iter()
            .filter(|&a| a != id)
            .map(|a| self.graph.iri(a).to_string())
            .collect();
        out.sort();
        out
    }

    /// The first disjointness axiom that `types` contradict, if any - the gate.
    ///
    /// Compares *ancestor chains*, not the types themselves: `cax-dw` fires on two
    /// type chains, and the axiom that separates two concrete classes almost always
    /// sits well above both of them.
    pub fn clash(&self, types: &[String]) -> Option<Clash> {
        let ids: Vec<(usize, Id)> = types
            .iter()
            .enumerate()
            .filter_map(|(i, t)| self.graph.id_of(t).map(|id| (i, id)))
            .collect();
        // Sorted, because several axioms can explain one clash and a `HashSet` scan
        // names a different one on each run.
        let sorted = |id: Id| {
            let mut v: Vec<Id> = self
                .graph
                .ancestor_closure_with(&HashSet::from([id]), &self.eq)
                .into_iter()
                .collect();
            v.sort_by_key(|&i| self.graph.iri(i));
            v
        };
        for (n, &(i, x)) in ids.iter().enumerate() {
            let ax = sorted(x);
            for &(j, y) in &ids[n + 1..] {
                let ay: HashSet<Id> = sorted(y).into_iter().collect();
                for &p in &ax {
                    for &q in self.dj.get(&p).map(|v| &v[..]).unwrap_or(&[]) {
                        if ay.contains(&q) {
                            return Some(Clash {
                                a: types[i].clone(),
                                b: types[j].clone(),
                                via: (self.graph.iri(p).to_string(), self.graph.iri(q).to_string()),
                            });
                        }
                    }
                }
            }
        }
        None
    }

    /// Would asserting `x ⊑ y` make some term unsatisfiable?
    ///
    /// The vetting gate a dropped-in ontology gets held to, same as the alignment
    /// tables upstream: a mapping is a hypothesis, the disjointness we ship is what
    /// the gate runs on, so the hypothesis loses. Comparing only the two endpoints'
    /// ancestors is **not** sufficient - the edge hands every *descendant* of `x` the
    /// ancestors of `y`, so the clash surfaces a level below where a naive check looks.
    pub fn edge_is_safe(&self, x: &str, y: &str) -> bool {
        let (Some(a), Some(b)) = (self.graph.id_of(x), self.graph.id_of(y)) else {
            return true;
        };
        self.graph
            .edge_is_safe(a, b, &self.eq, &self.dj, &self.children)
    }

    /// Cut the effective ontology `seeds` reason under: ancestors, equivalents, and the partners of
    /// any axiom they touch, with those partners' own ancestors.
    ///
    /// Memoised on the seed set. A pass over an unchanged vault re-derives identical
    /// seeds, so this is a map lookup after the first call.
    pub fn project(&self, seeds: &[String]) -> Arc<OntologyScope> {
        let mut key: Vec<String> = seeds.to_vec();
        key.sort();
        key.dedup();

        if let Some(hit) = self.cache().get(&key).cloned() {
            return hit;
        }

        let ids: HashSet<Id> = key.iter().filter_map(|s| self.graph.id_of(s)).collect();
        // `closure` is the *one* closure - build-time filtering upstream and run-time
        // projection here both call it, so they cannot drift.
        let terms = self.graph.closure(&ids);

        let mut spanned: BTreeSet<&str> = BTreeSet::new();
        for id in &terms {
            if let Some(&idx) = self.source_of.get(id) {
                spanned.insert(&self.sources[idx].name);
            }
        }

        let built = Arc::new(OntologyScope {
            triples: self.cut_triples(&terms),
            prefixes: self.prefixes.clone(),
            seeds: key.clone(),
            sources: spanned.into_iter().map(str::to_string).collect(),
            terms: terms.len(),
        });

        let mut w = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        w.entry(key).or_insert(built).clone()
    }

    /// Lower the cut to triples, straight out of the interned index.
    ///
    /// Not by re-reading the artifacts: gathering a 1,430-triple cut that way costs
    /// 174 ms of re-parsing against 1 ms to materialise it. The index already *is* the
    /// subject-keyed structure that re-read would be looking for.
    ///
    /// Emits the axioms plus `rdfs:label`, and deliberately **not** `skos:prefLabel`
    /// or `skos:definition`. Those are what makes ~21% of a materialisation noise:
    /// SKOS declares `skos:prefLabel ⊑ rdfs:label` so every label is stored twice, and
    /// KKO's `skos:definition ⊑ kko:descriptions ⊑ kko:denotatives ⊑
    /// kko:representations` chain turns each definition into six triples nobody
    /// queries. Since this emits rather than copies, the noise simply never exists.
    fn cut_triples(&self, terms: &HashSet<Id>) -> Vec<Triple> {
        let mut out = Vec::with_capacity(terms.len() * 3);
        let iri = |id: Id| NamedNode::new_unchecked(self.graph.iri(id).to_string());
        let edge = |s: Id, p: &str, o: Id, out: &mut Vec<Triple>| {
            out.push(Triple::new(
                NamedOrBlankNode::NamedNode(iri(s)),
                NamedNode::new_unchecked(p.to_string()),
                Term::NamedNode(iri(o)),
            ));
        };

        let mut ids: Vec<Id> = terms.iter().copied().collect();
        ids.sort_by_key(|&id| self.graph.iri(id));

        for id in ids {
            let i = id as usize;
            let Some(kind) = self.graph.kind[i] else {
                // Reached as the far end of an alignment, defined by no source we
                // carry. Nothing to emit; it still holds the edge that reached it.
                continue;
            };
            out.push(Triple::new(
                NamedOrBlankNode::NamedNode(iri(id)),
                NamedNode::new_unchecked(P_TYPE.to_string()),
                Term::NamedNode(NamedNode::new_unchecked(kind.iri().to_string())),
            ));

            let sub = if kind.is_property() {
                P_SUBPROP
            } else {
                P_SUBCLASS
            };
            for &p in &self.graph.sup[i] {
                if terms.contains(&p) {
                    edge(id, sub, p, &mut out);
                }
            }
            for &d in &self.graph.domain[i] {
                if terms.contains(&d) {
                    edge(id, P_DOMAIN, d, &mut out);
                }
            }
            for &r in &self.graph.range[i] {
                if terms.contains(&r) {
                    edge(id, P_RANGE, r, &mut out);
                }
            }
            if let Some(l) = &self.graph.label[i] {
                out.push(Triple::new(
                    NamedOrBlankNode::NamedNode(iri(id)),
                    NamedNode::new_unchecked(P_LABEL.to_string()),
                    Term::Literal(Literal::new_simple_literal(l.to_string())),
                ));
            }
        }

        // Symmetric axioms are stated once, on whichever end sorts first - the graph
        // holds each pair ordered already.
        for &(a, b) in &self.graph.disjoint {
            if terms.contains(&a) && terms.contains(&b) {
                edge(a, P_DISJOINT, b, &mut out);
            }
        }
        for &(a, b) in &self.graph.equivalent {
            if terms.contains(&a) && terms.contains(&b) {
                let p = if self.graph.kind[a as usize].is_some_and(Kind::is_property) {
                    P_EQ_PROP
                } else {
                    P_EQ_CLASS
                };
                edge(a, p, b, &mut out);
            }
        }
        for &(a, b) in &self.graph.inverse {
            if terms.contains(&a) && terms.contains(&b) {
                edge(a, P_INVERSE, b, &mut out);
            }
        }
        out
    }

    /// Search the **whole catalogue** for classes and properties whose name, label or
    /// synonym matches `term`, best match first.
    ///
    /// Catalogue-wide on purpose: finding a term brings it into the effective scope, so
    /// a returned reference can be resolved.
    pub fn search(&self, term: &str, limit: usize) -> Vec<VocabHit> {
        self.search_ranked(term, limit)
            .into_iter()
            .map(|(_, _, hit)| hit)
            .collect()
    }

    /// Search with the lexical rank and brevity retained for user-aware re-ranking.
    pub(crate) fn search_ranked(&self, term: &str, limit: usize) -> Vec<(u8, usize, VocabHit)> {
        let needle = normalize(term.trim());
        if needle.is_empty() {
            return Vec::new();
        }
        let squashed = squash(&needle);
        // Rank everything, then cap. Capping an alphabetically-ordered list drops the
        // exact hit whenever the vocabulary is large enough for the cap to bind -
        // "database" substring-matches thousands of KBpedia classes.
        let mut scored: Vec<(u8, usize, VocabHit)> = Vec::new();
        for id in self.graph.declared() {
            let i = id as usize;
            let Some(kind) = self.graph.kind[i] else {
                continue;
            };
            let iri = self.graph.iri(id);
            let label = self.graph.label[i].as_ref().map(|l| l.to_string());
            let from_name = normalize(&decamel(local_name(iri)));
            let from_label = label.as_deref().map(normalize).unwrap_or_default();
            // A synonym match is real ("DBMS" → DatabaseManagementSystem) but ranks one
            // step below the same match on the term's own name, so an exact name hit is
            // never displaced by someone else's alias.
            let syn = self.graph.synonyms[i]
                .iter()
                .filter_map(|a| match_rank(&needle, &squashed, &normalize(a)))
                .min()
                .map(|r| r.saturating_add(1));
            let Some(rank) = [&from_name, &from_label]
                .into_iter()
                .filter(|c| !c.is_empty())
                .filter_map(|c| match_rank(&needle, &squashed, c))
                .chain(syn)
                .min()
            else {
                continue;
            };
            // Shorter names are the more general term (`database` before `database
            // management system`), which is what a reuse lookup wants.
            let brevity = from_name.len().min(if from_label.is_empty() {
                usize::MAX
            } else {
                from_label.len()
            });
            let curie = self
                .prefixes
                .compact(iri)
                .unwrap_or_else(|| iri.to_string());
            let kind = if kind.is_property() {
                "property"
            } else {
                "class"
            };
            scored.push((rank, brevity, VocabHit { curie, label, kind }));
        }
        scored.sort_by(|a, b| (a.0, a.1, &a.2.curie).cmp(&(b.0, b.1, &b.2.curie)));
        scored.truncate(limit);
        scored
    }

    /// Apply the catalogue's lexical ranking rules to a user-schema term.
    pub(crate) fn match_term(
        &self,
        query: &str,
        iri: &str,
        label: Option<&str>,
    ) -> Option<(u8, usize)> {
        let needle = normalize(query.trim());
        if needle.is_empty() {
            return None;
        }
        let squashed = squash(&needle);
        let name = normalize(&decamel(local_name(iri)));
        let label = label.map(normalize).unwrap_or_default();
        let rank = [&name, &label]
            .into_iter()
            .filter(|candidate| !candidate.is_empty())
            .filter_map(|candidate| match_rank(&needle, &squashed, candidate))
            .min()?;
        let brevity = name.len().min(if label.is_empty() {
            usize::MAX
        } else {
            label.len()
        });
        Some((rank, brevity))
    }

    /// Exact foreground matching, including declared synonyms but excluding partial matches.
    pub(crate) fn exactly_matches_term(&self, query: &str, iri: &str, label: Option<&str>) -> bool {
        let needle = normalize(query.trim());
        if needle.is_empty() {
            return false;
        }
        let mut needles = vec![needle.clone()];
        if needle == "people" {
            needles.push("person".to_string());
        } else if let Some(stem) = needle.strip_suffix("ies") {
            needles.push(format!("{stem}y"));
        } else if let Some(stem) = needle.strip_suffix('s')
            && !stem.ends_with('s')
        {
            needles.push(stem.to_string());
        }
        let exact = |candidate: &str| {
            let candidate = normalize(candidate);
            needles.iter().any(|needle| {
                candidate == *needle || squashed_match(&squash(needle), &candidate, false)
            })
        };
        if exact(&decamel(local_name(iri))) || label.is_some_and(&exact) {
            return true;
        }
        self.graph.id_of(iri).is_some_and(|id| {
            self.graph.synonyms[id as usize]
                .iter()
                .any(|alias| exact(alias))
        })
    }

    fn cache(&self) -> std::sync::RwLockReadGuard<'_, HashMap<Vec<String>, Arc<OntologyScope>>> {
        self.cache.read().unwrap_or_else(PoisonError::into_inner)
    }
}
