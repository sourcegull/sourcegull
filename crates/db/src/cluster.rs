//! Connecting to a replicated Postgres cluster: several hosts, one of which is the primary.
//!
//! The database URL may list several hosts in the libpq form
//! `postgres://user:pass@host1:5432,host2:5432,host3/db?target_session_attrs=read-write`.
//! [`Cluster`] finds the writable host among them and points every registered pool at it. A
//! monitor task checks the primary every [`CHECK_INTERVAL`] over a connection of its own; when
//! the check fails it probes all hosts and, once another host is writable, switches:
//!
//! - every registered pool's connect options get the new host and port
//!   ([`PgPool::set_connect_options`]), so connections opened from then on reach it, and
//!   requests waiting for a connection retry against it;
//! - connections opened before the switch are closed when next acquired (`before_acquire`);
//! - each new connection checks that its server is writable (`after_connect`) and wakes the
//!   monitor when it is not, so a demoted primary that is still up is never used for writes.
//!
//! Code holding a dedicated connection outside the pools (the advisory-lock lease session, the
//! `LISTEN` connection) reads the host from its source pool's options and reconnects when it
//! changes.

use std::str::FromStr as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Connection as _, PgConnection};
use tokio::sync::Notify;

use crate::{DbError, Result};

/// How often the monitor checks that the current primary is still writable.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Bound on connecting to one host and running the check there.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Delay between rounds of probing every host while none is writable.
const SEARCH_RETRY: Duration = Duration::from_millis(500);

/// Whether the server accepts writes (what libpq's `target_session_attrs=read-write` checks).
const WRITABLE_SQL: &str =
    "SELECT NOT pg_is_in_recovery() AND current_setting('transaction_read_only') = 'off'";

/// A database URL split into one set of connect options per host.
#[derive(Debug, Clone)]
pub struct Target {
    /// One entry per host, in URL order; all other settings are shared.
    pub hosts: Vec<PgConnectOptions>,
    /// Whether connections must reach the writable primary: always with several hosts, and
    /// with one host when `target_session_attrs=read-write` (or `primary`) is given.
    pub require_primary: bool,
}

impl Target {
    /// Parses a `postgres://` URL with one host or a comma-separated list of hosts.
    ///
    /// `target_session_attrs` may be `read-write` or `primary` (the default with several
    /// hosts), or `any` with one host; Sourcegull writes, so other values are rejected.
    pub fn parse(url: &str) -> Result<Self> {
        let invalid = |msg: &str| DbError::Sqlx(sqlx::Error::Configuration(msg.into()));
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| invalid("database URL must start with postgres://"))?;
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path_query) = rest.split_at(authority_end);
        let (userinfo, hostlist) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };

        let (path, query) = match path_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_query, None),
        };
        let mut attrs = None;
        let params: Vec<&str> = query
            .into_iter()
            .flat_map(|q| q.split('&'))
            .filter(|p| !p.is_empty())
            .filter(|p| match p.split_once('=') {
                Some(("target_session_attrs", v)) => {
                    attrs = Some(v.to_owned());
                    false
                }
                _ => true,
            })
            .collect();

        let hosts: Vec<&str> = if hostlist.is_empty() {
            vec![""]
        } else {
            hostlist.split(',').collect()
        };
        if hosts.len() > 1 && hosts.iter().any(|h| h.is_empty()) {
            return Err(invalid("empty host in the database URL's host list"));
        }
        let require_primary = match attrs.as_deref() {
            None => hosts.len() > 1,
            Some("read-write" | "primary") => true,
            Some("any") if hosts.len() == 1 => false,
            Some(other) => {
                return Err(invalid(&format!(
                    "target_session_attrs={other} is not supported: Sourcegull needs the \
                     writable primary (use read-write)"
                )));
            }
        };

        let query = if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        };
        let hosts = hosts
            .into_iter()
            .map(|host| {
                let auth = match userinfo {
                    Some(u) => format!("{u}@{host}"),
                    None => host.to_owned(),
                };
                PgConnectOptions::from_str(&format!("{scheme}://{auth}{path}{query}"))
                    .map_err(DbError::Sqlx)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            hosts,
            require_primary,
        })
    }
}

