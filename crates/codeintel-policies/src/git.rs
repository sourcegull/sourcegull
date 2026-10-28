//! A repository's git data as policy matching and retention read it, from its repository
//! node, cached for the lifetime of one [`RepoGit`] (one expirer scan of a repository, or one
//! GraphQL request).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sourcegull_codeintel_scip::store::CodeIntelStore;
use sourcegull_codeintel_scip::visibility::{self, History, MAX_DISTANCE, Visible};
use sourcegull_db::DbError;
use sourcegull_rpc::RepoClients;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::git::{GitCallError, GitReader};
use sourcegull_types::RepoId;

use crate::PolicyError;

/// Commits listed per page of a branch's own commits.
const LOG_PAGE: u32 = 1000;
/// A branch's own commits are listed up to this many; past it, membership is asked per
/// commit.
pub const MAX_BRANCH_COMMITS: usize = 10_000;

/// A branch or tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    /// The short name (`main`, `v1.0`).
    pub name: String,
    pub kind: RefKind,
    pub commit: String,
    pub committed_at: DateTime<Utc>,
    /// The repository's default branch (what HEAD points to).
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
}

/// The commits only a branch has (not on the default branch).
#[derive(Debug, Clone)]
enum BranchCommits {
    Listed(Arc<HashSet<String>>),
    /// More than [`MAX_BRANCH_COMMITS`].
    TooMany,
}

/// Cached git reads of one repository.
pub struct RepoGit {
    reader: GitReader,
    ancestors: Mutex<HashMap<String, Arc<Vec<String>>>>,
    nearest: Mutex<HashMap<String, Arc<Vec<Visible>>>>,
    branch_commits: Mutex<HashMap<(String, String), BranchCommits>>,
    is_ancestor: Mutex<HashMap<(String, String), bool>>,
    merge_bases: Mutex<HashMap<(String, String), Option<String>>>,
}

