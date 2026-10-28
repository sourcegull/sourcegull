//! Copying a repo between two stores (rebalancing): a first full copy, an incremental
//! round after the source fetched (new commits, a force-moved branch, a deleted tag), an
//! up-to-date round without a pack, and a truncated pack that is rejected.

use std::path::PathBuf;
use std::sync::Arc;

use crate::common::{git_in, history_repo, store};
use futures::StreamExt;
use sourcegull_gitstore::{CopySource, GitStore, ObjectId};
use sourcegull_types::RepoId;

/// Writes the source's pack (if any) to a file in the target's temp directory.
async fn receive(
    target: &GitStore,
    src: CopySource,
) -> (sourcegull_gitstore::CopyHeader, Option<PathBuf>) {
    let Some(mut pack) = src.pack else {
        return (src.header, None);
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = pack.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let path = target
        .layout()
        .tmp_dir()
        .join(format!("incoming-{}.pack", bytes.len()));
    std::fs::write(&path, &bytes).unwrap();
    (src.header, Some(path))
}

async fn haves(store: &GitStore, repo: RepoId) -> Vec<ObjectId> {
    store
        .ref_snapshot(repo)
        .await
        .unwrap()
        .map(|s| s.into_values().collect())
        .unwrap_or_default()
}

async fn copy_round(a: &Arc<GitStore>, b: &Arc<GitStore>, repo: RepoId) -> bool {
    let src = a.copy_out(repo, haves(b, repo).await).await.unwrap();
    let had_pack = src.pack.is_some();
    let (header, pack) = receive(b, src).await;
    let out = b.copy_in(repo, &header, pack.as_deref()).await.unwrap();
    assert_eq!(
        b.ref_snapshot(repo).await.unwrap(),
        a.ref_snapshot(repo).await.unwrap()
    );
    assert_eq!(out.head_ref.as_deref(), Some("refs/heads/main"));
    had_pack
}

#[tokio::test]
async fn copies_fully_then_incrementally() {
    let src = history_repo();
    let (_da, a) = store();
    let (_db, b) = store();
    let repo = RepoId(42);
    a.ensure(repo, &src.remote(), false).await.unwrap();

    // Not on the source: RepoNotFound.
    assert!(a.copy_out(RepoId(1), Vec::new()).await.is_err());

    // First copy: built in tmp and moved into place, like a clone.
    assert!(!b.is_cloned(repo));
    assert!(copy_round(&a, &b, repo).await);
    assert!(b.is_cloned(repo));
    let dir = b.repo_dir(repo);
    git_in(&dir, &["fsck", "--connectivity-only", "--no-dangling"]);
    assert_eq!(git_in(&dir, &["config", "--get", "gc.auto"]).trim(), "0");
    assert_eq!(
        git_in(&dir, &["config", "--get", "remote.origin.url"]).trim(),
        src.path().display().to_string()
    );
    // Only the askpass helper is left in tmp.
    assert_eq!(std::fs::read_dir(b.layout().tmp_dir()).unwrap().count(), 1);

    // Nothing changed: no pack.
    assert!(!copy_round(&a, &b, repo).await);

    // The source fetches new commits, a force-moved branch and a deleted tag.
    src.commit("c5: more", &[("src/new.rs", "pub fn new() {}\n")]);
    src.git(&["checkout", "-q", "feature"]);
    src.git(&["reset", "-q", "--hard", "HEAD~1"]);
    src.commit("f2: rewritten", &[("src/lib.rs", "// rewritten\n")]);
    src.git(&["checkout", "-q", "main"]);
    src.git(&["tag", "-d", "light"]);
    a.fetch(repo, &src.remote()).await.unwrap();
    assert!(copy_round(&a, &b, repo).await);
    git_in(&dir, &["fsck", "--connectivity-only", "--no-dangling"]);

    // A branch moved back to a commit the target has: refs only.
    git_in(
        &a.repo_dir(repo),
        &[
            "update-ref",
            "refs/heads/feature",
            "refs/tags/v1.0^{commit}",
        ],
    );
    copy_round(&a, &b, repo).await;
}

#[tokio::test]
async fn truncated_pack_is_rejected() {
    let src = history_repo();
    let (_da, a) = store();
    let (_db, b) = store();
    let repo = RepoId(5);
    a.ensure(repo, &src.remote(), false).await.unwrap();
    let (header, pack) = receive(&b, a.copy_out(repo, Vec::new()).await.unwrap()).await;
    let pack = pack.unwrap();
    let bytes = std::fs::read(&pack).unwrap();
    std::fs::write(&pack, &bytes[..bytes.len() / 2]).unwrap();
    assert!(b.copy_in(repo, &header, Some(&pack)).await.is_err());
    assert!(!b.is_cloned(repo));

    // Refs pointing at objects that never arrived fail the connectivity check.
    assert!(b.copy_in(repo, &header, None).await.is_err());
    assert!(!b.is_cloned(repo));
}
