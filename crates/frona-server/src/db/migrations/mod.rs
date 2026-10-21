//! Forward-only migration runner.
//!
//! Migrations are declared with `#[migration("<rfc3339>")]` from the
//! `frona-derive` crate. The macro registers each one with `inventory`; this
//! module sorts them by timestamp at startup and applies any whose timestamp is
//! strictly greater than the single cursor stored at
//! `runtime_config.db_schema_version`.
//!
//! Rules for authors:
//!
//! 1. New migrations must have a timestamp strictly greater than every existing
//!    one. The `registry_timestamps_are_strictly_ascending` test enforces this.
//! 2. Code (non-SQL) migrations must be re-entrant: each separate `db.query`
//!    call is its own transaction, so a failure partway through means the whole
//!    migration re-runs on the next startup.
//! 3. Never rename or delete an applied migration's file or fn. Removing it
//!    from source means the runner has nothing to re-check; the DB row stays
//!    whatever shape it was left in.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

mod backfill_channel_enabled_and_reset_status;
mod backfill_empty_metadata;
mod drop_refuse_user_delete_event;
mod handle_unification;
mod migrate_agent_sandbox_config_to_policies;
mod migrate_agent_tools_to_policies;
mod normalize_inference_input_tokens;
mod rename_directory_entity_to_path;
mod rename_vault_grant_to_principal;
mod translate_tool_data_to_hitl;

pub type MigrationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), surrealdb::Error>> + Send + 'a>>;

pub struct Migration {
    /// Precomputed in the macro so `inventory::submit!` is pure-const.
    pub timestamp_nanos: i64,
    pub run: for<'a> fn(&'a Surreal<Db>) -> MigrationFuture<'a>,
}

impl Migration {
    pub fn datetime(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_nanos(self.timestamp_nanos)
    }
}

inventory::collect!(Migration);

pub fn all_migrations() -> Vec<&'static Migration> {
    let mut v: Vec<&'static Migration> = inventory::iter::<Migration>.into_iter().collect();
    v.sort_by_key(|m| m.timestamp_nanos);
    v
}

pub async fn run_migrations(db: &Surreal<Db>) -> Result<(), surrealdb::Error> {
    run(db, all_migrations()).await
}

async fn run(
    db: &Surreal<Db>,
    migrations: Vec<&'static Migration>,
) -> Result<(), surrealdb::Error> {
    let cursor = load_cursor(db).await?;
    for migration in migrations {
        let dt = migration.datetime();
        if cursor.is_some_and(|c| dt <= c) {
            continue;
        }
        tracing::info!(timestamp = %dt.to_rfc3339(), "applying migration");
        (migration.run)(db).await?;
        save_cursor(db, dt).await?;
        tracing::info!(timestamp = %dt.to_rfc3339(), "migration applied");
    }
    Ok(())
}

async fn load_cursor(db: &Surreal<Db>) -> Result<Option<DateTime<Utc>>, surrealdb::Error> {
    let raw: Option<String> = db
        .query("SELECT VALUE value FROM runtime_config WHERE key = 'db_schema_version' LIMIT 1")
        .await?
        .take(0)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parsed = DateTime::parse_from_rfc3339(&raw).map_err(|e| {
        surrealdb::Error::thrown(format!(
            "runtime_config.db_schema_version is not RFC 3339 (`{raw}`): {e}"
        ))
    })?;
    Ok(Some(parsed.with_timezone(&Utc)))
}

