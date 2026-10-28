//! Request budgets for code host API clients.
//!
//! Every API call first awaits [`RateLimiter::acquire`]. When the code host signals a limit
//! (HTTP 429 with `Retry-After`, or `RateLimit-Remaining: 0` with `RateLimit-Reset`), the
//! client calls [`RateLimiter::pause_until`] so every caller sharing the limiter waits.
//!
//! Limiters come from a [`RateLimiters`] registry, one per code host connection (keyed by
//! [`connection_key`]), so every client built for a connection draws from the same budget:
//!
//! - [`InProcessLimiters`] keeps [`TokenBucket`]s in memory: one budget per process. The
//!   default, right for a single-process install.
//! - The Postgres-backed registry in the `sourcegull-ratelimit` crate shares one bucket per
//!   connection between every process (web, worker, repo and their replicas).
//!
//! The process-wide registry ([`global`]) is in-process until startup installs another with
//! [`set_global`].

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::{CodeHostError, Result};

/// A shared request budget.
#[async_trait]
pub trait RateLimiter: Send + Sync + std::fmt::Debug {
    /// Waits until one request may be sent. Fails if requests are disabled entirely.
    async fn acquire(&self) -> Result<()>;

    /// Blocks every caller until `until` (the code host asked us to back off).
    async fn pause_until(&self, until: Instant);
}

/// A snapshot of a limiter, for the admin UI.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimiterState {
    /// Requests allowed per `interval` (`rateLimit.requestsPerHour`); 0 when unlimited.
    pub limit: i64,
    /// The refill interval in seconds (always 3600: the limit is per hour).
    pub interval: i64,
    /// The bucket size: the most requests that can be sent at once.
    pub burst: i64,
    /// Tokens in the bucket now.
    pub current_capacity: i64,
    /// The limiter does not limit (`rateLimit.enabled: false`).
    pub infinite: bool,
    /// When the bucket was last refilled.
    pub last_replenishment: DateTime<Utc>,
    /// Until when the code host asked every caller to back off, if it did.
    pub paused_until: Option<DateTime<Utc>>,
}

impl RateLimiterState {
    /// The state of an untouched (full) bucket for `requests_per_hour`.
    pub fn full(requests_per_hour: Option<f64>) -> Self {
        let (limit, burst) = match requests_per_hour {
            Some(rph) => (rph.max(0.0), burst_for(rph)),
            None => (0.0, 0.0),
        };
        Self {
            limit: limit.round() as i64,
            interval: 3600,
            burst: burst.round() as i64,
            current_capacity: burst.floor() as i64,
            infinite: requests_per_hour.is_none(),
            last_replenishment: Utc::now(),
            paused_until: None,
        }
    }
}

/// The bucket size for `requests_per_hour`: one second's worth of requests, at least one.
pub fn burst_for(requests_per_hour: f64) -> f64 {
    (requests_per_hour.max(0.0) / 3600.0).max(1.0)
}

/// The limiter key of a code host connection (an external service id).
pub fn connection_key(external_service_id: i64) -> String {
    format!("extsvc:{external_service_id}")
}

/// Hands out the limiter of each code host connection.
#[async_trait]
pub trait RateLimiters: Send + Sync + std::fmt::Debug {
    /// The limiter for `key` allowing `requests_per_hour` (`None` = unlimited). Every call
    /// with the same key returns a limiter drawing from the same budget; a changed rate
    /// applies to it from now on.
    fn limiter(&self, key: &str, requests_per_hour: Option<f64>) -> Arc<dyn RateLimiter>;

    /// The current state of `key`'s budget; a full bucket when it has not been used yet.
    async fn state(&self, key: &str, requests_per_hour: Option<f64>) -> Result<RateLimiterState>;
}

fn global_slot() -> &'static RwLock<Arc<dyn RateLimiters>> {
    static GLOBAL: OnceLock<RwLock<Arc<dyn RateLimiters>>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(Arc::new(InProcessLimiters::default())))
}

