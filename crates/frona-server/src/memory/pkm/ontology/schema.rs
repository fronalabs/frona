//! Building and round-tripping the per-user ontology **delta** with horned-owl.
//!
//! The delta is the set of `frona:` axioms a user's Classify has minted on top
//! of the shared reference base. Its canonical, persisted form is OWL functional
//! syntax (OFN) - human-readable and losslessly round-tripped by horned-owl. This
//! module is the only place that touches the horned-owl typed model:
//!   - [`apply_edits`] parses the stored OFN, inserts typed axioms, re-serializes.
//!   - [`delta_triples`] lowers the OFN to RDF triples the reasoner consumes.
//!   - [`catalog`] lists what the delta has minted (for the Classify prompt).

use std::io::Cursor;

use horned_owl::io::ParserConfiguration;
use horned_owl::io::ofn;
use horned_owl::io::rdf::writer::write_to_rdf_format;
use horned_owl::model::{
    AnnotatedComponent, Annotation, AnnotationAssertion, AnnotationSubject, AnnotationValue,
    ArcStr, AsymmetricObjectProperty, Build, ClassExpression, Component, DataPropertyRange,
    DataRange, DeclareClass, DeclareDataProperty, DeclareObjectProperty, DisjointClasses,
    EquivalentClasses, EquivalentDataProperties, EquivalentObjectProperties, FacetRestriction,
    FunctionalObjectProperty, InverseObjectProperties, IrreflexiveObjectProperty,
    Literal as HLiteral, MutableOntology, ObjectPropertyDomain, ObjectPropertyExpression,
    ObjectPropertyRange, SubClassOf, SubObjectPropertyExpression, SubObjectPropertyOf,
    SymmetricObjectProperty, TransitiveObjectProperty,
};
use horned_owl::ontology::component_mapped::ComponentMappedOntology;
use horned_owl::ontology::set::SetOntology;
use horned_owl::vocab::Facet as OwlFacet;
use oxigraph::io::RdfFormat;
use oxigraph::store::Store;
use oxrdf::Triple;
use serde::{Deserialize, Serialize};

use crate::core::error::AppError;

use super::prefixes::PrefixMap;

/// The per-user delta ontology, Arc-interned so it is `Send`.
type Delta = SetOntology<ArcStr>;
type Cmo = ComponentMappedOntology<ArcStr, AnnotatedComponent<ArcStr>>;

/// An empty OFN document - the delta of a user who has minted nothing.
const EMPTY_DELTA_OFN: &str = "Ontology()";

/// One schema edit the Classify stage proposes. All positions are CURIEs
/// (`frona:Service`, `schema:worksFor`); expansion to IRIs happens here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SchemaEdit {
    /// Persist the model-authored semantic intent of a term as `rdfs:comment`.
    AnnotateComment { term: String, comment: String },
    /// Mint a class.
    DeclareClass { class: String },
    /// `sub ⊑ sup` (subsumption).
    SubClassOf { sub: String, sup: String },
    /// `a ≡ b` (class equivalence / alignment).
    EquivalentClasses { a: String, b: String },
    /// `a ⊥ b` (class disjointness).
    DisjointClasses { a: String, b: String },
    /// Mint an object (entity-valued) property.
    DeclareObjectProperty { property: String },
    /// Mint a data (literal-valued) property.
    DeclareDataProperty { property: String },
    /// `sub ⊑ sup` on object properties.
    SubPropertyOf { sub: String, sup: String },
    /// `a ≡ b` on object properties (alignment).
    EquivalentProperties { a: String, b: String },
    /// `a` is the inverse of `b` (e.g. worksFor / employs).
    InverseProperties { a: String, b: String },
    /// Assert an OWL characteristic of an object property - what the relation
    /// *means*, beyond which terms it connects. Declares the property when it is a
    /// `frona:` mint. See [`Characteristic`].
    PropertyCharacteristic {
        property: String,
        characteristic: Characteristic,
    },
    /// The subject of `property` must be a `class`.
    ObjectPropertyDomain { property: String, class: String },
    /// The object of `property` must be a `class`.
    ObjectPropertyRange { property: String, class: String },
    /// Bound a data property's values: `property range datatype[min,max,pattern]`.
    /// Lowers to `DataPropertyRange(DatatypeRestriction(...))` = `owl:withRestrictions`.
    /// OWL RL ignores it; `validate` code-checks it over the closure. Declares the
    /// property when it is a `frona:` mint.
    RestrictDatatype {
        property: String,
        datatype: String,
        min: Option<i64>,
        max: Option<i64>,
        pattern: Option<String>,
    },
    /// Align a `frona:` proposal to a standard term: an equivalence axiom. The ABox
    /// re-key (usage of `frona` → `standard`) is applied in code by Assemble, not here.
    Align {
        frona: String,
        standard: String,
        kind: AlignKind,
    },
    /// Loosen one of the user's **own delta** axioms; base masking is not supported.
    /// Removes the matching axiom from the delta.
    AmendOverride { target: OverrideTarget },
}

/// What an [`SchemaEdit::Align`] aligns - decides the equivalence axiom kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignKind {
    Class,
    ObjectProperty,
    DataProperty,
}

