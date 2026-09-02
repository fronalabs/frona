/// Regression. `ontology_terms` seeds the effective ontology, so it runs on every
/// ontology load - and it has to survive the rows that actually exist on disk, not
/// just the ones this code writes today.
///
/// `object::keys` is a hard error on anything that is not an object, so a single
/// entity whose `attributes` is absent or null failed the entire seed set and took
/// the stored ontology down with it. `upsert_entity_skeleton` always writes `{}`, so
/// **no test going through the normal path can reproduce this** - the rows that
/// break it predate the field. Hence the raw inserts below: they are the shape
/// production had, and the reason this reached production at all.
#[tokio::test]
async fn ontology_terms_survive_entities_whose_attributes_are_absent_or_null() {
    let r = repo().await;
    let person = "https://schema.org/Person".to_string();

    // No `attributes` key at all - a row written before the field existed.
    r.db.query(
        "CREATE type::record('knowledge_entity', 'old') SET
                     user_id = 'u', path = 'people/bob', kinds = $kinds",
    )
    .bind(("kinds", vec![person.clone()]))
    .await
    .unwrap()
    .check()
    .unwrap();
    // Explicitly null.
    r.db.query(
        "CREATE type::record('knowledge_entity', 'nulled') SET
                 user_id = 'u', path = 'how/restart', kinds = [], attributes = NULL",
    )
    .await
    .unwrap()
    .check()
    .unwrap();
    // And a well-formed one, so the query is not trivially empty.
    r.upsert_entity_skeleton("u", "svc/pg", EntityCategory::Concept, &[], "PG", "", &[])
        .await
        .unwrap();
    seed_reconciled_entity(
        &r,
        "u",
        "svc/pg",
        "",
        "PG",
        &serde_json::json!({"schema:email": "b@x"}),
    )
    .await
    .unwrap();

    let terms = r
        .ontology_terms("u")
        .await
        .expect("a malformed row is not an error");
    assert!(
        terms.contains(&person),
        "the typed entity still contributes its class: {terms:?}"
    );
    assert!(
        terms.contains(&"schema:email".to_string()),
        "and a well-formed entity still contributes its attribute keys: {terms:?}"
    );
    assert!(
        terms.contains(&"https://schema.org/name".to_string())
            && terms.contains(&"https://schema.org/identifier".to_string()),
        "concepts contribute their built-in ABox metadata predicates: {terms:?}"
    );
}

