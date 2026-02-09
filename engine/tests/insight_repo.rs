use chrono::{Duration, Utc};
use frona::api::db;
use frona::api::repo::generic::SurrealRepo;
use frona::api::repo::insights::SurrealInsightRepo;
use frona::memory::insight::models::Insight;
use frona::memory::insight::repository::InsightRepository;
use frona::core::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_insight(agent_id: &str, content: &str, created_at: chrono::DateTime<chrono::Utc>) -> Insight {
    Insight {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        user_id: None,
        content: content.to_string(),
        source_chat_id: None,
        created_at,
    }
}

fn make_user_insight(user_id: &str, content: &str, created_at: chrono::DateTime<chrono::Utc>) -> Insight {
    Insight {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: String::new(),
        user_id: Some(user_id.to_string()),
        content: content.to_string(),
        source_chat_id: None,
        created_at,
    }
}

#[tokio::test]
async fn test_find_by_agent_id_after_returns_newer_insights() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old = make_insight("agent-1", "old insight", before);
    let new = make_insight("agent-1", "new insight", after);

    repo.create(&old).await.unwrap();
    repo.create(&new).await.unwrap();

    let all = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(all.len(), 2, "find_by_agent_id should return all insights");

    let after_cutoff = repo.find_by_agent_id_after("agent-1", cutoff).await.unwrap();
    assert_eq!(
        after_cutoff.len(),
        1,
        "find_by_agent_id_after should return only insights after cutoff, got {}",
        after_cutoff.len()
    );
    assert_eq!(after_cutoff[0].content, "new insight");
}

#[tokio::test]
async fn test_delete_by_agent_id_before_removes_older_insights() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old = make_insight("agent-1", "old insight", before);
    let new = make_insight("agent-1", "new insight", after);

    repo.create(&old).await.unwrap();
    repo.create(&new).await.unwrap();

    repo.delete_by_agent_id_before("agent-1", cutoff).await.unwrap();

    let remaining = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "delete_by_agent_id_before should remove old insights, {} remaining",
        remaining.len()
    );
    assert_eq!(remaining[0].content, "new insight");
}

#[tokio::test]
async fn test_datetime_roundtrip_preserves_value() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let now = Utc::now();
    let insight = make_insight("agent-1", "test insight", now);

    repo.create(&insight).await.unwrap();

    let found = repo.find_by_id(&insight.id).await.unwrap().unwrap();
    assert_eq!(found.created_at, now, "DateTime should round-trip exactly");
}

#[tokio::test]
async fn test_find_by_agent_id_after_with_utc_now_boundary() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let insight_time = Utc::now();
    let insight = make_insight("agent-1", "stored insight", insight_time);
    repo.create(&insight).await.unwrap();

    let cutoff_before = insight_time - Duration::seconds(1);
    let results = repo.find_by_agent_id_after("agent-1", cutoff_before).await.unwrap();
    assert_eq!(results.len(), 1, "Should find insight created after cutoff (1s before), got {}", results.len());

    let results = repo.find_by_agent_id_after("agent-1", insight_time).await.unwrap();
    assert_eq!(results.len(), 0, "Should NOT find insight with exact same timestamp (strict >), got {}", results.len());

    let cutoff_after = insight_time + Duration::seconds(1);
    let results = repo.find_by_agent_id_after("agent-1", cutoff_after).await.unwrap();
    assert_eq!(results.len(), 0, "Should NOT find insight created before cutoff (1s after), got {}", results.len());
}

#[tokio::test]
async fn test_stored_as_json_queried_as_datetime() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let before_store = Utc::now() - Duration::seconds(1);

    let insight = make_insight("agent-1", "an insight", Utc::now());
    repo.create(&insight).await.unwrap();

    let all = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(all.len(), 1);

    let after_results = repo.find_by_agent_id_after("agent-1", before_store).await.unwrap();
    assert_eq!(
        after_results.len(),
        1,
        "find_by_agent_id_after should find the insight stored after cutoff, got {}. \
         This fails when DateTime is stored as string but queried as native datetime.",
        after_results.len()
    );
}

// User-scoped repository tests

#[tokio::test]
async fn test_find_by_user_id_returns_user_insights() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let now = Utc::now();
    let insight1 = make_user_insight("user-1", "user 1 fact A", now);
    let insight2 = make_user_insight("user-1", "user 1 fact B", now + Duration::seconds(1));
    let insight3 = make_user_insight("user-2", "user 2 fact", now);

    repo.create(&insight1).await.unwrap();
    repo.create(&insight2).await.unwrap();
    repo.create(&insight3).await.unwrap();

    let user1_insights = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(user1_insights.len(), 2, "Should return 2 insights for user-1");

    let user2_insights = repo.find_by_user_id("user-2").await.unwrap();
    assert_eq!(user2_insights.len(), 1, "Should return 1 insight for user-2");
}

#[tokio::test]
async fn test_find_by_user_id_after_returns_newer_insights() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old = make_user_insight("user-1", "old user insight", before);
    let new = make_user_insight("user-1", "new user insight", after);

    repo.create(&old).await.unwrap();
    repo.create(&new).await.unwrap();

    let results = repo.find_by_user_id_after("user-1", cutoff).await.unwrap();
    assert_eq!(results.len(), 1, "Should return only newer insights");
    assert_eq!(results[0].content, "new user insight");
}

#[tokio::test]
async fn test_delete_by_user_id_before_removes_older_insights() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let cutoff = Utc::now();
    let before = cutoff - Duration::seconds(60);
    let after = cutoff + Duration::seconds(60);

    let old = make_user_insight("user-1", "old user insight", before);
    let new = make_user_insight("user-1", "new user insight", after);

    repo.create(&old).await.unwrap();
    repo.create(&new).await.unwrap();

    repo.delete_by_user_id_before("user-1", cutoff).await.unwrap();

    let remaining = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(remaining.len(), 1, "Should keep only newer insight");
    assert_eq!(remaining[0].content, "new user insight");
}

#[tokio::test]
async fn test_find_distinct_user_ids() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let now = Utc::now();
    repo.create(&make_user_insight("user-a", "fact 1", now)).await.unwrap();
    repo.create(&make_user_insight("user-b", "fact 2", now)).await.unwrap();
    repo.create(&make_user_insight("user-a", "fact 3", now + Duration::seconds(1))).await.unwrap();

    let mut user_ids = repo.find_distinct_user_ids().await.unwrap();
    user_ids.sort();
    assert_eq!(user_ids, vec!["user-a", "user-b"]);
}

#[tokio::test]
async fn test_agent_and_user_insights_are_independent() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    let now = Utc::now();
    let agent_insight = make_insight("agent-1", "agent-scoped insight", now);
    let user_insight = make_user_insight("user-1", "user-scoped insight", now);

    repo.create(&agent_insight).await.unwrap();
    repo.create(&user_insight).await.unwrap();

    let agent_results = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(agent_results.len(), 1, "Agent query should return only agent insights");
    assert_eq!(agent_results[0].content, "agent-scoped insight");

    let user_results = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(user_results.len(), 1, "User query should return only user insights");
    assert_eq!(user_results[0].content, "user-scoped insight");

    let agent_ids = repo.find_distinct_agent_ids().await.unwrap();
    assert_eq!(agent_ids.len(), 1, "Should only find agent-scoped agent IDs");

    let user_ids = repo.find_distinct_user_ids().await.unwrap();
    assert_eq!(user_ids.len(), 1, "Should only find user-scoped user IDs");
}
