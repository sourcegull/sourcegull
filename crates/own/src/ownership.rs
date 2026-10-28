//! The ranked owner list of a file or directory: every owner once, with
//! the reasons it owns the path.
//!
//! Reasons are CODEOWNERS entries and assignments, then the signals (recent contributors and
//! recent viewers, [`crate::signals`]) when enabled for the repository. Ranking orders owners
//! by their strongest reason ([`Reason::rank`]) and keeps first-seen order among equals, so
//! signals, added most commits or views first, rank after owners and in count order.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sourcegull_db::DbError;
use sourcegull_types::{RepoId, UserId};

use crate::codeowners::CompiledCodeowners;
use crate::resolve::{OwnerResolver, ResolvedOwner};
use crate::signals::SignalStore;
use crate::store::{Assignee, OwnStore};

/// Contributors and viewers listed per path at most.
pub const SIGNAL_LIMIT: i64 = 10;

type Result<T> = std::result::Result<T, DbError>;

/// Where the CODEOWNERS file in force comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeownersOrigin {
    /// A file in the repository at the commit (one of [`crate::codeowners_paths`]).
    Repo { path: String },
    /// The file a site admin ingested for the repository.
    Ingested,
}

/// The kinds of reasons (`OwnershipReasonType` in GraphQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonKind {
    CodeownersFileEntry,
    AssignedOwner,
    RecentContributor,
    RecentView,
}

/// Why an owner owns a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A CODEOWNERS entry matches the path.
    CodeownersFileEntry {
        origin: CodeownersOrigin,
        /// The entry's 1-based line.
        line: u32,
        section: String,
        pattern: String,
    },
    /// Assigned on the path itself (`direct`) or on an ancestor.
    AssignedOwner {
        direct: bool,
        /// The assigned path (`""` for the repository).
        path: String,
        who_assigned: Option<UserId>,
        assigned_at: DateTime<Utc>,
    },
    /// Authored commits changing the path in the recent window.
    RecentContributor { count: i64 },
    /// Viewed the file in the recent window.
    RecentView { count: i64 },
}

impl Reason {
    pub fn kind(&self) -> ReasonKind {
        match self {
            Reason::CodeownersFileEntry { .. } => ReasonKind::CodeownersFileEntry,
            Reason::AssignedOwner { .. } => ReasonKind::AssignedOwner,
            Reason::RecentContributor { .. } => ReasonKind::RecentContributor,
            Reason::RecentView { .. } => ReasonKind::RecentView,
        }
    }

    /// Lower ranks first: direct assignments, CODEOWNERS entries, inherited assignments, then
    /// recent contributors and recent viewers.
    pub fn rank(&self) -> u8 {
        match self {
            Reason::AssignedOwner { direct: true, .. } => 0,
            Reason::CodeownersFileEntry { .. } => 1,
            Reason::AssignedOwner { direct: false, .. } => 2,
            Reason::RecentContributor { .. } => 3,
            Reason::RecentView { .. } => 4,
        }
    }

    /// Whether the reason makes the entity an owner (as opposed to a signal suggesting one).
    pub fn is_owner(&self) -> bool {
        matches!(
            self.kind(),
            ReasonKind::CodeownersFileEntry | ReasonKind::AssignedOwner
        )
    }
}

/// An owner and its reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownership {
    pub owner: ResolvedOwner,
    pub reasons: Vec<Reason>,
}

impl Ownership {
    fn rank(&self) -> u8 {
        self.reasons
            .iter()
            .map(Reason::rank)
            .min()
            .unwrap_or(u8::MAX)
    }
}

/// Owners being collected, merged by identity.
#[derive(Debug, Default)]
pub struct OwnershipList {
    entries: Vec<Ownership>,
    by_identity: HashMap<String, usize>,
}

impl OwnershipList {
    /// Adds a reason for `owner` (merged with earlier reasons of the same user or team).
    pub fn add(&mut self, owner: ResolvedOwner, reason: Reason) {
        let id = owner.identity();
        match self.by_identity.get(&id) {
            Some(&i) => {
                if !self.entries[i].reasons.contains(&reason) {
                    self.entries[i].reasons.push(reason);
                }
            }
            None => {
                self.by_identity.insert(id, self.entries.len());
                self.entries.push(Ownership {
                    owner,
                    reasons: vec![reason],
                });
            }
        }
    }

    /// The owners, strongest reason first (stable among equals); reasons in rank order.
    pub fn finish(self) -> Vec<Ownership> {
        let mut out = self.entries;
        for o in &mut out {
            o.reasons.sort_by_key(Reason::rank);
        }
        out.sort_by_key(Ownership::rank);
        out
    }
}

/// What ownership of one path is computed from.
pub struct OwnershipQuery<'a> {
    pub repo: RepoId,
    /// Repository-relative, `""` for the root.
    pub path: &'a str,
    pub is_dir: bool,
    /// The CODEOWNERS file in force at the commit, if any.
    pub codeowners: Option<(&'a CompiledCodeowners, CodeownersOrigin)>,
    /// Add recent contributors (the signal applies to the repository).
    pub contributors: bool,
    /// Add recent viewers (the signal applies to the repository; files only).
    pub viewers: bool,
}

