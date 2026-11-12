#[tokio::test]
async fn proposed_layer_composes_into_the_closure_without_persisting() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/pg", "frona:Database").await;
    let before = mgr.load("u").await.unwrap().version();

    let proposed = [SchemaEdit::SubClassOf {
        sub: "frona:Database".into(),
        sup: "schema:SoftwareApplication".into(),
    }];
    let pass = mgr
        .portable("u")
        .await
        .unwrap()
        .reason_user_with_proposed("u", &proposed, &[])
        .await
        .unwrap();
    assert!(
        sparql::ask(
            &pass.reasoned.store,
            "ASK { frona:Database rdfs:subClassOf schema:SoftwareApplication }",
            pass.effective_ontology.prefixes(),
        )
        .unwrap(),
        "the proposed proposal is composed into the closure"
    );

    // Nothing was persisted: version unchanged, and the committed graph does not
    // carry the proposal.
    assert_eq!(
        mgr.load("u").await.unwrap().version(),
        before,
        "proposed layer not persisted"
    );
    let plain = mgr.reason_user("u").await.unwrap();
    assert!(
        !sparql::ask(
            &plain.reasoned.store,
            "ASK { frona:Database rdfs:subClassOf schema:SoftwareApplication }",
            plain.effective_ontology.prefixes(),
        )
        .unwrap(),
        "the committed graph is unchanged by the proposed layer"
    );
}

/// The inverted invariant. Search runs over the catalogue, so a term the user's
/// vault has never referenced - and which is therefore *not* in their scope - is
/// still offered. That is the point: finding it is what brings it into scope.
#[tokio::test]
async fn vocab_search_offers_terms_the_user_scope_does_not_yet_hold() {
    let (mgr, _repo) = manager().await;
    let effective = mgr
        .portable("u")
        .await
        .unwrap()
        .user_effective_ontology("u")
        .await
        .unwrap();
    assert!(effective.is_empty(), "an empty vault reasons over nothing");

    let hits = mgr.catalogue().unwrap().search("organization", 25);
    assert!(
        hits.iter()
            .any(|h| h.curie == "schema:Organization" && h.kind == "class"),
        "offered anyway, because the catalogue carries it: {hits:?}"
    );
    assert!(
        mgr.catalogue().unwrap().search("zyzzyx", 25).is_empty(),
        "a term no source declares is still not offered"
    );
}

#[tokio::test]
async fn inverse_property_writes_idempotent_inferred_link() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::InverseProperties {
            a: "frona:worksFor".into(),
            b: "frona:employs".into(),
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "people/sarah", "schema:Person").await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    seed_asserted_entity_link(&repo, "u", "people/sarah", "orgs/acme", "frona:worksFor")
        .await
        .unwrap();

    mgr.materialize("u").await.unwrap();
    let acme_links = repo.links_from_entity("u", "orgs/acme").await.unwrap();
    let inferred: Vec<_> = acme_links
        .iter()
        .filter(|l| l.origin == LinkOrigin::Inferred && l.to_entity_path == "people/sarah")
        .collect();
    assert_eq!(
        inferred.len(),
        1,
        "one inferred employs edge: {acme_links:?}"
    );

    mgr.materialize("u").await.unwrap();
    let acme_links = repo.links_from_entity("u", "orgs/acme").await.unwrap();
    assert_eq!(
        acme_links
            .iter()
            .filter(|l| l.origin == LinkOrigin::Inferred && l.to_entity_path == "people/sarah")
            .count(),
        1,
        "idempotent after a second pass"
    );
    let sarah_links = repo.links_from_entity("u", "people/sarah").await.unwrap();
    assert!(
        sarah_links
            .iter()
            .any(|l| l.origin == LinkOrigin::Asserted && l.to_entity_path == "orgs/acme"),
        "asserted edge survives"
    );
}

/// A transitive property's derived shortcut reaches the entity graph: the whole
/// point of declaring one is that a query for "everything in Germany" finds the
/// district nobody linked to it directly.
#[tokio::test]
async fn transitive_property_writes_back_the_derived_shortcut() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::PropertyCharacteristic {
            property: "frona:partOf".into(),
            characteristic: Characteristic::Transitive,
        }],
    )
    .await
    .unwrap();
    for p in ["places/kreuzberg", "places/berlin", "places/germany"] {
        seed_concept(&repo, "u", p, "schema:Place").await;
    }
    seed_asserted_entity_link(
        &repo,
        "u",
        "places/kreuzberg",
        "places/berlin",
        "frona:partOf",
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &repo,
        "u",
        "places/berlin",
        "places/germany",
        "frona:partOf",
    )
    .await
    .unwrap();

    mgr.materialize("u").await.unwrap();

    let links = repo
        .links_from_entity("u", "places/kreuzberg")
        .await
        .unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.origin == LinkOrigin::Inferred && l.to_entity_path == "places/germany"),
        "the a->c shortcut is written as inferred: {links:?}"
    );
}

