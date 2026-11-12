//! Disposition lifecycle through the repo + `classify_memories`, on an
//! in-memory SurrealDB. Covers the load-bearing behaviors: outdated → history,
//! erroneous → excluded everywhere, a `Replace` relation → history, the global
//! mark-erroneous on entity delete, the no-valid-memories entity GC set, and the
//! re-learn suppression set.

use std::collections::HashSet;

use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::db::repo::pkm::PkmRepo;
use frona::memory::pkm::model::{
    Disposition, EntityCategory, MemoryKind, RelationType, classify_memories,
};

async fn repo() -> (Surreal<Db>, PkmRepo) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let repo = PkmRepo::new(db.clone(), 8);
    (db, repo)
}

const U: &str = "u1";

async fn seed_page(repo: &PkmRepo, path: &str) {
    repo.upsert_entity_skeleton(
        U,
        path,
        EntityCategory::Concept,
        &["https://schema.org/Person".to_string()],
        "Bob",
        "",
        &[],
    )
    .await
    .unwrap();
}

async fn add_mem(repo: &PkmRepo, content: &str, paths: &[&str]) -> String {
    let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    repo.create_memory_with_entities(U, "a1", "c1", MemoryKind::Fact, content, &paths)
        .await
        .unwrap()
}

#[tokio::test]
async fn set_disposition_outdated_moves_memory_to_history() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/bob").await;
    let m = add_mem(&repo, "Bob works at Globex", &["people/bob"]).await;

    repo.set_disposition(U, &m, Disposition::Outdated)
        .await
        .unwrap();

    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert_eq!(mems.len(), 1);
    assert_eq!(mems[0].disposition, Disposition::Outdated);
    assert!(mems[0].ended_at.is_some(), "ended_at stamped");
    let (cur, hist) = classify_memories(&mems);
    assert!(cur.is_empty());
    assert_eq!(hist.len(), 1, "outdated shows as history");
}

#[tokio::test]
async fn mark_entity_memories_erroneous_excludes_all() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/bob").await;
    let older = add_mem(&repo, "Bob works at Acme", &["people/bob"]).await;
    let newer = add_mem(&repo, "Bob works at Globex", &["people/bob"]).await;
    repo.add_relation(U, &older, RelationType::Replace, &newer, "moved")
        .await
        .unwrap();

    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    let (cur, hist) = classify_memories(&mems);
    assert_eq!(cur.len(), 1, "Globex current");
    assert_eq!(hist.len(), 1, "Acme history");

    let n = repo
        .mark_entity_memories_erroneous(U, "people/bob")
        .await
        .unwrap();
    assert_eq!(n, 2, "both memories retired");

    let mems = repo.memories_for_entity(U, "people/bob").await.unwrap();
    assert!(mems.iter().all(|m| m.disposition == Disposition::Erroneous));
    let (cur, hist) = classify_memories(&mems);
    assert!(
        cur.is_empty() && hist.is_empty(),
        "erroneous appears nowhere"
    );
}

/// The `people/me` bug (regression): still-true restatements/merges must be DROPPED,
/// never rendered under "## History (do not use)". Only `replace`/`outdated` are History.
#[tokio::test]
async fn duplicate_and_absorbed_are_dropped_not_history() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/me").await;
    let survivor = add_mem(
        &repo,
        "Casey Owner is a software engineer at Former Corp",
        &["people/me"],
    )
    .await;
    let dup = add_mem(
        &repo,
        "Casey Owner is a software engineer at Former Corp",
        &["people/me"],
    )
    .await;
    let older = add_mem(&repo, "Casey Owner likes board games", &["people/me"]).await;
    let fan = add_mem(
        &repo,
        "Casey Owner is a big board games fan",
        &["people/me"],
    )
    .await;
    repo.add_relation(U, &dup, RelationType::Duplicate, &survivor, "identical")
        .await
        .unwrap();
    repo.add_relation(U, &older, RelationType::Absorbed, &fan, "reworded")
        .await
        .unwrap();

    let mems = repo.memories_for_entity(U, "people/me").await.unwrap();
    let (cur, hist) = classify_memories(&mems);
    let cur_ids: HashSet<&str> = cur.iter().map(|m| m.id.as_str()).collect();
    assert!(
        cur_ids.contains(survivor.as_str()) && cur_ids.contains(fan.as_str()),
        "survivors current"
    );
    assert!(
        !cur_ids.contains(dup.as_str()) && !cur_ids.contains(older.as_str()),
        "subordinates not current"
    );
    assert!(
        hist.is_empty(),
        "duplicate/absorbed are dropped, NOT 'do not use' History"
    );
}

