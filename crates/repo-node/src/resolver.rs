//! Database-backed [`RemoteResolver`]: turns a repo into the URL and credentials to clone or fetch
//! it from.
//!
//! The clone URL comes from `external_service_repos.clone_url` (credential-free, as reported by
//! discovery); the credential from the connection's decrypted configuration through the code
//! host registry ([`sourcegull_codehost::RepoSource::credential`]). Credentials are passed to git
//! through `GIT_ASKPASS` by the git store and never stored or logged.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sourcegull_codehost::{Credential, RepoSource};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, DbError};
use sourcegull_gitstore::{GitError, Remote, RemoteAuth, RemoteResolver, RepoRef, Secret};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalServiceId, RepoId};

/// SSH key used for `gitURLType: ssh` connections (a mounted secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshKey {
    /// Private key file.
    pub private_key: PathBuf,
    /// `known_hosts` file; host keys are checked strictly when set.
    pub known_hosts: Option<PathBuf>,
}

/// Why a remote could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The repo does not exist or is deleted.
    #[error("repo {0} not found")]
    RepoNotFound(RepoId),
    /// No live code host connection lists the repo.
    #[error("no code host connection lists repo {0}")]
    NoSource(RepoId),
    /// The connection's configuration cannot be used.
    #[error("code host connection {0}: {1}")]
    Config(ExternalServiceId, String),
    /// The clone URL is not allowed.
    #[error("clone URL rejected: {0}")]
    Url(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<ResolveError> for GitError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::RepoNotFound(id) => GitError::RepoNotFound(id),
            ResolveError::Db(e) => GitError::Io(std::io::Error::other(e.to_string())),
            other => GitError::InvalidArgument(other.to_string()),
        }
    }
}

/// Resolves remotes from the database. Cheap to clone.
#[derive(Clone)]
pub struct DbResolver {
    inner: Arc<Inner>,
    /// This node's address, for [`RemoteResolver::check_assigned`]; `None` allows every repo.
    me: Option<Arc<str>>,
}

/// A connection's code host source and the hash of the configuration it was built from.
type CachedSource = (u64, Arc<dyn RepoSource>);

struct Inner {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    registry: Registry,
    ssh: Option<SshKey>,
    allow_local: bool,
    /// Code host source per connection, keyed by a hash of the decrypted config so a config
    /// change is picked up at once (building a source creates an HTTP client). Its credential
    /// is read per clone, so an OAuth token is refreshed before it expires.
    sources: Mutex<HashMap<ExternalServiceId, CachedSource>>,
}

impl std::fmt::Debug for DbResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbResolver")
            .field("allow_local", &self.inner.allow_local)
            .finish_non_exhaustive()
    }
}

impl DbResolver {
    /// A resolver reading connection configs with `keyring`. `allow_local` permits `file://`
    /// URLs and local paths as clone URLs (tests and development only: on a shared node they
    /// would let a code host point clones at other repos on disk).
    pub fn new(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        registry: Registry,
        ssh: Option<SshKey>,
        allow_local: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                keyring,
                registry,
                ssh,
                allow_local,
                sources: Mutex::new(HashMap::new()),
            }),
            me: None,
        }
    }

    /// The resolver of the repo node `me`: writes through the gRPC service are refused for
    /// repos assigned (`repo.shard`) to another node. Unassigned repos are allowed.
    #[must_use]
    pub fn with_node(self, me: &str) -> Self {
        Self {
            me: Some(me.into()),
            ..self
        }
    }

    /// Whether `repo` may be written on this node: it is not assigned to another node.
    pub async fn assigned_here(&self, repo: RepoId) -> Result<bool, ResolveError> {
        let Some(me) = &self.me else {
            return Ok(true);
        };
        let shard: Option<String> = sqlx::query_scalar("SELECT shard FROM repo WHERE id = $1")
            .bind(repo)
            .fetch_optional(self.inner.db.pool())
            .await
            .map_err(DbError::from)?
            .flatten();
        Ok(shard.is_none_or(|s| *s == **me))
    }

    /// The remote of `repo`.
    pub async fn resolve_id(&self, repo: RepoId) -> Result<Remote, ResolveError> {
        let inner = &*self.inner;
        if inner.db.repos().get(repo).await?.is_none() {
            return Err(ResolveError::RepoNotFound(repo));
        }
        let (service, clone_url) = inner
            .db
            .repos()
            .sources(repo)
            .await?
            .into_iter()
            .next()
            .ok_or(ResolveError::NoSource(repo))?;
        let kind = check_url(&clone_url, inner.allow_local)?;
        let credential = self.credential(repo, service).await?;
        let auth = match (credential, kind) {
            (_, UrlKind::Local) => RemoteAuth::None,
            (Credential::Basic { username, password }, UrlKind::Http) => RemoteAuth::Basic {
                username,
                password: Secret::new(password),
            },
            (_, UrlKind::Ssh) => match &inner.ssh {
                Some(key) => RemoteAuth::Ssh {
                    private_key: key.private_key.clone(),
                    known_hosts: key.known_hosts.clone(),
                },
                None => RemoteAuth::None,
            },
            (Credential::None, UrlKind::Http) => RemoteAuth::None,
        };
        Ok(Remote::new(&clone_url, auth))
    }

    async fn credential(
        &self,
        repo: RepoId,
        service: ExternalServiceId,
    ) -> Result<Credential, ResolveError> {
        let inner = &*self.inner;
        let store = inner.db.external_services(inner.keyring.clone());
        let svc = store
            .get(service)
            .await?
            .ok_or(ResolveError::NoSource(repo))?;
        let config = store.config(service).await.map_err(|e| match e {
            DbError::Crypto(c) => ResolveError::Config(service, format!("decrypting: {c}")),
            DbError::Invalid(m) => ResolveError::Config(service, m.to_string()),
            other => ResolveError::Db(other),
        })?;
        let mut h = DefaultHasher::new();
        config.hash(&mut h);
        let hash = h.finish();
        let cached = inner
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&service)
            .filter(|(h, _)| *h == hash)
            .map(|(_, source)| source.clone());
        let source = match cached {
            Some(source) => source,
            None => {
                let source = inner
                    .registry
                    .for_connection(svc.id.get())
                    .source(svc.kind, &config)
                    .map_err(|e| ResolveError::Config(service, e.to_string()))?;
                inner
                    .sources
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(service, (hash, source.clone()));
                source
            }
        };
        source
            .current_credential()
            .await
            .map_err(|e| ResolveError::Config(service, e.to_string()))
    }
}

