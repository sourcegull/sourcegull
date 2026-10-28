//! Code host request budgets and per-key request limits, shared by every process through
//! Postgres.
//!
//! [`PgRateLimiters`] implements [`RateLimiters`]: one token bucket per code host connection
//! in the `code_host_rate_limits` table, refilled at `rateLimit.requestsPerHour` with a burst of
//! one second's worth of requests. Every web, worker and repo process (and replica) draws from
//! the same bucket, so together they stay within the connection's budget.
//!
//! The per-request cost stays low: a process reserves tokens in batches with one call of
//! `code_host_rate_limit_take` (a single round trip that refills, takes and returns the pause)
//! and hands them out locally. A batch is sized to the process's demand over the last second
//! (at most one second of budget) and is dropped if unused after [`LOCAL_TTL`], so idle
//! processes do not sit on tokens and a pause set by another process is seen within a second.
//!
//! Pauses (`Retry-After` on a 429, `RateLimit-Remaining: 0` until `RateLimit-Reset`) are
//! written to the bucket row, so a code host's back-off request stops every process. If the
//! database is unreachable, the limiter falls back to an in-process bucket (fail open, but
//! still limited per process) and logs a warning.
//!
//! [`keyed`] limits requests per client address or account ([`KeyedLimiter`]), with an
//! in-process and a Postgres implementation.

pub mod keyed;

pub use keyed::{KeyedLimiter, Limit, Limited, MemoryKeyedLimiter, PgKeyedLimiter};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sourcegull_codehost::ratelimit::{burst_for, disabled};
use sourcegull_codehost::{
    CodeHostError, RateLimiter, RateLimiterState, RateLimiters, Result, TokenBucket,
};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// How long locally held tokens (and an unlimited bucket's "not paused" answer) stay valid.
pub const LOCAL_TTL: Duration = Duration::from_secs(1);

/// Shortest wait before asking the database again for an empty bucket.
const MIN_WAIT: Duration = Duration::from_millis(10);

/// Per-connection limiters backed by the `code_host_rate_limits` table.
#[derive(Debug)]
pub struct PgRateLimiters {
    pool: PgPool,
    limiters: StdMutex<HashMap<String, Arc<PgRateLimiter>>>,
}

impl PgRateLimiters {
    /// Limiters stored through `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            limiters: StdMutex::new(HashMap::new()),
        }
    }

    /// The limiter of `key` (created on first use) with its rate set to `requests_per_hour`.
    pub fn get(&self, key: &str, requests_per_hour: Option<f64>) -> Arc<PgRateLimiter> {
        let mut limiters = self.limiters.lock().unwrap_or_else(PoisonError::into_inner);
        let limiter = limiters
            .entry(key.to_owned())
            .or_insert_with(|| {
                Arc::new(PgRateLimiter::new(
                    self.pool.clone(),
                    key.to_owned(),
                    requests_per_hour,
                ))
            })
            .clone();
        limiter.set_rate(requests_per_hour);
        limiter
    }
}

#[async_trait]
impl RateLimiters for PgRateLimiters {
    fn limiter(&self, key: &str, requests_per_hour: Option<f64>) -> Arc<dyn RateLimiter> {
        self.get(key, requests_per_hour)
    }

    async fn state(&self, key: &str, requests_per_hour: Option<f64>) -> Result<RateLimiterState> {
        let row: Option<(Option<f64>, f64, f64, DateTime<Utc>, Option<DateTime<Utc>>)> =
            sqlx::query_as(
                "SELECT requests_per_hour, burst,
                        CASE WHEN requests_per_hour IS NULL THEN 0
                             ELSE LEAST(burst, tokens + requests_per_hour / 3600.0
                                  * GREATEST(0, extract(epoch FROM now() - replenished_at))::float8)
                        END,
                        replenished_at,
                        CASE WHEN paused_until > now() THEN paused_until END
                 FROM code_host_rate_limits WHERE key = $1",
            )
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(match row {
            // A bucket last used with another rate is reported as the configured rate's.
            Some((rph, burst, tokens, replenished, paused_until)) if rph == requests_per_hour => {
                RateLimiterState {
                    limit: rph.map_or(0, |r| r.round() as i64),
                    interval: 3600,
                    burst: if rph.is_some() {
                        burst.round() as i64
                    } else {
                        0
                    },
                    current_capacity: tokens.max(0.0).floor() as i64,
                    infinite: rph.is_none(),
                    last_replenishment: replenished,
                    paused_until,
                }
            }
            Some((.., paused_until)) => RateLimiterState {
                paused_until,
                ..RateLimiterState::full(requests_per_hour)
            },
            None => RateLimiterState::full(requests_per_hour),
        })
    }
}

