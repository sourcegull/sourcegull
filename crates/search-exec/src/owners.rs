//! Code Ownership in search: the owner filter of `file:has.owner()` and
//! the owners `select:file.owners` reports, evaluated on repo nodes.
//!
//! `web` resolves the terms to owner keys and sends, per request, the ingested CODEOWNERS
//! files and assignments of the repositories that have any ([`OwnerSearch`]). The
//! node finds the CODEOWNERS file at the searched commit in the code host's locations, compiles it
//! once per blob OID ([`sourcegull_own::codeowners_cache`]) and checks candidate paths: in
//! unindexed search before blobs are read ([`PathFilter::with_owners`](crate::PathFilter)),
//! in indexed search on the index's candidates per branch.
//!
//! `file:has.contributor(pattern)` terms
//! ([`ContributorTerm`](sourcegull_search_client::ContributorTerm)) are checked on the node too,
//! against the authors of the commits that changed each path
//! ([`sourcegull_gitstore::changes::ContributorIndex`], built from the commit metadata log and
//! its changes file).

use std::sync::Arc;

use sourcegull_gitstore::changes::ChangeLog;
use sourcegull_gitstore::commitlog::CommitLog;
use sourcegull_gitstore::read;
use sourcegull_gitstore::{GitStore, ObjectId};
use sourcegull_own::codeowners::MAX_CODEOWNERS_BYTES;
use sourcegull_own::{
    CompiledCodeowners, OwnerTerm, RepoOwners, codeowners_cache, codeowners_paths,
};
use sourcegull_search_client::OwnerSearch;
use sourcegull_types::RepoId;

/// The node side of an [`OwnerSearch`]: the checks of one repository.
pub trait OwnerSearchExt {
    /// The contributor check of `repo_id` (opened as `repo`), or `None` without contributor
    /// terms. A repository whose commit log is not built yet has no contributors.
    fn contributor_check(
        &self,
        store: &GitStore,
        repo_id: i32,
        repo: &gix::Repository,
    ) -> Option<Arc<ContributorCheck>>;

    /// What decides ownership in `repo_id` at `commit`: the ingested CODEOWNERS file or the
    /// one in the commit, and the assignments.
    fn repo_owners(&self, repo: &gix::Repository, repo_id: i32, commit: ObjectId) -> RepoOwners;
}

impl OwnerSearchExt for OwnerSearch {
    fn contributor_check(
        &self,
        store: &GitStore,
        repo_id: i32,
        repo: &gix::Repository,
    ) -> Option<Arc<ContributorCheck>> {
        if self.contributors.is_empty() {
            return None;
        }
        let changes = match store.change_log(RepoId(repo_id), repo) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(repo = repo_id, error = %e, "loading contributors");
                None
            }
        };
        let terms = self
            .contributors
            .iter()
            .map(|t| {
                let re = regex::RegexBuilder::new(&t.pattern)
                    .case_insensitive(true)
                    .build()
                    .ok();
                let selected = match (&changes, re) {
                    (Some((log, ch)), Some(re)) => ch.contributors(log).select(|s| re.is_match(s)),
                    _ => Vec::new(),
                };
                (t.negated, selected)
            })
            .collect();
        Some(Arc::new(ContributorCheck { changes, terms }))
    }

    fn repo_owners(&self, repo: &gix::Repository, repo_id: i32, commit: ObjectId) -> RepoOwners {
        let data = self.repos.get(&repo_id);
        let codeowners = match data.and_then(|d| d.codeowners.as_deref()) {
            Some(text) => Some(codeowners_cache().get_text(text)),
            None => find_codeowners(
                repo,
                commit,
                codeowners_paths(data.map_or("", |d| d.service_type.as_str())),
            ),
        };
        RepoOwners {
            codeowners,
            assigned: data.map(|d| d.assigned.clone()).unwrap_or_default(),
        }
    }
}

