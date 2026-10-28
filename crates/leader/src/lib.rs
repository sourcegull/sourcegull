//! Leader election for singleton tasks.
//!
//! Singleton tasks (repo discovery, schedulers, enqueuers, janitors) run under
//! a lease via [`run_singleton`]. The lease is renewed every few seconds and
//! the task is cancelled as soon as a renewal fails.
//!
//! Backends implement [`LeaseBackend`]. [`PgAdvisoryLease`] uses a Postgres
//! session-level advisory lock and is the Docker default; a process's leases
//! share one session through a [`PgLeaseManager`].
//! [`kubernetes::KubernetesLease`] uses a `coordination.k8s.io/v1` `Lease`
//! object and is the Helm default. [`Leases`] picks one per process.
//!
//! Each time it takes a lease, [`run_singleton`] advances the lease's epoch in Postgres
//! (whatever the backend) and hands the task a [`Fence`]; the task's writes that a stale
//! leader must not make check it (see [`sourcegull_db::Fence`]).

pub mod kubernetes;

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
pub use sourcegull_db::Fence;
use sqlx::postgres::{PgConnectOptions, PgPool};
use sqlx::{Connection, PgConnection};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

/// Default renewal interval.
pub const DEFAULT_RENEW_INTERVAL: Duration = Duration::from_secs(5);

/// Errors from a lease backend.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    /// Database error; the lease, if held, is considered lost.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// The backend did not answer in time; the lease, if held, is considered lost.
    #[error("lease backend timed out")]
    Timeout,
    /// Kubernetes API error; the lease, if held, is considered lost.
    #[error("kubernetes: {0}")]
    Kubernetes(String),
}

/// A named, exclusive lease.
#[async_trait]
pub trait LeaseBackend: Send + Sync {
    /// Tries to take the lease. `Ok(true)` means this holder now leads.
    async fn try_acquire(&self) -> Result<bool, LeaseError>;
    /// Confirms the lease is still held. Anything but `Ok(true)` means it is lost.
    async fn renew(&self) -> Result<bool, LeaseError>;
    /// Gives the lease up. Best effort.
    async fn release(&self);
    /// Resolves once a held lease is known to be lost without a renewal (e.g. its shared
    /// connection failed), or at once if it isn't held. Never resolves by default.
    async fn lost(&self) {
        std::future::pending::<()>().await;
    }
}

#[async_trait]
impl LeaseBackend for Box<dyn LeaseBackend> {
    async fn try_acquire(&self) -> Result<bool, LeaseError> {
        (**self).try_acquire().await
    }
    async fn renew(&self) -> Result<bool, LeaseError> {
        (**self).renew().await
    }
    async fn release(&self) {
        (**self).release().await;
    }
    async fn lost(&self) {
        (**self).lost().await;
    }
}

/// Where a process's singleton leases live.
#[derive(Debug, Clone)]
pub enum Leases {
    /// Postgres advisory locks, all on one shared session.
    Postgres(PgLeaseManager),
    /// Kubernetes `Lease` objects.
    Kubernetes(kubernetes::KubernetesLeases),
}

impl Leases {
    /// Advisory-lock leases using the same connection settings as `pool`.
    pub fn postgres(pool: &PgPool) -> Self {
        Self::Postgres(PgLeaseManager::from_pool(pool))
    }

    /// The lease named `name`.
    pub fn lease(&self, name: &str) -> Box<dyn LeaseBackend> {
        match self {
            Self::Postgres(m) => Box::new(m.lease(name)),
            Self::Kubernetes(k) => Box::new(k.lease(name)),
        }
    }
}

/// Stable 64-bit advisory-lock key for a lease name (FNV-1a over
/// `"sourcegull.leader:" + name`), identical across builds and processes.
pub fn advisory_key(name: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = b"sourcegull.leader:"
        .iter()
        .chain(name.as_bytes())
        .fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME));
    i64::from_ne_bytes(hash.to_ne_bytes()) // bit-preserving reinterpretation
}

/// The session all of a [`PgLeaseManager`]'s advisory locks live on.
struct Session {
    conn: PgConnection,
    /// Keys locked on `conn`, one lock each.
    held: HashSet<i64>,
}

