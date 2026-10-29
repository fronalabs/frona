use super::*;
use crate::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Source {
    sections: Mutex<Vec<AgentContextSection>>,
    fail: AtomicBool,
}
#[async_trait]
impl AgentContextSource for Source {
    async fn collect(
        &self,
        _: &AgentContextScope<'_>,
        _: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Internal("source unavailable".into()));
        }
        Ok(self.sections.lock().unwrap().clone())
    }
}

fn skills(names: &[&str]) -> AgentContextSection {
    AgentContextSection::tagged(
        AgentContextKind::Skills,
        "available_skills",
        names
            .iter()
            .map(|name| (name.to_string(), format!("- {name}: description\n")))
            .collect(),
    )
}
fn test_user() -> crate::auth::User {
    crate::auth::User {
        id: "user".into(),
        handle: crate::handle!("user"),
        email: "user@example.com".into(),
        name: "User".into(),
        password_hash: String::new(),
        timezone: None,
        groups: vec![],
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}
pub(super) fn scope() -> AgentContextScope<'static> {
    static USER: std::sync::LazyLock<crate::auth::User> = std::sync::LazyLock::new(test_user);
    static AGENT: std::sync::LazyLock<crate::agent::models::Agent> =
        std::sync::LazyLock::new(|| crate::agent::models::Agent {
            id: "agent".into(),
            user_id: "user".into(),
            handle: crate::handle!("agent"),
            name: "Agent".into(),
            description: String::new(),
            model_group: "primary".into(),
            enabled: true,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            skills: None,
            avatar: None,
            identity: Default::default(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });
    AgentContextScope {
        user: &USER,
        agent: &AGENT,
        space_id: None,
        task: None,
    }
}
async fn database() -> surrealdb::Surreal<surrealdb::engine::local::Db> {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    db
}

#[tokio::test]
async fn snapshots_deduplicate_restart_and_revisit_previous_content() {
    let db = database().await;
    let repository = SurrealAgentContextSnapshotRepo(db.clone());
    let source = Arc::new(Source {
        sections: Mutex::new(vec![skills(&["deploy"])]),
        fail: AtomicBool::new(false),
    });
    let manager = AgentContextManager::new(vec![source.clone()], repository.clone());
    let first = manager
        .prepare(&scope(), None, &mut AgentContextState::default())
        .await
        .unwrap();
    assert_eq!(
        first.update.as_deref(),
        Some(first.snapshot.render().as_str())
    );
    let same = manager
        .prepare(
            &scope(),
            Some(&first.snapshot.signature),
            &mut AgentContextState::default(),
        )
        .await
        .unwrap();
    assert_eq!(same.snapshot.signature, first.snapshot.signature);
    assert!(same.update.is_none());
    *source.sections.lock().unwrap() = vec![skills(&["deploy", "review"])];
    let added = manager
        .prepare(
            &scope(),
            Some(&first.snapshot.signature),
            &mut AgentContextState::default(),
        )
        .await
        .unwrap();
    let update = added.update.unwrap();
    assert!(update.contains("review"));
    assert!(!update.contains("deploy"));
    // Reorder doesn't change content addressing.
    *source.sections.lock().unwrap() = vec![skills(&["review", "deploy"])];
    assert_eq!(
        manager
            .current(&scope(), &mut AgentContextState::default())
            .await
            .unwrap(),
        added.snapshot
    );
    *source.sections.lock().unwrap() = vec![skills(&["deploy"])];
    let removed = manager
        .prepare(
            &scope(),
            Some(&added.snapshot.signature),
            &mut AgentContextState::default(),
        )
        .await
        .unwrap();
    assert_eq!(removed.snapshot.signature, first.snapshot.signature);
    assert!(
        removed.update.is_none(),
        "absence is not a deletion announcement"
    );
    let restarted = AgentContextManager::new(Vec::new(), repository);
    assert_eq!(
        restarted
            .load(&first.snapshot.signature)
            .await
            .unwrap()
            .render(),
        first.snapshot.render()
    );
    let mut response = db
        .query("SELECT * FROM agent_context_snapshot")
        .await
        .unwrap();
    let rows: Vec<surrealdb::types::Value> = response.take(0).unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn failed_source_does_not_publish_partial_snapshot() {
    let db = database().await;
    let repository = SurrealAgentContextSnapshotRepo(db.clone());
    let good = Arc::new(Source {
        sections: Mutex::new(vec![skills(&["deploy"])]),
        fail: AtomicBool::new(false),
    });
    let bad = Arc::new(Source {
        sections: Mutex::new(Vec::new()),
        fail: AtomicBool::new(true),
    });
    let manager = AgentContextManager::new(vec![good, bad.clone()], repository);
    assert!(
        manager
            .prepare(&scope(), None, &mut AgentContextState::default())
            .await
            .is_err()
    );
    let mut response = db
        .query("SELECT * FROM agent_context_snapshot")
        .await
        .unwrap();
    let rows: Vec<surrealdb::types::Value> = response.take(0).unwrap();
    assert!(rows.is_empty());
    bad.fail.store(false, Ordering::SeqCst);
    let first = manager
        .prepare(&scope(), None, &mut AgentContextState::default())
        .await
        .unwrap();
    bad.fail.store(true, Ordering::SeqCst);
    assert!(
        manager
            .prepare(
                &scope(),
                Some(&first.snapshot.signature),
                &mut AgentContextState::default()
            )
            .await
            .is_err()
    );
    assert_eq!(
        manager.load(&first.snapshot.signature).await.unwrap(),
        first.snapshot
    );
}

#[test]
fn changed_prose_and_return_to_original_are_delivered() {
    let a = AgentContextSnapshot::new(vec![AgentContextSection::prose(
        AgentContextKind::UserProfile,
        "user_profile",
        "Timezone: UTC".into(),
    )])
    .unwrap();
    let b = AgentContextSnapshot::new(vec![AgentContextSection::prose(
        AgentContextKind::UserProfile,
        "user_profile",
        "Timezone: Asia/Tokyo".into(),
    )])
    .unwrap();
    assert!(b.diff(&a).unwrap().contains("Asia/Tokyo"));
    assert!(a.diff(&b).unwrap().contains("UTC"));
    assert!(a.diff(&a).is_none());
}

#[tokio::test]
async fn conflicting_content_cannot_replace_a_saved_signature() {
    let repo = SurrealAgentContextSnapshotRepo(database().await);
    let first = AgentContextSnapshot::new(vec![skills(&["deploy"])]).unwrap();
    repo.ensure_stored(&first).await.unwrap();
    let mut corrupt = AgentContextSnapshot::new(vec![skills(&["review"])]).unwrap();
    corrupt.signature = first.signature.clone();
    assert!(repo.ensure_stored(&corrupt).await.is_err());
    assert_eq!(repo.find(&first.signature).await.unwrap().unwrap(), first);
}

#[test]
fn typed_state_uses_full_type_names_and_round_trips() {
    mod first {
        #[derive(Default, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        pub struct State {
            pub count: u32,
        }
    }
    mod second {
        #[derive(Default, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        pub struct State {
            pub text: String,
        }
    }
    let mut state = AgentContextState::default();
    assert_eq!(state.get::<first::State>().unwrap().count, 0);
    state.set(first::State { count: 4 }).unwrap();
    state
        .set(second::State {
            text: "independent".into(),
        })
        .unwrap();
    let serialized = serde_json::to_value(&state).unwrap();
    assert_eq!(
        serialized[std::any::type_name::<first::State>()]["count"],
        4
    );
    let restored: AgentContextState = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored.get::<first::State>().unwrap().count, 4);
    assert_eq!(restored.get::<second::State>().unwrap().text, "independent");
    let malformed = AgentContextState(BTreeMap::from([(
        std::any::type_name::<first::State>().to_owned(),
        serde_json::json!({"count": "invalid"}),
    )]));
    assert!(malformed.get::<first::State>().is_err());
}

