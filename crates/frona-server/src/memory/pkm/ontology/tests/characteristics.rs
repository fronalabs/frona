//! What each OWL property characteristic is contracted to do - pinned in both
//! directions, over the real path a consolidation pass takes: entities + links →
//! ABox → `frona:` delta → OWL 2 RL closure.
//!
//! Two things break silently without this file, and both have already happened
//! once in this stack:
//!
//!   1. **A reasoner rule that stops firing.** `reasonable` is a pinned third-party
//!      crate. A characteristic that quietly stops deriving anything leaves the
//!      graph merely *thinner*, never wrong-looking, so nothing downstream
//!      complains - entities just stop being connected.
//!   2. **A reasoner rule that fires too widely.** This is why
//!      `owl:InverseFunctionalProperty` is not in [`Characteristic`]:
//!      `reasonable` 0.4.4's `prp-ifp` emits `owl:sameAs` between the subjects of
//!      *any* two assertions of the property, ignoring their objects entirely
//!      (`reasoner.rs` binds the second object as a fresh pattern variable instead
//!      of comparing it). The last test here measures that, so the exclusion stays
//!      justified by a number rather than by a comment - and so a future crate bump
//!      that fixes it shows up as a failure asking for the decision to be revisited.
//!
//! Every characteristic therefore gets a **pair**: the case that must fire, and the
//! near-miss that must not. A rule that over-fires passes the first and fails the
//! second.

use chrono::Utc;
use std::collections::HashSet;

use crate::memory::pkm::model::{
    EntityCategory, EntityOrigin, KnowledgeEntity, KnowledgeEntityLink, LinkOrigin,
};
use frona_pkm::ontology::{Characteristic, PrefixMap, SchemaEdit, individual_iri};
use frona_pkm::ontology::{abox, schema, sparql};

const USER: &str = "u1";

fn entity(path: &str) -> KnowledgeEntity {
    let now = Utc::now();
    KnowledgeEntity {
        id: path.replace('/', "_"),
        user_id: USER.into(),
        path: path.into(),
        origin: EntityOrigin::Internal,
        category: EntityCategory::Concept,
        // Typed as something, or `build_abox_triples` has no individual to hang the
        // link assertions off.
        kinds: vec!["https://schema.org/Thing".into()],
        name: path.rsplit('/').next().unwrap_or(path).into(),
        description: String::new(),
        identity_evidence: Vec::new(),
        attribute_sources: Vec::new(),
        source_memory_ids: Vec::new(),
        body: String::new(),
        sync_content: None,
        mirrored_rev: None,
        extracted_rev: None,
        related_playbooks: Vec::new(),
        search_text: String::new(),
        search_names: Vec::new(),
        search_name_tokens: Vec::new(),
        search_assertions: Vec::new(),
        attributes: serde_json::Value::Null,
        use_count: 0,
        aliases: HashSet::new(),
        rev: None,
        updated_at: now,
        rendered_at: now,
    }
}

fn link(from: &str, relation: &str, to: &str) -> KnowledgeEntityLink {
    KnowledgeEntityLink {
        id: format!("{from}|{relation}|{to}"),
        user_id: USER.into(),
        from_entity_path: from.into(),
        to_entity_path: to.into(),
        relation: relation.into(),
        source_memory_ids: Vec::new(),
        origin: LinkOrigin::Asserted,
        created_at: Utc::now(),
    }
}

/// The outcome of one pass: what can be asked of the closure, and what the
/// reasoner complained about.
struct Closure {
    reasoned: frona_pkm::ontology::Reasoned,
}

impl Closure {
    /// Reason over `delta ⊕ ABox` for the given edits, entities and links.
    ///
    /// The base ontology is empty on purpose: a `frona:` mint declares itself (see
    /// `mint_op` in `schema.rs`), so nothing here depends on a downloaded catalogue
    /// release being present. That keeps the file a pure statement about the rules.
    fn build(
        edits: &[SchemaEdit],
        entities: &[KnowledgeEntity],
        links: &[KnowledgeEntityLink],
    ) -> Self {
        let px = PrefixMap::standard();
        let ofn = schema::apply_edits("", edits, &px).expect("apply edits");
        let delta = schema::delta_triples(&ofn).expect("lower delta");
        let abox = abox::build_abox_triples(
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &links.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &px,
        );
        let reasoned = frona_pkm::ontology::materialize(&[], &delta, &abox).expect("reason");
        Self { reasoned }
    }