/// Retracting a characteristic has to stop it *deriving*, not merely stop asserting
/// it - and the edges it already wrote have to go.
///
/// This is the recovery path for the worst kind of wrong axiom. A `transitive` claim
/// passes the gate almost unconditionally (adding edges rarely violates anything the
/// gate can see) and then fabricates edges on every reasoning pass. Without a way back
/// the false edges accumulate for the life of the knowledge base.
#[tokio::test]
async fn retracting_a_transitive_claim_stops_deriving_and_removes_what_it_derived() {
    let (mgr, repo) = manager().await;
    let transitive = SchemaEdit::PropertyCharacteristic {
        property: "frona:partOf".into(),
        characteristic: Characteristic::Transitive,
    };
    mgr.commit("u", &[transitive]).await.unwrap();
    for p in ["places/kreuzberg", "places/berlin", "places/germany"] {
        seed_concept(&repo, "u", p, "schema:Place").await;
    }
    seed_asserted_entity_link(
        &repo,
        "u",
        "places/kreuzberg",
        "places/berlin",
        "frona:partOf",
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &repo,
        "u",
        "places/berlin",
        "places/germany",
        "frona:partOf",
    )
    .await
    .unwrap();

    mgr.materialize("u").await.unwrap();
    let derived = |links: &[crate::memory::pkm::model::KnowledgeEntityLink]| {
        links
            .iter()
            .any(|l| l.origin == LinkOrigin::Inferred && l.to_entity_path == "places/germany")
    };
    assert!(
        derived(
            &repo
                .links_from_entity("u", "places/kreuzberg")
                .await
                .unwrap()
        ),
        "the claim is deriving while it is in force"
    );

    let targets = mgr
        .portable("u")
        .await
        .unwrap()
        .retractable("u")
        .await
        .unwrap();
    let target = OverrideTarget::Characteristic {
        property: "frona:partOf".into(),
        characteristic: Characteristic::Transitive,
    };
    assert!(
        targets.contains(&target),
        "adjudicate can see it: {targets:?}"
    );
    mgr.commit("u", &[SchemaEdit::AmendOverride { target }])
        .await
        .unwrap();

    mgr.materialize("u").await.unwrap();
    assert!(
        !derived(
            &repo
                .links_from_entity("u", "places/kreuzberg")
                .await
                .unwrap()
        ),
        "the derived shortcut is gone — inferred links are rewritten every pass, so \
             retracting the axiom is what un-writes them"
    );
    // The asserted chain is untouched: loosening a claim about the property is not a
    // statement about the edges anyone actually stated.
    let asserted = repo
        .links_from_entity("u", "places/kreuzberg")
        .await
        .unwrap();
    assert!(
        asserted
            .iter()
            .any(|l| l.origin == LinkOrigin::Asserted && l.to_entity_path == "places/berlin"),
        "{asserted:?}"
    );
    assert!(
        mgr.portable("u")
            .await
            .unwrap()
            .retractable("u")
            .await
            .unwrap()
            .is_empty(),
        "nothing left in force"
    );
}

/// The `eq-rep` mirror filter, at the seam it actually matters: a `functional`
/// property identifies two entities, OWL RL then copies each one's edges onto the
/// other, and none of those copies may reach the entity graph.
///
/// Sarah can only have been born once, so `berlin` and `berlin-de` are the same
/// place. `berlin` has an asserted `partOf germany`; without the filter that edge
/// reappears on `berlin-de` (and `sarah bornIn berlin` reappears pointing at the
/// twin) as inferred links that assert nothing new.
#[tokio::test]
async fn identified_entities_do_not_mirror_each_others_edges_into_the_graph() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::PropertyCharacteristic {
            property: "frona:bornIn".into(),
            characteristic: Characteristic::Functional,
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "people/sarah", "schema:Person").await;
    for p in ["places/berlin", "places/berlin-de", "places/germany"] {
        seed_concept(&repo, "u", p, "schema:Place").await;
    }
    seed_asserted_entity_link(&repo, "u", "people/sarah", "places/berlin", "frona:bornIn")
        .await
        .unwrap();
    seed_asserted_entity_link(
        &repo,
        "u",
        "people/sarah",
        "places/berlin-de",
        "frona:bornIn",
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &repo,
        "u",
        "places/berlin",
        "places/germany",
        "frona:partOf",
    )
    .await
    .unwrap();

    mgr.materialize("u").await.unwrap();

    // `owl:sameAs` is identity, not a navigable edge - it never becomes a link.
    for entity in ["places/berlin", "places/berlin-de"] {
        let links = repo.links_from_entity("u", entity).await.unwrap();
        assert!(
            !links.iter().any(|l| l.relation.contains("sameAs")),
            "sameAs is not an entity edge, on {entity}: {links:?}"
        );
    }
    let twin = repo
        .links_from_entity("u", "places/berlin-de")
        .await
        .unwrap();
    assert!(
        !twin
            .iter()
            .any(|l| l.origin == LinkOrigin::Inferred && l.to_entity_path == "places/germany"),
        "berlin's partOf must not be mirrored onto its twin: {twin:?}"
    );
    let berlin = repo.links_from_entity("u", "places/berlin").await.unwrap();
    assert!(
        berlin
            .iter()
            .any(|l| l.origin == LinkOrigin::Asserted && l.to_entity_path == "places/germany"),
        "the real edge survives: {berlin:?}"
    );
}
use super::*;