/// The ranked owners of a path: CODEOWNERS entries and assignments, resolved to users and
/// teams, then the enabled signals.
pub async fn compute(
    store: &OwnStore,
    resolver: &OwnerResolver,
    q: OwnershipQuery<'_>,
) -> Result<Vec<Ownership>> {
    let mut list = OwnershipList::default();
    if let Some((c, origin)) = &q.codeowners {
        let matches = if q.is_dir {
            c.matches_dir(q.path)
        } else {
            c.matches(q.path)
        };
        let texts: Vec<String> = matches
            .iter()
            .flat_map(|m| m.owners.iter().map(|o| o.text()))
            .collect();
        let resolved = resolver.resolve(&texts).await?;
        for m in &matches {
            for o in m.owners {
                let Some(owner) = resolved.get(&o.key()) else {
                    continue;
                };
                list.add(
                    owner.clone(),
                    Reason::CodeownersFileEntry {
                        origin: origin.clone(),
                        line: m.rule.line,
                        section: m.section.name.clone(),
                        pattern: m.rule.pattern.clone(),
                    },
                );
            }
        }
    }
    let path = crate::store::normalize_path(q.path);
    let assignments = store.assignments_for_path(q.repo, &path).await?;
    let user_ids: Vec<UserId> = assignments
        .iter()
        .filter_map(|a| match a.assignee {
            Assignee::User(u) => Some(u),
            Assignee::Team(_) => None,
        })
        .collect();
    let team_ids: Vec<i64> = assignments
        .iter()
        .filter_map(|a| match a.assignee {
            Assignee::Team(t) => Some(t),
            Assignee::User(_) => None,
        })
        .collect();
    let users: HashMap<UserId, _> = resolver
        .users(&user_ids)
        .await?
        .into_iter()
        .map(|u| (u.id, u))
        .collect();
    let teams: HashMap<i64, _> = store
        .get_teams(&team_ids)
        .await?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    for a in assignments {
        let owner = match a.assignee {
            Assignee::User(u) => users
                .get(&u)
                .cloned()
                .map(|u| ResolvedOwner::User(Box::new(u))),
            Assignee::Team(t) => teams.get(&t).cloned().map(ResolvedOwner::Team),
        };
        let Some(owner) = owner else { continue };
        list.add(
            owner,
            Reason::AssignedOwner {
                direct: a.file_path == path,
                path: a.file_path,
                who_assigned: a.who_assigned,
                assigned_at: a.assigned_at,
            },
        );
    }
    let signals = SignalStore::from_pool(store.pool().clone());
    if q.contributors {
        let contributors = signals
            .recent_contributors(q.repo, &path, SIGNAL_LIMIT)
            .await?;
        let emails: Vec<String> = contributors.iter().map(|c| c.email.clone()).collect();
        let resolved = resolver.resolve(&emails).await?;
        for c in contributors {
            let owner = match resolved.get(&crate::owner_key(&c.email)) {
                Some(ResolvedOwner::User(u)) => ResolvedOwner::User(u.clone()),
                _ => ResolvedOwner::Person {
                    handle: None,
                    email: (!c.email.is_empty()).then(|| c.email.clone()),
                    name: Some(c.name.clone()),
                },
            };
            list.add(owner, Reason::RecentContributor { count: c.count });
        }
    }
    if q.viewers && !q.is_dir {
        let viewers = signals.recent_viewers(q.repo, &path, SIGNAL_LIMIT).await?;
        let ids: Vec<UserId> = viewers.iter().map(|v| v.0).collect();
        let users: HashMap<UserId, _> = resolver
            .users(&ids)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();
        for (id, count) in viewers {
            if let Some(u) = users.get(&id) {
                list.add(
                    ResolvedOwner::User(Box::new(u.clone())),
                    Reason::RecentView { count },
                );
            }
        }
    }
    Ok(list.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(h: &str) -> ResolvedOwner {
        ResolvedOwner::Person {
            handle: Some(h.into()),
            email: None,
            name: None,
        }
    }

    fn entry(line: u32) -> Reason {
        Reason::CodeownersFileEntry {
            origin: CodeownersOrigin::Ingested,
            line,
            section: "codeowners".into(),
            pattern: "*".into(),
        }
    }

    fn assigned(direct: bool) -> Reason {
        Reason::AssignedOwner {
            direct,
            path: String::new(),
            who_assigned: None,
            assigned_at: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    #[test]
    fn merges_and_ranks() {
        let mut l = OwnershipList::default();
        l.add(person("a"), assigned(false));
        l.add(person("b"), entry(1));
        l.add(person("A"), entry(2));
        l.add(person("c"), assigned(true));
        l.add(person("b"), entry(1));
        let out = l.finish();
        let names: Vec<_> = out.iter().map(|o| o.owner.display()).collect();
        assert_eq!(names, vec!["c", "a", "b"]);
        // `a` and `A` are one owner; its CODEOWNERS reason ranks before the inherited one.
        assert_eq!(out[1].reasons, vec![entry(2), assigned(false)]);
        assert_eq!(out[2].reasons.len(), 1);
    }

    #[test]
    fn signals_rank_after_owners_in_count_order() {
        let mut l = OwnershipList::default();
        l.add(person("v"), Reason::RecentView { count: 9 });
        l.add(person("c1"), Reason::RecentContributor { count: 5 });
        l.add(person("c2"), Reason::RecentContributor { count: 2 });
        l.add(person("o"), entry(1));
        l.add(person("c2"), Reason::RecentView { count: 1 });
        let out = l.finish();
        let names: Vec<_> = out.iter().map(|o| o.owner.display()).collect();
        assert_eq!(names, vec!["o", "c1", "c2", "v"]);
        assert!(out[0].reasons.iter().all(Reason::is_owner));
        assert!(!out[1].reasons[0].is_owner());
    }
}
