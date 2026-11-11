//! Data-quality validation over a reasoned graph:
//!   1. **logical clashes** - read from the reasoner's diagnostics (disjointness
//!      via `cax-dw`, unsatisfiability via `cls-nothing2`, …).
//!   2. **datatype facet bounds** - code-checked with `oxsdatatypes`, because OWL
//!      RL does not evaluate `owl:withRestrictions`.
//!
//! The result is a flat `Vec<Violation>` the Classify's arbitration ladder acts
//! on (instance-repair → schema-amend → `Suspect` quarantine → defer).

use std::collections::HashMap;
use std::str::FromStr;

use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use oxrdf::{NamedNode, NamedOrBlankNode, Term, Triple};
use oxsdatatypes::Integer;
use serde::Serialize;

use crate::Error;
use crate::model::{KnowledgeEntity, KnowledgeEntityLink};

use super::abox;
use super::prefixes::{KB_NAMESPACE, PrefixMap, individual_iri, path_from_individual};
use super::reasoning::{self, Reasoned};
use super::schema::{self, SchemaEdit};
use super::sparql;
use super::{ComposedOntology, Ontology, UserOntology};

/// A datatype facet bound, read back out of the reasoned closure's
/// `owl:withRestrictions` declarations and code-checked via `oxsdatatypes` - OWL RL
/// does not evaluate facets. Both `property` and `datatype` are full IRIs.
///
/// No release ontology declares one today, but a user ontology or `frona:` delta can
/// declare facets at any time.
#[derive(Debug, Clone)]
pub struct Facet {
    /// The datatype property this constrains (e.g. `urn:frona:port`).
    pub property: String,
    /// The XSD datatype the value must lexically be (`…XMLSchema#integer`).
    pub datatype: String,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationSource {
    Reasoner,
    Facet,
}

/// One data-quality violation. `subject` is the entity path of the offending
/// individual when it can be identified (always for facet checks; best-effort for
/// reasoner clashes, parsed from the diagnostic message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    pub source: ViolationSource,
    pub rule: String,
    pub detail: String,
    pub subject: Option<String>,
}

pub fn validate(reasoned: &Reasoned, prefixes: &PrefixMap) -> Vec<Violation> {
    let mut out = Vec::new();

    for d in reasoned.clashes() {
        out.push(Violation {
            source: ViolationSource::Reasoner,
            rule: d.rule.clone(),
            detail: d.message.clone(),
            subject: subject_from_message(&d.message),
        });
    }

    // OWL RL does not evaluate `owl:withRestrictions`, so check facets in the closure.
    // A value on a sub-property or sub-class individual is then checked against its
    // inherited constraint.
    for facet in extract_facets(&reasoned.store, prefixes) {
        let q = format!(
            "SELECT ?s ?v WHERE {{ ?s <{}> ?v . FILTER(isLiteral(?v)) }}",
            facet.property
        );
        let Ok(QueryResults::Solutions(sols)) = sparql::query(&reasoned.store, &q, prefixes) else {
            continue;
        };
        let prop = prefixes
            .compact(&facet.property)
            .unwrap_or_else(|| facet.property.clone());
        for sol in sols.flatten() {
            let (Some(Term::NamedNode(s)), Some(Term::Literal(v))) = (sol.get("s"), sol.get("v"))
            else {
                continue;
            };
            let Some(detail) = check_facet(&facet, v.value()) else {
                continue;
            };
            let path = path_from_individual(s.as_str());
            out.push(Violation {
                source: ViolationSource::Facet,
                rule: facet_rule(&facet),
                detail: format!(
                    "{prop} on {}: {detail}",
                    path.as_deref().unwrap_or(s.as_str())
                ),
                subject: path,
            });
        }
    }
    out
}

const XSD_MIN_INCLUSIVE: &str = "http://www.w3.org/2001/XMLSchema#minInclusive";
const XSD_MAX_INCLUSIVE: &str = "http://www.w3.org/2001/XMLSchema#maxInclusive";
const XSD_PATTERN: &str = "http://www.w3.org/2001/XMLSchema#pattern";

