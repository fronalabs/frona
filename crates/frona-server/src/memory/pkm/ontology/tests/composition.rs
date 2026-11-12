/// The cut is written on first load and reused verbatim afterwards. Reused means
/// *identical* - one that silently differed run to run would make every
/// classification non-reproducible.
#[tokio::test]
async fn projection_is_stored_on_first_load_and_reused() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;

    let first = mgr.load("u").await.unwrap();
    assert!(
        !first.effective_ontology().is_empty(),
        "a typed entity seeds a cut"
    );

    let row = repo
        .ontology_get("u")
        .await
        .unwrap()
        .expect("the cut was persisted");
    assert!(!row.effective_ontology.is_empty(), "N-Triples stored");
    assert!(row.seeds.contains(&"https://schema.org/Person".to_string()));
    assert_eq!(row.sources, ["schema-core"], "the source it spans");
    assert!(!row.catalog_fingerprint.is_empty());

    let second = mgr.load("u").await.unwrap();
    assert_eq!(
        second.effective_ontology().triples().len(),
        first.effective_ontology().triples().len(),
        "the stored cut is reused, not re-derived differently"
    );
}

/// The gate: a pass reasons from the stored triples **without a catalogue at all**.
/// This is what makes the stored projection authoritative rather than a cache.
#[tokio::test]
async fn stored_projection_reasons_without_the_catalogue() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    let stored = mgr.load("u").await.unwrap();
    let triples = stored.effective_ontology().triples().len();
    assert!(triples > 0);

    // Rebuild it the way `load` would from the row alone - no catalogue is
    // consulted, and the result still carries the ancestors reasoning needs.
    let row = repo.ontology_get("u").await.unwrap().unwrap();
    let rebuilt = OntologyScope::from_ntriples(
        &row.effective_ontology,
        row.seeds.clone(),
        row.sources.clone(),
        PrefixMap::standard(),
    )
    .unwrap();
    assert_eq!(rebuilt.triples().len(), triples);

    let bob = individual_iri("people/bob");
    let reasoned = frona_pkm::ontology::materialize(
        rebuilt.triples(),
        &[],
        &[type_triple(&bob, "https://schema.org/Person")],
    )
    .unwrap();
    let q = format!("ASK {{ <{bob}> a <https://schema.org/Thing> }}");
    assert!(
        sparql::ask(&reasoned.store, &q, rebuilt.prefixes()).unwrap(),
        "ancestors survive the round-trip"
    );
}

/// A vault that starts using a new term re-cuts; one that does not, does not.
#[tokio::test]
async fn changed_seed_set_re_cuts_and_an_unchanged_one_does_not() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    mgr.load("u").await.unwrap();
    let before = repo.ontology_get("u").await.unwrap().unwrap();

    mgr.load("u").await.unwrap();
    let same = repo.ontology_get("u").await.unwrap().unwrap();
    assert_eq!(
        same.updated_at, before.updated_at,
        "no rewrite when nothing moved"
    );

    // An entity under a term the vault had not used → the seed set grows, and the row
    // is rewritten. The *triples* need not grow: `Organization` was already in the
    // cut as `Person`'s disjointness partner, which is exactly the axiom-partner
    // step doing its job - the term an entity might clash with is in scope before any
    // entity uses it.
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    mgr.load("u").await.unwrap();
    let after = repo.ontology_get("u").await.unwrap().unwrap();
    assert!(
        !before
            .seeds
            .contains(&"https://schema.org/Organization".to_string()),
        "not a seed before"
    );
    assert!(
        after
            .seeds
            .contains(&"https://schema.org/Organization".to_string()),
        "is a seed after: {:?}",
        after.seeds
    );
    assert_ne!(after.updated_at, before.updated_at, "the row was re-cut");
}

