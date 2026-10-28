//! Copying a repo from one repo node to another (rebalancing).
//!
//! The source answers with a [`CopyHeader`] (its refs, HEAD and origin URL, read at one point
//! in time) and a pack of every object reachable from those refs that the target does not
//! already have (`git pack-objects --revs`, the target's ref tips as `^have`). The target
//! indexes the pack, checks that the new tips are connected (as `git fetch` does), then sets
//! its refs to exactly the header's in one `update-ref` transaction and verifies them. A first
//! copy is built in `<root>/tmp` and renamed into place like a clone; later rounds (catching
//! up with fetches the source did meanwhile) update the copy in place.
//!
//! Nothing here talks to the code host: a move puts no load on GitLab.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use gix::ObjectId;
use tokio_stream::wrappers::ReceiverStream;

use crate::archive::stream_stdout;
use crate::command::GitCommand;
use crate::error::{GitError, Result};
use crate::refs::RefSnapshot;
use crate::sync::{self, snapshot_blocking};

/// What the source sends before the pack: the state the target must end up in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyHeader {
    /// Every direct ref of the source.
    pub refs: RefSnapshot,
    /// HEAD's symbolic target (`refs/heads/main`).
    pub head_ref: Option<String>,
    /// The credential-free `remote.origin.url` the source was cloned from.
    pub origin_url: Option<String>,
}

/// The source side of a copy: the header and, when the target lacks objects, the pack.
pub struct CopySource {
    /// The refs to copy.
    pub header: CopyHeader,
    /// The pack stream (`None` when the target already has every object).
    pub pack: Option<ReceiverStream<Result<Bytes>>>,
}

impl std::fmt::Debug for CopySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopySource")
            .field("header", &self.header)
            .field("pack", &self.pack.is_some())
            .finish()
    }
}

/// The ref tips to pack: the header's targets the target does not have, deduplicated.
pub(crate) fn wanted_tips(refs: &RefSnapshot, haves: &HashSet<ObjectId>) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    refs.values()
        .filter(|id| !haves.contains(*id) && seen.insert(**id))
        .copied()
        .collect()
}

/// `pack-objects --revs` input: the wanted tips, then the haves negated.
fn revs_input(tips: &[ObjectId], haves: &[ObjectId]) -> Bytes {
    let mut s = String::with_capacity((tips.len() + haves.len()) * 42);
    for t in tips {
        s.push_str(&t.to_string());
        s.push('\n');
    }
    for h in haves {
        s.push('^');
        s.push_str(&h.to_string());
        s.push('\n');
    }
    Bytes::from(s)
}

/// Streams a pack of the objects reachable from `tips` but not from `haves` in `repo_dir`.
pub(crate) fn pack_stream(
    git_bin: &Path,
    repo_dir: &Path,
    tips: &[ObjectId],
    haves: &[ObjectId],
    timeout: Duration,
) -> Result<ReceiverStream<Result<Bytes>>> {
    let running = GitCommand::new(git_bin, "pack-objects")
        .repo(repo_dir)
        .args(["--stdout", "--revs", "--delta-base-offset", "-q"])
        .stdin(revs_input(tips, haves))
        .timeout(timeout)
        .spawn_streaming()?;
    Ok(stream_stdout(running, |e| e))
}

/// The origin URL configured in `repo_dir`, if any.
pub(crate) async fn origin_url(git_bin: &Path, repo_dir: &Path) -> Option<String> {
    let out = GitCommand::new(git_bin, "config")
        .repo(repo_dir)
        .args(["--get", "remote.origin.url"])
        .timeout(Duration::from_secs(60))
        .run()
        .await
        .ok()?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!url.is_empty()).then_some(url)
}

/// Creates an empty bare repo at `dir` with the settings of a clone.
pub(crate) async fn init_bare(git_bin: &Path, dir: &Path, origin_url: Option<&str>) -> Result<()> {
    GitCommand::new(git_bin, "init")
        .args(["--bare", "--quiet"])
        .arg(dir)
        .timeout(Duration::from_secs(60))
        .run()
        .await?;
    let mut settings = vec![
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("core.logAllRefUpdates", "false"),
    ];
    if let Some(url) = origin_url {
        settings.push(("remote.origin.url", url));
    }
    for (k, v) in settings {
        GitCommand::new(git_bin, "config")
            .repo(dir)
            .args([k, v])
            .timeout(Duration::from_secs(60))
            .run()
            .await?;
    }
    Ok(())
}

