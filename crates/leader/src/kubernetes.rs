//! A [`LeaseBackend`] on a Kubernetes `coordination.k8s.io/v1` `Lease`, the
//! default for Helm installs.
//!
//! The protocol follows client-go's leader election:
//!
//! - A candidate takes the lease by creating the object, or by replacing it when it is free
//!   (no `holderIdentity`, e.g. after a graceful release) or expired.
//! - Expiry is judged by the candidate's own clock: a lease held by someone else counts as
//!   expired once its record (holder, `renewTime`, `resourceVersion`) has not changed for
//!   `leaseDurationSeconds` since this candidate first saw it, so clock skew between pods
//!   does not matter.
//! - Every write is a `replace` carrying the `resourceVersion` that was read, so the API
//!   server's optimistic concurrency lets exactly one of two racing candidates win (the other
//!   gets 409 Conflict).
//! - The holder renews by replacing its own last-written object (bumping `renewTime`); a
//!   conflict or a renewal later than the lease duration means the lease is lost.
//! - Release clears `holderIdentity` and sets `leaseDurationSeconds: 1`, so the next candidate
//!   takes over at once.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, PostParams};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::{LeaseBackend, LeaseError};

/// Default `leaseDurationSeconds` (client-go's default).
pub const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(15);

/// Settings shared by every lease of a process.
#[derive(Clone)]
pub struct KubernetesLeases {
    client: kube::Client,
    namespace: String,
    prefix: String,
    identity: String,
    duration: Duration,
    timeout: Duration,
}

impl std::fmt::Debug for KubernetesLeases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubernetesLeases")
            .field("namespace", &self.namespace)
            .field("prefix", &self.prefix)
            .field("identity", &self.identity)
            .field("duration", &self.duration)
            .finish_non_exhaustive()
    }
}

impl KubernetesLeases {
    /// Leases in `namespace`, named `<prefix>-<lease name>`, held as `identity` (unique per
    /// process, e.g. the pod name and a random suffix).
    pub fn new(
        client: kube::Client,
        namespace: impl Into<String>,
        prefix: impl Into<String>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            client,
            namespace: namespace.into(),
            prefix: prefix.into(),
            identity: identity.into(),
            duration: DEFAULT_LEASE_DURATION,
            timeout: Duration::from_secs(5),
        }
    }

    /// Leases through the in-cluster configuration (else `KUBECONFIG`), in `namespace` or,
    /// when `None`, the pod's own namespace.
    pub async fn infer(
        namespace: Option<String>,
        prefix: impl Into<String>,
        identity: impl Into<String>,
    ) -> Result<Self, LeaseError> {
        let client = kube::Client::try_default()
            .await
            .map_err(|e| LeaseError::Kubernetes(format!("no Kubernetes client: {e}")))?;
        let namespace = namespace
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| client.default_namespace().to_owned());
        Ok(Self::new(client, namespace, prefix, identity))
    }

    /// Sets `leaseDurationSeconds` (default 15 s; whole seconds, at least 1). It must be
    /// longer than the renewal interval.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Duration::from_secs(duration.as_secs().max(1));
        self
    }

    /// Bounds each API call (default 5 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The holder identity.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The namespace of the `Lease` objects.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The lease named `name`.
    pub fn lease(&self, name: &str) -> KubernetesLease {
        KubernetesLease {
            api: Api::namespaced(self.client.clone(), &self.namespace),
            object: object_name(&self.prefix, name),
            identity: self.identity.clone(),
            duration: self.duration,
            timeout: self.timeout,
            state: Mutex::new(State::default()),
        }
    }
}

/// A valid object name (RFC 1123 subdomain) for lease `name`: `<prefix>-<name>`, lowercased,
/// other characters replaced by `-`.
pub fn object_name(prefix: &str, name: &str) -> String {
    let raw = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}-{name}")
    };
    let mut out: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(253);
    let trimmed = out.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        "lease".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[derive(Default)]
struct State {
    /// The object as we last wrote it while holding the lease.
    held: Option<Lease>,
    /// When we last wrote it successfully.
    renewed_at: Option<Instant>,
    /// Another holder's record as last observed, and when it last changed.
    observed: Option<(Record, Instant)>,
}

#[derive(PartialEq, Eq)]
struct Record {
    holder: String,
    renew_time: Option<String>,
    resource_version: Option<String>,
}

impl Record {
    fn of(lease: &Lease) -> Self {
        let spec = lease.spec.as_ref();
        Self {
            holder: holder(lease).to_owned(),
            renew_time: spec
                .and_then(|s| s.renew_time.as_ref())
                .map(|t| t.0.to_string()),
            resource_version: lease.metadata.resource_version.clone(),
        }
    }
}

fn holder(lease: &Lease) -> &str {
    lease
        .spec
        .as_ref()
        .and_then(|s| s.holder_identity.as_deref())
        .unwrap_or("")
}

fn duration_of(lease: &Lease) -> Option<Duration> {
    lease
        .spec
        .as_ref()
        .and_then(|s| s.lease_duration_seconds)
        .map(|s| Duration::from_secs(u64::try_from(s).unwrap_or(0)))
}

fn api_error(e: kube::Error) -> LeaseError {
    LeaseError::Kubernetes(e.to_string())
}

fn is_status(e: &kube::Error, code: u16) -> bool {
    matches!(e, kube::Error::Api(s) if s.code == code)
}

/// One named lease on a Kubernetes `Lease` object.
pub struct KubernetesLease {
    api: Api<Lease>,
    object: String,
    identity: String,
    duration: Duration,
    timeout: Duration,
    state: Mutex<State>,
}