/// An OWL characteristic asserted of an object property - the semantics a relation
/// carries beyond which terms it connects.
///
/// Two of these *derive* edges and one derives identity; the other two *reject*
/// data, so they reach the arbitration ladder as ordinary data violations rather
/// than as schema incoherence (a probe individual carries no edges for them to fire
/// on). Which bucket a characteristic lands in is the thing to keep in mind when
/// declaring one.
///
/// `owl:InverseFunctionalProperty` is deliberately absent. `reasonable` 0.4.4's
/// `prp-ifp` emits `owl:sameAs` between the subjects of *any* two assertions of an
/// inverse-functional property, whatever their objects, so declaring one collapses
/// every entity that carries the property into a single entity. The five below are
/// pinned in both directions by `ontology/tests/characteristics.rs`, which is also
/// where the exclusion is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Characteristic {
    /// `prp-fp` - one subject cannot have two different values, so any two it does
    /// have are the same thing (`owl:sameAs`). "Everyone has exactly one birthplace."
    Functional,
    /// `prp-trp` - chains close: `a ⊑ b`, `b ⊑ c` ⊢ `a ⊑ c`. "Part of, part of."
    Transitive,
    /// `prp-symp` - the reverse edge holds too. "Knows, is married to."
    Symmetric,
    /// `prp-asyp` - the reverse edge is a contradiction. "Parent of, reports to."
    Asymmetric,
    /// `prp-irp` - nothing bears it to itself. "Parent of."
    Irreflexive,
}

/// What an [`SchemaEdit::AmendOverride`] loosens (a user-delta axiom to remove).
///
/// Every variant names an axiom that **constrains or derives** rather than one that merely
/// declares a term, because those are the ones a later pass can discover were wrong. The
/// gate only ever sees the ABox as it stands, so an axiom nothing currently contradicts
/// commits cleanly and turns out to be false later - which is precisely when there has to
/// be a way back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverrideTarget {
    /// Drop a `DisjointClasses(a, b)` the delta declared.
    Disjoint { a: String, b: String },
    /// Drop the datatype facet (`DataPropertyRange`) the delta put on `property`.
    Facet { property: String },
    /// Drop a [`Characteristic`] the delta asserted of `property`.
    ///
    /// The most consequential of the three, because two of these *derive* rather than
    /// constrain: a wrong `Transitive` or `Symmetric` writes edges into the entity graph on
    /// every reasoning pass, and adding edges rarely trips anything the gate can see, so
    /// it commits almost unconditionally. Retraction is the only thing that stops it.
    Characteristic {
        property: String,
        characteristic: Characteristic,
    },
}

/// Parse a stored OFN delta into the horned-owl model. An empty/blank string is
/// the empty ontology.
pub fn parse_delta(ofn: &str) -> Result<Delta, AppError> {
    let doc = if ofn.trim().is_empty() {
        EMPTY_DELTA_OFN
    } else {
        ofn
    };
    let (onto, _prefixes): (Delta, _) = ofn::reader::read(
        &mut Cursor::new(doc.as_bytes()),
        ParserConfiguration::default(),
    )
    .map_err(|e| AppError::Internal(format!("ontology: parse delta OFN: {e}")))?;
    Ok(onto)
}

fn to_cmo(onto: &Delta) -> Cmo {
    onto.clone().into_iter().collect()
}

