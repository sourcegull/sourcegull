//! [`RepoAccess`]: which repositories a viewer may read.

use std::sync::Arc;

use roaring::RoaringBitmap;

use crate::{RepoId, UserId};

/// The repositories a viewer may read, resolved once per request by `authz::Checker`.
///
/// In memory it answers from two bitmaps: the repositories readable by everyone, and the
/// restricted ones the viewer was granted. SQL queries apply the same rule with the
/// `repo_restricted()` function and [`user_id`](Self::user_id) /
/// [`user_mapping`](Self::user_mapping), so lookups by name see exactly what search sees.
#[derive(Debug, Clone)]
pub enum RepoAccess {
    /// Every repository (site admins, internal callers).
    All,
    /// Unrestricted repositories, plus the viewer's granted ones.
    Restricted {
        /// The viewer; `None` for anonymous users, who get no grants.
        user_id: Option<UserId>,
        /// `permissions.userMapping.enabled`.
        user_mapping: bool,
        /// Repositories readable by everyone. A repository missing from it (e.g. one added
        /// since the bitmap was loaded) is unreadable until the next load: the rule fails
        /// closed.
        unrestricted: Arc<RoaringBitmap>,
        /// Restricted repositories granted to the viewer.
        granted: Arc<RoaringBitmap>,
    },
}

impl RepoAccess {
    /// Access for a viewer with no grants: only unrestricted repositories.
    pub fn anonymous(user_mapping: bool, unrestricted: Arc<RoaringBitmap>) -> Self {
        Self::Restricted {
            user_id: None,
            user_mapping,
            unrestricted,
            granted: Arc::default(),
        }
    }

    /// Whether the viewer may read repository `id`.
    pub fn can_read(&self, id: RepoId) -> bool {
        match self {
            Self::All => true,
            Self::Restricted {
                unrestricted,
                granted,
                ..
            } => u32::try_from(id.get())
                .is_ok_and(|id| unrestricted.contains(id) || granted.contains(id)),
        }
    }

    /// The ids of the repositories the viewer may read, as bound to `= ANY($n)` in SQL, or
    /// `None` for all.
    pub fn readable_ids(&self) -> Option<Vec<i32>> {
        match self {
            Self::All => None,
            Self::Restricted {
                unrestricted,
                granted,
                ..
            } => Some(
                (unrestricted.as_ref() | granted.as_ref())
                    .iter()
                    .filter_map(|id| i32::try_from(id).ok())
                    .collect(),
            ),
        }
    }

    /// Whether the viewer may read every repository in `repos` (bitmap containment; Code
    /// Trends reads its pre-summed points when this holds).
    pub fn can_read_all(&self, repos: &RoaringBitmap) -> bool {
        match self {
            Self::All => true,
            Self::Restricted {
                unrestricted,
                granted,
                ..
            } => {
                let mut rest = repos - &**unrestricted;
                rest -= &**granted;
                rest.is_empty()
            }
        }
    }

    /// The repositories in `repos` the viewer may read.
    pub fn readable(&self, repos: &RoaringBitmap) -> RoaringBitmap {
        match self {
            Self::All => repos.clone(),
            Self::Restricted {
                unrestricted,
                granted,
                ..
            } => repos & (&**unrestricted | &**granted),
        }
    }

    /// Whether every repository is readable (no filtering needed).
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// The viewer whose grants SQL queries check; `None` for [`All`](Self::All) and anonymous
    /// viewers.
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Self::All => None,
            Self::Restricted { user_id, .. } => *user_id,
        }
    }

    /// `permissions.userMapping.enabled` as seen when the access was resolved.
    pub fn user_mapping(&self) -> bool {
        match self {
            Self::All => false,
            Self::Restricted { user_mapping, .. } => *user_mapping,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(ids: &[u32]) -> Arc<RoaringBitmap> {
        Arc::new(ids.iter().copied().collect())
    }

    #[test]
    fn restricted_reads_unrestricted_and_granted_repos() {
        let access = RepoAccess::Restricted {
            user_id: Some(UserId(7)),
            user_mapping: false,
            unrestricted: bitmap(&[1, 2]),
            granted: bitmap(&[5]),
        };
        assert!(access.can_read(RepoId(1)));
        assert!(access.can_read(RepoId(5)));
        assert!(!access.can_read(RepoId(3)));
        assert!(!access.can_read(RepoId(-1)));
        assert_eq!(access.user_id(), Some(UserId(7)));
        assert!(RepoAccess::All.can_read(RepoId(3)));
        let anon = RepoAccess::anonymous(true, bitmap(&[2]));
        assert!(anon.can_read(RepoId(2)) && !anon.can_read(RepoId(5)));
        assert!(anon.user_mapping() && anon.user_id().is_none());
        assert!(access.can_read_all(&bitmap(&[1, 5])));
        assert!(!access.can_read_all(&bitmap(&[1, 3])));
        assert_eq!(access.readable(&bitmap(&[1, 3, 5])), *bitmap(&[1, 5]));
        assert!(RepoAccess::All.can_read_all(&bitmap(&[3])));
    }
}
