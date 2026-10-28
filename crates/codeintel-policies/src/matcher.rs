//! Which git objects of a repository configuration policies name.
//!
//! Branch and tag policies match ref names with their glob; commit policies resolve their
//! pattern as a revision. A branch policy that includes intermediate commits also covers the
//! commits only that branch has (all of the default branch's history, for the default
//! branch); those are returned as [`BranchMatch`]es for the caller to test commits against,
//! rather than listed.

use crate::store::{ConfigurationPolicy, GitObjectType};
use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_types::glob::Glob;

use crate::PolicyError;
use crate::git::{RefKind, RepoGit};

/// What policies are matched for: they carry separate durations and intermediate commit
/// settings for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Data retention: durations are compared with upload dates later, and the default
    /// branch's tip always matches (with no policy).
    Retention,
    /// Auto-indexing: refs whose commit is older than the policy's max age don't match.
    Indexing,
}

impl Purpose {
    /// Whether `p` is enabled for this purpose.
    pub fn enabled(self, p: &ConfigurationPolicy) -> bool {
        match self {
            Self::Retention => p.retention_enabled,
            Self::Indexing => p.indexing_enabled,
        }
    }

    /// The policy's duration (none: unbounded) and whether it covers intermediate commits.
    pub fn extract(self, p: &ConfigurationPolicy) -> (Option<TimeDelta>, bool) {
        let (hours, intermediate) = match self {
            Self::Retention => (p.retention_duration_hours, p.retain_intermediate_commits),
            Self::Indexing => (p.index_commit_max_age_hours, p.index_intermediate_commits),
        };
        (
            hours.map(|h| TimeDelta::hours(i64::from(h))),
            intermediate && p.object_type == GitObjectType::Branch,
        )
    }
}

/// A commit named by a policy: a tag, a branch tip, a commit, or the default branch's tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipMatch {
    /// `None` for the default branch's tip in [`Purpose::Retention`].
    pub policy: Option<i32>,
    /// The ref name, or the commit for commit policies.
    pub name: String,
    pub commit: String,
    pub committed_at: DateTime<Utc>,
    pub duration: Option<TimeDelta>,
}

/// A branch whose intermediate commits policies cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchMatch {
    pub name: String,
    pub tip: String,
    pub is_default: bool,
    /// (policy, duration)
    pub policies: Vec<(i32, Option<TimeDelta>)>,
}

/// What policies match in one repository.
#[derive(Debug, Clone, Default)]
pub struct PolicyMatches {
    pub tips: Vec<TipMatch>,
    pub branches: Vec<BranchMatch>,
    /// The default branch's tip commit.
    pub head: Option<String>,
}

/// Matches `policies` (those enabled for `purpose`) against the refs of `git`'s repository.
/// Policies with a pattern that does not compile match nothing.
pub async fn match_policies(
    git: &RepoGit,
    policies: &[ConfigurationPolicy],
    purpose: Purpose,
    now: DateTime<Utc>,
) -> Result<PolicyMatches, PolicyError> {
    let policies: Vec<(&ConfigurationPolicy, Option<Glob>)> = policies
        .iter()
        .filter(|p| purpose.enabled(p))
        .filter_map(|p| {
            if p.object_type == GitObjectType::Commit {
                return Some((p, None));
            }
            match Glob::new(&p.pattern) {
                Ok(g) => Some((p, Some(g))),
                Err(e) => {
                    tracing::warn!(policy = p.id, error = %e, "skipping a configuration policy");
                    None
                }
            }
        })
        .collect();
    let mut out = PolicyMatches::default();
    let too_old = |duration: Option<TimeDelta>, at: DateTime<Utc>| {
        purpose == Purpose::Indexing && duration.is_some_and(|d| now - at > d)
    };
    for r in git.refs().await? {
        if r.is_default {
            out.head = Some(r.commit.clone());
            if purpose == Purpose::Retention {
                out.tips.push(TipMatch {
                    policy: None,
                    name: r.name.clone(),
                    commit: r.commit.clone(),
                    committed_at: r.committed_at,
                    duration: None,
                });
            }
        }
        let want = match r.kind {
            RefKind::Branch => GitObjectType::Branch,
            RefKind::Tag => GitObjectType::Tag,
        };
        let mut branch = BranchMatch {
            name: r.name.clone(),
            tip: r.commit.clone(),
            is_default: r.is_default,
            policies: Vec::new(),
        };
        for (p, glob) in &policies {
            if p.object_type != want || !glob.as_ref().is_some_and(|g| g.is_match(&r.name)) {
                continue;
            }
            let (duration, intermediate) = purpose.extract(p);
            if too_old(duration, r.committed_at) {
                continue;
            }
            out.tips.push(TipMatch {
                policy: Some(p.id),
                name: r.name.clone(),
                commit: r.commit.clone(),
                committed_at: r.committed_at,
                duration,
            });
            if intermediate {
                branch.policies.push((p.id, duration));
            }
        }
        if !branch.policies.is_empty() {
            out.branches.push(branch);
        }
    }
    for (p, _) in &policies {
        if p.object_type != GitObjectType::Commit {
            continue;
        }
        let Some((commit, committed_at)) = git.commit(&p.pattern).await? else {
            continue;
        };
        let (duration, _) = purpose.extract(p);
        if too_old(duration, committed_at) {
            continue;
        }
        out.tips.push(TipMatch {
            policy: Some(p.id),
            name: commit.clone(),
            commit,
            committed_at,
            duration,
        });
    }
    Ok(out)
}