fn db_error(e: sqlx::Error) -> CodeHostError {
    CodeHostError::Transport(format!("rate limiter database: {e}"))
}

/// Encodes `Option<f64>` in an atomic: `None` as NaN bits.
const UNLIMITED: u64 = u64::MAX;

#[derive(Debug)]
struct Local {
    /// Tokens reserved from the shared bucket and not handed out yet.
    tokens: f64,
    /// When `tokens` (or, unlimited, the "not paused" answer) was fetched.
    fetched_at: Option<Instant>,
    /// Acquisitions in the current and the previous one-second window (the demand estimate).
    window_start: Instant,
    window_count: u32,
    last_window_count: u32,
    /// The database failed at this time: use the fallback bucket until `LOCAL_TTL` later.
    failed_at: Option<Instant>,
}

/// One connection's limiter, shared by every process through its `code_host_rate_limits` row.
#[derive(Debug)]
pub struct PgRateLimiter {
    pool: PgPool,
    key: String,
    rate_bits: AtomicU64,
    local: Mutex<Local>,
    /// The longest pause this process knows of.
    paused_until: StdMutex<Option<Instant>>,
    /// Used while the database is unreachable.
    fallback: StdMutex<Arc<TokenBucket>>,
    round_trips: AtomicU64,
    /// Callers in `acquire` now (part of the demand estimate).
    callers: AtomicU64,
}

impl PgRateLimiter {
    fn new(pool: PgPool, key: String, requests_per_hour: Option<f64>) -> Self {
        let now = Instant::now();
        Self {
            pool,
            key,
            rate_bits: AtomicU64::new(encode(requests_per_hour)),
            local: Mutex::new(Local {
                tokens: 0.0,
                fetched_at: None,
                window_start: now,
                window_count: 0,
                last_window_count: 0,
                failed_at: None,
            }),
            paused_until: StdMutex::new(None),
            fallback: StdMutex::new(Arc::new(fallback_bucket(requests_per_hour))),
            round_trips: AtomicU64::new(0),
            callers: AtomicU64::new(0),
        }
    }

    /// The bucket key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Database round trips made so far (reservations and pauses).
    pub fn round_trips(&self) -> u64 {
        self.round_trips.load(Ordering::Relaxed)
    }

    fn rate(&self) -> Option<f64> {
        decode(self.rate_bits.load(Ordering::Relaxed))
    }

    fn set_rate(&self, requests_per_hour: Option<f64>) {
        let bits = encode(requests_per_hour);
        if self.rate_bits.swap(bits, Ordering::Relaxed) != bits {
            *self.fallback.lock().unwrap_or_else(PoisonError::into_inner) =
                Arc::new(fallback_bucket(requests_per_hour));
        }
    }

    fn local_pause(&self, now: Instant) -> Option<Instant> {
        self.paused_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .filter(|p| *p > now)
    }

    fn set_local_pause(&self, until: Instant) {
        let mut p = self
            .paused_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if p.is_none_or(|p| p < until) {
            *p = Some(until);
        }
    }

    /// One reservation: `(granted, tokens left, pause seconds)`.
    async fn take(&self, rph: Option<f64>, want: f64) -> sqlx::Result<(f64, f64, f64)> {
        self.round_trips.fetch_add(1, Ordering::Relaxed);
        sqlx::query_as("SELECT * FROM code_host_rate_limit_take($1, $2, $3)")
            .bind(&self.key)
            .bind(rph)
            .bind(want)
            .fetch_one(&self.pool)
            .await
    }
}

fn encode(rph: Option<f64>) -> u64 {
    rph.map_or(UNLIMITED, f64::to_bits)
}

fn decode(bits: u64) -> Option<f64> {
    (bits != UNLIMITED).then(|| f64::from_bits(bits))
}

