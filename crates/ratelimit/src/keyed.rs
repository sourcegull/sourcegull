//! Request limits per key (a client address or an account), for endpoints that strangers can
//! call: sign-in, password reset, access requests, OAuth authorization and webhooks.
//!
//! A [`Limit`] allows `requests` hits per fixed window of `period`, starting at the key's first
//! hit. [`KeyedLimiter::hit`] counts a hit and answers whether it is within the limit;
//! [`KeyedLimiter::check`] only asks (for limits on failures, counted after the outcome is
//! known). Over the limit, both return how long until the window ends (`Retry-After`).
//!
//! [`PgKeyedLimiter`] keeps the counters in the `rate_limit_counters` table, so every process
//! shares them; when the database is unreachable it falls back to an in-process
//! [`MemoryKeyedLimiter`] (still limited, per process) and logs a warning.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use tokio::time::Instant;

/// `requests` hits allowed per window of `period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    /// Hits allowed per window; 0 refuses every hit.
    pub requests: u32,
    /// The window length.
    pub period: Duration,
}

impl Limit {
    /// `requests` per `period`.
    pub const fn new(requests: u32, period: Duration) -> Self {
        Self { requests, period }
    }
}

/// Over the limit: retry after this long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limited {
    /// Time until the window ends, at least one second.
    pub retry_after: Duration,
}

/// Counters per key, shared by the processes using the same implementation.
#[async_trait]
pub trait KeyedLimiter: Send + Sync + std::fmt::Debug {
    /// Counts one hit on `key` and fails when it exceeds `limit`.
    async fn hit(&self, key: &str, limit: Limit) -> Result<(), Limited>;

    /// Fails when `key` already used up `limit` in its current window, without counting.
    async fn check(&self, key: &str, limit: Limit) -> Result<(), Limited>;
}

fn limited(left: Duration) -> Limited {
    Limited {
        retry_after: Duration::from_secs(left.as_secs_f64().ceil().max(1.0) as u64),
    }
}

/// Most keys kept by [`MemoryKeyedLimiter`]; expired windows are dropped first when full.
const MEMORY_MAX_KEYS: usize = 100_000;

/// In-process counters: for a single process, tests, and as the database fallback.
#[derive(Debug, Default)]
pub struct MemoryKeyedLimiter {
    windows: Mutex<HashMap<String, (Instant, Duration, u32)>>,
}

impl MemoryKeyedLimiter {
    /// An empty limiter.
    pub fn new() -> Self {
        Self::default()
    }

    fn apply(&self, key: &str, limit: Limit, count: bool) -> Result<(), Limited> {
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if windows.len() >= MEMORY_MAX_KEYS && !windows.contains_key(key) {
            windows.retain(|_, (start, period, _)| now.duration_since(*start) < *period);
            if windows.len() >= MEMORY_MAX_KEYS {
                windows.clear();
            }
        }
        let entry = windows
            .entry(key.to_owned())
            .or_insert((now, limit.period, 0));
        if now.duration_since(entry.0) >= limit.period {
            *entry = (now, limit.period, 0);
        }
        entry.1 = limit.period;
        if count {
            entry.2 = entry.2.saturating_add(1);
        }
        let over = if count {
            entry.2 > limit.requests
        } else {
            entry.2 >= limit.requests
        };
        if over {
            return Err(limited(
                (entry.0 + limit.period).saturating_duration_since(now),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl KeyedLimiter for MemoryKeyedLimiter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<(), Limited> {
        self.apply(key, limit, true)
    }

    async fn check(&self, key: &str, limit: Limit) -> Result<(), Limited> {
        self.apply(key, limit, false)
    }
}

/// How often a process deletes ended windows from `rate_limit_counters`.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

/// Counters in the `rate_limit_counters` table, shared by every process.
#[derive(Debug)]
pub struct PgKeyedLimiter {
    pool: PgPool,
    fallback: MemoryKeyedLimiter,
    last_cleanup: Mutex<Option<Instant>>,
}

impl PgKeyedLimiter {
    /// Counters stored through `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            fallback: MemoryKeyedLimiter::new(),
            last_cleanup: Mutex::new(None),
        }
    }

    /// Deletes windows that ended more than a day ago (no limit is that long), at most once per
    /// [`CLEANUP_INTERVAL`] per process.
    async fn cleanup(&self) {
        {
            let mut last = self
                .last_cleanup
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < CLEANUP_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        let res = sqlx::query(
            "DELETE FROM rate_limit_counters WHERE window_start < now() - interval '1 day'",
        )
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "could not delete old rate limit counters");
        }
    }

    /// `(hits in the window, seconds until it ends)` after counting `add` hits.
    async fn apply(&self, key: &str, limit: Limit, add: i32) -> sqlx::Result<(i32, f64)> {
        let secs = limit.period.as_secs_f64();
        if add == 0 {
            let row: Option<(i32, f64)> = sqlx::query_as(
                "SELECT hits, extract(epoch FROM window_start + make_interval(secs => $2) - now())::float8
                 FROM rate_limit_counters
                 WHERE key = $1 AND window_start + make_interval(secs => $2) > now()",
            )
            .bind(key)
            .bind(secs)
            .fetch_optional(&self.pool)
            .await?;
            return Ok(row.unwrap_or((0, secs)));
        }
        sqlx::query_as(
            "INSERT INTO rate_limit_counters AS c (key, window_start, hits) VALUES ($1, now(), $3)
             ON CONFLICT (key) DO UPDATE SET
                 hits = CASE WHEN c.window_start + make_interval(secs => $2) > now()
                             THEN c.hits + $3 ELSE $3 END,
                 window_start = CASE WHEN c.window_start + make_interval(secs => $2) > now()
                                     THEN c.window_start ELSE now() END
             RETURNING hits,
                 extract(epoch FROM window_start + make_interval(secs => $2) - now())::float8",
        )
        .bind(key)
        .bind(secs)
        .bind(add)
        .fetch_one(&self.pool)
        .await
    }

    async fn run(&self, key: &str, limit: Limit, count: bool) -> Result<(), Limited> {
        let (hits, left) = match self.apply(key, limit, i32::from(count)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(key, error = %e, "shared rate limiter unavailable; limiting in-process");
                return if count {
                    self.fallback.hit(key, limit).await
                } else {
                    self.fallback.check(key, limit).await
                };
            }
        };
        if count {
            self.cleanup().await;
        }
        let max = i32::try_from(limit.requests).unwrap_or(i32::MAX);
        let over = if count { hits > max } else { hits >= max };
        if over {
            return Err(limited(Duration::from_secs_f64(left.max(0.0))));
        }
        Ok(())
    }
}

#[async_trait]
impl KeyedLimiter for PgKeyedLimiter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<(), Limited> {
        self.run(key, limit, true).await
    }

    async fn check(&self, key: &str, limit: Limit) -> Result<(), Limited> {
        self.run(key, limit, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn memory_window_counts_and_resets() {
        let l = MemoryKeyedLimiter::new();
        let limit = Limit::new(2, Duration::from_secs(60));
        assert!(l.check("a", limit).await.is_ok());
        assert!(l.hit("a", limit).await.is_ok());
        assert!(l.hit("a", limit).await.is_ok());
        let err = l.hit("a", limit).await.unwrap_err();
        assert_eq!(err.retry_after, Duration::from_secs(60));
        assert!(l.check("a", limit).await.is_err());
        // Other keys are independent.
        assert!(l.hit("b", limit).await.is_ok());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(l.check("a", limit).await.is_ok());
        assert!(l.hit("a", limit).await.is_ok());
    }
}