/// The CODEOWNERS file of `commit` (the first of `paths`, the code host's locations, that
/// holds a file), compiled and cached by blob OID. Unreadable or oversized files count as
/// none.
pub fn find_codeowners(
    repo: &gix::Repository,
    commit: ObjectId,
    paths: &[&str],
) -> Option<Arc<CompiledCodeowners>> {
    for &path in paths {
        let Ok(entry) = read::stat(repo, commit, path) else {
            continue;
        };
        if !entry.kind.is_blob() {
            continue;
        }
        let key = entry.oid.to_string();
        if let Some(size) = entry.size
            && size > MAX_CODEOWNERS_BYTES as u64
        {
            return None;
        }
        return Some(codeowners_cache().get_or_compile(&key, || {
            read::read_blob_by_oid(repo, entry.oid, MAX_CODEOWNERS_BYTES as u64)
                .map(|b| String::from_utf8_lossy(&b.content).into_owned())
                .unwrap_or_default()
        }));
    }
    None
}

/// The `file:has.contributor()` terms of one repository: per term, the authors it selects.
pub struct ContributorCheck {
    changes: Option<(Arc<CommitLog>, Arc<ChangeLog>)>,
    /// `(negated, selected authors)` per term.
    terms: Vec<(bool, Vec<bool>)>,
}

impl std::fmt::Debug for ContributorCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContributorCheck")
            .field("terms", &self.terms.len())
            .finish_non_exhaustive()
    }
}

impl ContributorCheck {
    /// Whether `path` passes every term.
    pub fn passes(&self, path: &str) -> bool {
        let found = self
            .changes
            .as_ref()
            .and_then(|(log, ch)| Some((ch.contributors(log), ch.path_id(path)?)));
        self.terms.iter().all(|(negated, selected)| {
            let has = found.is_some_and(|(idx, id)| idx.any_selected(id, selected));
            has != *negated
        })
    }
}

/// The owner check of a path filter: one repository's ownership at one commit, its
/// contributors and the terms.
#[derive(Debug, Clone)]
pub struct OwnerCheck {
    pub owners: Arc<RepoOwners>,
    pub terms: Arc<[OwnerTerm]>,
    pub contributors: Option<Arc<ContributorCheck>>,
}

impl OwnerCheck {
    pub fn passes(&self, path: &str) -> bool {
        self.owners.passes(&self.terms, path)
            && self.contributors.as_ref().is_none_or(|c| c.passes(path))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use sourcegull_search_client::RepoOwnerData;

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn commit(dir: &Path, files: &[(&str, &str)]) -> ObjectId {
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "c"]);
        ObjectId::from_hex(git(dir, &["rev-parse", "HEAD"]).trim().as_bytes()).unwrap()
    }

    /// Which test file (`gh`, `gl`, `gt` or `root`) is the CODEOWNERS file in force for a
    /// repository of `service_type`; `None` sends no entry for the repository.
    fn found(repo: &gix::Repository, commit: ObjectId, service_type: Option<&str>) -> &'static str {
        let mut search = OwnerSearch::default();
        if let Some(t) = service_type {
            search.repos.insert(
                1,
                RepoOwnerData {
                    service_type: t.to_owned(),
                    ..Default::default()
                },
            );
        }
        let c = search.repo_owners(repo, 1, commit).codeowners.unwrap();
        ["gh", "gl", "gt", "root"]
            .into_iter()
            .find(|p| !c.matches(p).is_empty())
            .unwrap()
    }

    #[test]
    fn codeowners_locations_follow_the_code_host() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        let first = commit(
            p,
            &[
                (".github/CODEOWNERS", "/gh @a\n"),
                (".gitlab/CODEOWNERS", "/gl @a\n"),
                (".gitea/CODEOWNERS", "/gt @a\n"),
            ],
        );
        let second = commit(p, &[("CODEOWNERS", "/root @a\n")]);
        let repo = gix::open(p).unwrap();

        // Each host's own directory; a repository without an entry is on GitLab.
        assert_eq!(found(&repo, first, Some("github")), "gh");
        assert_eq!(found(&repo, first, Some("gitea")), "gt");
        assert_eq!(found(&repo, first, Some("")), "gl");
        assert_eq!(found(&repo, first, None), "gl");
        // GitHub prefers `.github/` to the root; GitLab and Gitea prefer the root.
        assert_eq!(found(&repo, second, Some("github")), "gh");
        assert_eq!(found(&repo, second, Some("gitea")), "root");
        assert_eq!(found(&repo, second, Some("gitlab")), "root");
    }
}
