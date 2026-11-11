use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use frona_ontologies::rdf::{P_ALT_LABEL, P_LABEL, P_TYPE};
use oxrdf::{NamedNode, NamedOrBlankNode, Term, Triple};

use super::roots::Root;
use crate::ontology::OntologyCatalogue;
use crate::ontology::catalogue::loading::scan_file;
use crate::ontology::catalogue::search::{match_rank, normalize, squash};
use crate::ontology::individual_iri;
use crate::ontology::{reasoning, sparql};

const MINI: &str = "http://example.org/mini/";
const MINE: &str = "http://example.org/mine/";

fn ex(local: &str) -> String {
    format!("{MINI}{local}")
}
fn mine(local: &str) -> String {
    format!("{MINE}{local}")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ontology")
        .join(name)
}

fn catalogue() -> Arc<OntologyCatalogue> {
    let (share, data) = (fixture("share"), fixture("data"));
    OntologyCatalogue::load(&[(Root::Release, &share), (Root::User, &data)]).expect("loads")
}

fn typed(individual: &str, class_iri: &str) -> Triple {
    Triple::new(
        NamedOrBlankNode::NamedNode(NamedNode::new_unchecked(individual.to_string())),
        NamedNode::new_unchecked(P_TYPE.to_string()),
        Term::NamedNode(NamedNode::new_unchecked(class_iri.to_string())),
    )
}

#[test]
fn both_roots_absorb_into_one_graph_with_first_sight_attribution() {
    let c = catalogue();
    let names: Vec<(&str, Root)> = c
        .sources()
        .iter()
        .map(|s| (s.name.as_str(), s.root))
        .collect();
    assert_eq!(
        names,
        [("mini", Root::Release), ("user", Root::User)],
        "share scanned first"
    );
    assert!(c.declares(&ex("Person")), "a share term is present");
    assert!(c.declares(&mine("Contractor")), "a data term is present");
    assert_eq!(
        c.terms(),
        c.sources().iter().map(|s| s.terms).sum::<usize>()
    );
    // Identity comes off the `owl:Ontology` header, not the filename.
    assert_eq!(
        c.sources()
            .iter()
            .map(|s| s.iri.as_deref())
            .collect::<Vec<_>>(),
        [Some(MINI), Some(MINE)]
    );
}

/// Two files claiming to be the same ontology is a packaging mistake - a stale copy
/// in the user root, an artifact unpacked twice. Merging would double every axiom
/// while looking like it worked, so it fails instead.
///
/// Note this is identity, not overlap: sources are *expected* to mention each
/// other's terms, and the catalogue unions those axioms with no precedence.
#[test]
fn two_sources_claiming_the_same_identity_is_an_error() {
    let dup = fixture("duplicate");
    let Err(err) = OntologyCatalogue::load(&[(Root::Release, &dup)]) else {
        panic!("two files declaring one ontology IRI must not load");
    };
    let msg = format!("{err}");
    assert!(msg.contains("both identify as"), "got: {msg}");
    assert!(
        msg.contains("http://example.org/same/"),
        "names the IRI: {msg}"
    );
}

/// The fixtures deliberately have headers; a file without one is still legal and
/// simply cannot collide. Two such files load side by side.
#[test]
fn files_without_a_header_cannot_collide() {
    let tmp = tempfile::tempdir().unwrap();
    for (f, ns) in [("a.ttl", "a"), ("b.ttl", "b")] {
        std::fs::write(
            tmp.path().join(f),
            format!(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                     <http://example.org/{ns}/X> a owl:Class .\n"
            ),
        )
        .unwrap();
    }
    let c = OntologyCatalogue::load(&[(Root::Release, tmp.path())]).expect("both load");
    assert_eq!(c.sources().len(), 2);
    assert!(c.sources().iter().all(|s| s.iri.is_none()));
}

/// A missing user root is the normal state, not a failure - nobody has dropped a
/// file in yet.
#[test]
fn absent_root_is_not_an_error() {
    let (share, missing) = (fixture("share"), fixture("does-not-exist"));
    let c = OntologyCatalogue::load(&[(Root::Release, &share), (Root::User, &missing)]).unwrap();
    assert_eq!(c.sources().len(), 1);
}