/// Coverage union is a read-only map. Differently scoped wording must never acquire the
/// subordinate's pages merely because the memories overlap semantically.
#[tokio::test]
async fn memory_entity_union_does_not_persist_subordinate_entities() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/me").await;
    seed_page(&repo, "organization/former-corp").await;
    let atom = add_mem(
        &repo,
        "Casey Owner is a SWE at Former Corp",
        &["people/me", "organization/former-corp"],
    )
    .await;
    let bio = add_mem(
        &repo,
        "Casey Owner: SWE at Former Corp, from Exampleland",
        &["people/me"],
    )
    .await;
    repo.add_relation(U, &atom, RelationType::Absorbed, &bio, "folded into bio")
        .await
        .unwrap();
    let by_page = repo.union_memory_entities(U, &bio, &atom).await.unwrap();
    assert_eq!(
        by_page["organization/former-corp"],
        std::slice::from_ref(&atom)
    );
    assert_eq!(by_page["people/me"], [atom.clone(), bio.clone()]);

    // No speculative source link was written. Since this test directly persisted an
    // unsafe global relation, the org page has no current representation; production
    // Reconcile rejects that relation before applying it.
    let org = repo
        .current_memories_for_entity(U, "organization/former-corp")
        .await
        .unwrap();
    assert!(
        !org.iter().any(|m| m.id == bio),
        "survivor was not attached speculatively"
    );
    assert!(
        !org.iter().any(|m| m.id == atom),
        "the absorbed atom is not current"
    );
}

/// Dead-row GC: only `duplicate`/`absorbed` (dropped) memories are dead weight;
/// `replace`/`outdated` (History) and `erroneous` (re-learn suppression) are kept.
#[tokio::test]
async fn gc_drops_duplicate_absorbed_keeps_history_and_erroneous() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/bob").await;
    let survivor = add_mem(&repo, "current fact", &["people/bob"]).await;
    let dup = add_mem(&repo, "dup", &["people/bob"]).await;
    let abs = add_mem(&repo, "absorbed", &["people/bob"]).await;
    let replaced = add_mem(&repo, "old value", &["people/bob"]).await;
    let outdated = add_mem(&repo, "past fact", &["people/bob"]).await;
    let erroneous = add_mem(&repo, "wrong fact", &["people/bob"]).await;
    repo.add_relation(U, &dup, RelationType::Duplicate, &survivor, "")
        .await
        .unwrap();
    repo.add_relation(U, &abs, RelationType::Absorbed, &survivor, "")
        .await
        .unwrap();
    repo.add_relation(U, &replaced, RelationType::Replace, &survivor, "")
        .await
        .unwrap();
    repo.set_disposition(U, &outdated, Disposition::Outdated)
        .await
        .unwrap();
    repo.set_disposition(U, &erroneous, Disposition::Erroneous)
        .await
        .unwrap();

    let mut dead = repo.dropped_memory_ids(U).await.unwrap();
    dead.sort();
    let mut want = vec![abs.clone(), dup.clone()];
    want.sort();
    assert_eq!(dead, want, "only duplicate/absorbed are dead weight");

    for id in &dead {
        repo.delete_memory(U, id).await.unwrap();
    }
    let ids: HashSet<String> = repo
        .list_all_memories(U)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert!(
        !ids.contains(&dup) && !ids.contains(&abs),
        "dead rows deleted"
    );
    assert!(
        ids.contains(&replaced) && ids.contains(&outdated) && ids.contains(&erroneous),
        "History (replace/outdated) + erroneous (suppression) kept"
    );
}

#[tokio::test]
async fn entity_with_all_erroneous_memories_is_gc_candidate() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/bob").await;
    seed_page(&repo, "people/alice").await;
    let bob_mem = add_mem(&repo, "Bob likes tea", &["people/bob"]).await;
    add_mem(&repo, "Alice likes coffee", &["people/alice"]).await;

    assert!(
        repo.entities_with_no_valid_memories(U)
            .await
            .unwrap()
            .is_empty()
    );

    repo.set_disposition(U, &bob_mem, Disposition::Erroneous)
        .await
        .unwrap();

    let dead = repo.entities_with_no_valid_memories(U).await.unwrap();
    assert_eq!(dead, ["people/bob"], "only bob (all erroneous) is dead");

    repo.delete_entity(U, "people/bob").await.unwrap();
    assert!(
        repo.entity_by_path(U, "people/bob")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !repo
            .memories_for_entity(U, "people/bob")
            .await
            .unwrap()
            .is_empty(),
        "erroneous memory stays linked for suppression"
    );
}

#[tokio::test]
async fn erroneous_contents_feed_relearn_suppression_set() {
    let (_db, repo) = repo().await;
    seed_page(&repo, "people/bob").await;
    let m = add_mem(&repo, "Bob lives in Paris", &["people/bob"]).await;
    repo.set_disposition(U, &m, Disposition::Erroneous)
        .await
        .unwrap();

    let set = repo
        .erroneous_contents_for_entity(U, "people/bob")
        .await
        .unwrap();
    assert!(
        set.contains("bob lives in paris"),
        "normalized content suppressible"
    );
    assert!(!set.contains("bob lives in berlin"));
}