/// `host:port` of `o`, for logs and comparisons.
pub fn endpoint(o: &PgConnectOptions) -> String {
    format!("{}:{}", o.get_host(), o.get_port())
}

/// The hosts of one cluster and the pools that follow its primary.
pub struct Cluster {
    hosts: Vec<PgConnectOptions>,
    current: Mutex<usize>,
    pools: Mutex<Vec<PgPool>>,
    /// Reference point for `switched_at`.
    start: Instant,
    /// Milliseconds after `start` of the last switch (0: none yet).
    switched_at: AtomicU64,
    /// Wakes the monitor early (a new connection found its server read-only).
    wake: Notify,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field(
                "hosts",
                &self.hosts.iter().map(endpoint).collect::<Vec<_>>(),
            )
            .field("primary", &self.primary())
            .finish_non_exhaustive()
    }
}

impl Cluster {
    /// Finds the primary among `target`'s hosts (an error when none is writable) and starts
    /// the monitor on the current Tokio runtime. The monitor ends once the cluster is dropped.
    pub async fn connect(target: Target) -> Result<Arc<Self>> {
        let current = find_primary(&target.hosts).await.ok_or_else(|| {
            DbError::Sqlx(sqlx::Error::Configuration(
                format!(
                    "no writable primary among the database hosts {}",
                    target
                        .hosts
                        .iter()
                        .map(endpoint)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into(),
            ))
        })?;
        let cluster = Arc::new(Self {
            hosts: target.hosts,
            current: Mutex::new(current),
            pools: Mutex::new(Vec::new()),
            start: Instant::now(),
            switched_at: AtomicU64::new(0),
            wake: Notify::new(),
        });
        if cluster.hosts.len() > 1 {
            tokio::spawn(monitor(Arc::downgrade(&cluster)));
        }
        Ok(cluster)
    }

    /// The current primary's connect options (without pool-specific settings).
    pub fn primary_options(&self) -> PgConnectOptions {
        self.hosts[self.current_index()].clone()
    }

    /// `host:port` of the current primary.
    pub fn primary(&self) -> String {
        endpoint(&self.hosts[self.current_index()])
    }

    fn current_index(&self) -> usize {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds the primary checks to a pool's options: new connections must be writable and
    /// connections from before the last switch are closed.
    pub fn pool_options(self: &Arc<Self>, options: PgPoolOptions) -> PgPoolOptions {
        let on_connect = Arc::downgrade(self);
        let on_acquire = Arc::downgrade(self);
        options
            .after_connect(move |conn, _| {
                let cluster = on_connect.clone();
                Box::pin(async move {
                    if writable(conn).await? {
                        return Ok(());
                    }
                    if let Some(c) = cluster.upgrade() {
                        c.wake.notify_one();
                    }
                    Err(sqlx::Error::Protocol(
                        "the database server is read-only (not the primary)".into(),
                    ))
                })
            })
            .before_acquire(move |_, meta| {
                let keep = on_acquire.upgrade().is_none_or(|c| {
                    let switched = c.switched_at.load(Ordering::Relaxed);
                    let opened = c.start.elapsed().saturating_sub(meta.age).as_millis();
                    switched == 0 || opened >= u128::from(switched)
                });
                Box::pin(async move { Ok(keep) })
            })
    }

    /// Makes `pool` follow the primary from now on.
    pub fn register(&self, pool: &PgPool) {
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pool.clone());
    }

    fn switch_to(&self, index: usize) {
        let new = &self.hosts[index];
        let old = {
            let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *current, index)
        };
        for pool in self
            .pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let options = (*pool.connect_options())
                .clone()
                .host(new.get_host())
                .port(new.get_port());
            pool.set_connect_options(options);
        }
        let now = u64::try_from(self.start.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        self.switched_at.store(now, Ordering::Relaxed);
        tracing::warn!(
            from = %endpoint(&self.hosts[old]),
            to = %endpoint(new),
            "database primary changed; switched connections to the new primary"
        );
    }
}

/// Whether the server behind `conn` accepts writes.
async fn writable(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(WRITABLE_SQL).fetch_one(conn).await
}