/// The stored cut is the **effective ontology**, so a term nothing references any
/// more leaves it. Keeping it would make the row grow without bound and stop
/// describing what the knowledge base actually reasons over.
#[tokio::test]
async fn deleting_the_last_entity_using_a_term_drops_it_from_the_cut() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    // `SoftwareApplication` takes part in no disjointness, so nothing else in the
    // cut pulls it in as an axiom partner - it is in scope only because an entity uses
    // it. Picking a term that *is* a partner would prove nothing: it would stay
    // regardless, which is the axiom-partner step working as intended.
    seed_concept(&repo, "u", "services/pg", "schema:SoftwareApplication").await;
    mgr.load("u").await.unwrap();
    let before = repo.ontology_get("u").await.unwrap().unwrap();
    assert!(
        before
            .effective_ontology
            .contains("schema.org/SoftwareApplication"),
        "in scope while an entity uses it"
    );

    repo.delete_entity("u", "services/pg").await.unwrap();
    mgr.load("u").await.unwrap();
    let after = repo.ontology_get("u").await.unwrap().unwrap();

    assert!(
        !after
            .seeds
            .contains(&"https://schema.org/SoftwareApplication".to_string()),
        "no longer referenced, so no longer a seed"
    );
    assert!(
        !after
            .effective_ontology
            .contains("schema.org/SoftwareApplication"),
        "and it left the cut rather than accumulating there"
    );
    assert!(
        after.effective_ontology.contains("schema.org/Person"),
        "the entity that remains keeps its type"
    );
}

/// The same pruning, for a user who has minted something - which is every user the
/// Classify has ever run for.
///
/// This is the case the two tests either side of it both miss: the pruning test
/// above uses only standard terms, and the carry-forward test below strands one by
/// deleting a vocabulary. Neither has a `frona:` delta, so neither ever reaches the
/// branch that decides between a fresh cut and a carried-forward one.
///
/// A mint drags the RDF vocabulary its own axiom is written in into the seed set
/// (`frona:Foo rdf:type owl:Class` seeds `rdf:type` and `owl:Class`). Those are not
/// stranded - nothing lost them - but if they are treated as such they seed the
/// carry-forward walk, and since adjacency is undirected and `owl:Class` is the
/// object of every term's type triple, the walk reaches the whole previous cut and
/// pruning stops happening at all. Silently, and permanently.
#[tokio::test]
async fn minted_term_does_not_stop_the_cut_from_pruning() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:AIAssistant".into(),
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    seed_concept(&repo, "u", "services/pg", "schema:SoftwareApplication").await;
    mgr.load("u").await.unwrap();
    let before = repo.ontology_get("u").await.unwrap().unwrap();
    assert!(
        before
            .effective_ontology
            .contains("schema.org/SoftwareApplication"),
        "in scope while an entity uses it"
    );

    repo.delete_entity("u", "services/pg").await.unwrap();
    mgr.load("u").await.unwrap();
    let after = repo.ontology_get("u").await.unwrap().unwrap();

    assert!(
        !after
            .effective_ontology
            .contains("schema.org/SoftwareApplication"),
        "the delta must not pin a term the vault stopped using"
    );
    assert!(
        after.effective_ontology.contains("schema.org/Person"),
        "the entity that remains keeps its type"
    );
    // Nor may the cut start describing the RDF vocabulary the delta is *written in*.
    // Subject position is the test: every cut names `owl:Class` as the object of a
    // term's `rdf:type`, but a cut holding axioms *about* `owl:Class` has mistaken
    // the language for the subject matter.
    assert!(
        !after
            .effective_ontology
            .lines()
            .any(|l| l.starts_with("<http://www.w3.org/2002/07/owl#Class>")
                || l.starts_with("<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>")),
        "the cut describes domain terms, not the language the delta is written in"
    );
}