/// Read the datatype facet bounds out of the reasoned graph: any property whose
/// range is an `owl:withRestrictions` datatype restriction. Reading from the
/// closure means a facet declared in the base *or* a per-user delta is honoured.
pub(super) fn extract_facets(store: &Store, prefixes: &PrefixMap) -> Vec<Facet> {
    const Q: &str = "SELECT ?prop ?dt ?facet ?val WHERE { \
        ?prop rdfs:range ?dr . \
        ?dr owl:onDatatype ?dt ; owl:withRestrictions ?list . \
        ?list rdf:rest*/rdf:first ?fr . \
        ?fr ?facet ?val . }";
    let Ok(QueryResults::Solutions(sols)) = sparql::query(store, Q, prefixes) else {
        return Vec::new();
    };
    let mut by_prop: HashMap<String, Facet> = HashMap::new();
    for sol in sols.flatten() {
        let (
            Some(Term::NamedNode(prop)),
            Some(Term::NamedNode(dt)),
            Some(Term::NamedNode(facet)),
            Some(val),
        ) = (
            sol.get("prop"),
            sol.get("dt"),
            sol.get("facet"),
            sol.get("val"),
        )
        else {
            continue;
        };
        let entry = by_prop
            .entry(prop.as_str().to_string())
            .or_insert_with(|| Facet {
                property: prop.as_str().to_string(),
                datatype: dt.as_str().to_string(),
                min: None,
                max: None,
                pattern: None,
            });
        let lex = sparql::term_lexical(val);
        match facet.as_str() {
            XSD_MIN_INCLUSIVE => entry.min = lex.parse().ok(),
            XSD_MAX_INCLUSIVE => entry.max = lex.parse().ok(),
            XSD_PATTERN => entry.pattern = Some(lex),
            _ => {}
        }
    }
    by_prop.into_values().collect()
}

fn facet_rule(f: &Facet) -> String {
    if f.pattern.is_some() {
        "xsd-pattern".into()
    } else {
        "xsd-range".into()
    }
}

/// Check one literal's lexical value against a facet. Returns a violation detail,
/// or `None` if the value satisfies the facet. Integer bounds + optional pattern
/// are supported (the facet kinds we ship).
fn check_facet(f: &Facet, value: &str) -> Option<String> {
    if f.datatype == "xsd:integer" || f.datatype.ends_with("#integer") {
        let Ok(v) = Integer::from_str(value) else {
            return Some(format!("\"{value}\" is not a valid xsd:integer"));
        };
        if let Some(min) = f.min
            && v < Integer::from(min)
        {
            return Some(format!("{value} < min {min}"));
        }
        if let Some(max) = f.max
            && v > Integer::from(max)
        {
            return Some(format!("{value} > max {max}"));
        }
    }
    if let Some(pattern) = &f.pattern {
        match regex::Regex::new(pattern) {
            Ok(re) if !re.is_match(value) => {
                return Some(format!("\"{value}\" does not match /{pattern}/"));
            }
            Ok(_) => {}
            Err(_) => return Some(format!("facet pattern /{pattern}/ is invalid")),
        }
    }
    None
}

/// Best-effort extraction of the offending entity path from a reasoner diagnostic
/// message (which typically embeds the clashing individual's IRI).
fn subject_from_message(msg: &str) -> Option<String> {
    let idx = msg.find(KB_NAMESPACE)?;
    let tail = &msg[idx..];
    let end = tail
        .find(|c: char| c.is_whitespace() || matches!(c, '>' | ',' | ')' | '"' | '\''))
        .unwrap_or(tail.len());
    path_from_individual(&tail[..end])
}