/// Connects to `options` and checks it is writable, within [`PROBE_TIMEOUT`].
async fn probe(options: &PgConnectOptions) -> Option<PgConnection> {
    let attempt = async {
        let mut conn = PgConnection::connect_with(options).await?;
        if writable(&mut conn).await? {
            Ok(Some(conn))
        } else {
            let _ = conn.close().await;
            Ok::<_, sqlx::Error>(None)
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            tracing::debug!(host = %endpoint(options), error = %e, "database host probe failed");
            None
        }
        Err(_) => {
            tracing::debug!(host = %endpoint(options), "database host probe timed out");
            None
        }
    }
}

/// Probes every host at once; the first writable one in URL order wins.
async fn find_primary(hosts: &[PgConnectOptions]) -> Option<usize> {
    if let [only] = hosts {
        return probe(only).await.map(|_| 0);
    }
    let mut set = tokio::task::JoinSet::new();
    for (i, host) in hosts.iter().enumerate() {
        let host = host.clone().application_name("sourcegull:primary-check");
        set.spawn(async move { (i, probe(&host).await.is_some()) });
    }
    let mut found = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok((i, true)) = r {
            found.push(i);
        }
    }
    found.into_iter().min()
}

/// Checks the primary every [`CHECK_INTERVAL`] (or when woken) on a connection of its own,
/// and switches the pools when another host has become the primary.
async fn monitor(cluster: Weak<Cluster>) {
    let mut conn: Option<PgConnection> = None;
    loop {
        let Some(c) = cluster.upgrade() else { return };
        let primary = c
            .primary_options()
            .application_name("sourcegull:primary-check");
        let healthy = match conn.as_mut() {
            Some(conn) => matches!(
                tokio::time::timeout(PROBE_TIMEOUT, writable(conn)).await,
                Ok(Ok(true))
            ),
            None => {
                conn = probe(&primary).await;
                conn.is_some()
            }
        };
        if healthy {
            // Holds the cluster for at most one interval after its last user drops it.
            let woken = c.wake.notified();
            tokio::select! {
                () = woken => {}
                () = tokio::time::sleep(CHECK_INTERVAL) => {}
            }
            continue;
        }
        conn = None;
        tracing::warn!(primary = %endpoint(&primary), "database primary not writable; looking for the new primary");
        let hosts = c.hosts.clone();
        let current = c.current_index();
        drop(c);
        match find_primary(&hosts).await {
            Some(i) => {
                let Some(c) = cluster.upgrade() else { return };
                if i != current {
                    c.switch_to(i);
                }
            }
            None => tokio::time::sleep(SEARCH_RETRY).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(t: &Target) -> Vec<String> {
        t.hosts.iter().map(endpoint).collect()
    }

    #[test]
    fn one_host() {
        let t = Target::parse("postgres://u:p@db:6432/app?sslmode=require").unwrap();
        assert_eq!(hosts(&t), ["db:6432"]);
        assert!(!t.require_primary);
        assert_eq!(t.hosts[0].get_database(), Some("app"));
        assert_eq!(t.hosts[0].get_username(), "u");
    }

    #[test]
    fn host_list() {
        let t = Target::parse(
            "postgresql://u:p%40ss@a:5432,b,[::1]:5433/app?sslmode=verify-full&target_session_attrs=read-write",
        )
        .unwrap();
        assert_eq!(hosts(&t), ["a:5432", "b:5432", "[::1]:5433"]);
        assert!(t.require_primary);
        for h in &t.hosts {
            assert_eq!(h.get_database(), Some("app"));
            assert_eq!(h.get_username(), "u");
            assert!(matches!(
                h.get_ssl_mode(),
                sqlx::postgres::PgSslMode::VerifyFull
            ));
        }
    }

    #[test]
    fn several_hosts_require_the_primary() {
        let t = Target::parse("postgres://u@a,b/app").unwrap();
        assert!(t.require_primary);
        let t = Target::parse("postgres://u@a/app?target_session_attrs=primary").unwrap();
        assert!(t.require_primary);
    }

    #[test]
    fn rejects_other_session_attrs() {
        assert!(Target::parse("postgres://u@a,b/app?target_session_attrs=any").is_err());
        assert!(Target::parse("postgres://u@a,b/app?target_session_attrs=standby").is_err());
        assert!(Target::parse("postgres://u@a,,b/app").is_err());
        assert!(Target::parse("a,b/app").is_err());
    }
}
