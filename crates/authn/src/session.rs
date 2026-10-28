//! Server-side sessions behind the [`SessionStore`] trait.
//!
//! The cookie carries a random session id ([`SessionToken`]); stores keep only
//! its SHA-256. [`PgSessionStore`] persists sessions in Postgres with a short
//! in-process cache; [`MemorySessionStore`] keeps them in memory (tests and
//! single-process setups).

use crate::AuthError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::Db;
use sourcegull_types::UserId;
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

/// The plaintext session id, to be put in the session cookie. Its `Debug`
/// output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// Wraps a cookie value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The cookie value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the token, returning the cookie value.
    pub fn into_string(self) -> String {
        self.0
    }

    fn hash(&self) -> [u8; 32] {
        hash_session_id(&self.0)
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

impl From<String> for SessionToken {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionToken {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// A live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// The signed-in user.
    pub user_id: UserId,
    /// When the session ends.
    pub expires_at: DateTime<Utc>,
    /// When the session was created (sign-in time).
    pub created_at: DateTime<Utc>,
}

/// Storage for sessions. Implementations store only a hash of the id.
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Creates a session for `user_id` valid for `ttl`; returns the cookie value.
    async fn create(&self, user_id: UserId, ttl: Duration) -> Result<SessionToken, AuthError>;
    /// The session for a cookie value, if it exists and has not expired.
    async fn get(&self, token: &SessionToken) -> Result<Option<Session>, AuthError>;
    /// Ends a session (no-op if it does not exist).
    async fn delete(&self, token: &SessionToken) -> Result<(), AuthError>;
    /// Ends all sessions of a user.
    async fn delete_for_user(&self, user_id: UserId) -> Result<(), AuthError>;
    /// Ends all sessions of a user except `keep`.
    async fn delete_other_sessions(
        &self,
        user_id: UserId,
        keep: &SessionToken,
    ) -> Result<(), AuthError>;
}

fn expiry(ttl: Duration) -> Result<DateTime<Utc>, AuthError> {
    chrono::Duration::from_std(ttl)
        .ok()
        .and_then(|d| Utc::now().checked_add_signed(d))
        .ok_or_else(|| AuthError::InvalidInput("session lifetime is too long".into()))
}

/// How long [`PgSessionStore`] caches a session in process by default.
pub const DEFAULT_SESSION_CACHE_TTL: Duration = Duration::from_secs(30);

/// Postgres-backed sessions with an in-process cache.
///
/// Cached entries live for a short TTL (default 30 s) and are invalidated by
/// [`delete`](SessionStore::delete) / [`delete_for_user`](SessionStore::delete_for_user)
/// in this process. A session deleted by another process may therefore stay
/// valid here for up to the cache TTL.
#[derive(Clone)]
pub struct PgSessionStore {
    db: Db,
    cache: moka::future::Cache<[u8; 32], Session>,
}

impl fmt::Debug for PgSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSessionStore").finish_non_exhaustive()
    }
}

impl PgSessionStore {
    /// Creates a store with the default cache TTL.
    pub fn new(db: Db) -> Self {
        Self::with_cache_ttl(db, DEFAULT_SESSION_CACHE_TTL)
    }

