use std::time::Duration;

use tokio::time::Instant;

use super::AuthService;

#[derive(Clone)]
pub struct LoginAttemptTracker {
    cache: moka::future::Cache<String, Attempts>,
    max_attempts: u32,
    lockout_duration: Duration,
}

#[derive(Clone)]
struct Attempts {
    failures: u32,
    expires_at: Instant,
}

impl LoginAttemptTracker {
    pub fn new(max_attempts: u32, lockout_minutes: u64) -> Self {
        let lockout_duration = Duration::from_secs(lockout_minutes.saturating_mul(60));
        let cache = moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(lockout_duration)
            .build();
        Self {
            cache,
            max_attempts,
            lockout_duration,
        }
    }

    fn disabled(&self) -> bool {
        self.max_attempts == 0 || self.lockout_duration.is_zero()
    }

    fn key(identifier: &str) -> String {
        // Match email lookup and Handle::try_new normalization, respectively.
        if identifier.contains('@') {
            AuthService::normalize_email(identifier)
        } else {
            identifier.trim().to_ascii_lowercase()
        }
    }

    pub async fn is_locked(&self, identifier: &str) -> bool {
        if self.disabled() {
            return false;
        }
        self.cache
            .get(&Self::key(identifier))
            .await
            .is_some_and(|attempts| {
                attempts.expires_at > Instant::now() && attempts.failures >= self.max_attempts
            })
    }

    pub async fn record_failure(&self, identifier: &str) {
        if self.disabled() {
            return;
        }
        self.cache
            .entry(Self::key(identifier))
            .and_upsert_with(|entry| async move {
                let now = Instant::now();
                let mut attempts = entry
                    .map(|entry| entry.into_value())
                    .filter(|attempts| attempts.expires_at > now)
                    .unwrap_or(Attempts {
                        failures: 0,
                        expires_at: now + self.lockout_duration,
                    });
                if attempts.failures < self.max_attempts {
                    attempts.failures += 1;
                    if attempts.failures == self.max_attempts {
                        attempts.expires_at = now + self.lockout_duration;
                    }
                }
                // Replacing the cache entry also refreshes its TTL, so cache eviction
                // cannot truncate the lockout to the original first-failure window.
                // The explicit deadline prevents late in-flight failures extending it.
                attempts
            })
            .await;
    }

    pub async fn clear(&self, identifier: &str) {
        self.cache.invalidate(&Self::key(identifier)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allows_login_before_max_attempts() {
        let tracker = LoginAttemptTracker::new(3, 15);
        tracker.record_failure("user@test.com").await;
        tracker.record_failure("user@test.com").await;
        assert!(!tracker.is_locked("user@test.com").await);
    }

    #[tokio::test]
    async fn locks_after_max_attempts() {
        let tracker = LoginAttemptTracker::new(3, 15);
        for _ in 0..3 {
            tracker.record_failure("user@test.com").await;
        }
        assert!(tracker.is_locked("user@test.com").await);
    }

    #[tokio::test]
    async fn clear_resets_lockout() {
        let tracker = LoginAttemptTracker::new(3, 15);
        for _ in 0..3 {
            tracker.record_failure("user@test.com").await;
        }
        tracker.clear("user@test.com").await;
        assert!(!tracker.is_locked("user@test.com").await);
    }

    #[tokio::test]
    async fn different_identifiers_tracked_separately() {
        let tracker = LoginAttemptTracker::new(2, 15);
        for _ in 0..2 {
            tracker.record_failure("a@test.com").await;
        }
        assert!(tracker.is_locked("a@test.com").await);
        assert!(!tracker.is_locked("b@test.com").await);
    }

    #[tokio::test(start_paused = true)]
    async fn threshold_starts_a_full_lockout_without_extending_on_late_failures() {
        let tracker = LoginAttemptTracker::new(2, 15);
        tracker.record_failure("  Alice@Example.COM  ").await;
        tokio::time::advance(Duration::from_secs(14 * 60)).await;
        tracker.record_failure("alice@example.com").await;
        assert!(tracker.is_locked("ALICE@example.com").await);

        // Past the original failure window, but still inside the threshold's window.
        tokio::time::advance(Duration::from_secs(14 * 60 + 59)).await;
        assert!(tracker.is_locked("alice@example.com").await);
        tracker.record_failure("alice@example.com").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!tracker.is_locked("alice@example.com").await);
        tracker.record_failure("alice@example.com").await;
        assert!(!tracker.is_locked("alice@example.com").await);
        tracker.record_failure("alice@example.com").await;
        assert!(tracker.is_locked("alice@example.com").await);
    }

    #[tokio::test(start_paused = true)]
    async fn subthreshold_failures_expire_at_the_original_deadline() {
        let tracker = LoginAttemptTracker::new(3, 1);
        tracker.record_failure("alice").await;
        tokio::time::advance(Duration::from_secs(59)).await;
        tracker.record_failure("alice").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tracker.record_failure("alice").await;
        assert!(!tracker.is_locked("alice").await);
        tracker.record_failure("alice").await;
        assert!(!tracker.is_locked("alice").await);
        tracker.record_failure("alice").await;
        assert!(tracker.is_locked("alice").await);
    }

    #[tokio::test]
    async fn normalization_applies_to_failure_checks_and_clear() {
        let tracker = LoginAttemptTracker::new(2, 15);
        for variants in [
            ["  Alice  ", "ALICE", "alice"],
            [
                "  Älice@Example.COM  ",
                "ÄLICE@example.com",
                "älice@example.com",
            ],
        ] {
            tracker.record_failure(variants[0]).await;
            tracker.record_failure(variants[1]).await;
            assert!(tracker.is_locked(variants[2]).await);
            tracker.clear(variants[0]).await;
            assert!(!tracker.is_locked(variants[1]).await);
            tracker.record_failure(variants[2]).await;
            assert!(!tracker.is_locked(variants[0]).await);
        }
    }

    #[tokio::test]
    async fn zero_threshold_or_duration_disables_tracking() {
        for tracker in [
            LoginAttemptTracker::new(0, 15),
            LoginAttemptTracker::new(2, 0),
        ] {
            for _ in 0..3 {
                tracker.record_failure("alice").await;
            }
            assert!(!tracker.is_locked("alice").await);
            assert!(tracker.cache.get("alice").await.is_none());
        }
    }

    #[tokio::test]
    async fn concurrent_failures_share_one_counter() {
        let tracker = LoginAttemptTracker::new(20, 15);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let tracker = tracker.clone();
            tasks.spawn(async move { tracker.record_failure("alice").await });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert!(tracker.is_locked("alice").await);
    }
}