#[test]
fn no_sources_at_all_is_an_error() {
    let empty = tempfile::tempdir().unwrap();
    assert!(
        OntologyCatalogue::load(&[(Root::Release, empty.path())]).is_err(),
        "the PKM backend needs a catalogue"
    );
}

/// The gate. `absorb` parks disjointness in scaffolding until
/// `decompose_disjointness` runs; skipping the call leaves the table empty, every
/// clash check passes, and nothing reports a problem. Assert the *number*, not
/// merely that checks pass: a transposed `rdf:first`/`rdf:rest` took this 666 → 20
/// upstream and no test noticed.
#[test]
fn disjointness_is_decomposed_and_counted() {
    let c = catalogue();
    assert_eq!(
        c.disjoint_pairs(),
        3,
        "Agent⊥Event (via a union list), Person⊥Organization (plain), \
             Contractor⊥Organization (a union list in the user root)"
    );
}

#[test]
fn ancestors_follow_subclass_and_equivalence() {
    let c = catalogue();
    let anc = c.ancestors(&mine("Freelancer"));
    // Reached only through `owl:equivalentClass` - a plain parent walk finds none
    // of these, because Freelancer has no `subClassOf` edge at all.
    assert!(
        anc.contains(&mine("Contractor")),
        "the equivalent peer: {anc:?}"
    );
    for want in ["Person", "Agent", "Thing"] {
        assert!(anc.contains(&ex(want)), "{want} in {anc:?}");
    }
    assert!(
        !anc.contains(&mine("Freelancer")),
        "a term is not its own ancestor"
    );
}

/// `cax-dw` fires on two type *chains*, so the axiom that separates two concrete
/// classes almost always sits well above both of them.
#[test]
fn clash_reports_the_axiom_above_both_types() {
    let c = catalogue();
    let found = c
        .clash(&[ex("Employee"), ex("Meeting")])
        .expect("Employee and Meeting clash");
    assert_eq!(
        found.via,
        (ex("Agent"), ex("Event")),
        "named by the axiom, not the types"
    );
    assert!(
        c.clash(&[ex("Employee"), ex("Person")]).is_none(),
        "a chain is not a clash"
    );
    assert!(
        c.clash(&[ex("Employee")]).is_none(),
        "one type cannot contradict itself"
    );
}

/// Vetting an edge by comparing only its two endpoints' ancestors is not enough:
/// the edge hands every *descendant* of `x` the ancestors of `y`, so the clash
/// surfaces a level below where a naive check looks.
#[test]
fn unsafe_edge_is_refused_a_level_down() {
    let c = catalogue();
    // Meeting ⊑ Person would put Employee's siblings under Agent *and* Event.
    assert!(
        !c.edge_is_safe(&ex("Meeting"), &ex("Person")),
        "contradicts Agent ⊥ Event"
    );
    assert!(
        c.edge_is_safe(&mine("Contractor"), &ex("Person")),
        "already holds, and is fine"
    );
}