/// Brings the bare repo at `dir` (whose refs are `before`) to the header's state: indexes
/// `pack` into it, checks connectivity, sets the refs and HEAD, and verifies the result.
/// Returns the refs after.
pub(crate) async fn apply(
    git_bin: &Path,
    dir: &Path,
    header: &CopyHeader,
    pack: Option<&Path>,
    before: &RefSnapshot,
    timeout: Duration,
) -> Result<RefSnapshot> {
    if let Some(pack) = pack {
        install_pack(git_bin, dir, pack, timeout).await?;
    }
    let haves: HashSet<ObjectId> = before.values().copied().collect();
    let tips = wanted_tips(&header.refs, &haves);
    if !tips.is_empty() {
        // Every object reachable from the new tips must be here now (git's own check after a
        // fetch); the old tips bound the walk.
        let old: Vec<ObjectId> = haves.into_iter().collect();
        GitCommand::new(git_bin, "rev-list")
            .repo(dir)
            .args(["--objects", "--quiet", "--stdin"])
            .stdin(revs_input(&tips, &old))
            .timeout(timeout)
            .run()
            .await
            .map_err(|e| GitError::Corrupt(format!("the copied objects are incomplete: {e}")))?;
    }
    let mut updates = String::new();
    for (name, id) in &header.refs {
        if before.get(name) != Some(id) {
            updates.push_str(&format!("update {name} {id}\n"));
        }
    }
    for name in before.keys() {
        if !header.refs.contains_key(name) {
            updates.push_str(&format!("delete {name}\n"));
        }
    }
    if !updates.is_empty() {
        GitCommand::new(git_bin, "update-ref")
            .repo(dir)
            .arg("--stdin")
            .stdin(Bytes::from(updates))
            .timeout(timeout)
            .run()
            .await?;
    }
    sync::set_head(git_bin, dir, header.head_ref.as_deref(), &header.refs).await?;
    let after = snapshot_blocking(dir).await?;
    if after != header.refs {
        return Err(GitError::Corrupt(format!(
            "the copy's refs differ from the source's ({} refs, expected {})",
            after.len(),
            header.refs.len()
        )));
    }
    Ok(after)
}

/// Indexes the pack file `pack` (on the same filesystem) and moves it into `dir`'s object
/// store as `pack-<hash>.pack` with its index.
async fn install_pack(git_bin: &Path, dir: &Path, pack: &Path, timeout: Duration) -> Result<()> {
    let out = GitCommand::new(git_bin, "index-pack")
        .repo(dir)
        .arg(pack)
        .timeout(timeout)
        .run()
        .await?;
    let hash = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .map(str::to_owned)
        .filter(|h| h.len() >= 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| GitError::Corrupt("index-pack printed no pack hash".into()))?;
    let idx = pack.with_extension("idx");
    let pack_dir = dir.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;
    // The pack first: readers find packs through their index.
    std::fs::rename(pack, pack_dir.join(format!("pack-{hash}.pack")))?;
    std::fs::rename(&idx, pack_dir.join(format!("pack-{hash}.idx")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[n; 20])
    }

    #[test]
    fn wanted_tips_skip_haves_and_duplicates() {
        let refs: RefSnapshot = [
            ("refs/heads/a".to_owned(), oid(1)),
            ("refs/heads/b".to_owned(), oid(2)),
            ("refs/tags/t".to_owned(), oid(2)),
            ("refs/heads/c".to_owned(), oid(3)),
        ]
        .into_iter()
        .collect();
        let haves: HashSet<ObjectId> = [oid(3)].into_iter().collect();
        assert_eq!(wanted_tips(&refs, &haves), vec![oid(1), oid(2)]);
        let input = revs_input(&[oid(1)], &[oid(3)]);
        assert_eq!(
            std::str::from_utf8(&input).unwrap(),
            format!("{}\n^{}\n", oid(1), oid(3))
        );
    }
}
