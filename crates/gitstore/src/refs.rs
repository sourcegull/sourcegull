//! Ref snapshots, ref update detection and branch/tag listing.
//!
//! After every fetch the store diffs the refs: the caller records the
//! changes as `repo_ref_events` and publishes `refs_updated`, which drives
//! reindexing, watches and trends.

use std::collections::BTreeMap;

use gix::ObjectId;

use crate::error::{GitError, Result};

/// Ref name → direct target. Symbolic refs (HEAD) are not included.
pub type RefSnapshot = BTreeMap<String, ObjectId>;

/// A changed ref. `old` is `None` for a created ref, `new` is `None` for a deleted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name, e.g. `refs/heads/main`.
    pub name: String,
    /// Previous target.
    pub old: Option<ObjectId>,
    /// New target.
    pub new: Option<ObjectId>,
}

/// Reads every direct ref of `repo`.
pub fn snapshot(repo: &gix::Repository) -> Result<RefSnapshot> {
    let mut out = RefSnapshot::new();
    let platform = repo.references()?;
    for r in platform.all()? {
        let r = r.map_err(GitError::gix)?;
        if let Some(id) = r.target().try_id() {
            out.insert(r.name().as_bstr().to_string(), id.to_owned());
        }
    }
    Ok(out)
}

/// The changes from `before` to `after`, sorted by ref name.
pub fn diff(before: &RefSnapshot, after: &RefSnapshot) -> Vec<RefUpdate> {
    let mut out = Vec::new();
    for (name, old) in before {
        match after.get(name) {
            Some(new) if new == old => {}
            new => out.push(RefUpdate {
                name: name.clone(),
                old: Some(*old),
                new: new.copied(),
            }),
        }
    }
    for (name, new) in after {
        if !before.contains_key(name) {
            out.push(RefUpdate {
                name: name.clone(),
                old: None,
                new: Some(*new),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The kind of a listed ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// `refs/heads/*`.
    Branch,
    /// `refs/tags/*`.
    Tag,
    /// Anything else (e.g. `refs/merge-requests/*`).
    Other,
}

/// A branch or tag with its peeled commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// Full name.
    pub name: String,
    /// Name without `refs/heads/` or `refs/tags/`.
    pub short_name: String,
    /// Branch, tag or other.
    pub kind: RefKind,
    /// Direct target (a tag object for annotated tags).
    pub target: ObjectId,
    /// The commit the ref peels to, if it peels to a commit.
    pub commit: Option<ObjectId>,
    /// Committer time of `commit`, seconds since the epoch.
    pub commit_time: Option<i64>,
    /// Whether the ref is an annotated tag.
    pub annotated: bool,
}

/// What [`list_refs`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListRefsFilter<'a> {
    /// Include `refs/heads/*`.
    pub branches: bool,
    /// Include `refs/tags/*`.
    pub tags: bool,
    /// Only refs whose short name starts with this.
    pub prefix: &'a str,
}

/// Lists branches and/or tags, newest commit first (ties by name).
pub fn list_refs(repo: &gix::Repository, filter: ListRefsFilter<'_>) -> Result<Vec<RefInfo>> {
    let mut out = Vec::new();
    let platform = repo.references()?;
    let mut collect = |iter: gix::reference::iter::Iter<'_, '_>, kind: RefKind| -> Result<()> {
        for r in iter {
            let mut r = r.map_err(GitError::gix)?;
            let Some(target) = r.target().try_id().map(|id| id.to_owned()) else {
                continue;
            };
            let name = r.name().as_bstr().to_string();
            let short_name = r.name().shorten().to_string();
            if !short_name.starts_with(filter.prefix) {
                continue;
            }
            let commit = r.peel_to_commit().ok();
            let commit_time = commit
                .as_ref()
                .and_then(|c| c.time().ok())
                .map(|t| t.seconds);
            let commit_id = commit.map(|c| c.id);
            out.push(RefInfo {
                name,
                short_name,
                kind,
                annotated: kind == RefKind::Tag
                    && repo
                        .find_header(target)
                        .map(|h| h.kind() == gix::object::Kind::Tag)
                        .unwrap_or(false),
                target,
                commit: commit_id,
                commit_time,
            });
        }
        Ok(())
    };
    if filter.branches {
        collect(platform.local_branches()?, RefKind::Branch)?;
    }
    if filter.tags {
        collect(platform.tags()?, RefKind::Tag)?;
    }
    out.sort_by(|a, b| {
        b.commit_time
            .cmp(&a.commit_time)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// HEAD's symbolic target (e.g. `refs/heads/main`) and the commit it points
/// to; either is `None` when HEAD is detached, unborn or dangling.
pub fn head(repo: &gix::Repository) -> Result<(Option<String>, Option<ObjectId>)> {
    let name = repo.head_name()?.map(|n| n.as_bstr().to_string());
    let id = repo.head_id().ok().map(|id| id.detach());
    Ok((name, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[n; 20])
    }

    #[test]
    fn diff_reports_created_updated_deleted() {
        let before: RefSnapshot = [
            ("refs/heads/main".to_owned(), oid(1)),
            ("refs/heads/old".to_owned(), oid(2)),
            ("refs/tags/v1".to_owned(), oid(3)),
        ]
        .into();
        let after: RefSnapshot = [
            ("refs/heads/main".to_owned(), oid(4)),
            ("refs/heads/new".to_owned(), oid(5)),
            ("refs/tags/v1".to_owned(), oid(3)),
        ]
        .into();
        assert_eq!(
            diff(&before, &after),
            vec![
                RefUpdate {
                    name: "refs/heads/main".into(),
                    old: Some(oid(1)),
                    new: Some(oid(4))
                },
                RefUpdate {
                    name: "refs/heads/new".into(),
                    old: None,
                    new: Some(oid(5))
                },
                RefUpdate {
                    name: "refs/heads/old".into(),
                    old: Some(oid(2)),
                    new: None
                },
            ]
        );
        assert!(diff(&after, &after).is_empty());
    }
}