struct ManagerState {
    session: Option<Session>,
    /// Bumped (and broadcast) whenever the session is dropped, so every lease
    /// acquired in an earlier epoch is lost.
    epoch: u64,
}

struct ManagerInner {
    options: PgConnectOptions,
    state: Mutex<ManagerState>,
    epoch_tx: watch::Sender<u64>,
}

impl ManagerInner {
    /// Drops the session: Postgres releases every lock on it, and every lease
    /// held on it is lost at once.
    fn drop_session(&self, state: &mut ManagerState) {
        if let Some(session) = state.session.take() {
            if !session.held.is_empty() {
                tracing::warn!(
                    leases = session.held.len(),
                    "lease connection lost; all leases on it are lost"
                );
            }
        }
        state.epoch += 1;
        self.epoch_tx.send_replace(state.epoch);
    }
}

/// Shares one dedicated, unpooled Postgres session among all of a process's
/// advisory-lock leases.
///
/// Advisory locks belong to a session, so each lease is a
/// `pg_try_advisory_lock` key on this one connection. When the connection
/// fails, Postgres releases all its locks and every lease held on it is
/// reported lost at once (see [`LeaseBackend::lost`]); the next
/// [`try_acquire`](LeaseBackend::try_acquire) reconnects. Releasing one lease
/// unlocks only its key. The connection must go to Postgres directly, not
/// through PgBouncer. Cloning shares the session.
#[derive(Clone)]
pub struct PgLeaseManager {
    inner: Arc<ManagerInner>,
    timeout: Duration,
}

impl std::fmt::Debug for PgLeaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLeaseManager")
            .field("host", &self.inner.options.get_host())
            .field("database", &self.inner.options.get_database())
            .finish_non_exhaustive()
    }
}

impl PgLeaseManager {
    /// A manager connecting with `options` when a lease first needs it.
    pub fn new(options: PgConnectOptions) -> Self {
        let (epoch_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(ManagerInner {
                options: options.application_name("sourcegull-leader"),
                state: Mutex::new(ManagerState {
                    session: None,
                    epoch: 0,
                }),
                epoch_tx,
            }),
            timeout: Duration::from_secs(5),
        }
    }

    /// A manager using the same connection settings as `pool` (but its own connection).
    pub fn from_pool(pool: &PgPool) -> Self {
        Self::new((*pool.connect_options()).clone())
    }

    /// Bounds each database round trip of leases made after this (default 5 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The lease named `name` on this manager's session.
    pub fn lease(&self, name: impl Into<String>) -> PgAdvisoryLease {
        let name = name.into();
        PgAdvisoryLease {
            key: advisory_key(&name),
            name,
            manager: self.inner.clone(),
            timeout: self.timeout,
            epoch: Mutex::new(None),
        }
    }

    /// The backend PID of the current session, if connected (for tests and diagnostics).
    pub async fn backend_pid(&self) -> Option<i32> {
        let mut state = self.inner.state.lock().await;
        let session = state.session.as_mut()?;
        let q = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut session.conn);
        match tokio::time::timeout(self.timeout, q).await {
            Ok(Ok(pid)) => Some(pid),
            _ => {
                self.inner.drop_session(&mut state);
                None
            }
        }
    }
}

/// A lease backed by `pg_try_advisory_lock` on a [`PgLeaseManager`]'s shared session.
pub struct PgAdvisoryLease {
    name: String,
    key: i64,
    manager: Arc<ManagerInner>,
    timeout: Duration,
    /// The manager epoch in which this lease was acquired, while held.
    epoch: Mutex<Option<u64>>,
}

impl PgAdvisoryLease {
    /// A lease named `name` on a session of its own, connecting with `options` when needed.
    /// Use [`PgLeaseManager::lease`] to share one session among many leases.
    pub fn new(options: PgConnectOptions, name: impl Into<String>) -> Self {
        PgLeaseManager::new(options).lease(name)
    }

    /// A lease on a session of its own, using the same connection settings as `pool`.
    pub fn from_pool(pool: &PgPool, name: impl Into<String>) -> Self {
        Self::new((*pool.connect_options()).clone(), name)
    }

    /// Bounds each database round trip (default 5 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The lease name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The advisory-lock key.
    pub fn key(&self) -> i64 {
        self.key
    }