    /// Creates a store whose cache keeps entries for `cache_ttl`.
    pub fn with_cache_ttl(db: Db, cache_ttl: Duration) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(100_000)
            .time_to_live(cache_ttl)
            .support_invalidation_closures()
            .build();
        Self { db, cache }
    }

    /// Deletes expired session rows; returns how many. Run periodically.
    pub async fn delete_expired(&self) -> Result<u64, AuthError> {
        Ok(self.db.sessions().delete_expired().await?)
    }
}

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn create(&self, user_id: UserId, ttl: Duration) -> Result<SessionToken, AuthError> {
        let token = SessionToken(generate_session_id());
        let hash = token.hash();
        let expires_at = expiry(ttl)?;
        let created_at = self
            .db
            .sessions()
            .insert(&hash, Some(user_id), expires_at, &serde_json::json!({}))
            .await?;
        let session = Session {
            user_id,
            expires_at,
            created_at,
        };
        self.cache.insert(hash, session).await;
        Ok(token)
    }

    async fn get(&self, token: &SessionToken) -> Result<Option<Session>, AuthError> {
        let hash = token.hash();
        if let Some(s) = self.cache.get(&hash).await {
            if s.expires_at > Utc::now() {
                return Ok(Some(s));
            }
            self.cache.invalidate(&hash).await;
            return Ok(None);
        }
        let Some(row) = self.db.sessions().get(&hash).await? else {
            return Ok(None);
        };
        // Anonymous sessions are not used for authentication.
        let Some(user_id) = row.user_id else {
            return Ok(None);
        };
        let session = Session {
            user_id,
            expires_at: row.expires_at,
            created_at: row.created_at,
        };
        self.cache.insert(hash, session).await;
        Ok(Some(session))
    }

    async fn delete(&self, token: &SessionToken) -> Result<(), AuthError> {
        let hash = token.hash();
        self.cache.invalidate(&hash).await;
        self.db.sessions().delete(&hash).await?;
        Ok(())
    }

    async fn delete_for_user(&self, user_id: UserId) -> Result<(), AuthError> {
        self.cache
            .invalidate_entries_if(move |_, s| s.user_id == user_id)
            .map_err(AuthError::internal)?;
        self.db.sessions().delete_for_user(user_id).await?;
        Ok(())
    }

    async fn delete_other_sessions(
        &self,
        user_id: UserId,
        keep: &SessionToken,
    ) -> Result<(), AuthError> {
        let keep = keep.hash();
        self.cache
            .invalidate_entries_if(move |h, s| s.user_id == user_id && *h != keep)
            .map_err(AuthError::internal)?;
        self.db
            .sessions()
            .delete_for_user_except(user_id, &keep)
            .await?;
        Ok(())
    }
}

/// In-memory sessions for tests and single-process deployments. Lost on restart.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<[u8; 32], Session>>,
}

impl MemorySessionStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Session>> {
        // The map stays consistent even if a holder panicked.
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn create(&self, user_id: UserId, ttl: Duration) -> Result<SessionToken, AuthError> {
        let token = SessionToken(generate_session_id());
        let session = Session {
            user_id,
            expires_at: expiry(ttl)?,
            created_at: Utc::now(),
        };
        let mut map = self.lock();
        let now = Utc::now();
        map.retain(|_, s| s.expires_at > now);
        map.insert(token.hash(), session);
        Ok(token)
    }

    async fn get(&self, token: &SessionToken) -> Result<Option<Session>, AuthError> {
        let now = Utc::now();
        Ok(self
            .lock()
            .get(&token.hash())
            .copied()
            .filter(|s| s.expires_at > now))
    }

    async fn delete(&self, token: &SessionToken) -> Result<(), AuthError> {
        self.lock().remove(&token.hash());
        Ok(())
    }

    async fn delete_for_user(&self, user_id: UserId) -> Result<(), AuthError> {
        self.lock().retain(|_, s| s.user_id != user_id);
        Ok(())
    }

    async fn delete_other_sessions(
        &self,
        user_id: UserId,
        keep: &SessionToken,
    ) -> Result<(), AuthError> {
        let keep = keep.hash();
        self.lock()
            .retain(|h, s| s.user_id != user_id || *h == keep);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_lifecycle() {
        let store = MemorySessionStore::new();
        let a = store
            .create(UserId(1), Duration::from_secs(60))
            .await
            .unwrap();
        let b = store
            .create(UserId(1), Duration::from_secs(60))
            .await
            .unwrap();
        let c = store
            .create(UserId(2), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(store.get(&a).await.unwrap().unwrap().user_id, UserId(1));
        assert!(
            store
                .get(&SessionToken::from("nope"))
                .await
                .unwrap()
                .is_none()
        );
        store.delete(&a).await.unwrap();
        assert!(store.get(&a).await.unwrap().is_none());
        store.delete_for_user(UserId(1)).await.unwrap();
        assert!(store.get(&b).await.unwrap().is_none());
        assert!(store.get(&c).await.unwrap().is_some());
        let expired = store.create(UserId(3), Duration::ZERO).await.unwrap();
        assert!(store.get(&expired).await.unwrap().is_none());
        assert_eq!(format!("{c:?}"), "SessionToken(<redacted>)");
    }

    #[tokio::test]
    async fn memory_store_delete_other_sessions() {
        let store = MemorySessionStore::new();
        let ttl = Duration::from_secs(60);
        let keep = store.create(UserId(1), ttl).await.unwrap();
        let other = store.create(UserId(1), ttl).await.unwrap();
        let someone_else = store.create(UserId(2), ttl).await.unwrap();
        store.delete_other_sessions(UserId(1), &keep).await.unwrap();
        assert!(store.get(&keep).await.unwrap().is_some());
        assert!(store.get(&other).await.unwrap().is_none());
        assert!(store.get(&someone_else).await.unwrap().is_some());
    }
}
