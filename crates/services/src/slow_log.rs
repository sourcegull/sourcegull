//! The slow request log: GraphQL requests and searches slower than
//! `observability.logSlowGraphQLRequests` / `observability.logSlowSearches` (milliseconds) are
//! logged as a structured warning and kept in `slow_requests` for the site admin "Slow
//! requests" page.

use std::time::Duration;

use sourcegull_db::{Db, NewSlowRequest};

/// Whether a request that took `elapsed` crosses `threshold_ms` (`None`: logging is off).
pub fn exceeded(threshold_ms: Option<u32>, elapsed: Duration) -> bool {
    threshold_ms.is_some_and(|t| elapsed >= Duration::from_millis(u64::from(t)))
}

/// Logs `entry` and stores it in the background.
pub fn record(db: &Db, entry: NewSlowRequest) {
    tracing::warn!(
        kind = entry.kind.as_str(),
        name = entry.name,
        duration_ms = entry.duration_ms,
        user_id = entry.user_id,
        source = entry.source,
        query = entry.query,
        "slow request"
    );
    let store = db.slow_requests();
    tokio::spawn(async move {
        if let Err(e) = store.insert(&entry).await {
            tracing::warn!(error = %e, "storing a slow request failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        assert!(!exceeded(None, Duration::from_secs(100)));
        assert!(exceeded(Some(0), Duration::ZERO));
        assert!(exceeded(Some(500), Duration::from_millis(500)));
        assert!(!exceeded(Some(500), Duration::from_millis(499)));
    }
}