#[test]
fn projection_cuts_ancestors_and_axiom_partners() {
    let c = catalogue();
    let cut = c.project(&[ex("Employee"), ex("Meeting")]);
    let mut got: Vec<&str> = cut
        .triples()
        .iter()
        .filter_map(|t| match &t.subject {
            NamedOrBlankNode::NamedNode(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    got.sort();
    got.dedup();
    assert_eq!(
        got,
        [
            ex("Agent"),
            ex("Employee"),
            ex("Event"),
            ex("Meeting"),
            ex("Organization"),
            ex("Person"),
            ex("Thing"),
        ]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>(),
        "seeds + ancestors + Organization, pulled in as Person's disjointness partner"
    );
    assert_eq!(cut.terms(), 7);
    assert_eq!(
        cut.sources(),
        ["mini"],
        "nothing in the user root is reachable from here"
    );
    assert!(
        !got.contains(&mine("Contractor").as_str()),
        "a descendant is not in scope — the cut closes upward"
    );
}

/// A pass over an unchanged vault re-derives identical seeds, so this has to be a
/// map lookup rather than a fresh walk and a fresh triple set.
#[test]
fn projections_are_memoized_on_the_seed_set() {
    let c = catalogue();
    let a = c.project(&[ex("Employee"), ex("Meeting")]);
    let b = c.project(&[ex("Meeting"), ex("Employee"), ex("Meeting")]);
    assert!(
        Arc::ptr_eq(&a, &b),
        "order and duplicates do not make a different cut"
    );
    assert!(
        !Arc::ptr_eq(&a, &c.project(&[ex("Employee")])),
        "a different seed set does"
    );
}

#[test]
fn seed_the_catalogue_does_not_declare_is_skipped_not_fatal() {
    let c = catalogue();
    let cut = c.project(&[ex("Employee"), "urn:frona:MintedYesterday".into()]);
    assert!(
        cut.terms() > 1,
        "the known seed still projects: {}",
        cut.terms()
    );
}

/// The projection reaching *into* the shared root from the user root is the case
/// that falls through code written for the within-vocabulary case.
#[test]
fn cut_seeded_in_the_user_root_spans_both() {
    let c = catalogue();
    let cut = c.project(&[mine("Freelancer")]);
    assert_eq!(cut.sources(), ["mini", "user"], "spans both roots");
}

/// The runtime path the walk-vs-reasoner contract does *not* cover: the server
/// reasons over a **cut**, and a cut missing something returns fewer types with
/// nothing reporting a problem.
#[test]
fn reasoning_over_a_cut_infers_every_ancestor() {
    let c = catalogue();
    let cut = c.project(&[ex("Employee")]);
    let bob = individual_iri("people/bob");
    let reasoned =
        reasoning::materialize(cut.triples(), &[], &[typed(&bob, &ex("Employee"))]).unwrap();

    let mut checked = 0;
    for want in ["Employee", "Person", "Agent", "Thing"] {
        let q = format!("ASK {{ <{bob}> a <{}> }}", ex(want));
        assert!(
            sparql::ask(&reasoned.store, &q, cut.prefixes()).unwrap(),
            "bob inferred {want}"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 4,
        "non-vacuous: an empty comparison would pass silently"
    );
    assert_eq!(
        reasoned.clashes().count(),
        0,
        "well-typed data has no clashes"
    );
}

/// The gate, end to end: every disjoint pair in a projected scope must actually
/// fire. A regression here means the gate is decorative.
#[test]
fn gate_fires_over_a_projected_cut() {
    let c = catalogue();
    let cut = c.project(&[ex("Employee"), ex("Meeting")]);
    let x = individual_iri("things/x");
    let reasoned = reasoning::materialize(
        cut.triples(),
        &[],
        &[typed(&x, &ex("Employee")), typed(&x, &ex("Meeting"))],
    )
    .unwrap();
    assert!(
        reasoned.clashes().any(|d| d.rule == "cax-dw"),
        "Employee + Meeting must fire cax-dw over the cut: {:?}",
        reasoned.diagnostics
    );
    // ...and the walk agrees with the reasoner about it, which is the whole claim.
    assert!(c.clash(&[ex("Employee"), ex("Meeting")]).is_some());
}

/// Labels ride along for display and SPARQL; `skos:prefLabel` and
/// `skos:definition` deliberately do not, because SKOS and KKO turn them into
/// ~21% of a materialisation that nobody queries.
#[test]
fn cut_carries_labels_but_not_the_skos_fan_out() {
    let c = catalogue();
    let cut = c.project(&[ex("Employee")]);
    let predicates: HashSet<&str> = cut.triples().iter().map(|t| t.predicate.as_str()).collect();
    assert!(predicates.contains(P_LABEL), "rdfs:label present");
    assert!(
        !predicates.contains(P_ALT_LABEL),
        "synonyms are a search surface, not a TBox"
    );
    assert!(
        !predicates.contains("http://www.w3.org/2004/02/skos/core#prefLabel"),
        "emitted as rdfs:label directly, so the prefLabel ⊑ label doubling never exists"
    );
}

/// The failure mode this guards is *thin answers*, not an error: the graph drops
/// the blank object, so the term silently ends up with no parent at all.
#[test]
fn anonymous_class_expression_in_a_user_file_is_refused() {
    let (share, anon) = (fixture("share"), fixture("anonymous"));
    let Err(err) = OntologyCatalogue::load(&[(Root::Release, &share), (Root::User, &anon)]) else {
        panic!("an ontology using class expressions must not load silently");
    };
    let msg = format!("{err}");
    assert!(msg.contains("anonymous class expressions"), "got: {msg}");
    assert!(
        msg.contains("http://example.org/anon/Supervisor"),
        "names the term: {msg}"
    );
}

/// The carve-out that matters: `X disjointWith [ owl:unionOf (…) ]` is how the
/// shipped catalogue states every one of its axioms. A check without this rejects
/// KBpedia itself.
#[test]
fn union_list_disjointness_is_not_an_anonymous_class_expression() {
    let path = fixture("data").join("user.ttl");
    let bytes = std::fs::read(&path).unwrap();
    let found = scan_file(&bytes, &path).unwrap().anonymous;
    assert!(
        found.is_empty(),
        "the user file's union-list disjointness is legal: {found:?}"
    );
}

#[test]
fn search_finds_terms_across_both_roots() {
    let c = catalogue();
    let hits = c.search("contractor", 10);
    assert!(
        hits.iter()
            .any(|h| h.curie == mine("Contractor") && h.kind == "class"),
        "a user-root term is discoverable: {hits:?}"
    );
    assert!(
        c.search("organization", 10)
            .iter()
            .any(|h| h.curie == ex("Organization")),
        "and a share-root one"
    );
    assert!(
        c.search("works", 10).iter().any(|h| h.kind == "property"),
        "properties are searchable too"
    );
}

/// A synonym match is real ("staffer" → Employee) but must never displace an exact
/// hit on a term's own name.
#[test]
fn synonym_matches_but_ranks_below_the_name() {
    let c = catalogue();
    let hits = c.search("staffer", 10);
    assert_eq!(
        hits.first().map(|h| h.curie.as_str()),
        Some(ex("Employee").as_str())
    );
}

/// Names as published are not comparable words: vocabularies disambiguate
/// colliding labels with a trailing Wikidata id, which has to come off before an
/// exact match can register.
#[test]
fn names_normalize_to_comparable_words() {
    assert_eq!(
        normalize("Database_Interface_Q1172367"),
        "database interface"
    );
    assert_eq!(
        normalize("Database_management_system"),
        "database management system"
    );
    // `_Q` without a numeric tail is part of the name, not a disambiguator.
    assert_eq!(normalize("Sample_Queue"), "sample queue");
    // Encoded punctuation degrades to its own token; the real words still match.
    assert_eq!(
        normalize("Interface__u0028_computing_u0029_"),
        "interface u0028 computing u0029"
    );
    assert_eq!(
        rank("interface", &normalize("Interface__u0028_computing_u0029_")),
        Some(1)
    );
}

fn rank(needle: &str, candidate: &str) -> Option<u8> {
    match_rank(needle, &squash(needle), candidate)
}

/// The ordering that makes the tool survive a large vocabulary: "database"
/// substring-matches thousands of KBpedia classes, so an exact hit must outrank
/// `abstract database` or the result cap discards the term worth reusing.
#[test]
fn exact_matches_outrank_substring_matches() {
    assert_eq!(rank("database", "database"), Some(0));
    assert_eq!(rank("database", "database management system"), Some(1));
    assert_eq!(rank("database", "abstract database"), Some(2));
    assert_eq!(rank("database", "nodatabasehere"), Some(3));
    assert_eq!(rank("database", "unrelated concept"), None);
}

/// A camelCase query reaches a spaced term.
///
/// The bug this pins cost a real run: `ontology_term_search("ProgrammingLanguage")`
/// returned neither `kbpedia:ProgrammingLanguage` nor `schema:ComputerLanguage`'s
/// neighbours, because a term's name is decameled into words before comparison while the
/// query never was. The same catalogue answered `"programming language"` at rank 0. Two
/// entities of the same kind ended up with two different classes from that alone.
#[test]
fn camel_case_query_reaches_a_spaced_term() {
    assert_eq!(rank("programminglanguage", "programming language"), Some(0));
    assert_eq!(rank("computerlanguage", "computer language"), Some(0));
    assert_eq!(rank("programming", "programming language"), Some(1));
    assert_eq!(rank("programminglang", "programming language"), Some(1));
    assert_eq!(
        rank("programming language", "programming language"),
        Some(0)
    );
}

/// An acronym must not be shredded. The obvious fix - decamel the query - turns `PHP`
/// into `p h p`, which matches nothing; squashing both sides instead leaves it alone.
#[test]
fn acronym_query_still_matches() {
    assert_eq!(rank("php", "php"), Some(0));
    assert_eq!(rank("php", "php programming language"), Some(1));
    assert_eq!(rank("dbms", "dbms"), Some(0));
    assert_eq!(rank("php", "unrelated concept"), None);
    assert_eq!(rank("programminglanguage", "programming paradigm"), None);
}

/// The numbers the design is costed against, asserted against the real artifacts.
/// Opt-in: point `ONTOLOGY_DIST_DIR` at a built `frona-ontologies` release. The
/// fixtures above cover the logic; this covers the *scale*, which is where the
/// silent failures were found.
#[test]
fn shipped_catalogue_loads_with_its_axioms_intact() {
    let Ok(dir) = std::env::var("ONTOLOGY_DIST_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let t = std::time::Instant::now();
    let c = OntologyCatalogue::load(&[(Root::Release, &dir)]).expect("release loads");
    let load_ms = t.elapsed().as_millis();

    println!("catalogue load {load_ms} ms");

    assert_eq!(c.sources().len(), 5);
    assert_eq!(c.terms(), 30_445, "declared terms across the release");
    assert_eq!(
        c.disjoint_pairs(),
        654,
        "KKO's 646 plus schema.org 1, FOAF 4, SKOS 3"
    );

    // A projection is ~0.5% of the catalogue and spans the vocabularies the
    // alignment tables connect.
    let cut = c.project(&["http://kbpedia.org/kko/rc/Doctor-Medical".into()]);
    assert_eq!(cut.terms(), 161, "the cut a single seed opens");
    assert_eq!(
        cut.sources(),
        ["dublincore", "foaf", "kbpedia", "schema-org"]
    );
    assert_eq!(
        cut.triples().len(),
        1_014,
        "the cut lowers to axioms + labels; re-reading the artifacts for the same \
             scope yields 1,430, the difference being the prefLabel/definition fan-out \
             this deliberately does not emit"
    );

    // Seeding at the other end of the alignment reaches the same scope - which is
    // what "the vault uses schema.org and KBpedia together" actually requires.
    let other = c.project(&["https://schema.org/Physician".into()]);
    assert!(
        other.sources().len() > 1,
        "cross-vocabulary: {:?}",
        other.sources()
    );

    // Schema.org explicitly allows Organization → location → Place. Its
    // domainIncludes/rangeIncludes metadata must stay advisory through artifact
    // generation, catalogue loading, projection, alignments, and reasoning.
    let location = "https://schema.org/location";
    let organization = "https://schema.org/Organization";
    let place = "https://schema.org/Place";
    let location_cut = c.project(&[location.into(), organization.into(), place.into()]);
    let subject = individual_iri("organizations/acme");
    let object = individual_iri("places/office");
    let abox = [
        typed(&subject, organization),
        typed(&object, place),
        Triple::new(
            NamedOrBlankNode::NamedNode(NamedNode::new_unchecked(subject)),
            NamedNode::new_unchecked(location),
            Term::NamedNode(NamedNode::new_unchecked(object)),
        ),
    ];
    let reasoned = reasoning::materialize(location_cut.triples(), &[], &abox).unwrap();
    assert_eq!(
        reasoned.clashes().count(),
        0,
        "an allowed Schema.org location assertion must not become conjunctive"
    );
}