pub fn serialize_delta(onto: &Delta) -> Result<String, AppError> {
    let cmo = to_cmo(onto);
    let bytes = ofn::writer::write(Vec::<u8>::new(), &cmo, None)
        .map_err(|e| AppError::Internal(format!("ontology: serialize delta OFN: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| AppError::Internal(format!("ontology: delta OFN utf8: {e}")))
}

pub fn apply_edits(
    ofn: &str,
    edits: &[SchemaEdit],
    prefixes: &PrefixMap,
) -> Result<String, AppError> {
    let mut onto = parse_delta(ofn)?;
    let b = Build::new_arc();
    for edit in edits {
        insert_edit(&mut onto, &b, prefixes, edit);
    }
    serialize_delta(&onto)
}

/// Lower a stored OFN delta to RDF triples (via horned-owl's Turtle writer, parsed
/// back with oxigraph). These are unioned with the base + ABox for reasoning.
pub fn delta_triples(ofn: &str) -> Result<Vec<Triple>, AppError> {
    let onto = parse_delta(ofn)?;
    if onto.iter().next().is_none() {
        return Ok(Vec::new());
    }
    let cmo = to_cmo(&onto);
    let ttl = write_to_rdf_format(Vec::<u8>::new(), &cmo, "ttl")
        .map_err(|e| AppError::Internal(format!("ontology: delta to turtle: {e}")))?;
    let store =
        Store::new().map_err(|e| AppError::Internal(format!("ontology: delta store: {e}")))?;
    store
        .load_from_reader(RdfFormat::Turtle, ttl.as_slice())
        .map_err(|e| AppError::Internal(format!("ontology: parse delta turtle: {e}")))?;
    store
        .iter()
        .map(|q| {
            q.map(|q| Triple::new(q.subject, q.predicate, q.object))
                .map_err(|e| AppError::Internal(format!("ontology: iterate delta: {e}")))
        })
        .collect()
}

/// The `frona:` terms the delta has minted - the compact "what exists so far"
/// view handed to the Classify stage (the full catalogue is explored through term tools).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Catalog {
    pub classes: Vec<String>,
    pub object_properties: Vec<String>,
    pub data_properties: Vec<String>,
}

/// Read the minted terms out of a stored OFN delta, as CURIEs.
pub fn catalog(ofn: &str, prefixes: &PrefixMap) -> Result<Catalog, AppError> {
    let onto = parse_delta(ofn)?;
    let mut cat = Catalog::default();
    for ac in onto.iter() {
        match &ac.component {
            Component::DeclareClass(DeclareClass(c)) => {
                push_curie(&mut cat.classes, prefixes, c.0.as_ref());
            }
            Component::DeclareObjectProperty(DeclareObjectProperty(p)) => {
                push_curie(&mut cat.object_properties, prefixes, p.0.as_ref());
            }
            Component::DeclareDataProperty(DeclareDataProperty(p)) => {
                push_curie(&mut cat.data_properties, prefixes, p.0.as_ref());
            }
            _ => {}
        }
    }
    for v in [
        &mut cat.classes,
        &mut cat.object_properties,
        &mut cat.data_properties,
    ] {
        v.sort();
        v.dedup();
    }
    Ok(cat)
}

fn push_curie(out: &mut Vec<String>, prefixes: &PrefixMap, iri: &str) {
    out.push(prefixes.compact(iri).unwrap_or_else(|| iri.to_string()));
}

/// The axioms in this delta that an [`SchemaEdit::AmendOverride`] could loosen.
///
/// `catalog` answers "what terms exist"; this answers "what claims are in force and could
/// be wrong". Assemble needs the second: its proposal set is only the terms *this pass*
/// used, so without this it cannot name - and therefore cannot amend - an axiom committed
/// by an earlier pass, which is where a wrong axiom almost always comes from.
///
/// Declarations, subsumptions and equivalences are deliberately absent. Retracting those
/// would strand entities typed by the term, so amend is limited to *loosening a constraint*,
/// not withdrawing vocabulary.
pub fn retractable(ofn: &str, prefixes: &PrefixMap) -> Result<Vec<OverrideTarget>, AppError> {
    let onto = parse_delta(ofn)?;
    let curie = |iri: &str| prefixes.compact(iri).unwrap_or_else(|| iri.to_string());
    // The property an axiom is about, if it is a plainly-named one. An inverse expression
    // is not something this build writes, and there is no CURIE to name it by.
    let named = |ope: &ObjectPropertyExpression<ArcStr>| match ope {
        ObjectPropertyExpression::ObjectProperty(p) => Some(curie(p.0.as_ref())),
        ObjectPropertyExpression::InverseObjectProperty(_) => None,
    };
    let mut out = Vec::new();
    for ac in onto.iter() {
        let target = match &ac.component {
            Component::DisjointClasses(DisjointClasses(ces)) => {
                let mut pair = ces.iter().filter_map(class_iri).map(curie);
                match (pair.next(), pair.next()) {
                    // Only a plain two-class axiom is nameable by an `OverrideTarget`;
                    // `insert_edit` never writes any other shape.
                    (Some(a), Some(b)) => Some(OverrideTarget::Disjoint { a, b }),
                    _ => None,
                }
            }
            Component::DataPropertyRange(r) => Some(OverrideTarget::Facet {
                property: curie(r.dp.0.as_ref()),
            }),
            Component::FunctionalObjectProperty(a) => {
                named(&a.0).map(|p| characteristic_target(p, Characteristic::Functional))
            }
            Component::TransitiveObjectProperty(a) => {
                named(&a.0).map(|p| characteristic_target(p, Characteristic::Transitive))
            }
            Component::SymmetricObjectProperty(a) => {
                named(&a.0).map(|p| characteristic_target(p, Characteristic::Symmetric))
            }
            Component::AsymmetricObjectProperty(a) => {
                named(&a.0).map(|p| characteristic_target(p, Characteristic::Asymmetric))
            }
            Component::IrreflexiveObjectProperty(a) => {
                named(&a.0).map(|p| characteristic_target(p, Characteristic::Irreflexive))
            }
            _ => None,
        };
        if let Some(t) = target
            && !out.contains(&t)
        {
            out.push(t);
        }
    }
    Ok(out)
}

fn characteristic_target(property: String, characteristic: Characteristic) -> OverrideTarget {
    OverrideTarget::Characteristic {
        property,
        characteristic,
    }
}

fn insert_edit(onto: &mut Delta, b: &Build<ArcStr>, px: &PrefixMap, edit: &SchemaEdit) {
    let cls = |c: &str| b.class(px.expand(c));
    let op = |p: &str| b.object_property(px.expand(p));
    let dp = |p: &str| b.data_property(px.expand(p));
    // Declare `frona:`-namespaced terms so the reasoner gets the owl:Class /
    // owl:ObjectProperty typing its RL rules key on (base terms are already typed).
    let mint_class = |onto: &mut Delta, c: &str| {
        if is_mint(px, c) {
            onto.insert(DeclareClass(cls(c)));
        }
    };
    let mint_op = |onto: &mut Delta, p: &str| {
        if is_mint(px, p) {
            onto.insert(DeclareObjectProperty(op(p)));
        }
    };
    match edit {
        SchemaEdit::AnnotateComment { term, comment } => {
            onto.insert(AnnotationAssertion {
                subject: AnnotationSubject::IRI(b.iri(px.expand(term))),
                ann: Annotation {
                    ap: b.annotation_property("http://www.w3.org/2000/01/rdf-schema#comment"),
                    av: AnnotationValue::Literal(HLiteral::Simple {
                        literal: comment.trim().to_string(),
                    }),
                    ann: Default::default(),
                },
            });
        }
        SchemaEdit::DeclareClass { class } => {
            onto.insert(DeclareClass(cls(class)));
        }
        SchemaEdit::SubClassOf { sub, sup } => {
            mint_class(onto, sub);
            mint_class(onto, sup);
            onto.insert(SubClassOf {
                sub: cls(sub).into(),
                sup: cls(sup).into(),
            });
        }
        SchemaEdit::EquivalentClasses { a, b: bb } => {
            mint_class(onto, a);
            mint_class(onto, bb);
            onto.insert(EquivalentClasses(vec![cls(a).into(), cls(bb).into()]));
        }
        SchemaEdit::DisjointClasses { a, b: bb } => {
            mint_class(onto, a);
            mint_class(onto, bb);
            onto.insert(DisjointClasses(vec![cls(a).into(), cls(bb).into()]));
        }
        SchemaEdit::DeclareObjectProperty { property } => {
            onto.insert(DeclareObjectProperty(op(property)));
        }
        SchemaEdit::DeclareDataProperty { property } => {
            onto.insert(DeclareDataProperty(dp(property)));
        }
        SchemaEdit::SubPropertyOf { sub, sup } => {
            mint_op(onto, sub);
            mint_op(onto, sup);
            onto.insert(SubObjectPropertyOf {
                sub: SubObjectPropertyExpression::ObjectPropertyExpression(op(sub).into()),
                sup: op(sup).into(),
            });
        }
        SchemaEdit::EquivalentProperties { a, b: bb } => {
            mint_op(onto, a);
            mint_op(onto, bb);
            onto.insert(EquivalentObjectProperties(vec![
                op(a).into(),
                op(bb).into(),
            ]));
        }
        SchemaEdit::InverseProperties { a, b: bb } => {
            mint_op(onto, a);
            mint_op(onto, bb);
            onto.insert(InverseObjectProperties(op(a), op(bb)));
        }
        SchemaEdit::PropertyCharacteristic {
            property,
            characteristic,
        } => {
            mint_op(onto, property);
            let ope = op(property).into();
            match characteristic {
                Characteristic::Functional => {
                    onto.insert(FunctionalObjectProperty(ope));
                }
                Characteristic::Transitive => {
                    onto.insert(TransitiveObjectProperty(ope));
                }
                Characteristic::Symmetric => {
                    onto.insert(SymmetricObjectProperty(ope));
                }
                Characteristic::Asymmetric => {
                    onto.insert(AsymmetricObjectProperty(ope));
                }
                Characteristic::Irreflexive => {
                    onto.insert(IrreflexiveObjectProperty(ope));
                }
            }
        }
        SchemaEdit::ObjectPropertyDomain { property, class } => {
            mint_op(onto, property);
            mint_class(onto, class);
            onto.insert(ObjectPropertyDomain {
                ope: op(property).into(),
                ce: cls(class).into(),
            });
        }
        SchemaEdit::ObjectPropertyRange { property, class } => {
            mint_op(onto, property);
            mint_class(onto, class);
            onto.insert(ObjectPropertyRange {
                ope: op(property).into(),
                ce: cls(class).into(),
            });
        }
        SchemaEdit::RestrictDatatype {
            property,
            datatype,
            min,
            max,
            pattern,
        } => {
            if is_mint(px, property) {
                onto.insert(DeclareDataProperty(dp(property)));
            }
            let dt_iri = px.expand(datatype);
            let dlit = |v: &str, ns: &str| HLiteral::Datatype {
                literal: v.to_string(),
                datatype_iri: b.iri(ns.to_string()),
            };
            let mut facets = Vec::new();
            if let Some(mn) = min {
                facets.push(FacetRestriction {
                    f: OwlFacet::MinInclusive,
                    l: dlit(&mn.to_string(), &dt_iri),
                });
            }
            if let Some(mx) = max {
                facets.push(FacetRestriction {
                    f: OwlFacet::MaxInclusive,
                    l: dlit(&mx.to_string(), &dt_iri),
                });
            }
            if let Some(pat) = pattern {
                facets.push(FacetRestriction {
                    f: OwlFacet::Pattern,
                    l: dlit(pat, XSD_STRING),
                });
            }
            // OFN's `DatatypeRestriction` requires **at least one** facet/value pair, so an
            // unbounded range has to be the bare datatype. Emitting `DatatypeRestriction(<dt>)`
            // with none produced a delta that could not be re-parsed - and since every later
            // `load`/`apply_edits`/`catalog` goes through that parse, a single facet-less
            // datatype (which is what a plain `declare` with a datatype and no bounds is)
            // silently disabled the whole schema layer: nothing declared, every term left
            // undeclared, every entity untyped.
            let dt = b.datatype(dt_iri);
            let dr = if facets.is_empty() {
                DataRange::Datatype(dt)
            } else {
                DataRange::DatatypeRestriction(dt, facets)
            };
            onto.insert(DataPropertyRange {
                dp: dp(property),
                dr,
            });
        }
        SchemaEdit::Align {
            frona,
            standard,
            kind,
        } => match kind {
            AlignKind::Class => {
                mint_class(onto, frona);
                onto.insert(EquivalentClasses(vec![
                    cls(frona).into(),
                    cls(standard).into(),
                ]));
            }
            AlignKind::ObjectProperty => {
                mint_op(onto, frona);
                onto.insert(EquivalentObjectProperties(vec![
                    op(frona).into(),
                    op(standard).into(),
                ]));
            }
            AlignKind::DataProperty => {
                if is_mint(px, frona) {
                    onto.insert(DeclareDataProperty(dp(frona)));
                }
                onto.insert(EquivalentDataProperties(vec![dp(frona), dp(standard)]));
            }
        },
        SchemaEdit::AmendOverride { target } => {
            // Relax only an axiom the user's own delta declared: filter it out and
            // rebuild. Base axioms are immutable per-user (no subtraction of `base`).
            *onto = onto
                .iter()
                .filter(|ac| !override_matches(target, &ac.component, px))
                .cloned()
                .collect();
        }
    }
}

/// XSD string IRI - the datatype of a `pattern` facet literal.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Whether a delta component is the axiom an [`OverrideTarget`] names (by expanded
/// IRI, order-insensitive for the disjoint pair).
fn override_matches(target: &OverrideTarget, cmp: &Component<ArcStr>, px: &PrefixMap) -> bool {
    match target {
        OverrideTarget::Disjoint { a, b } => {
            let Component::DisjointClasses(DisjointClasses(ces)) = cmp else {
                return false;
            };
            let got: std::collections::HashSet<&str> = ces.iter().filter_map(class_iri).collect();
            let want = [px.expand(a), px.expand(b)];
            want.iter().all(|w| got.contains(w.as_str())) && got.len() == want.len()
        }
        OverrideTarget::Facet { property } => {
            matches!(cmp, Component::DataPropertyRange(r) if r.dp.0.as_ref() == px.expand(property))
        }
        // Matched on the characteristic *and* the property: a property may carry several,
        // and loosening one must not silently take the rest with it.
        OverrideTarget::Characteristic {
            property,
            characteristic,
        } => {
            let iri = px.expand(property);
            let named = |ope: &ObjectPropertyExpression<ArcStr>| match ope {
                ObjectPropertyExpression::ObjectProperty(p) => p.0.as_ref() == iri,
                // An inverse expression is not something this build ever writes.
                ObjectPropertyExpression::InverseObjectProperty(_) => false,
            };
            match (characteristic, cmp) {
                (Characteristic::Functional, Component::FunctionalObjectProperty(a)) => named(&a.0),
                (Characteristic::Transitive, Component::TransitiveObjectProperty(a)) => named(&a.0),
                (Characteristic::Symmetric, Component::SymmetricObjectProperty(a)) => named(&a.0),
                (Characteristic::Asymmetric, Component::AsymmetricObjectProperty(a)) => named(&a.0),
                (Characteristic::Irreflexive, Component::IrreflexiveObjectProperty(a)) => {
                    named(&a.0)
                }
                _ => false,
            }
        }
    }
}

fn class_iri(ce: &ClassExpression<ArcStr>) -> Option<&str> {
    match ce {
        ClassExpression::Class(c) => Some(c.0.as_ref()),
        _ => None,
    }
}

/// A term is ours to declare iff it expands into the `frona:` namespace.
fn is_mint(px: &PrefixMap, curie: &str) -> bool {
    px.expand(curie).starts_with("urn:frona:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px() -> PrefixMap {
        PrefixMap::standard()
    }

    #[test]
    fn empty_delta_round_trips() {
        let onto = parse_delta("").unwrap();
        let ofn = serialize_delta(&onto).unwrap();
        // re-parses cleanly; mints nothing and carries no schema axioms (an
        // ontology-header component may survive the round-trip - that is inert).
        let cat = catalog(&ofn, &px()).unwrap();
        assert_eq!(cat, Catalog::default(), "empty delta mints nothing");
        let triples = delta_triples(&ofn).unwrap();
        let has_axiom = triples.iter().any(|t| {
            let p = t.predicate.as_str();
            p.contains("subClassOf")
                || p.contains("equivalentClass")
                || p.contains("disjointWith")
                || p.contains("inverseOf")
        });
        assert!(!has_axiom, "empty delta has no schema axioms: {triples:?}");
    }

    #[test]
    fn rdfs_comment_round_trips_and_lowers_to_a_searchable_triple() {
        let comment = "A device whose firmware the person intends to update.";
        let ofn = apply_edits(
            "",
            &[
                SchemaEdit::DeclareObjectProperty {
                    property: "frona:firmwareUpdateTarget".into(),
                },
                SchemaEdit::AnnotateComment {
                    term: "frona:firmwareUpdateTarget".into(),
                    comment: comment.into(),
                },
            ],
            &px(),
        )
        .unwrap();

        assert!(
            ofn.contains("AnnotationAssertion"),
            "comment is persisted in OFN: {ofn}"
        );
        assert!(
            ofn.contains(comment),
            "comment text survives serialization: {ofn}"
        );
        let roundtrip = apply_edits(&ofn, &[], &px()).unwrap();
        assert!(
            roundtrip.contains(comment),
            "comment survives parse and reload: {roundtrip}"
        );

        let triples = delta_triples(&roundtrip).unwrap();
        assert!(triples.iter().any(|triple| {
                triple.subject.to_string().contains("urn:frona:firmwareUpdateTarget")
                    && triple.predicate.as_str()
                        == "http://www.w3.org/2000/01/rdf-schema#comment"
                    && matches!(&triple.object, oxrdf::Term::Literal(value) if value.value() == comment)
            }), "rdfs:comment is available to RDF search: {triples:?}");
    }

    #[test]
    fn subclass_edit_lowers_to_triples_and_catalogs() {
        let ofn = apply_edits(
            "",
            &[SchemaEdit::SubClassOf {
                sub: "frona:Database".into(),
                sup: "schema:SoftwareApplication".into(),
            }],
            &px(),
        )
        .unwrap();

        let triples = delta_triples(&ofn).unwrap();
        let has_subclass = triples.iter().any(|t| {
            t.subject.to_string().contains("urn:frona:Database")
                && t.predicate.as_str().contains("subClassOf")
                && t.object
                    .to_string()
                    .contains("schema.org/SoftwareApplication")
        });
        assert!(has_subclass, "subClassOf edge present in {triples:?}");

        let cat = catalog(&ofn, &px()).unwrap();
        assert_eq!(cat.classes, vec!["frona:Database".to_string()]);
    }

    fn asserts(ofn: &str, needle: &str) -> bool {
        ofn.lines().any(|l| l.contains(needle))
    }

    fn amend(target: OverrideTarget) -> SchemaEdit {
        SchemaEdit::AmendOverride { target }
    }

    fn characteristic(property: &str, characteristic: Characteristic) -> SchemaEdit {
        SchemaEdit::PropertyCharacteristic {
            property: property.into(),
            characteristic,
        }
    }

    /// The retraction that matters most: `Transitive` and `Symmetric` *derive* edges, and
    /// adding edges rarely trips anything the gate can see, so a wrong one commits almost
    /// unconditionally and then writes false edges on every reasoning pass. Nothing but
    /// removing the axiom stops it.
    #[test]
    fn characteristic_can_be_retracted_after_it_was_committed() {
        let ofn = apply_edits(
            "",
            &[characteristic("frona:partOf", Characteristic::Transitive)],
            &px(),
        )
        .unwrap();
        assert!(
            asserts(&ofn, "TransitiveObjectProperty"),
            "committed first: {ofn}"
        );

        let ofn = apply_edits(
            &ofn,
            &[amend(OverrideTarget::Characteristic {
                property: "frona:partOf".into(),
                characteristic: Characteristic::Transitive,
            })],
            &px(),
        )
        .unwrap();
        assert!(
            !asserts(&ofn, "TransitiveObjectProperty"),
            "and retracted: {ofn}"
        );
        // The reasoner is what actually has to stop deriving, so check the lowering too.
        let triples = delta_triples(&ofn).unwrap();
        assert!(
            !triples
                .iter()
                .any(|t| t.object.to_string().contains("TransitiveProperty")),
            "the axiom is gone from the triples the reasoner reads: {triples:?}"
        );
    }

    /// A property may carry several characteristics. Loosening one must not take the rest
    /// with it - that would silently retract claims nobody asked to withdraw.
    #[test]
    fn retracting_one_characteristic_leaves_the_others_asserted() {
        let ofn = apply_edits(
            "",
            &[
                characteristic("frona:parentOf", Characteristic::Asymmetric),
                characteristic("frona:parentOf", Characteristic::Irreflexive),
            ],
            &px(),
        )
        .unwrap();
        let ofn = apply_edits(
            &ofn,
            &[amend(OverrideTarget::Characteristic {
                property: "frona:parentOf".into(),
                characteristic: Characteristic::Asymmetric,
            })],
            &px(),
        )
        .unwrap();
        assert!(
            !asserts(&ofn, "AsymmetricObjectProperty"),
            "the named one went: {ofn}"
        );
        assert!(
            asserts(&ofn, "IrreflexiveObjectProperty"),
            "the other stayed: {ofn}"
        );
    }

    #[test]
    fn retracting_a_characteristic_is_scoped_to_its_property() {
        let ofn = apply_edits(
            "",
            &[
                characteristic("frona:partOf", Characteristic::Transitive),
                characteristic("frona:ancestorOf", Characteristic::Transitive),
            ],
            &px(),
        )
        .unwrap();
        let ofn = apply_edits(
            &ofn,
            &[amend(OverrideTarget::Characteristic {
                property: "frona:partOf".into(),
                characteristic: Characteristic::Transitive,
            })],
            &px(),
        )
        .unwrap();
        // The *axiom*, not the declaration: retracting a characteristic does not
        // un-declare the property, and entities are still keyed by it.
        let transitive_on = |p: &str| {
            ofn.lines()
                .any(|l| l.contains("TransitiveObjectProperty") && l.contains(p))
        };
        assert!(
            !transitive_on("urn:frona:partOf"),
            "partOf's axiom went: {ofn}"
        );
        assert!(
            transitive_on("urn:frona:ancestorOf"),
            "ancestorOf's stayed: {ofn}"
        );
        assert!(
            asserts(&ofn, "Declaration(ObjectProperty(<urn:frona:partOf>))"),
            "and partOf is still a declared property: {ofn}"
        );
    }

    /// A disjointness committed while nothing held both classes is exactly the axiom that
    /// starts blocking legitimate classifications later - and `classify` responds by
    /// quarantining the entity's facts, so without retraction those facts stay hidden.
    #[test]
    fn disjointness_can_be_retracted_in_either_pair_order() {
        for (a, b) in [
            ("frona:Tool", "frona:Service"),
            ("frona:Service", "frona:Tool"),
        ] {
            let ofn = apply_edits(
                "",
                &[SchemaEdit::DisjointClasses {
                    a: "frona:Tool".into(),
                    b: "frona:Service".into(),
                }],
                &px(),
            )
            .unwrap();
            assert!(asserts(&ofn, "DisjointClasses"), "committed: {ofn}");
            let ofn = apply_edits(
                &ofn,
                &[amend(OverrideTarget::Disjoint {
                    a: a.into(),
                    b: b.into(),
                })],
                &px(),
            )
            .unwrap();
            assert!(
                !asserts(&ofn, "DisjointClasses"),
                "named as ({a}, {b}) — order is not part of the axiom: {ofn}"
            );
        }
    }

    /// A datatype with **no** bounds - which is what a plain `declare` with a datatype is,
    /// and so the overwhelmingly common case - must still produce OFN that parses back.
    ///
    /// It did not. `DatatypeRestriction` requires at least one facet/value pair, so a
    /// facet-less one serialized to `DatatypeRestriction(<xsd:anyURI> )` and every
    /// subsequent read of that delta failed with `expected IRI`. Since `load`, `apply_edits`
    /// and `catalog` all go through that parse, one such edit disabled the entire schema
    /// layer for a user: nothing could be declared afterwards, so every term stayed
    /// undeclared and every entity untyped - with no error anywhere near the cause.
    ///
    /// The round trip is the assertion. Serializing is not the hard part; reading it back is.
    #[test]
    fn datatype_range_with_no_facets_still_parses_back() {
        let declare = |property: &str, datatype: &str| SchemaEdit::RestrictDatatype {
            property: property.into(),
            datatype: datatype.into(),
            min: None,
            max: None,
            pattern: None,
        };
        let ofn = apply_edits(
            "",
            &[declare("frona:firmwareDownloadUrl", "xsd:anyURI")],
            &px(),
        )
        .unwrap();
        assert!(asserts(&ofn, "DataPropertyRange"), "committed: {ofn}");
        assert!(
            !ofn.contains("DatatypeRestriction"),
            "an unbounded range is the bare datatype, not an empty restriction: {ofn}"
        );

        // The delta is only useful if it can be read again - a second edit has to parse the
        // first, which is exactly what was failing.
        let ofn = apply_edits(&ofn, &[declare("frona:releaseDate", "xsd:date")], &px())
            .expect("a committed delta must parse on the next edit");
        for t in ["frona:firmwareDownloadUrl", "frona:releaseDate"] {
            assert!(
                catalog(&ofn, &px())
                    .unwrap()
                    .data_properties
                    .contains(&t.to_string()),
                "{ofn}"
            );
        }

        let bounded = apply_edits(
            "",
            &[SchemaEdit::RestrictDatatype {
                property: "frona:port".into(),
                datatype: "xsd:integer".into(),
                min: Some(1),
                max: Some(65535),
                pattern: None,
            }],
            &px(),
        )
        .unwrap();
        assert!(
            bounded.contains("DatatypeRestriction"),
            "bounds still restrict: {bounded}"
        );
    }

    #[test]
    fn datatype_facet_can_be_retracted() {
        let ofn = apply_edits(
            "",
            &[SchemaEdit::RestrictDatatype {
                property: "frona:port".into(),
                datatype: "xsd:integer".into(),
                min: Some(1),
                max: Some(1024),
                pattern: None,
            }],
            &px(),
        )
        .unwrap();
        assert!(asserts(&ofn, "DataPropertyRange"), "committed: {ofn}");
        let ofn = apply_edits(
            &ofn,
            &[amend(OverrideTarget::Facet {
                property: "frona:port".into(),
            })],
            &px(),
        )
        .unwrap();
        assert!(!asserts(&ofn, "DataPropertyRange"), "retracted: {ofn}");
        // The property itself survives - loosening a bound is not un-declaring the term,
        // and entities are still keyed by it.
        let cat = catalog(&ofn, &px()).unwrap();
        assert!(
            cat.data_properties.contains(&"frona:port".to_string()),
            "{cat:?}"
        );
    }

    /// What Assemble is shown, so it can name an axiom an earlier pass committed. Every
    /// entry has to round-trip: naming it back as an `Amend` must retract that axiom, or
    /// the list is advice the model cannot act on.
    #[test]
    fn every_retractable_axiom_round_trips_through_an_amend() {
        let ofn = apply_edits(
            "",
            &[
                SchemaEdit::DisjointClasses {
                    a: "frona:Tool".into(),
                    b: "frona:Service".into(),
                },
                SchemaEdit::RestrictDatatype {
                    property: "frona:port".into(),
                    datatype: "xsd:integer".into(),
                    min: Some(1),
                    max: None,
                    pattern: None,
                },
                characteristic("frona:partOf", Characteristic::Transitive),
                characteristic("frona:knows", Characteristic::Symmetric),
                characteristic("frona:parentOf", Characteristic::Irreflexive),
                // Declarations and subsumptions are NOT retractable - withdrawing
                // vocabulary would strand the entities typed by it.
                SchemaEdit::SubClassOf {
                    sub: "frona:Db".into(),
                    sup: "schema:Thing".into(),
                },
                SchemaEdit::DeclareClass {
                    class: "frona:Loose".into(),
                },
            ],
            &px(),
        )
        .unwrap();

        let targets = retractable(&ofn, &px()).unwrap();
        assert_eq!(
            targets.len(),
            5,
            "constraints and derivations only: {targets:?}"
        );
        assert!(targets.contains(&OverrideTarget::Facet {
            property: "frona:port".into()
        }));
        assert!(targets.contains(&OverrideTarget::Characteristic {
            property: "frona:partOf".into(),
            characteristic: Characteristic::Transitive,
        }));

        // Naming each one back retracts it, and retracting all of them empties the
        // constraint layer while leaving the vocabulary standing.
        let mut ofn = ofn;
        for t in targets {
            ofn = apply_edits(&ofn, &[amend(t.clone())], &px()).unwrap();
            assert!(
                !retractable(&ofn, &px()).unwrap().contains(&t),
                "{t:?} survived being named: {ofn}"
            );
        }
        assert!(retractable(&ofn, &px()).unwrap().is_empty(), "{ofn}");
        let cat = catalog(&ofn, &px()).unwrap();
        assert!(
            cat.classes.contains(&"frona:Loose".to_string()),
            "terms survive: {cat:?}"
        );
    }

    /// A delta that has only declared things has nothing to loosen - the list is empty
    /// rather than absent, so the prompt can say "nothing to amend" honestly.
    #[test]
    fn delta_with_no_constraints_offers_nothing_to_retract() {
        let ofn = apply_edits(
            "",
            &[SchemaEdit::SubClassOf {
                sub: "frona:Database".into(),
                sup: "schema:SoftwareApplication".into(),
            }],
            &px(),
        )
        .unwrap();
        assert!(retractable(&ofn, &px()).unwrap().is_empty());
        assert!(
            retractable("", &px()).unwrap().is_empty(),
            "and an empty delta likewise"
        );
    }

    /// Retraction is a no-op against an axiom the delta never held. It must not throw, and
    /// it must not disturb anything else: Assemble can name a target from a stale view.
    #[test]
    fn amending_an_axiom_that_was_never_asserted_changes_nothing() {
        let before = apply_edits(
            "",
            &[characteristic("frona:partOf", Characteristic::Transitive)],
            &px(),
        )
        .unwrap();
        let after = apply_edits(
            &before,
            &[
                amend(OverrideTarget::Characteristic {
                    property: "frona:partOf".into(),
                    characteristic: Characteristic::Symmetric, // never asserted
                }),
                amend(OverrideTarget::Facet {
                    property: "frona:nothing".into(),
                }),
                amend(OverrideTarget::Disjoint {
                    a: "frona:A".into(),
                    b: "frona:B".into(),
                }),
            ],
            &px(),
        )
        .unwrap();
        assert!(
            asserts(&after, "TransitiveObjectProperty"),
            "untouched: {after}"
        );
    }

    #[test]
    fn edits_accumulate_across_applies() {
        let ofn = apply_edits(
            "",
            &[SchemaEdit::DeclareClass {
                class: "frona:Service".into(),
            }],
            &px(),
        )
        .unwrap();
        let ofn = apply_edits(
            &ofn,
            &[SchemaEdit::InverseProperties {
                a: "frona:worksFor".into(),
                b: "frona:employs".into(),
            }],
            &px(),
        )
        .unwrap();
        let cat = catalog(&ofn, &px()).unwrap();
        assert!(cat.classes.contains(&"frona:Service".to_string()));
        assert!(
            cat.object_properties
                .contains(&"frona:worksFor".to_string())
        );
        assert!(cat.object_properties.contains(&"frona:employs".to_string()));
    }
}