impl std::fmt::Debug for RepoGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoGit")
            .field("repo", &self.reader.repo_name())
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RepoGit {
    pub fn new(clients: Arc<RepoClients>, id: RepoId, name: &str) -> Self {
        Self {
            reader: GitReader::from_parts(clients, id, name),
            ancestors: Mutex::default(),
            nearest: Mutex::default(),
            branch_commits: Mutex::default(),
            is_ancestor: Mutex::default(),
            merge_bases: Mutex::default(),
        }
    }

    pub fn repo_id(&self) -> RepoId {
        self.reader.repo_id()
    }

    pub fn repo_name(&self) -> &str {
        self.reader.repo_name()
    }

    /// Branches and tags that point to commits.
    pub async fn refs(&self) -> Result<Vec<GitRef>, PolicyError> {
        let resp = self.reader.list_refs(true, true, "").await?;
        Ok(resp
            .refs
            .into_iter()
            .filter(|r| !r.commit_oid.is_empty())
            .filter_map(|r| {
                let kind = match pb::RefKind::try_from(r.kind).ok()? {
                    pb::RefKind::Branch => RefKind::Branch,
                    pb::RefKind::Tag => RefKind::Tag,
                    _ => return None,
                };
                Some(GitRef {
                    is_default: kind == RefKind::Branch && r.name == resp.head_ref,
                    name: r.short_name,
                    kind,
                    commit: r.commit_oid,
                    committed_at: DateTime::from_timestamp(r.commit_time, 0).unwrap_or_default(),
                })
            })
            .collect())
    }

    /// The commit `rev` resolves to and its committer date; `None` when it does not resolve.
    pub async fn commit(&self, rev: &str) -> Result<Option<(String, DateTime<Utc>)>, PolicyError> {
        let oid = match self.reader.resolve_revision(rev).await {
            Ok(oid) => oid,
            Err(GitCallError::RevisionNotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let c = self.reader.get_commit(&oid).await?;
        let at = c
            .committer
            .and_then(|s| DateTime::from_timestamp(s.time, 0))
            .unwrap_or_default();
        Ok(Some((oid, at)))
    }

    /// Whether `commit` exists in the repository.
    pub async fn has_commit(&self, commit: &str) -> Result<bool, PolicyError> {
        match self.reader.resolve_revision(commit).await {
            Ok(oid) => Ok(oid == commit),
            Err(GitCallError::RevisionNotFound(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The uploads serving `commit` ([`visibility::nearest_uploads`]).
    pub async fn nearest_uploads(
        &self,
        store: &CodeIntelStore,
        commit: &str,
    ) -> Result<Arc<Vec<Visible>>, PolicyError> {
        if let Some(v) = lock(&self.nearest).get(commit) {
            return Ok(v.clone());
        }
        let v = Arc::new(visibility::nearest_uploads(store, self.repo_id(), commit, self).await?);
        lock(&self.nearest).insert(commit.to_owned(), v.clone());
        Ok(v)
    }

    /// Whether `commit` is on the branch at `tip`, and (for a branch other than the default
    /// one, at `head`) not also on the default branch.
    pub async fn on_branch(
        &self,
        commit: &str,
        tip: &str,
        head: Option<&str>,
    ) -> Result<bool, PolicyError> {
        let Some(head) = head else {
            return self.ancestor(commit, tip).await;
        };
        match self.branch_commits(tip, head).await? {
            BranchCommits::Listed(set) => Ok(set.contains(commit)),
            BranchCommits::TooMany => {
                Ok(self.ancestor(commit, tip).await? && !self.ancestor(commit, head).await?)
            }
        }
    }

    /// The best common ancestor of `a` and `b`.
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, PolicyError> {
        let key = (a.to_owned(), b.to_owned());
        if let Some(v) = lock(&self.merge_bases).get(&key) {
            return Ok(v.clone());
        }
        let v = self.reader.merge_base(a, b).await?;
        lock(&self.merge_bases).insert(key, v.clone());
        Ok(v)
    }

    async fn ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, PolicyError> {
        let key = (ancestor.to_owned(), descendant.to_owned());
        if let Some(&v) = lock(&self.is_ancestor).get(&key) {
            return Ok(v);
        }
        let v = self.reader.is_ancestor(ancestor, descendant).await?;
        lock(&self.is_ancestor).insert(key, v);
        Ok(v)
    }

    /// `tip ^head`, up to [`MAX_BRANCH_COMMITS`].
    async fn branch_commits(&self, tip: &str, head: &str) -> Result<BranchCommits, PolicyError> {
        let key = (tip.to_owned(), head.to_owned());
        if let Some(v) = lock(&self.branch_commits).get(&key) {
            return Ok(v.clone());
        }
        let hide = [head.to_owned()];
        let mut set = HashSet::new();
        let mut v = BranchCommits::TooMany;
        loop {
            let skip = u32::try_from(set.len()).unwrap_or(u32::MAX);
            let (commits, more) = self
                .reader
                .commit_log_hiding(tip, &hide, "", skip, LOG_PAGE)
                .await?;
            let got = commits.len();
            set.extend(commits.into_iter().map(|c| c.oid));
            if set.len() > MAX_BRANCH_COMMITS {
                break;
            }
            if !more || got == 0 {
                v = BranchCommits::Listed(Arc::new(set));
                break;
            }
        }
        lock(&self.branch_commits).insert(key, v.clone());
        Ok(v)
    }
}

#[async_trait]
impl History for RepoGit {
    type Error = PolicyError;

    async fn ancestors(&self, commit: &str) -> Result<Arc<Vec<String>>, PolicyError> {
        if let Some(v) = lock(&self.ancestors).get(commit) {
            return Ok(v.clone());
        }
        let (commits, _) = self.reader.commit_log(commit, "", 0, MAX_DISTANCE).await?;
        let v = Arc::new(commits.into_iter().map(|c| c.oid).collect::<Vec<_>>());
        lock(&self.ancestors).insert(commit.to_owned(), v.clone());
        Ok(v)
    }
}

impl From<DbError> for PolicyError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}
