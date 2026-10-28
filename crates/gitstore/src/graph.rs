//! Commit graph helpers: merge base and ancestry. Both use the commit-graph
//! file when present (written after each fetch).

use gix::ObjectId;

use crate::error::Result;
use crate::read;

/// The best common ancestor of `a` and `b`, or `None` for unrelated histories.
pub fn merge_base(repo: &gix::Repository, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    read::find_commit(repo, a)?;
    read::find_commit(repo, b)?;
    if a == b {
        return Ok(Some(a));
    }
    Ok(repo.merge_base(a, b)?.map(|id| id.detach()))
}

/// Whether `ancestor` is reachable from `descendant` (a commit is its own ancestor).
pub fn is_ancestor(
    repo: &gix::Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    Ok(merge_base(repo, ancestor, descendant)? == Some(ancestor))
}
