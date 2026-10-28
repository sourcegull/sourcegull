//! Finding the [`ChangesetSource`] of a changeset's repository and the credential it acts
//! with.
//!
//! [`SourceProvider`] maps a repository to the changeset source of the code host connection
//! that lists it (acting with the connection's own token) and to the repository as the code
//! host sees it ([`RepoMeta`]). [`DbSourceProvider`] reads connections from the database and
//! builds sources through the code host [`Registry`]; tests plug in fakes.
//!
//! Writes act with a bulk changes credential: the user credential of the bulk change's
//! last applier, else the site credential ([`BatchesCredentialStore::resolve`]).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sourcegull_batches::store::{CredentialKind, CredentialSecret};
use sourcegull_codehost::{ChangesetSource, Credential, RepoMeta};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, DbError};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalServiceId, Repo};

use crate::error::{ReconcileError, Result};

/// Resolves the changeset source serving a repository.
#[async_trait]
pub trait SourceProvider: Send + Sync {
    /// The changeset source of the connection listing `repo` (acting with the connection's
    /// token) and `repo` as the code host sees it.
    async fn source(&self, repo: &Repo) -> Result<(Arc<dyn ChangesetSource>, RepoMeta)>;

    /// Whether the code host connection listing `repo` receives webhooks (its changesets
    /// are then synced less often).
    async fn has_webhooks(&self, _repo: &Repo) -> bool {
        false
    }
}

/// The code host credential for a bulk changes credential secret. Tokens act as GitLab
/// private tokens (`git` as the git username), OAuth tokens as bearer tokens (`oauth2`).
pub fn credential(secret: &CredentialSecret) -> Credential {
    let username = match secret.kind {
        CredentialKind::PersonalAccessToken => "git",
        CredentialKind::OAuthBearer => "oauth2",
    };
    Credential::Basic {
        username: username.to_owned(),
        password: secret.token.clone(),
    }
}

/// `repo` as a [`RepoMeta`], with the clone URL its connection reported and its stored
/// host metadata.
pub fn repo_meta(repo: &Repo, clone_url: String, metadata: serde_json::Value) -> RepoMeta {
    RepoMeta {
        external: repo.external_repo(),
        name: repo.name.clone(),
        uri: repo.uri.clone(),
        url: repo.url.clone(),
        description: repo.description.clone(),
        fork: repo.fork,
        archived: repo.archived,
        private: repo.private,
        visibility: repo.visibility,
        stars: repo.stars,
        topics: repo.topics.clone(),
        default_branch: repo.default_branch.clone(),
        last_activity_at: repo.last_activity_at,
        empty: false,
        clone_url,
        metadata,
    }
}

/// A source with the hash of the configuration it was built from.
type CachedSource = (u64, Arc<dyn ChangesetSource>);

/// Builds changeset sources from the code host connections in the database. Sources are
/// cached per connection until its configuration changes.
pub struct DbSourceProvider {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    registry: Registry,
    cache: Mutex<HashMap<ExternalServiceId, CachedSource>>,
}

impl DbSourceProvider {
    /// `keyring` decrypts connection configurations.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>, registry: Registry) -> Self {
        Self {
            db,
            keyring,
            registry,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SourceProvider for DbSourceProvider {
    async fn source(&self, repo: &Repo) -> Result<(Arc<dyn ChangesetSource>, RepoMeta)> {
        let (service, clone_url) = self
            .db
            .repos()
            .sources(repo.id)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                ReconcileError::Invalid(format!(
                    "{} is not listed by any code host connection",
                    repo.name
                ))
            })?;
        let services = self.db.external_services(self.keyring.clone());
        let svc = services.get(service).await?.ok_or_else(|| {
            ReconcileError::Invalid(format!("the code host connection of {} is gone", repo.name))
        })?;
        let config = services.config(service).await.map_err(|e| match e {
            DbError::Crypto(_) | DbError::Invalid(_) => ReconcileError::Invalid(format!(
                "reading the code host connection of {}: {e}",
                repo.name
            )),
            e => ReconcileError::Db(e),
        })?;
        let mut h = DefaultHasher::new();
        config.hash(&mut h);
        let hash = h.finish();
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&service)
            .filter(|(h, _)| *h == hash)
            .map(|(_, s)| s.clone());
        let source = match cached {
            Some(s) => s,
            None => {
                let s = self
                    .registry
                    .for_connection(svc.id.get())
                    .changeset_source(svc.kind, &config)
                    .map_err(|e| {
                        ReconcileError::Invalid(format!(
                            "the code host connection of {} is unusable: {e}",
                            repo.name
                        ))
                    })?;
                self.cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(service, (hash, s.clone()));
                s
            }
        };
        let metadata = self
            .db
            .repos()
            .metadata(repo.id)
            .await?
            .unwrap_or_else(|| serde_json::json!({}));
        Ok((source, repo_meta(repo, clone_url, metadata)))
    }

    async fn has_webhooks(&self, repo: &Repo) -> bool {
        let lookup = async {
            let services = self.db.external_services(self.keyring.clone());
            for (service, _) in self.db.repos().sources(repo.id).await? {
                let Some(svc) = services.get(service).await? else {
                    continue;
                };
                if self
                    .registry
                    .has_webhooks(svc.kind, &services.config(service).await?)
                {
                    return Ok(true);
                }
            }
            Ok::<_, DbError>(false)
        };
        lookup.await.unwrap_or_else(|e| {
            tracing::debug!(repo = %repo.name, error = %e, "checking for webhooks failed");
            false
        })
    }
}