/// The invariant parallel ingest depends on: many chats naming the same entity
/// upsert the same path at once, and the result is exactly **one** row, no errors,
/// aliases merged.
///
/// Note what this does *not* cover. `upsert_entity_skeleton` is a read-then-insert
/// against a UNIQUE `(user_id, path)`, so it carries a recovery branch for losing
/// that race. Removing that branch does not fail this test - 32 racers on 8 threads
/// never collide, because the embedded engine serializes the statements. The branch
/// is therefore defensive: correct by inspection, unexercised here, and worth
/// keeping for a networked backend that would not serialize them.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_upserts_of_one_path_converge_on_a_single_entity() {
    let r = std::sync::Arc::new(repo().await);
    let racers: Vec<_> = (0..32)
        .map(|i| {
            let r = r.clone();
            tokio::spawn(async move {
                r.upsert_entity_skeleton(
                    "u",
                    "services/postgres",
                    EntityCategory::Concept,
                    &["urn:frona:Service".to_string()],
                    "Postgres",
                    "the db",
                    &[format!("alias{i}")],
                )
                .await
            })
        })
        .collect();
    for t in racers {
        t.await
            .unwrap()
            .expect("a lost insert race must recover, not error");
    }

    let mut q =
        r.db.query(
            "SELECT count() FROM knowledge_entity
                 WHERE user_id = 'u' AND path = 'services/postgres' GROUP ALL",
        )
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = q.take(0).unwrap();
    let n = rows
        .first()
        .and_then(|v| v.get("count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(n, 1, "one row for the path, not a duplicate per racer");

    let entity = r
        .entity_by_path("u", "services/postgres")
        .await
        .unwrap()
        .expect("exactly one entity exists");
    assert_eq!(entity.name, "Postgres");
    assert!(
        !entity.aliases.is_empty(),
        "the recovery path merges rather than discarding: {:?}",
        entity.aliases
    );
}

/// Single-writer fields. The extractor is instance-blind, so it re-emits every
/// entity it sees and a returning entity always takes the update branch. That branch
/// must not touch what a later stage owns: the Classify's `kinds`, reconcile's
/// `name`/`description`, or the `category` fixed at creation.
///
/// Before this, the update wrote all four unconditionally - and extract passes
/// `kinds = &[]`, so every re-mention silently reset the entity to untyped, forcing a
/// full re-classify and narrowing any multi-class set to whatever the last mention
/// implied.
#[tokio::test]
async fn re_mention_keeps_everything_a_later_stage_owns() {
    let r = repo().await;
    let person = "https://schema.org/Person".to_string();
    let employee = "https://schema.org/Employee".to_string();

    // Pass 1: extract mints the entity, the Classify stage types it (twice - an entity is
    // genuinely several things), reconcile writes a curated description.
    r.upsert_entity_skeleton(
        "u",
        "people/sarah",
        EntityCategory::Concept,
        &[person.clone(), employee.clone()],
        "Sarah",
        "Engineer at Acme.",
        &[],
    )
    .await
    .unwrap();

    // Pass 2: the same entity comes up again. The extractor knows nothing about the
    // entity that exists, so it proposes a bare characterization and no type at all.
    r.upsert_entity_skeleton(
        "u",
        "people/sarah",
        EntityCategory::Concept,
        &[],
        "sarah",
        "someone mentioned in passing",
        &["S".to_string()],
    )
    .await
    .unwrap();

    let entity = r
        .entity_by_path("u", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entity.kinds,
        [person, employee],
        "both classes survive an untyped mention"
    );
    assert_eq!(
        entity.description, "Engineer at Acme.",
        "reconcile's description is not clobbered"
    );
    assert_eq!(
        entity.name, "Sarah",
        "the name is not rewritten by a passing mention"
    );
    assert!(
        entity.aliases.contains("S"),
        "but new aliases still union in: {:?}",
        entity.aliases
    );
    assert!(
        entity.search_text.contains("Engineer at Acme.") && entity.search_text.contains('S'),
        "search_text is re-derived from what the entity holds plus the new aliases: {}",
        entity.search_text
    );
}

/// An entity is clean only once its article is on disk. Reconcile used to stamp the
/// completion marker itself, three stages early, so an entity whose author then failed
/// was marked done and never re-rendered - the `.md` and the memories diverged
/// permanently.
#[tokio::test]
async fn only_page_author_marks_an_entity_done() {
    let r = repo().await;
    r.upsert_entity_skeleton("u", "svc/pg", EntityCategory::Concept, &[], "PG", "", &[])
        .await
        .unwrap();
    let dirty = |r: &PkmRepo| {
        let r = r.clone();
        async move { r.entities_needing_reconciliation("u").await.unwrap() }
    };

    assert_eq!(dirty(&r).await, ["svc/pg"], "a fresh entity is dirty");
    seed_reconciled_entity(&r, "u", "svc/pg", "", "the db", &serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        dirty(&r).await,
        ["svc/pg"],
        "still dirty after reconcile — author has not run"
    );
    mark_entity_rendered(&r, "u", "svc/pg").await.unwrap();
    assert!(
        dirty(&r).await.is_empty(),
        "clean once the article is written"
    );
}

/// Reconcile owns the display title. A rename must keep the entity findable under the
/// old one - the term the user has been typing - so the previous name is folded into
/// the aliases, as the identity merge does with the old name.
#[tokio::test]
async fn reconcile_renames_an_entity_and_keeps_the_old_name_searchable() {
    let r = repo().await;
    r.upsert_entity_skeleton("u", "svc/pg", EntityCategory::Concept, &[], "PG", "", &[])
        .await
        .unwrap();

    // Blank name = keep the current one. This is what most passes emit.
    seed_reconciled_entity(&r, "u", "svc/pg", "", "the db", &serde_json::json!({}))
        .await
        .unwrap();
    let entity = r.entity_by_path("u", "svc/pg").await.unwrap().unwrap();
    assert_eq!(entity.name, "PG", "a blank name leaves the title alone");
    assert!(
        entity.aliases.is_empty(),
        "and mints no alias: {:?}",
        entity.aliases
    );

    // A real rename: the entries revealed what the abbreviation stood for.
    seed_reconciled_entity(
        &r,
        "u",
        "svc/pg",
        "PostgreSQL",
        "the db",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let entity = r.entity_by_path("u", "svc/pg").await.unwrap().unwrap();
    assert_eq!(entity.name, "PostgreSQL");
    assert!(
        entity.aliases.contains("PG"),
        "the old title survives: {:?}",
        entity.aliases
    );
    assert!(
        entity.search_text.contains("PostgreSQL") && entity.search_text.contains("PG"),
        "findable under both: {}",
        entity.search_text
    );
    assert_eq!(
        entity.path, "svc/pg",
        "renaming the title does not move the entity"
    );

    // Re-stating the same name is not a rename, so it mints no further alias.
    seed_reconciled_entity(
        &r,
        "u",
        "svc/pg",
        "PostgreSQL",
        "the db",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let entity = r.entity_by_path("u", "svc/pg").await.unwrap().unwrap();
    assert_eq!(
        entity.aliases.len(),
        1,
        "no duplicate alias: {:?}",
        entity.aliases
    );
}

#[tokio::test]
async fn search_finds_body_only_matches_but_ranks_metadata_matches_first() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "services/primary",
        EntityCategory::Concept,
        &[],
        "Postgres deployment",
        "Database operations",
        &[],
    )
    .await
    .unwrap();
    r.upsert_entity_skeleton(
        "u",
        "notes/incidental",
        EntityCategory::Concept,
        &[],
        "Weekly notes",
        "Assorted observations",
        &[],
    )
    .await
    .unwrap();
    r.db.query(
        "UPDATE knowledge_entity SET body = 'The postgres deployment was mentioned in passing.'
         WHERE user_id = 'u' AND path = 'notes/incidental'",
    )
    .await
    .unwrap()
    .check()
    .unwrap();

    let hits = r.search_entities("u", "postgres deployment").await.unwrap();
    assert_eq!(hits.len(), 2, "body-only matches participate: {hits:?}");
    assert_eq!(
        hits[0].path, "services/primary",
        "metadata matches outrank body-only mentions: {hits:?}"
    );
    assert_eq!(
        hits[1].match_snippet("postgres deployment").as_deref(),
        Some("The postgres deployment was mentioned in passing.")
    );
}

/// Retiring or re-homing a memory changes what its entities render, but touches only
/// memory rows. Every such write must bump the entities, or an entity made stale by
/// *another* entity's reconcile carries no dirty signal at all and is never re-rendered.
#[tokio::test]
async fn retiring_a_memory_re_dirties_the_entities_that_render_it() {
    let r = repo().await;
    // Clean every entity, so the next assertion sees only what the write under test
    // dirtied.
    let clean_all = |r: &PkmRepo| {
        let r = r.clone();
        async move {
            for p in ["svc/pg", "svc/redis"] {
                seed_reconciled_entity(&r, "u", p, "", "x", &serde_json::json!({}))
                    .await
                    .unwrap();
                mark_entity_rendered(&r, "u", p).await.unwrap();
            }
            assert!(
                r.entities_needing_reconciliation("u")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    };
    let mem = |r: &PkmRepo, content: &'static str, path: &'static str| {
        let r = r.clone();
        async move {
            r.create_sourced_memory(
                "u",
                MemoryKind::Fact,
                content,
                &[path.to_string()],
                human_evidence(content, path),
            )
            .await
            .unwrap()
        }
    };

    for p in ["svc/pg", "svc/redis"] {
        r.upsert_entity_skeleton("u", p, EntityCategory::Concept, &[], p, "", &[])
            .await
            .unwrap();
    }
    let old = mem(&r, "runs on 5432", "svc/pg").await;
    let new = mem(&r, "runs on 5433", "svc/redis").await;

    // Outdated disposition.
    clean_all(&r).await;
    r.set_disposition("u", &old, Disposition::Outdated)
        .await
        .unwrap();
    assert_eq!(
        r.entities_needing_reconciliation("u").await.unwrap(),
        ["svc/pg"]
    );

    // union_memory_entities is a read-only entity-scoped view. It must not make a fact
    // about one entity appear as a fact about another.
    clean_all(&r).await;
    let by_page = r.union_memory_entities("u", &new, &old).await.unwrap();
    assert_eq!(by_page["svc/pg"], [old.clone()]);
    assert_eq!(by_page["svc/redis"], [new.clone()]);
    assert!(
        r.entities_needing_reconciliation("u")
            .await
            .unwrap()
            .is_empty()
    );

    // Suspect disposition - the quarantine case. Without this bump the entity went clean, and
    // the reinstate sweep (which only walks the dirty set) could never release it.
    clean_all(&r).await;
    r.set_disposition("u", &new, Disposition::Suspect)
        .await
        .unwrap();
    let mut entities = r.entities_needing_reconciliation("u").await.unwrap();
    entities.sort();
    assert_eq!(
        entities,
        ["svc/redis"],
        "only the memory's persisted entity is dirtied; the read-only union added no scope"
    );
}

/// A property stated about an entity that **already exists** is recorded, not discarded.
///
/// Creation order must not decide whether an otherwise identical later statement
/// reaches the working consolidation state.
#[tokio::test]
async fn property_stated_about_an_existing_entity_is_merged_in() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "orgs/example-corp",
        EntityCategory::Concept,
        &[],
        "Example Corp",
        "a company",
        &[],
    )
    .await
    .unwrap();
    let window = |attrs: serde_json::Value| IngestBatch {
        entities: vec![PendingEntity {
            path: "orgs/example-corp".into(),
            name: "Example Corp".into(),
            description: "a company".into(),
            aliases: Vec::new(),
            identity_evidence: Vec::new(),
            attribute_evidence: attribute_evidence("orgs/example-corp", &attrs),
            attributes: attrs,
        }],
        entity_updates: Vec::new(),
        memories: Vec::new(),
        playbook_candidates: Vec::new(),
        grounding_corrections: 0,
        grounding_items_dropped: 0,
        recall_result_lookups: 0,
        ..Default::default()
    };

    let entity = r
        .entity_by_path("u", "orgs/example-corp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.attributes, serde_json::json!({}));

    // A later conversation states a property about it.
    let counts = commit_checkpointed_extract_patch(
        &r,
        "u",
        &window(serde_json::json!({ "employer_of": "Casey Owner" })),
        None,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(counts.entities_created, 0, "the entity already existed");
    let entity = r
        .entity_by_path("u", "orgs/example-corp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entity.attributes,
        serde_json::json!({}),
        "extraction must not mutate an existing live entity"
    );
    assert!(
        r.memories_for_entity("u", "orgs/example-corp")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn explicit_existing_entity_update_adds_attributes_without_minting_entities() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "people/me",
        EntityCategory::Concept,
        &[],
        "Casey Owner",
        "",
        &[],
    )
    .await
    .unwrap();

    let updates = IngestBatch {
        entities: Vec::new(),
        entity_updates: vec![
            PendingEntityUpdate {
                path: "people/me".into(),
                attribute_evidence: attribute_evidence(
                    "people/me",
                    &serde_json::json!({ "employer": "Example Corp" }),
                ),
                attributes: serde_json::json!({ "employer": "Example Corp" }),
            },
            PendingEntityUpdate {
                path: "organizations/missing".into(),
                attribute_evidence: attribute_evidence(
                    "organizations/missing",
                    &serde_json::json!({ "name": "Missing" }),
                ),
                attributes: serde_json::json!({ "name": "Missing" }),
            },
        ],
        memories: Vec::new(),
        playbook_candidates: Vec::new(),
        grounding_corrections: 0,
        grounding_items_dropped: 0,
        recall_result_lookups: 0,
        ..Default::default()
    };
    let counts = commit_checkpointed_extract_patch(&r, "u", &updates, None, &[])
        .await
        .unwrap();

    assert_eq!(
        counts.entities_created, 0,
        "updates are never entity-creation requests"
    );
    assert!(
        r.entity_by_path("u", "organizations/missing")
            .await
            .unwrap()
            .is_none(),
        "a stale or invented update path must not be minted"
    );
    let me = r.entity_by_path("u", "people/me").await.unwrap().unwrap();
    assert_eq!(me.attributes, serde_json::json!({}));
    let memories = r.memories_for_entity("u", "people/me").await.unwrap();
    assert_eq!(
        memories
            .iter()
            .filter(|memory| memory.content == "employer: Example Corp")
            .count(),
        0,
        "associations are deferred until classification"
    );
}

/// Re-stating what is already recorded changes nothing - including after the key has been
/// **promoted to an edge**, which deletes it from the attribute map.
///
/// That deletion is why the dedupe cannot test the map: a promoted key is missing from it,
/// so "the map lacks this key" reads a successful promotion as a fact never seen. Merging
/// on that test would put the literal back beside the edge, and do it again every pass.
#[tokio::test]
async fn restated_property_is_not_duplicated_or_resurrected() {
    let r = repo().await;
    r.upsert_entity_skeleton(
        "u",
        "devices/device-x",
        EntityCategory::Concept,
        &[],
        "Device X",
        "a screwdriver",
        &[],
    )
    .await
    .unwrap();
    let window = IngestBatch {
        entities: vec![PendingEntity {
            path: "devices/device-x".into(),
            name: "Device X".into(),
            description: "a screwdriver".into(),
            aliases: Vec::new(),
            identity_evidence: Vec::new(),
            attribute_evidence: attribute_evidence(
                "devices/device-x",
                &serde_json::json!({ "manufacturer": "Example Tools" }),
            ),
            attributes: serde_json::json!({ "manufacturer": "Example Tools" }),
        }],
        entity_updates: Vec::new(),
        memories: Vec::new(),
        playbook_candidates: Vec::new(),
        grounding_corrections: 0,
        grounding_items_dropped: 0,
        recall_result_lookups: 0,
        ..Default::default()
    };
    let send = async |b: &IngestBatch| {
        commit_checkpointed_extract_patch(&r, "u", b, None, &[])
            .await
            .unwrap()
    };

    send(&window).await;
    // Same statement again: no second copy of the fact, map unchanged.
    send(&window).await;
    let mems = r
        .memories_for_entity("u", "devices/device-x")
        .await
        .unwrap();
    assert_eq!(
        mems.iter()
            .filter(|m| m.content == "manufacturer: Example Tools")
            .count(),
        0,
        "the repo does not synthesize attribute memories: {:?}",
        mems.iter().map(|m| &m.content).collect::<Vec<_>>()
    );

    // Now promote it, as classify would: the key leaves the map and becomes an edge.
    seed_reconciled_entity(
        &r,
        "u",
        "devices/device-x",
        "",
        "a screwdriver",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &r,
        "u",
        "devices/device-x",
        "orgs/example-tools",
        "schema:manufacturer",
    )
    .await
    .unwrap();

    // The entity comes up again and the transcript states the same thing.
    send(&window).await;
    let entity = r
        .entity_by_path("u", "devices/device-x")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entity.attributes,
        serde_json::json!({}),
        "a fact already held as an edge must not come back as a literal"
    );
}
use super::*;