    async fn bounded<F, T>(&self, fut: F) -> Result<T, LeaseError>
    where
        F: Future<Output = Result<T, sqlx::Error>>,
    {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(LeaseError::Timeout),
        }
    }

    /// Whether this lease holds its key on the manager's current session.
    fn holds(&self, mine: Option<u64>, state: &ManagerState) -> bool {
        mine == Some(state.epoch)
            && state
                .session
                .as_ref()
                .is_some_and(|s| s.held.contains(&self.key))
    }
}

#[async_trait]
impl LeaseBackend for PgAdvisoryLease {
    async fn try_acquire(&self) -> Result<bool, LeaseError> {
        let mut mine = self.epoch.lock().await;
        let mut state = self.manager.state.lock().await;
        if self.holds(*mine, &state) {
            drop(state);
            drop(mine);
            return self.renew().await;
        }
        *mine = None;
        if state.session.is_none() {
            let conn = self
                .bounded(PgConnection::connect_with(&self.manager.options))
                .await?;
            state.session = Some(Session {
                conn,
                held: HashSet::new(),
            });
        }
        let epoch = state.epoch;
        let Some(session) = state.session.as_mut() else {
            return Ok(false);
        };
        if session.held.contains(&self.key) {
            // Another lease of this manager holds the key; advisory locks are
            // reentrant within a session, so don't lock it twice.
            return Ok(false);
        }
        let q = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(self.key)
            .fetch_one(&mut session.conn);
        match self.bounded(q).await {
            Ok(true) => {
                session.held.insert(self.key);
                *mine = Some(epoch);
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e) => {
                self.manager.drop_session(&mut state);
                Err(e)
            }
        }
    }

    async fn renew(&self) -> Result<bool, LeaseError> {
        let mut mine = self.epoch.lock().await;
        let mut state = self.manager.state.lock().await;
        if !self.holds(*mine, &state) {
            *mine = None;
            return Ok(false);
        }
        let Some(session) = state.session.as_mut() else {
            return Ok(false);
        };
        // A bigint advisory lock shows in pg_locks as classid = high 32 bits,
        // objid = low 32 bits, objsubid = 1.
        let q = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_locks
                WHERE locktype = 'advisory' AND granted AND pid = pg_backend_pid()
                  AND objsubid = 1
                  AND classid::bigint = ($1 >> 32) & 4294967295
                  AND objid::bigint = $1 & 4294967295)",
        )
        .bind(self.key)
        .fetch_one(&mut session.conn);
        match self.bounded(q).await {
            Ok(true) => Ok(true),
            Ok(false) => {
                session.held.remove(&self.key);
                *mine = None;
                Ok(false)
            }
            Err(e) => {
                // Dropping the connection also releases every lock on it server-side.
                *mine = None;
                self.manager.drop_session(&mut state);
                Err(e)
            }
        }
    }

    async fn release(&self) {
        let mut mine = self.epoch.lock().await;
        let mut state = self.manager.state.lock().await;
        let held = self.holds(*mine, &state);
        *mine = None;
        if !held {
            return;
        }
        let Some(session) = state.session.as_mut() else {
            return;
        };
        session.held.remove(&self.key);
        let q = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .fetch_one(&mut session.conn);
        if !matches!(self.bounded(q).await, Ok(true)) {
            // We can't tell whether the key is still locked; start over on a
            // fresh session rather than leak it.
            self.manager.drop_session(&mut state);
        }
    }

    async fn lost(&self) {
        let Some(mine) = *self.epoch.lock().await else {
            return;
        };
        let mut rx = self.manager.epoch_tx.subscribe();
        let _ = rx.wait_for(|e| *e != mine).await;
    }
}

/// Told when this process takes or gives up a singleton's lease, e.g. to show which process
/// runs each routine on the site admin "Background jobs" page. Called inline from the
/// lease loop, so implementations must return quickly (spawn any I/O).
pub trait LeaseObserver: Send + Sync {
    /// This process now holds the lease `name` and starts its task.
    fn acquired(&self, name: &str);
    /// This process stopped the task of lease `name` and released the lease.
    fn released(&self, name: &str);
}