impl RemoteResolver for DbResolver {
    fn resolve(
        &self,
        repo: &RepoRef,
    ) -> impl Future<Output = sourcegull_gitstore::Result<Remote>> + Send {
        let this = self.clone();
        let id = repo.id;
        async move { this.resolve_id(id).await.map_err(GitError::from) }
    }

    fn check_assigned(
        &self,
        repo: &RepoRef,
    ) -> impl Future<Output = sourcegull_gitstore::Result<()>> + Send {
        let this = self.clone();
        let id = repo.id;
        async move {
            if this.assigned_here(id).await.map_err(GitError::from)? {
                Ok(())
            } else {
                Err(GitError::NotAssigned(id))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlKind {
    Http,
    Ssh,
    Local,
}

/// Classifies a clone URL, rejecting schemes git should not be pointed at from code host data
/// (`ext::`, `fd::`, local paths unless allowed).
fn check_url(url: &str, allow_local: bool) -> Result<UrlKind, ResolveError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(UrlKind::Http);
    }
    if lower.starts_with("ssh://") {
        return Ok(UrlKind::Ssh);
    }
    let local = lower.starts_with("file://") || url.starts_with('/');
    if local {
        return if allow_local {
            Ok(UrlKind::Local)
        } else {
            Err(ResolveError::Url(
                "local paths and file:// URLs are not allowed".into(),
            ))
        };
    }
    // scp-like `user@host:path` (no scheme, no `::` transport helper syntax).
    if !lower.contains("::")
        && !lower.contains("://")
        && let Some((host, _path)) = url.split_once(':')
        && !host.is_empty()
        && !host.starts_with('-')
        && !host.contains('/')
    {
        return Ok(UrlKind::Ssh);
    }
    Err(ResolveError::Url(format!(
        "unsupported clone URL {:?}",
        sourcegull_gitstore::remote::Redactor::default().redact(url)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_urls_are_classified() {
        assert_eq!(
            check_url("https://gitlab.com/g/p.git", false).unwrap(),
            UrlKind::Http
        );
        assert_eq!(
            check_url("git@gitlab.com:g/p.git", false).unwrap(),
            UrlKind::Ssh
        );
        assert_eq!(
            check_url("ssh://git@gitlab.com/g/p.git", false).unwrap(),
            UrlKind::Ssh
        );
        assert!(check_url("/srv/repo.git", false).is_err());
        assert!(check_url("file:///srv/repo.git", false).is_err());
        assert_eq!(check_url("/srv/repo.git", true).unwrap(), UrlKind::Local);
        assert!(check_url("ext::sh -c touch% /tmp/pwned", true).is_err());
        assert!(check_url("-oProxyCommand=x:y", false).is_err());
        assert!(check_url("relative/path", false).is_err());
    }
}