fn fallback_bucket(rph: Option<f64>) -> TokenBucket {
    rph.map_or_else(TokenBucket::unlimited, TokenBucket::per_hour)
}

impl PgRateLimiter {
    async fn acquire_inner(&self, rph: Option<f64>) -> Result<()> {
        loop {
            let now = Instant::now();
            if let Some(until) = self.local_pause(now) {
                tokio::time::sleep_until(until).await;
                continue;
            }
            let mut local = self.local.lock().await;
            let now = Instant::now();
            if now.duration_since(local.window_start) >= LOCAL_TTL {
                let fresh = now.duration_since(local.window_start) < 2 * LOCAL_TTL;
                local.last_window_count = if fresh { local.window_count } else { 0 };
                local.window_count = 0;
                local.window_start = now;
            }
            if local
                .failed_at
                .is_some_and(|t| now.duration_since(t) < LOCAL_TTL)
            {
                drop(local);
                let fallback = self
                    .fallback
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                return fallback.acquire().await;
            }
            let fresh = local
                .fetched_at
                .is_some_and(|t| now.duration_since(t) < LOCAL_TTL);
            if !fresh {
                local.tokens = 0.0;
            }
            if fresh && (rph.is_none() || local.tokens >= 1.0) {
                if rph.is_some() {
                    local.tokens -= 1.0;
                }
                local.window_count += 1;
                return Ok(());
            }
            // Reserve a batch sized to the recent demand, at most one second of budget.
            let max_batch = rph.map_or(1.0, |r| burst_for(r).floor().max(1.0));
            let callers = u32::try_from(self.callers.load(Ordering::Relaxed)).unwrap_or(u32::MAX);
            let want = f64::from(
                local
                    .last_window_count
                    .max(local.window_count + 1)
                    .max(callers),
            )
            .clamp(1.0, max_batch);
            let (granted, left, pause) = match self.take(rph, want).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %self.key, error = %e, "shared rate limiter unavailable; limiting in-process");
                    local.failed_at = Some(Instant::now());
                    continue;
                }
            };
            local.failed_at = None;
            let now = Instant::now();
            if pause > 0.0 {
                drop(local);
                self.set_local_pause(now + Duration::from_secs_f64(pause));
                continue;
            }
            if granted >= 1.0 || rph.is_none() {
                local.tokens = granted - 1.0;
                local.fetched_at = Some(now);
                local.window_count += 1;
                return Ok(());
            }
            // Not enough tokens: wait until the batch has accrued (at most about a second, as
            // a batch is at most a second of budget), keeping our place in line, so a
            // contended bucket costs one round trip per batch rather than per poll.
            let rate = rph.unwrap_or(1.0) / 3600.0;
            let wait = Duration::from_secs_f64(((want - left).max(0.0) / rate).min(3600.0));
            tokio::time::sleep(wait.max(MIN_WAIT)).await;
        }
    }
}

#[async_trait]
impl RateLimiter for PgRateLimiter {
    async fn acquire(&self) -> Result<()> {
        let rph = self.rate();
        if rph == Some(0.0) {
            return Err(disabled());
        }
        self.callers.fetch_add(1, Ordering::Relaxed);
        let result = self.acquire_inner(rph).await;
        self.callers.fetch_sub(1, Ordering::Relaxed);
        result
    }

    async fn pause_until(&self, until: Instant) {
        self.set_local_pause(until);
        let secs = until
            .saturating_duration_since(Instant::now())
            .as_secs_f64();
        if secs <= 0.0 {
            return;
        }
        let rph = self.rate();
        let burst = rph.map_or(1.0, burst_for);
        self.round_trips.fetch_add(1, Ordering::Relaxed);
        let res = sqlx::query(
            "INSERT INTO code_host_rate_limits AS r
                 (key, requests_per_hour, burst, tokens, paused_until)
             VALUES ($1, $2, $3, $3, now() + make_interval(secs => $4))
             ON CONFLICT (key) DO UPDATE SET
                 paused_until = GREATEST(r.paused_until, EXCLUDED.paused_until),
                 updated_at = now()",
        )
        .bind(&self.key)
        .bind(rph)
        .bind(burst)
        .bind(secs)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::warn!(key = %self.key, error = %e, "could not share a code host pause");
        }
    }
}