#[derive(Debug, Default)]
pub struct EditImpact {
    pub incoherence: Vec<String>,
    pub data_violations: Vec<Violation>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphValidation {
    pub diagnostics: Vec<ValidationDiagnostic>,
}

impl GraphValidation {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn grouped(&self) -> Vec<ValidationDiagnosticGroup> {
        let mut groups = std::collections::BTreeMap::<String, ValidationDiagnosticGroup>::new();
        for diagnostic in &self.diagnostics {
            let key = format!("{}|{:?}", diagnostic.rule, diagnostic.causal_axioms);
            let group = groups
                .entry(key)
                .or_insert_with(|| ValidationDiagnosticGroup {
                    rule: diagnostic.rule.clone(),
                    causal_axioms: diagnostic.causal_axioms.clone(),
                    affected_count: 0,
                    examples: Vec::new(),
                });
            group.affected_count += diagnostic.affected_count;
            if group.examples.len() < 5 {
                group.examples.push(diagnostic.clone());
            }
        }
        groups.into_values().collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationDiagnosticGroup {
    pub rule: String,
    pub causal_axioms: Vec<String>,
    pub affected_count: usize,
    pub examples: Vec<ValidationDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDiagnosticKind {
    TboxIncoherence,
    AboxViolation,
    UndeclaredTerm,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationDiagnostic {
    pub id: String,
    pub kind: ValidationDiagnosticKind,
    pub rule: String,
    pub detail: String,
    pub subject: Option<String>,
    pub introduced_by_candidate: bool,
    pub causal_axioms: Vec<String>,
    pub witness_triples: Vec<String>,
    pub affected_count: usize,
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

fn edit_classes(edits: &[SchemaEdit]) -> Vec<String> {
    let mut out = Vec::new();
    for edit in edits {
        match edit {
            SchemaEdit::DeclareClass { class } => out.push(class.clone()),
            SchemaEdit::SubClassOf { sub, sup } => out.extend([sub.clone(), sup.clone()]),
            SchemaEdit::EquivalentClasses { a, b } | SchemaEdit::DisjointClasses { a, b } => {
                out.extend([a.clone(), b.clone()]);
            }
            SchemaEdit::ObjectPropertyDomain { class, .. }
            | SchemaEdit::ObjectPropertyRange { class, .. } => out.push(class.clone()),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn type_triple(individual: &str, class_iri: &str) -> Triple {
    Triple::new(
        NamedOrBlankNode::NamedNode(NamedNode::new_unchecked(individual.to_string())),
        NamedNode::new_unchecked(RDF_TYPE.to_string()),
        Term::NamedNode(NamedNode::new_unchecked(class_iri.to_string())),
    )
}

fn edit_impact(
    current: &UserOntology,
    edits: &[SchemaEdit],
    abox: &[Triple],
) -> Result<EditImpact, Error> {
    let prefixes = current.prefixes();
    let scratch_ofn = schema::apply_edits(current.delta_ofn(), edits, prefixes)?;
    let scratch_triples = schema::delta_triples(&scratch_ofn)?;
    let effective = current.effective_ontology_admitting_all(&[&scratch_triples, abox]);

    let probes: Vec<Triple> = edit_classes(edits)
        .iter()
        .enumerate()
        .map(|(index, class)| {
            type_triple(
                &individual_iri(&format!("__probe{index}")),
                &prefixes.expand(class),
            )
        })
        .collect();
    let coherent = reasoning::materialize(effective.triples(), &scratch_triples, &probes)?;
    let incoherence = coherent
        .clashes()
        .map(|diagnostic| format!("{}: {}", diagnostic.rule, diagnostic.message))
        .collect();

    let key = |violation: &Violation| {
        (
            violation.rule.clone(),
            violation.subject.clone(),
            violation.detail.clone(),
        )
    };
    let baseline_reasoned =
        reasoning::materialize(effective.triples(), current.delta_triples(), abox)?;
    let baseline: std::collections::HashSet<_> = validate(&baseline_reasoned, prefixes)
        .iter()
        .map(key)
        .collect();
    let scratch_reasoned = reasoning::materialize(effective.triples(), &scratch_triples, abox)?;
    let data_violations = validate(&scratch_reasoned, prefixes)
        .into_iter()
        .filter(|violation| violation.subject.is_some() && !baseline.contains(&key(violation)))
        .collect();

    Ok(EditImpact {
        incoherence,
        data_violations,
    })
}

impl Ontology {
    pub async fn validate_graph(
        &self,
        user_id: &str,
        entities: &[KnowledgeEntity],
        links: &[KnowledgeEntityLink],
        pending_edits: &[SchemaEdit],
        candidate_edits: &[SchemaEdit],
    ) -> Result<GraphValidation, Error> {
        let current = self.load(user_id).await?;
        let prefixes = current.prefixes();
        let abox = abox::build_abox_triples(entities, links, prefixes);

        let baseline = ComposedOntology::with_proposed(&current, prefixes, pending_edits, &abox)?;
        let baseline_reasoned = baseline.reason(&abox)?;
        let baseline_keys: std::collections::HashSet<_> =
            validate(&baseline_reasoned, baseline.effective_ontology().prefixes())
                .into_iter()
                .map(|violation| (violation.rule, violation.detail, violation.subject))
                .collect();

        let mut combined = pending_edits.to_vec();
        for edit in candidate_edits {
            if !combined.contains(edit) {
                combined.push(edit.clone());
            }
        }
        let projected = ComposedOntology::with_proposed(&current, prefixes, &combined, &abox)?;
        let reasoned = projected.reason(&abox)?;
        let mut diagnostics: Vec<_> =
            validate(&reasoned, projected.effective_ontology().prefixes())
                .into_iter()
                .enumerate()
                .map(|(index, violation)| {
                    let key = (
                        violation.rule.clone(),
                        violation.detail.clone(),
                        violation.subject.clone(),
                    );
                    let witness_triples = violation
                        .subject
                        .as_deref()
                        .map(|path| {
                            abox.iter()
                                .filter(|triple| match &triple.subject {
                                    NamedOrBlankNode::NamedNode(node) => {
                                        path_from_individual(node.as_str()).as_deref() == Some(path)
                                    }
                                    NamedOrBlankNode::BlankNode(_) => false,
                                })
                                .take(5)
                                .map(ToString::to_string)
                                .collect()
                        })
                        .unwrap_or_else(|| abox.iter().take(5).map(ToString::to_string).collect());
                    ValidationDiagnostic {
                        id: format!("projection-{index}"),
                        kind: if violation.subject.is_some() {
                            ValidationDiagnosticKind::AboxViolation
                        } else {
                            ValidationDiagnosticKind::TboxIncoherence
                        },
                        rule: violation.rule,
                        detail: violation.detail,
                        subject: violation.subject,
                        introduced_by_candidate: !baseline_keys.contains(&key),
                        causal_axioms: candidate_edits
                            .iter()
                            .filter_map(|edit| serde_json::to_string(edit).ok())
                            .collect(),
                        witness_triples,
                        affected_count: 1,
                    }
                })
                .collect();

        let catalog = current.catalog()?;
        let mut declared: std::collections::HashSet<String> = catalog
            .classes
            .into_iter()
            .chain(catalog.object_properties)
            .chain(catalog.data_properties)
            .map(|term| prefixes.expand(&term))
            .collect();
        for edit in &combined {
            let term = match edit {
                SchemaEdit::DeclareClass { class } => Some(class),
                SchemaEdit::SubClassOf { sub, .. } => Some(sub),
                SchemaEdit::DeclareObjectProperty { property }
                | SchemaEdit::DeclareDataProperty { property }
                | SchemaEdit::PropertyCharacteristic { property, .. }
                | SchemaEdit::ObjectPropertyDomain { property, .. }
                | SchemaEdit::ObjectPropertyRange { property, .. }
                | SchemaEdit::RestrictDatatype { property, .. } => Some(property),
                SchemaEdit::SubPropertyOf { sub, .. } => Some(sub),
                SchemaEdit::Align { frona, .. } => Some(frona),
                _ => None,
            };
            if let Some(term) = term {
                declared.insert(prefixes.expand(term));
            }
        }

        let mut undeclared = Vec::<(String, String, String)>::new();
        for entity in entities
            .iter()
            .filter(|entity| abox::entity_is_eligible(entity))
        {
            for kind in &entity.kinds {
                let iri = prefixes.expand(kind);
                if iri.starts_with("urn:frona:") && !declared.contains(&iri) {
                    undeclared.push((iri, entity.path.clone(), format!("entity kind {kind}")));
                }
            }
            if let Some(attributes) = entity.attributes.as_object() {
                for key in attributes.keys() {
                    let iri = prefixes.expand(key);
                    if iri.starts_with("urn:frona:") && !declared.contains(&iri) {
                        undeclared.push((iri, entity.path.clone(), format!("attribute {key}")));
                    }
                }
            }
        }
        let eligible_paths = abox::eligible_entity_paths(entities);
        for link in links
            .iter()
            .filter(|link| abox::link_is_eligible(link, &eligible_paths))
        {
            let iri = prefixes.expand(&link.relation);
            if iri.starts_with("urn:frona:") && !declared.contains(&iri) {
                undeclared.push((
                    iri,
                    link.from_entity_path.clone(),
                    format!(
                        "link {} --{}--> {}",
                        link.from_entity_path, link.relation, link.to_entity_path,
                    ),
                ));
            }
        }
        undeclared.sort();
        undeclared.dedup();
        for (term, subject, witness) in undeclared {
            let index = diagnostics.len();
            diagnostics.push(ValidationDiagnostic {
                id: format!("undeclared-{index}"),
                kind: ValidationDiagnosticKind::UndeclaredTerm,
                rule: "declared-before-use".into(),
                detail: format!("{term} is used but absent from the projected TBox"),
                subject: Some(subject),
                introduced_by_candidate: true,
                causal_axioms: Vec::new(),
                witness_triples: vec![witness],
                affected_count: 1,
            });
        }
        Ok(GraphValidation { diagnostics })
    }

    pub async fn test_edits(
        &self,
        user_id: &str,
        edits: &[SchemaEdit],
    ) -> Result<EditImpact, Error> {
        let current = self.load(user_id).await?;
        let abox = self
            .user_abox(user_id, current.effective_ontology())
            .await?;
        edit_impact(&current, edits, &abox)
    }

    pub async fn test_edits_with_abox(
        &self,
        user_id: &str,
        edits: &[SchemaEdit],
        abox: &[Triple],
    ) -> Result<EditImpact, Error> {
        let current = self.load(user_id).await?;
        edit_impact(&current, edits, abox)
    }

    async fn user_abox(
        &self,
        user_id: &str,
        effective: &super::OntologyScope,
    ) -> Result<Vec<Triple>, Error> {
        self.ensure_scope(user_id)?;
        let entities = self.graph.knowledge_entities();
        let links = self.asserted_links();
        Ok(abox::build_abox_triples(
            &entities,
            &links,
            effective.prefixes(),
        ))
    }
}