    fn holds(&self, from: &str, relation: &str, to: &str) -> bool {
        let px = PrefixMap::standard();
        sparql::ask(
            &self.reasoned.store,
            &format!(
                "ASK {{ <{}> <{}> <{}> }}",
                individual_iri(from),
                px.expand(relation),
                individual_iri(to)
            ),
            &PrefixMap::standard(),
        )
        .expect("ask")
    }

    /// Did the reasoner raise `rule`, *and* does the ladder treat it as a clash?
    /// Both halves matter: a diagnostic missing from `CLASH_RULES` is dropped by
    /// `validate` and the characteristic asserting it is inert.
    fn clashes_on(&self, rule: &str) -> bool {
        self.reasoned.clashes().any(|d| d.rule == rule)
    }

    /// The entity paths a clash rule names, sorted and deduped - what the ladder would
    /// quarantine. Runs the real `validate` pass, so it also covers the
    /// diagnostic-message → entity-path extraction.
    fn clash_subjects(&self, rule: &str) -> Vec<String> {
        let mut out: Vec<String> =
            frona_pkm::ontology::validation::validate(&self.reasoned, &PrefixMap::standard())
                .into_iter()
                .filter(|v| v.rule == rule)
                .filter_map(|v| v.subject)
                .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Non-reflexive `owl:sameAs` pairs - how far an identity rule reached.
    fn same_as_pairs(&self) -> usize {
        let oxigraph::sparql::QueryResults::Solutions(mut rows) = sparql::query(
            &self.reasoned.store,
            "SELECT (COUNT(*) AS ?n) WHERE { ?a owl:sameAs ?b . FILTER(?a != ?b) }",
            &PrefixMap::standard(),
        )
        .expect("count") else {
            panic!("expected count solution");
        };
        let row = rows.next().expect("count row").expect("valid count row");
        let oxrdf::Term::Literal(count) = row.get("n").expect("count binding") else {
            panic!("expected numeric literal");
        };
        count.value().parse().expect("numeric count")
    }
}

fn characteristic(property: &str, characteristic: Characteristic) -> SchemaEdit {
    SchemaEdit::PropertyCharacteristic {
        property: property.into(),
        characteristic,
    }
}

/// `partOf` chains: Kreuzberg ⊑ Berlin ⊑ Germany means Kreuzberg is in Germany, and
/// that edge is what makes a query for "everything in Germany" reach the district.
#[test]
fn transitive_closes_a_chain() {
    let c = Closure::build(
        &[characteristic("frona:partOf", Characteristic::Transitive)],
        &[
            entity("places/kreuzberg"),
            entity("places/berlin"),
            entity("places/germany"),
        ],
        &[
            link("places/kreuzberg", "frona:partOf", "places/berlin"),
            link("places/berlin", "frona:partOf", "places/germany"),
        ],
    );
    assert!(
        c.holds("places/kreuzberg", "frona:partOf", "places/germany"),
        "a->b->c must derive a->c"
    );
}

/// Transitivity relates what is actually connected. Two unrelated pairs stay two
/// unrelated pairs - a rule that joined them would fabricate geography.
#[test]
fn transitive_does_not_join_unconnected_pairs() {
    let c = Closure::build(
        &[characteristic("frona:partOf", Characteristic::Transitive)],
        &[
            entity("places/kreuzberg"),
            entity("places/berlin"),
            entity("places/lisbon"),
            entity("places/portugal"),
        ],
        &[
            link("places/kreuzberg", "frona:partOf", "places/berlin"),
            link("places/lisbon", "frona:partOf", "places/portugal"),
        ],
    );
    assert!(
        !c.holds("places/kreuzberg", "frona:partOf", "places/portugal"),
        "disjoint chains must not be spliced"
    );
}

#[test]
fn symmetric_derives_the_reverse_edge() {
    let c = Closure::build(
        &[characteristic("frona:knows", Characteristic::Symmetric)],
        &[entity("people/alice"), entity("people/bob")],
        &[link("people/alice", "frona:knows", "people/bob")],
    );
    assert!(
        c.holds("people/bob", "frona:knows", "people/alice"),
        "knows(a,b) must derive knows(b,a)"
    );
}

/// The characteristic is per-property. Declaring `knows` symmetric must not make
/// `worksFor` symmetric - that would have Acme working for Alice.
#[test]
fn symmetric_does_not_leak_to_other_properties() {
    let c = Closure::build(
        &[characteristic("frona:knows", Characteristic::Symmetric)],
        &[entity("people/alice"), entity("orgs/acme")],
        &[link("people/alice", "frona:worksFor", "orgs/acme")],
    );
    assert!(
        !c.holds("orgs/acme", "frona:worksFor", "people/alice"),
        "an undeclared property must not be reversed"
    );
}

/// Asymmetric *rejects* data rather than deriving it: two people cannot each be the
/// other's parent, and the pass must see that as a clash so the ladder can act.
#[test]
fn asymmetric_flags_a_mutual_edge_as_a_clash() {
    let c = Closure::build(
        &[characteristic("frona:parentOf", Characteristic::Asymmetric)],
        &[entity("people/alice"), entity("people/bob")],
        &[
            link("people/alice", "frona:parentOf", "people/bob"),
            link("people/bob", "frona:parentOf", "people/alice"),
        ],
    );
    assert!(
        c.clashes_on("prp-asyp"),
        "a mutual edge on an asymmetric property is a clash"
    );
}

/// A mutual edge implicates **both** entities, and the reasoner says so once per
/// ordering. That is two violations naming two distinct subjects, not one violation
/// reported twice - `reasonable` already dedupes diagnostics by `(rule, message)`,
/// and these two differ in both subject and message.
///
/// Both subjects must be reported so validation feedback identifies the complete
/// contradiction rather than arbitrarily blaming one endpoint.
#[test]
fn asymmetric_implicates_both_ends_of_the_mutual_edge() {
    let c = Closure::build(
        &[characteristic("frona:parentOf", Characteristic::Asymmetric)],
        &[entity("people/alice"), entity("people/bob")],
        &[
            link("people/alice", "frona:parentOf", "people/bob"),
            link("people/bob", "frona:parentOf", "people/alice"),
        ],
    );
    let subjects = c.clash_subjects("prp-asyp");
    assert_eq!(
        subjects,
        vec!["people/alice".to_string(), "people/bob".to_string()],
        "both ends are named, each exactly once"
    );
}

#[test]
fn asymmetric_leaves_a_one_way_edge_alone() {
    let c = Closure::build(
        &[characteristic("frona:parentOf", Characteristic::Asymmetric)],
        &[entity("people/alice"), entity("people/bob")],
        &[link("people/alice", "frona:parentOf", "people/bob")],
    );
    assert!(
        !c.clashes_on("prp-asyp"),
        "a one-way edge is exactly what asymmetric permits"
    );
}

#[test]
fn irreflexive_flags_a_self_loop_as_a_clash() {
    let c = Closure::build(
        &[characteristic(
            "frona:parentOf",
            Characteristic::Irreflexive,
        )],
        &[entity("people/alice")],
        &[link("people/alice", "frona:parentOf", "people/alice")],
    );
    assert!(c.clashes_on("prp-irp"), "nothing is its own parent");
}

#[test]
fn irreflexive_leaves_an_edge_between_two_entities_alone() {
    let c = Closure::build(
        &[characteristic(
            "frona:parentOf",
            Characteristic::Irreflexive,
        )],
        &[entity("people/alice"), entity("people/bob")],
        &[link("people/alice", "frona:parentOf", "people/bob")],
    );
    assert!(
        !c.clashes_on("prp-irp"),
        "an edge between two distinct entities is fine"
    );
}

/// Functional is the identity rule we do ship: one subject cannot have two different
/// birthplaces, so the two entities it names must be the same place. That `owl:sameAs`
/// is what `resolve` picks up as a merge candidate.
#[test]
fn functional_identifies_two_values_of_one_subject() {
    let c = Closure::build(
        &[characteristic("frona:bornIn", Characteristic::Functional)],
        &[
            entity("people/sarah"),
            entity("places/berlin"),
            entity("places/berlin-de"),
        ],
        &[
            link("people/sarah", "frona:bornIn", "places/berlin"),
            link("people/sarah", "frona:bornIn", "places/berlin-de"),
        ],
    );
    assert!(
        c.holds("places/berlin", "owl:sameAs", "places/berlin-de"),
        "one subject's two values must be identified"
    );
}

/// The rule keys on a shared *subject*. Two people born in two places says nothing
/// about those places - identifying them would merge unrelated entities.
#[test]
fn functional_identifies_nothing_across_different_subjects() {
    let c = Closure::build(
        &[characteristic("frona:bornIn", Characteristic::Functional)],
        &[
            entity("people/sarah"),
            entity("people/tom"),
            entity("places/berlin"),
            entity("places/paris"),
        ],
        &[
            link("people/sarah", "frona:bornIn", "places/berlin"),
            link("people/tom", "frona:bornIn", "places/paris"),
        ],
    );
    assert!(
        !c.holds("places/berlin", "owl:sameAs", "places/paris"),
        "different subjects must identify nothing"
    );
    assert_eq!(
        c.same_as_pairs(),
        0,
        "and no identity at all should have been derived"
    );
}

/// The measurement behind leaving `owl:InverseFunctionalProperty` out of
/// [`Characteristic`].
///
/// Ten people, ten **distinct** employers, one hypothetical inverse-functional
/// declaration. Correct OWL 2 RL derives no identity whatsoever: `prp-ifp` requires
/// the two subjects to share an object, and here no two do. `reasonable` 0.4.4
/// derives every ordered pair.
///
/// This asserts the *sound* answer. It passes today because we never emit the
/// axiom - `Characteristic` has no such variant, so the property is inert. If
/// someone adds one, or a crate bump changes the rule, this test is where the
/// consequence surfaces instead of in a user's collapsed knowledge base.
///
/// Each characteristic is declared **on its own**, which is the claim being made:
/// no single thing the Classify stage can say about a property collapses entities that
/// share no value. Stacking several on one property is a different question and
/// genuinely can produce identity - `worksFor` declared both transitive and
/// symmetric derives `worksFor(p, p)`, giving every subject a second value for
/// `Functional` to identify. That is sound OWL, not a defect, and it is why the
/// loop below is per-characteristic rather than one combined declaration.
#[test]
fn no_single_characteristic_collapses_entities_that_share_nothing() {
    let people: Vec<KnowledgeEntity> = (0..10).map(|i| entity(&format!("people/p{i}"))).collect();
    let orgs: Vec<KnowledgeEntity> = (0..10).map(|i| entity(&format!("orgs/o{i}"))).collect();
    let links: Vec<KnowledgeEntityLink> = (0..10)
        .map(|i| {
            link(
                &format!("people/p{i}"),
                "frona:worksFor",
                &format!("orgs/o{i}"),
            )
        })
        .collect();
    let entities: Vec<KnowledgeEntity> = people.into_iter().chain(orgs).collect();

    for ch in [
        Characteristic::Functional,
        Characteristic::Transitive,
        Characteristic::Symmetric,
        Characteristic::Asymmetric,
        Characteristic::Irreflexive,
    ] {
        let c = Closure::build(&[characteristic("frona:worksFor", ch)], &entities, &links);
        assert_eq!(
            c.same_as_pairs(),
            0,
            "{ch:?} on ten people at ten distinct employers: they share no value, \
             so nothing is identifiable (an inverse-functional declaration here \
             yields 90 pairs under reasonable 0.4.4 — that is the excluded rule)"
        );
    }
}