/// The process-wide limiter registry: in-process unless [`set_global`] replaced it.
pub fn global() -> Arc<dyn RateLimiters> {
    global_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Replaces the process-wide limiter registry (at startup, before clients are built).
pub fn set_global(limiters: Arc<dyn RateLimiters>) {
    *global_slot().write().unwrap_or_else(|e| e.into_inner()) = limiters;
}

type BucketEntry = (Option<u64>, Arc<TokenBucket>);

/// In-process limiters: one [`TokenBucket`] per key.
#[derive(Debug, Default)]
pub struct InProcessLimiters {
    /// Key → (requests per hour as bits, bucket).
    buckets: std::sync::Mutex<HashMap<String, BucketEntry>>,
}

impl InProcessLimiters {
    fn bucket(&self, key: &str, requests_per_hour: Option<f64>) -> Arc<TokenBucket> {
        let rate_bits = requests_per_hour.map(f64::to_bits);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        match buckets.get(key) {
            Some((bits, bucket)) if *bits == rate_bits => bucket.clone(),
            _ => {
                let bucket = Arc::new(match requests_per_hour {
                    Some(rph) => TokenBucket::per_hour(rph),
                    None => TokenBucket::unlimited(),
                });
                buckets.insert(key.to_owned(), (rate_bits, bucket.clone()));
                bucket
            }
        }
    }
}

#[async_trait]
impl RateLimiters for InProcessLimiters {
    fn limiter(&self, key: &str, requests_per_hour: Option<f64>) -> Arc<dyn RateLimiter> {
        self.bucket(key, requests_per_hour)
    }

    async fn state(&self, key: &str, requests_per_hour: Option<f64>) -> Result<RateLimiterState> {
        let existing = {
            let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
            buckets.get(key).cloned()
        };
        match existing {
            Some((bits, bucket)) if bits == requests_per_hour.map(f64::to_bits) => {
                Ok(bucket.state().await)
            }
            _ => Ok(RateLimiterState::full(requests_per_hour)),
        }
    }
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    paused_until: Option<Instant>,
}

/// An in-process token bucket: `requests_per_hour` spread evenly, with a burst of up to one
/// second's worth of requests (at least one).
#[derive(Debug)]
pub struct TokenBucket {
    /// Refill rate in tokens per second; `None` = unlimited.
    rate: Option<f64>,
    burst: f64,
    state: Mutex<Bucket>,
}

impl TokenBucket {
    /// A bucket allowing `requests_per_hour` (0 disables requests entirely).
    pub fn per_hour(requests_per_hour: f64) -> Self {
        let rate = requests_per_hour.max(0.0) / 3600.0;
        let burst = burst_for(requests_per_hour);
        Self {
            rate: Some(rate),
            burst,
            state: Mutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
                paused_until: None,
            }),
        }
    }

    /// A bucket that never limits (but still honours [`RateLimiter::pause_until`]).
    pub fn unlimited() -> Self {
        Self {
            rate: None,
            burst: 0.0,
            state: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
                paused_until: None,
            }),
        }
    }

    /// A snapshot of the bucket (refilled up to now, without taking anything).
    pub async fn state(&self) -> RateLimiterState {
        let b = self.state.lock().await;
        let now = Instant::now();
        let since_last = now.saturating_duration_since(b.last);
        let tokens = match self.rate {
            Some(rate) => (b.tokens + since_last.as_secs_f64() * rate).min(self.burst),
            None => 0.0,
        };
        let wall = Utc::now();
        let paused_until = b
            .paused_until
            .filter(|p| *p > now)
            .and_then(|p| chrono::Duration::from_std(p - now).ok())
            .map(|d| wall + d);
        RateLimiterState {
            limit: self.rate.map_or(0, |r| (r * 3600.0).round() as i64),
            interval: 3600,
            burst: self.burst.round() as i64,
            current_capacity: tokens.floor() as i64,
            infinite: self.rate.is_none(),
            last_replenishment: wall - chrono::Duration::from_std(since_last).unwrap_or_default(),
            paused_until,
        }
    }
}

#[async_trait]
impl RateLimiter for TokenBucket {
    async fn acquire(&self) -> Result<()> {
        if self.rate == Some(0.0) {
            return Err(disabled());
        }
        loop {
            let wait = {
                let mut b = self.state.lock().await;
                let now = Instant::now();
                match (b.paused_until, self.rate) {
                    (Some(until), _) if until > now => until - now,
                    (_, None) => {
                        b.paused_until = None;
                        return Ok(());
                    }
                    (_, Some(rate)) => {
                        b.paused_until = None;
                        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
                        b.tokens = (b.tokens + elapsed * rate).min(self.burst);
                        b.last = now;
                        if b.tokens >= 1.0 {
                            b.tokens -= 1.0;
                            return Ok(());
                        }
                        Duration::from_secs_f64((1.0 - b.tokens) / rate)
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn pause_until(&self, until: Instant) {
        let mut b = self.state.lock().await;
        if b.paused_until.is_none_or(|p| p < until) {
            b.paused_until = Some(until);
        }
    }
}

/// The error of a limiter whose `rateLimit.requestsPerHour` is 0.
pub fn disabled() -> CodeHostError {
    CodeHostError::Config(
        "rateLimit.requestsPerHour is 0: requests to this code host are disabled".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn bucket_spreads_requests() {
        // 7200/h = 2/s, burst 2.
        let b = TokenBucket::per_hour(7200.0);
        let start = Instant::now();
        for _ in 0..4 {
            b.acquire().await.unwrap();
        }
        // Two from the burst, two more at 0.5 s intervals.
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_blocks_everyone_and_zero_disables() {
        let b = TokenBucket::unlimited();
        let start = Instant::now();
        b.pause_until(start + Duration::from_secs(5)).await;
        b.pause_until(start + Duration::from_secs(1)).await; // never shortens a pause
        b.acquire().await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        assert!(TokenBucket::per_hour(0.0).acquire().await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn registry_shares_a_bucket_per_key() {
        let reg = InProcessLimiters::default();
        // 3600/h = 1/s, burst 1: two limiters for one key share the single token.
        let a = reg.limiter("extsvc:1", Some(3600.0));
        let b = reg.limiter("extsvc:1", Some(3600.0));
        let other = reg.limiter("extsvc:2", Some(3600.0));
        let start = Instant::now();
        a.acquire().await.unwrap();
        other.acquire().await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
        b.acquire().await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(1));

        let s = reg.state("extsvc:1", Some(3600.0)).await.unwrap();
        assert_eq!((s.limit, s.burst, s.current_capacity), (3600, 1, 0));
        assert!(!s.infinite);
        // A changed rate starts a new bucket.
        let s = reg.state("extsvc:1", Some(7200.0)).await.unwrap();
        assert_eq!((s.limit, s.burst, s.current_capacity), (7200, 2, 2));
        assert!(reg.state("extsvc:9", None).await.unwrap().infinite);
    }

    #[tokio::test(start_paused = true)]
    async fn state_reports_pause() {
        let b = TokenBucket::per_hour(36_000.0);
        b.acquire().await.unwrap();
        let s = b.state().await;
        assert_eq!((s.limit, s.burst, s.current_capacity), (36_000, 10, 9));
        assert!(s.paused_until.is_none());
        b.pause_until(Instant::now() + Duration::from_secs(30))
            .await;
        let s = b.state().await;
        let left = s.paused_until.unwrap() - Utc::now();
        assert!(
            left.num_seconds() > 25 && left.num_seconds() <= 30,
            "{left}"
        );
    }
}
