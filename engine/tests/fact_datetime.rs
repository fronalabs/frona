use chrono::{Duration, Utc};
use frona::api::db;
use frona::api::repo::facts::SurrealFactRepo;
use frona::api::repo::generic::SurrealRepo;
use frona::memory::fact::models::Fact;
use frona::memory::fact::repository::FactRepository;
use frona::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_fact(agent_id: &str, content: &str, created_at: chrono::DateTime<chrono::Utc>) -> Fact {
    Fact {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        content: content.to_string(),
        source_chat_id: None,
        created_at,
    }
}

#[tokio::test]
async fn test_find_by_agent_id_after_returns_newer_facts() {
    let db = test_db().await;
    let repo: SurrealFactRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old_fact = make_fact("agent-1", "old fact", before);
    let new_fact = make_fact("agent-1", "new fact", after);

    repo.create(&old_fact).await.unwrap();
    repo.create(&new_fact).await.unwrap();

    let all = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(all.len(), 2, "find_by_agent_id should return all facts");

    let after_cutoff = repo.find_by_agent_id_after("agent-1", cutoff).await.unwrap();
    assert_eq!(
        after_cutoff.len(),
        1,
        "find_by_agent_id_after should return only facts after cutoff, got {}",
        after_cutoff.len()
    );
    assert_eq!(after_cutoff[0].content, "new fact");
}

#[tokio::test]
async fn test_delete_by_agent_id_before_removes_older_facts() {
    let db = test_db().await;
    let repo: SurrealFactRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old_fact = make_fact("agent-1", "old fact", before);
    let new_fact = make_fact("agent-1", "new fact", after);

    repo.create(&old_fact).await.unwrap();
    repo.create(&new_fact).await.unwrap();

    repo.delete_by_agent_id_before("agent-1", cutoff).await.unwrap();

    let remaining = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "delete_by_agent_id_before should remove old facts, {} remaining",
        remaining.len()
    );
    assert_eq!(remaining[0].content, "new fact");
}

#[tokio::test]
async fn test_datetime_roundtrip_preserves_value() {
    let db = test_db().await;
    let repo: SurrealFactRepo = SurrealRepo::new(db);

    let now = Utc::now();
    let fact = make_fact("agent-1", "test fact", now);

    repo.create(&fact).await.unwrap();

    let found = repo.find_by_id(&fact.id).await.unwrap().unwrap();
    assert_eq!(found.created_at, now, "DateTime should round-trip exactly");
}

#[tokio::test]
async fn test_find_by_agent_id_after_with_utc_now_boundary() {
    let db = test_db().await;
    let repo: SurrealFactRepo = SurrealRepo::new(db);

    // Store a fact with Utc::now()
    let fact_time = Utc::now();
    let fact = make_fact("agent-1", "stored fact", fact_time);
    repo.create(&fact).await.unwrap();

    // Query with a cutoff 1 second before — should find the fact
    let cutoff_before = fact_time - Duration::seconds(1);
    let results = repo.find_by_agent_id_after("agent-1", cutoff_before).await.unwrap();
    assert_eq!(
        results.len(),
        1,
        "Should find fact created after cutoff (1s before), got {}",
        results.len()
    );

    // Query with the exact same time — should NOT find the fact (strict >)
    let results = repo.find_by_agent_id_after("agent-1", fact_time).await.unwrap();
    assert_eq!(
        results.len(),
        0,
        "Should NOT find fact with exact same timestamp (strict >), got {}",
        results.len()
    );

    // Query with a cutoff 1 second after — should NOT find the fact
    let cutoff_after = fact_time + Duration::seconds(1);
    let results = repo.find_by_agent_id_after("agent-1", cutoff_after).await.unwrap();
    assert_eq!(
        results.len(),
        0,
        "Should NOT find fact created before cutoff (1s after), got {}",
        results.len()
    );
}

/// This test reproduces the exact bug: store via generic create (serde_json),
/// then query with DateTime<Utc> bound parameter.
#[tokio::test]
async fn test_stored_as_json_queried_as_datetime() {
    let db = test_db().await;
    let repo: SurrealFactRepo = SurrealRepo::new(db);

    let before_store = Utc::now() - Duration::seconds(1);

    let fact = make_fact("agent-1", "a fact", Utc::now());
    repo.create(&fact).await.unwrap();

    // Verify fact exists
    let all = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(all.len(), 1);

    // This is the query that was returning 0 in production
    let after_results = repo.find_by_agent_id_after("agent-1", before_store).await.unwrap();
    assert_eq!(
        after_results.len(),
        1,
        "find_by_agent_id_after should find the fact stored after cutoff, got {}. \
         This fails when DateTime is stored as string but queried as native datetime.",
        after_results.len()
    );
}
