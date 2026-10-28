//! The owner part of a search as `web` sends it to the repo nodes (Code Ownership):
//! `file:has.owner()` terms resolved to owner keys, `file:has.contributor()` terms, and the
//! ingested CODEOWNERS files and assignments of the searched repositories.

use std::collections::{HashMap, HashSet};

use sourcegull_own::{AssignedEntry, OwnerTerm};
use sourcegull_rpc::repo::v1 as pb;

/// What a repository adds to the CODEOWNERS file in its commits, and where its code host
/// looks for that file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoOwnerData {
    /// An ingested CODEOWNERS file, which replaces the repository's.
    pub codeowners: Option<String>,
    pub assigned: Vec<AssignedEntry>,
    /// The lower-case service type for [`sourcegull_own::codeowners_paths`]; empty for GitLab.
    pub service_type: String,
}

/// One `file:has.contributor(pattern)` term: the pattern is a case-insensitive regular
/// expression matched against the names and emails of the authors who changed a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorTerm {
    pub negated: bool,
    pub pattern: String,
}

/// The owner part of a search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerSearch {
    /// All must hold.
    pub terms: Vec<OwnerTerm>,
    /// `file:has.contributor()` terms; all must hold.
    pub contributors: Vec<ContributorTerm>,
    /// Per repository id; repositories without an entry have neither.
    pub repos: HashMap<i32, RepoOwnerData>,
    /// Report each file's owners (`select:file.owners`).
    pub report: bool,
}

impl OwnerSearch {
    /// Whether the search filters or reports owners at all.
    pub fn is_active(&self) -> bool {
        self.filters() || self.report
    }

    /// Whether the search drops files (owner or contributor terms).
    pub fn filters(&self) -> bool {
        !self.terms.is_empty() || !self.contributors.is_empty()
    }

    /// The wire form for a request covering `repos`.
    pub fn to_pb(&self, repos: impl IntoIterator<Item = i32>) -> pb::OwnerFilter {
        pb::OwnerFilter {
            terms: self
                .terms
                .iter()
                .map(|t| {
                    let mut keys: Vec<String> =
                        t.keys.iter().flatten().cloned().collect::<Vec<_>>();
                    keys.sort();
                    pb::OwnerTerm {
                        negated: t.negated,
                        any: t.keys.is_none(),
                        keys,
                    }
                })
                .collect(),
            repos: repos
                .into_iter()
                .filter_map(|id| {
                    let d = self.repos.get(&id)?;
                    Some(pb::RepoOwnership {
                        repo_id: id,
                        codeowners: d.codeowners.clone(),
                        assigned: d
                            .assigned
                            .iter()
                            .map(|a| pb::AssignedOwnerEntry {
                                path: a.path.clone(),
                                owner: a.owner.clone(),
                            })
                            .collect(),
                        service_type: d.service_type.clone(),
                    })
                })
                .collect(),
            report_owners: self.report,
            contributors: self
                .contributors
                .iter()
                .map(|t| pb::ContributorTerm {
                    negated: t.negated,
                    pattern: t.pattern.clone(),
                })
                .collect(),
        }
    }

    /// Reads the wire form.
    pub fn from_pb(f: pb::OwnerFilter) -> Self {
        OwnerSearch {
            terms: f
                .terms
                .into_iter()
                .map(|t| OwnerTerm {
                    negated: t.negated,
                    keys: (!t.any).then(|| t.keys.into_iter().collect::<HashSet<_>>()),
                })
                .collect(),
            repos: f
                .repos
                .into_iter()
                .map(|r| {
                    (
                        r.repo_id,
                        RepoOwnerData {
                            codeowners: r.codeowners,
                            assigned: r
                                .assigned
                                .into_iter()
                                .map(|a| AssignedEntry {
                                    path: a.path,
                                    owner: a.owner,
                                })
                                .collect(),
                            service_type: r.service_type,
                        },
                    )
                })
                .collect(),
            report: f.report_owners,
            contributors: f
                .contributors
                .into_iter()
                .map(|t| ContributorTerm {
                    negated: t.negated,
                    pattern: t.pattern,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip() {
        let mut s = OwnerSearch {
            terms: vec![
                OwnerTerm {
                    negated: true,
                    keys: None,
                },
                OwnerTerm {
                    negated: false,
                    keys: Some(HashSet::from(["alice".to_owned()])),
                },
            ],
            report: true,
            contributors: vec![ContributorTerm {
                negated: false,
                pattern: "ann".into(),
            }],
            ..Default::default()
        };
        s.repos.insert(
            3,
            RepoOwnerData {
                codeowners: Some("* @a".into()),
                assigned: vec![AssignedEntry {
                    path: "src".into(),
                    owner: "@bob".into(),
                }],
                service_type: String::new(),
            },
        );
        s.repos.insert(
            5,
            RepoOwnerData {
                service_type: "github".into(),
                ..Default::default()
            },
        );
        let wire = s.to_pb([3, 4, 5]);
        assert_eq!(wire.repos.len(), 2);
        assert_eq!(OwnerSearch::from_pb(wire), s);
        assert!(s.to_pb([4]).repos.is_empty());
    }
}