impl KubernetesLease {
    /// The `Lease` object's name.
    pub fn object_name(&self) -> &str {
        &self.object
    }

    async fn bounded<F, T>(&self, fut: F) -> Result<T, LeaseError>
    where
        F: Future<Output = Result<T, kube::Error>>,
    {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => r.map_err(api_error),
            Err(_) => Err(LeaseError::Timeout),
        }
    }

    fn spec(&self, acquire_time: Option<MicroTime>, transitions: i32) -> LeaseSpec {
        let now = MicroTime(Timestamp::now());
        LeaseSpec {
            holder_identity: Some(self.identity.clone()),
            lease_duration_seconds: Some(i32::try_from(self.duration.as_secs()).unwrap_or(15)),
            acquire_time: Some(acquire_time.unwrap_or_else(|| now.clone())),
            renew_time: Some(now),
            lease_transitions: Some(transitions),
            ..LeaseSpec::default()
        }
    }

    /// Writes `lease` with our holder fields; `Ok(None)` on a conflict (someone else won).
    async fn write(&self, lease: Lease, create: bool) -> Result<Option<Lease>, LeaseError> {
        let pp = PostParams::default();
        let res = if create {
            tokio::time::timeout(self.timeout, self.api.create(&pp, &lease)).await
        } else {
            tokio::time::timeout(self.timeout, self.api.replace(&self.object, &pp, &lease)).await
        };
        match res {
            Err(_) => Err(LeaseError::Timeout),
            Ok(Ok(l)) => Ok(Some(l)),
            Ok(Err(e)) if is_status(&e, 409) => Ok(None),
            Ok(Err(e)) => Err(api_error(e)),
        }
    }
}

#[async_trait]
impl LeaseBackend for KubernetesLease {
    async fn try_acquire(&self) -> Result<bool, LeaseError> {
        let mut state = self.state.lock().await;
        if state.held.is_some() {
            drop(state);
            return self.renew().await;
        }
        let current = self.bounded(self.api.get_opt(&self.object)).await?;
        let now = Instant::now();
        let written = match current {
            None => {
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(self.object.clone()),
                        ..ObjectMeta::default()
                    },
                    spec: Some(self.spec(None, 0)),
                };
                self.write(lease, true).await?
            }
            Some(mut lease) => {
                let other = holder(&lease).to_owned();
                if !other.is_empty() && other != self.identity {
                    // Held by someone else: take it only once its record has been unchanged
                    // for the lease duration (by our clock).
                    let record = Record::of(&lease);
                    let since = match state.observed.take() {
                        Some((r, at)) if r == record => at,
                        _ => now,
                    };
                    state.observed = Some((record, since));
                    let duration = duration_of(&lease).unwrap_or(self.duration);
                    if now.duration_since(since) < duration {
                        return Ok(false);
                    }
                    tracing::info!(lease = %self.object, holder = %other, "taking over an expired lease");
                }
                let spec = lease.spec.take().unwrap_or_default();
                let transitions = spec.lease_transitions.unwrap_or(0);
                lease.spec = Some(if other == self.identity {
                    self.spec(spec.acquire_time, transitions)
                } else {
                    self.spec(None, transitions.saturating_add(1))
                });
                // `resourceVersion` stays as read: a concurrent change makes this a conflict.
                self.write(lease, false).await?
            }
        };
        match written {
            Some(lease) => {
                state.held = Some(lease);
                state.renewed_at = Some(now);
                state.observed = None;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn renew(&self) -> Result<bool, LeaseError> {
        let mut state = self.state.lock().await;
        let Some(mut lease) = state.held.take() else {
            return Ok(false);
        };
        let now = Instant::now();
        // Past the lease duration since our last write, another candidate may already lead.
        if state
            .renewed_at
            .is_none_or(|t| now.duration_since(t) >= self.duration)
        {
            tracing::warn!(lease = %self.object, "lease not renewed within its duration");
            return Ok(false);
        }
        if let Some(spec) = lease.spec.as_mut() {
            spec.renew_time = Some(MicroTime(Timestamp::now()));
        }
        match self.write(lease.clone(), false).await {
            Ok(Some(written)) => {
                state.held = Some(written);
                state.renewed_at = Some(now);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => {
                // Unknown outcome: keep the lease until it is due to expire, but report the
                // failure so the singleton stops.
                state.held = Some(lease);
                Err(e)
            }
        }
    }

    async fn release(&self) {
        let mut state = self.state.lock().await;
        state.renewed_at = None;
        let Some(mut lease) = state.held.take() else {
            return;
        };
        if let Some(spec) = lease.spec.as_mut() {
            spec.holder_identity = None;
            spec.lease_duration_seconds = Some(1);
            spec.renew_time = Some(MicroTime(Timestamp::now()));
        }
        if let Err(e) = self.write(lease, false).await {
            tracing::debug!(lease = %self.object, error = %e, "releasing the lease failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_are_valid() {
        assert_eq!(
            object_name("sourcegull", "session-janitor"),
            "sourcegull-session-janitor"
        );
        assert_eq!(
            object_name("Sourcegull", "repos::Sync_Scheduler"),
            "sourcegull-repos--sync-scheduler"
        );
        assert_eq!(object_name("", "-x-"), "x");
        assert_eq!(object_name("", "__"), "lease");
        assert_eq!(object_name("p", &"a".repeat(300)).len(), 253);
    }
}