fn observer_slot() -> &'static RwLock<Option<Arc<dyn LeaseObserver>>> {
    static OBSERVER: OnceLock<RwLock<Option<Arc<dyn LeaseObserver>>>> = OnceLock::new();
    OBSERVER.get_or_init(|| RwLock::new(None))
}

/// Installs the process-wide [`LeaseObserver`] (at startup).
pub fn set_lease_observer(observer: Arc<dyn LeaseObserver>) {
    *observer_slot().write().unwrap_or_else(|e| e.into_inner()) = Some(observer);
}

fn observer() -> Option<Arc<dyn LeaseObserver>> {
    observer_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Runs `task_factory` while `backend` holds the lease, until `cancel` fires.
///
/// Loops: try to acquire every `renew_interval`; once acquired, advance the
/// lease's epoch on `epochs` (when given; the task gets [`Fence::none`]
/// otherwise), start a task with a child token and the [`Fence`], and renew
/// every `renew_interval`. When the epoch cannot be advanced the lease is
/// released and acquisition retried. When a renewal fails
/// the token is cancelled at once, the task is awaited (aborted if it takes
/// longer than `renew_interval` to stop), the lease is released and
/// acquisition is retried. A task that returns on its own also releases the
/// lease and is restarted on the next acquisition.
pub async fn run_singleton<B, F, Fut>(
    name: &str,
    backend: B,
    epochs: Option<PgPool>,
    renew_interval: Duration,
    mut task_factory: F,
    cancel: CancellationToken,
) where
    B: LeaseBackend,
    F: FnMut(CancellationToken, Fence) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    while !cancel.is_cancelled() {
        match backend.try_acquire().await {
            Ok(true) => {
                let fence = match &epochs {
                    Some(pool) => match Fence::advance(pool, name).await {
                        Ok(fence) => fence,
                        Err(e) => {
                            tracing::warn!(lease = name, error = %e, "advancing the lease epoch failed");
                            backend.release().await;
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(renew_interval) => {}
                            }
                            continue;
                        }
                    },
                    None => Fence::none(name),
                };
                tracing::info!(lease = name, epoch = ?fence.epoch(), "acquired lease; starting singleton");
                let observer = observer();
                if let Some(o) = &observer {
                    o.acquired(name);
                }
                lead(
                    name,
                    &backend,
                    renew_interval,
                    &mut task_factory,
                    fence,
                    &cancel,
                )
                .await;
                backend.release().await;
                if let Some(o) = &observer {
                    o.released(name);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(lease = name, error = %e, "lease acquisition failed"),
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(renew_interval) => {}
        }
    }
    backend.release().await;
}

/// Runs one leadership term; returns when the lease is lost, the task ends or
/// `cancel` fires.
async fn lead<B, F, Fut>(
    name: &str,
    backend: &B,
    renew_interval: Duration,
    task_factory: &mut F,
    fence: Fence,
    cancel: &CancellationToken,
) where
    B: LeaseBackend,
    F: FnMut(CancellationToken, Fence) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let token = cancel.child_token();
    let mut task = tokio::spawn(task_factory(token.clone(), fence));
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + renew_interval, renew_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = backend.lost() => {
                tracing::warn!(lease = name, "lease lost; stopping singleton");
                break;
            }
            res = &mut task => {
                if let Err(e) = res {
                    tracing::error!(lease = name, error = %e, "singleton task ended abnormally");
                } else {
                    tracing::info!(lease = name, "singleton task returned");
                }
                return;
            }
            _ = tick.tick() => match backend.renew().await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(lease = name, "lease lost; stopping singleton");
                    break;
                }
                Err(e) => {
                    tracing::warn!(lease = name, error = %e, "lease renewal failed; stopping singleton");
                    break;
                }
            },
        }
    }
    token.cancel();
    if tokio::time::timeout(renew_interval, &mut task)
        .await
        .is_err()
    {
        tracing::warn!(
            lease = name,
            "singleton task ignored cancellation; aborting"
        );
        task.abort();
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable() {
        assert_eq!(
            advisory_key("repo-discovery"),
            advisory_key("repo-discovery")
        );
        assert_ne!(advisory_key("a"), advisory_key("b"));
        // Pinned so an accidental change to the hash (which would let two
        // versions lead at once during a rolling upgrade) fails the build.
        assert_eq!(advisory_key(""), -8_970_288_401_366_396_129);
    }
}