/// The one case that *is* carried forward. A source leaving the catalogue must not
/// untype entities classified under it - the entity did not change, and a packaging
/// change must not become a data change.
#[tokio::test]
async fn removing_a_source_leaves_existing_entities_reasoning() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    std::fs::copy(
        fixtures.join("standard/schema-core.ttl"),
        tmp.path().join("schema-core.ttl"),
    )
    .unwrap();
    // A second, unrelated vocabulary, so removing the first still leaves a loadable
    // catalogue. Without it `install_catalogue` would fail with "no sources found",
    // the old catalogue would stay in place, and this test would pass while proving
    // nothing.
    std::fs::copy(fixtures.join("share/mini.ttl"), tmp.path().join("mini.ttl")).unwrap();

    let (mgr, repo) = manager_over(tmp.path()).await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    let before = mgr.load("u").await.unwrap();
    let before_triples = before.effective_ontology().triples().len();
    assert!(before_triples > 0);

    std::fs::remove_file(tmp.path().join("schema-core.ttl")).unwrap();
    mgr.install_catalogue()
        .expect("the catalogue still loads without it");
    assert!(
        !mgr.catalogue()
            .unwrap()
            .declares("https://schema.org/Person"),
        "the term really is gone from the catalogue"
    );

    let after = mgr.load("u").await.unwrap();
    assert_eq!(
        after.effective_ontology().triples().len(),
        before_triples,
        "the departed source's triples are kept, not dropped"
    );
    let bob = individual_iri("people/bob");
    let reasoned = frona_pkm::ontology::materialize(
        after.effective_ontology().triples(),
        &[],
        &[type_triple(&bob, "https://schema.org/Person")],
    )
    .unwrap();
    let q = format!("ASK {{ <{bob}> a <https://schema.org/Thing> }}");
    assert!(
        sparql::ask(&reasoned.store, &q, after.prefixes()).unwrap(),
        "bob is still typed"
    );

    // And the gate still fires. Disjointness is the easiest half to lose here -
    // symmetric axioms are stated on one end only, so a carry-forward that walked
    // in one direction would keep the taxonomy and quietly drop every ⊥, leaving a
    // scope that looks intact and cannot contradict anything.
    let clash = frona_pkm::ontology::materialize(
        after.effective_ontology().triples(),
        &[],
        &[
            type_triple(&bob, "https://schema.org/Person"),
            type_triple(&bob, "https://schema.org/Organization"),
        ],
    )
    .unwrap();
    assert!(
        clash.clashes().any(|d| d.rule == "cax-dw"),
        "Person ⊥ Organization survived the source going away: {:?}",
        clash.diagnostics
    );
}

/// A different catalogue is a different cut, even when the vault has not moved.
/// This is how an image upgrade reaches users who changed nothing.
#[tokio::test]
async fn changed_catalogue_fingerprint_re_cuts() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    mgr.load("u").await.unwrap();

    let row = repo.ontology_get("u").await.unwrap().unwrap();
    repo.ontology_set_effective(
        "u",
        row.version,
        &row.effective_ontology,
        &row.seeds,
        &row.sources,
        "a-different-catalogue",
    )
    .await
    .unwrap();

    mgr.load("u").await.unwrap();
    let after = repo.ontology_get("u").await.unwrap().unwrap();
    assert_ne!(
        after.catalog_fingerprint, "a-different-catalogue",
        "re-cut and restamped"
    );
}

/// Stamp one entity type the way the consolidation stage does: plan against the
/// current delta, then write through the transactional commit. Returns whether the
/// entity ends up carrying the class.
///
/// Deliberately not a shortcut past the planner - the production path has no
/// "add one type" call any more, so a test that used one would be pinning a rule
/// nothing enforces.
pub(super) async fn stamp(mgr: &OntologyManager, repo: &PkmRepo, path: &str, class: &str) -> bool {
    let planned = mgr
        .portable("u")
        .await
        .unwrap()
        .plan_schema("u", &[])
        .await
        .unwrap();
    let entity = repo.entity_by_path("u", path).await.unwrap().unwrap();
    match frona_pkm::ontology::plan_entity_type(
        &mgr.catalogue().unwrap(),
        &entity.kinds,
        class,
        &planned.triples,
    ) {
        TypePlan::Write(kinds) => {
            repo.commit_schema_and_types(
                "u",
                &planned.owl,
                DELTA_FORMAT,
                planned.version,
                &[(path.to_string(), kinds)],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
            )
            .await
            .unwrap();
            true
        }
        TypePlan::AlreadyHeld => true,
        TypePlan::Refused => false,
    }
}

/// Seeds an entity typed with `kind` as a CURIE - expanded here, because the store
/// holds IRIs.
pub(super) async fn seed_concept(repo: &PkmRepo, user: &str, path: &str, kind: &str) {
    let kinds = [PrefixMap::standard().expand(kind)];
    repo.upsert_entity_skeleton(user, path, EntityCategory::Concept, &kinds, path, "", &[])
        .await
        .unwrap();
}
use super::*;
