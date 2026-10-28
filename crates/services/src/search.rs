//! Search backends over this process's database: [`StoreOwnership`] reads code ownership from
//! the ownership store for `sourcegull-search-web`, and [`search_backend`] builds a backend for
//! roles without an [`AppState`](crate::AppState).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sourcegull_db::{Db, DbError};
use sourcegull_own::owner_key;
use sourcegull_own::resolve::{OwnerResolver, ResolvedOwner as StoredOwner};
use sourcegull_own::store::OwnStore;
use sourcegull_rpc::RepoClients;
use sourcegull_search::stream::PersonUser;
use sourcegull_search_client::RepoOwnerData;
use sourcegull_search_web::{Owner, Ownership, ResolvedOwner, SearchBackend};

/// A search backend with its own repo catalog cache and ownership read from `db`.
pub fn search_backend(
    db: Db,
    authz: sourcegull_authz::Checker,
    repo_clients: Arc<RepoClients>,
) -> SearchBackend {
    let ownership = Arc::new(StoreOwnership::new(db.clone()));
    SearchBackend::new(db, authz, repo_clients, ownership)
}

/// Code ownership from the ownership store: owner resolution, ingested CODEOWNERS files
/// and assigned owners.
#[derive(Debug, Clone)]
pub struct StoreOwnership {
    db: Db,
}

impl StoreOwnership {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait::async_trait]
impl Ownership for StoreOwnership {
    async fn term_keys(&self, term: &str) -> Result<HashSet<String>, DbError> {
        OwnerResolver::new(&self.db).term_keys(term).await
    }

    async fn repo_owner_data(&self, repos: &[i32]) -> Result<HashMap<i32, RepoOwnerData>, DbError> {
        let store = OwnStore::new(&self.db);
        let mut out: HashMap<i32, RepoOwnerData> = HashMap::new();
        for f in store.codeowners_for_repos(repos).await? {
            out.entry(f.repo_id.get()).or_default().codeowners = Some(f.contents);
        }
        for (repo, entry) in store.assigned_entries(repos).await? {
            out.entry(repo).or_default().assigned.push(entry);
        }
        for (repo, service_type) in self.db.repos().non_gitlab_service_types(repos).await? {
            out.entry(repo).or_default().service_type = service_type;
        }
        Ok(out)
    }

    async fn resolve(&self, refs: &[String]) -> Result<HashMap<String, ResolvedOwner>, DbError> {
        let resolved = OwnerResolver::new(&self.db).resolve(refs).await?;
        Ok(refs
            .iter()
            .filter_map(|r| {
                let owner = resolved.get(&owner_key(r))?;
                Some((r.clone(), to_resolved(owner)))
            })
            .collect())
    }
}

fn to_resolved(owner: &StoredOwner) -> ResolvedOwner {
    ResolvedOwner {
        identity: owner.identity(),
        owner: match owner {
            StoredOwner::User(u) => Owner::User(PersonUser {
                username: u.username.clone(),
                display_name: u.display_name.clone().unwrap_or_default(),
                avatar_url: u.avatar_url.clone().unwrap_or_default(),
            }),
            StoredOwner::Team(t) => Owner::Team {
                name: t.name.clone(),
                display_name: t.display_name.clone().unwrap_or_default(),
            },
            StoredOwner::ExternalTeam { handle } => Owner::ExternalTeam {
                handle: handle.clone(),
            },
            StoredOwner::Person { .. } => Owner::Person,
        },
    }
}