async fn save_cursor(db: &Surreal<Db>, timestamp: DateTime<Utc>) -> Result<(), surrealdb::Error> {
    db.query(
        "DELETE FROM runtime_config WHERE key = 'db_schema_version';
         CREATE runtime_config CONTENT {
             key: 'db_schema_version',
             value: $value,
             updated_at: $now,
         };",
    )
    .bind(("value", timestamp.to_rfc3339()))
    .bind(("now", Utc::now()))
    .await?
    .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::engine::local::Mem;

    async fn mem_db() -> Surreal<Db> {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        db
    }

    fn make_migration(
        ts: &str,
        run: for<'a> fn(&'a Surreal<Db>) -> MigrationFuture<'a>,
    ) -> Migration {
        Migration {
            timestamp_nanos: DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .with_timezone(&Utc)
                .timestamp_nanos_opt()
                .unwrap(),
            run,
        }
    }

    fn noop_run(_: &Surreal<Db>) -> MigrationFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    #[tokio::test]
    async fn load_cursor_is_none_on_fresh_db() {
        let db = mem_db().await;
        assert!(load_cursor(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_then_load_roundtrips_the_cursor() {
        let db = mem_db().await;
        let ts = DateTime::parse_from_rfc3339("2026-04-09T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        save_cursor(&db, ts).await.unwrap();
        assert_eq!(load_cursor(&db).await.unwrap(), Some(ts));
    }

    #[tokio::test]
    async fn save_cursor_overwrites_previous_value() {
        let db = mem_db().await;
        let a = DateTime::parse_from_rfc3339("2026-04-09T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2026-04-15T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        save_cursor(&db, a).await.unwrap();
        save_cursor(&db, b).await.unwrap();
        assert_eq!(load_cursor(&db).await.unwrap(), Some(b));
    }

    #[tokio::test]
    async fn empty_registry_does_not_create_row() {
        let db = mem_db().await;
        run(&db, vec![]).await.unwrap();
        assert!(load_cursor(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn applies_migrations_in_timestamp_order_not_input_order() {
        use std::sync::Mutex;
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

        fn a(_: &Surreal<Db>) -> MigrationFuture<'_> {
            ORDER.lock().unwrap().push("a");
            Box::pin(async { Ok(()) })
        }
        fn b(_: &Surreal<Db>) -> MigrationFuture<'_> {
            ORDER.lock().unwrap().push("b");
            Box::pin(async { Ok(()) })
        }
        fn c(_: &Surreal<Db>) -> MigrationFuture<'_> {
            ORDER.lock().unwrap().push("c");
            Box::pin(async { Ok(()) })
        }

        ORDER.lock().unwrap().clear();
        let mut entries = vec![
            Box::leak(Box::new(make_migration("2026-04-15T09:00:00Z", b))) as &'static Migration,
            Box::leak(Box::new(make_migration("2026-04-09T21:00:00Z", a))),
            Box::leak(Box::new(make_migration("2026-05-01T00:00:00Z", c))),
        ];
        entries.sort_by_key(|m| m.timestamp_nanos);

        let db = mem_db().await;
        run(&db, entries).await.unwrap();
        assert_eq!(*ORDER.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn already_applied_migrations_are_skipped_on_rerun() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counting(_: &Surreal<Db>) -> MigrationFuture<'_> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        CALLS.store(0, Ordering::SeqCst);
        let m: &'static Migration =
            Box::leak(Box::new(make_migration("2026-04-09T21:00:00Z", counting)));

        let db = mem_db().await;
        run(&db, vec![m]).await.unwrap();
        run(&db, vec![m]).await.unwrap();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_migration_leaves_cursor_at_previous() {
        fn fails(_: &Surreal<Db>) -> MigrationFuture<'_> {
            Box::pin(async { Err(surrealdb::Error::thrown("boom".to_string())) })
        }

        let a: &'static Migration =
            Box::leak(Box::new(make_migration("2026-04-09T21:00:00Z", noop_run)));
        let b: &'static Migration =
            Box::leak(Box::new(make_migration("2026-04-15T09:00:00Z", fails)));

        let db = mem_db().await;
        assert!(run(&db, vec![a, b]).await.is_err());
        let cursor = load_cursor(&db).await.unwrap().unwrap();
        assert_eq!(cursor.to_rfc3339(), a.datetime().to_rfc3339());
    }

    #[test]
    fn macro_registers_migration_into_inventory() {
        let registered = all_migrations();
        assert!(
            registered
                .iter()
                .any(|m| m.datetime().to_rfc3339() == "2026-04-09T21:00:00+00:00"),
            "rename_vault_grant_to_principal should be present in the inventory \
             (proves #[migration] → inventory::submit! path works end-to-end)"
        );
    }

    #[test]
    fn registry_timestamps_are_strictly_ascending() {
        let migrations = all_migrations();
        for pair in migrations.windows(2) {
            assert!(
                pair[0].timestamp_nanos < pair[1].timestamp_nanos,
                "two migrations share a timestamp or are out of order: {} and {}",
                pair[0].datetime().to_rfc3339(),
                pair[1].datetime().to_rfc3339(),
            );
        }
    }
}