#[tokio::test]
async fn transient_sections_are_delivered_without_changing_or_storing_snapshots() {
    let db = database().await;
    let repository = SurrealAgentContextSnapshotRepo(db.clone());
    let mut clock =
        AgentContextSection::prose(AgentContextKind::TaskTime, "task_time", "now: first".into());
    clock.transient = true;
    let source = Arc::new(Source {
        sections: Mutex::new(vec![skills(&["deploy"]), clock.clone()]),
        fail: AtomicBool::new(false),
    });
    let manager = AgentContextManager::new(vec![source.clone()], repository.clone());
    let mut state = AgentContextState::default();
    let first = manager.prepare(&scope(), None, &mut state).await.unwrap();
    assert!(first.update.as_ref().unwrap().contains("now: first"));
    assert!(!first.snapshot.render().contains("task_time"));
    assert!(first.snapshot.sections.iter().all(|s| !s.transient));
    assert!(
        !serde_json::to_string(&first.snapshot.sections)
            .unwrap()
            .contains("transient")
    );
    clock
        .entries
        .insert("summary".into(), "now: second\n".into());
    *source.sections.lock().unwrap() = vec![skills(&["deploy"]), clock.clone()];
    let next = manager
        .prepare(&scope(), Some(&first.snapshot.signature), &mut state)
        .await
        .unwrap();
    assert_eq!(next.snapshot, first.snapshot);
    assert_eq!(next.update.as_deref(), Some(clock.render().as_str()));
    let restarted = AgentContextManager::new(vec![source], repository);
    let again = restarted
        .prepare(
            &scope(),
            Some(&first.snapshot.signature),
            &mut AgentContextState::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        again.update, next.update,
        "transient sections are emitted even if their text is unchanged"
    );
    assert_eq!(
        restarted.load(&first.snapshot.signature).await.unwrap(),
        first.snapshot
    );
    let mut result = db
        .query("SELECT * FROM agent_context_snapshot")
        .await
        .unwrap();
    let rows: Vec<surrealdb::types::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
}
