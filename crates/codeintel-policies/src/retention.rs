//! Data retention: whether a policy still protects an upload.
//!
//! A policy protects an upload while the upload is younger than the policy's duration and the
//! upload serves a commit the policy names. Rather than precomputing every commit each upload
//! is visible from, the question is asked the other way round, from the few commits policies
//! name:
//! - a matched tag, branch tip or commit protects the uploads that serve it
//!   ([`visibility::nearest_uploads`](sourcegull_codeintel_scip::visibility::nearest_uploads));
//!   the default branch's tip always does, with no time limit;
//! - a branch policy with intermediate commits protects the uploads at commits only that
//!   branch has (any commit of the default branch, for the default branch), and those
//!   serving the commit the branch forked from (which its oldest commits see).
use sourcegull_codeintel_scip::store::{CodeIntelStore, Upload};

use crate::store::{ConfigurationPolicy, ListPolicies, PolicyStore};
use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_types::RepoId;

use crate::PolicyError;
use crate::git::RepoGit;
use crate::matcher::{PolicyMatches, Purpose, match_policies};

/// A reason an upload is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protection {
    /// `None` for the default branch's tip.
    pub policy: Option<i32>,
    /// The commit the policy names that the upload serves (the upload's own commit when the
    /// policy names it directly).
    pub commit: String,
}

/// The retention policies of one repository, matched against its refs.
#[derive(Debug)]
pub struct Retention<'g> {
    git: &'g RepoGit,
    store: CodeIntelStore,
    matches: PolicyMatches,
    policies: Vec<ConfigurationPolicy>,
}

fn within(duration: Option<TimeDelta>, upload: &Upload, now: DateTime<Utc>) -> bool {
    duration.is_none_or(|d| now - upload.uploaded_at < d)
}

impl<'g> Retention<'g> {
    /// Loads the retention policies that apply to `git`'s repository and matches them.
    pub async fn load(
        git: &'g RepoGit,
        db: &sourcegull_db::Db,
        now: DateTime<Utc>,
    ) -> Result<Self, PolicyError> {
        let policies = PolicyStore::new(db)
            .list(&ListPolicies {
                repo: Some((git.repo_id(), git.repo_name().to_owned())),
                for_retention: Some(true),
                ..ListPolicies::default()
            })
            .await?;
        let matches = match_policies(git, &policies, Purpose::Retention, now).await?;
        Ok(Self {
            git,
            store: CodeIntelStore::new(db),
            matches,
            policies,
        })
    }

    /// The retention policies that apply to the repository.
    pub fn policies(&self) -> &[ConfigurationPolicy] {
        &self.policies
    }

    /// The repository this applies to.
    pub fn repo(&self) -> RepoId {
        self.git.repo_id()
    }

    /// Why `upload` is kept at `now`; with `first`, stops at the first reason found. Cheap
    /// checks come first: names that point at the upload's commit, then commits on matched
    /// branches, then visibility from the other matched commits.
    pub async fn protections(
        &self,
        upload: &Upload,
        now: DateTime<Utc>,
        first: bool,
    ) -> Result<Vec<Protection>, PolicyError> {
        let mut out = Vec::new();
        macro_rules! found {
            ($policy:expr, $commit:expr) => {{
                let p = Protection {
                    policy: $policy,
                    commit: $commit,
                };
                if !out.contains(&p) {
                    out.push(p);
                }
                if first {
                    return Ok(out);
                }
            }};
        }
        for t in &self.matches.tips {
            if t.commit == upload.commit && within(t.duration, upload, now) {
                found!(t.policy, t.commit.clone());
            }
        }
        let head = self.matches.head.as_deref();
        for b in &self.matches.branches {
            let live: Vec<i32> = b
                .policies
                .iter()
                .filter(|(_, d)| within(*d, upload, now))
                .map(|(id, _)| *id)
                .collect();
            if live.is_empty() {
                continue;
            }
            let base = if b.is_default { None } else { head };
            if self.git.on_branch(&upload.commit, &b.tip, base).await? {
                for id in &live {
                    found!(Some(*id), upload.commit.clone());
                }
            }
        }
        for t in &self.matches.tips {
            if t.commit != upload.commit
                && within(t.duration, upload, now)
                && self.serves(upload, &t.commit).await?
            {
                found!(t.policy, t.commit.clone());
            }
        }
        if let Some(head) = head {
            for b in self.matches.branches.iter().filter(|b| !b.is_default) {
                let live: Vec<i32> = b
                    .policies
                    .iter()
                    .filter(|(_, d)| within(*d, upload, now))
                    .map(|(id, _)| *id)
                    .collect();
                if live.is_empty() {
                    continue;
                }
                let Some(base) = self.git.merge_base(&b.tip, head).await? else {
                    continue;
                };
                if self.serves(upload, &base).await? {
                    for id in &live {
                        found!(Some(*id), base.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    async fn serves(&self, upload: &Upload, commit: &str) -> Result<bool, PolicyError> {
        Ok(self
            .git
            .nearest_uploads(&self.store, commit)
            .await?
            .iter()
            .any(|v| v.upload.id == upload.id))
    }
}

/// One line of an upload's retention overview (`retentionPolicyOverview`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyOverview {
    /// `None` for the default branch's tip.
    pub policy: Option<ConfigurationPolicy>,
    /// Whether the policy keeps the upload now.
    pub matches: bool,
    /// The commits the policy names that the upload serves, other than its own.
    pub protecting_commits: Vec<String>,
}

/// The retention policies of `upload`'s repository and whether each keeps it at `now`
/// (the default branch's tip first, when it keeps the upload).
pub async fn overview(
    retention: &Retention<'_>,
    upload: &Upload,
    now: DateTime<Utc>,
) -> Result<Vec<PolicyOverview>, PolicyError> {
    let protections = retention.protections(upload, now, false).await?;
    let commits_of = |policy: Option<i32>| -> (bool, Vec<String>) {
        let mine: Vec<&Protection> = protections.iter().filter(|p| p.policy == policy).collect();
        let mut commits: Vec<String> = mine
            .iter()
            .filter(|p| p.commit != upload.commit)
            .map(|p| p.commit.clone())
            .collect();
        commits.sort();
        commits.dedup();
        (!mine.is_empty(), commits)
    };
    let mut out = Vec::new();
    let (tip, commits) = commits_of(None);
    if tip {
        out.push(PolicyOverview {
            policy: None,
            matches: true,
            protecting_commits: commits,
        });
    }
    for p in retention.policies() {
        let (matches, protecting_commits) = commits_of(Some(p.id));
        out.push(PolicyOverview {
            policy: Some(p.clone()),
            matches,
            protecting_commits,
        });
    }
    Ok(out)
}
