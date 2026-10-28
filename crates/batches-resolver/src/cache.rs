//! What resolution caches, so a re-apply re-resolves only what changed.
//!
//! - **Step results** live in `batch_spec_execution_cache_entries`, per user and
//!   [`step_cache_key`]. The resolver looks the keys of every workspace up and records hits;
//!   whoever stores step results must use the same key. A hit is provisional: the step runner
//!   (`sourcegull-batches-steps`) also checks the entry's fingerprint of image digests, secret
//!   values and mounted files, and runs the steps again when it no longer matches.
//! - **Per-commit facts** ([`ResolutionCache`], in process): whether a repository has a
//!   `.batchignore` at a commit, and the directories holding a `rootAtLocationOf` file at a
//!   commit. Both are fixed for a commit, so entries never go stale; they only save repo node
//!   calls and searches when a spec is resolved again and most commits did not move. The `on`
//!   queries and branch heads are always resolved again, because they decide what changed.

use std::sync::Arc;

pub use sourcegull_batches::cache_key::{STEP_CACHE_KEY_VERSION, step_cache_key};
use sourcegull_types::RepoId;

/// Entries kept per kind.
const CAPACITY: u64 = 100_000;

/// In-process memo of per-commit facts (see the module docs). Cheap to clone; clones share
/// entries.
#[derive(Clone)]
pub struct ResolutionCache {
    ignored: moka::future::Cache<(RepoId, String), bool>,
    dirs: moka::future::Cache<(RepoId, String, String), Arc<Vec<String>>>,
}

impl std::fmt::Debug for ResolutionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolutionCache")
            .field("ignored", &self.ignored.entry_count())
            .field("dirs", &self.dirs.entry_count())
            .finish()
    }
}

impl Default for ResolutionCache {
    fn default() -> Self {
        Self {
            ignored: moka::future::Cache::new(CAPACITY),
            dirs: moka::future::Cache::new(CAPACITY),
        }
    }
}

impl ResolutionCache {
    /// Whether the repository has a `.batchignore` at `commit`, if known.
    pub async fn ignored(&self, repo: RepoId, commit: &str) -> Option<bool> {
        self.ignored.get(&(repo, commit.to_owned())).await
    }

    pub async fn set_ignored(&self, repo: RepoId, commit: &str, ignored: bool) {
        self.ignored
            .insert((repo, commit.to_owned()), ignored)
            .await;
    }

    /// The directories holding a file named `file` at `commit`, if known.
    pub async fn dirs(&self, repo: RepoId, commit: &str, file: &str) -> Option<Arc<Vec<String>>> {
        self.dirs
            .get(&(repo, commit.to_owned(), file.to_owned()))
            .await
    }

    pub async fn set_dirs(&self, repo: RepoId, commit: &str, file: &str, dirs: Arc<Vec<String>>) {
        self.dirs
            .insert((repo, commit.to_owned(), file.to_owned()), dirs)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_batches::BatchSpec;

    const TEMPLATE: &str =
        "changesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n";

    fn spec(steps: &str) -> BatchSpec {
        BatchSpec::parse(&format!("name: x\nsteps:\n{steps}{TEMPLATE}")).unwrap()
    }

    #[test]
    fn keys_change_only_from_the_edited_step() {
        let a = spec(
            "  - run: echo 1\n    container: alpine\n  - run: echo 2\n    container: alpine\n",
        );
        let b = spec(
            "  - run: echo 1\n    container: alpine\n  - run: echo 3\n    container: alpine\n",
        );
        let key = |s: &BatchSpec, i| step_cache_key(s, "r", "c", &[], "", false, i);
        assert_eq!(key(&a, 0), key(&b, 0));
        assert_ne!(key(&a, 1), key(&b, 1));
        assert_ne!(key(&a, 0), key(&a, 1));
        // Commit and path are part of the key.
        assert_ne!(key(&a, 0), step_cache_key(&a, "r", "d", &[], "", false, 0));
        assert_ne!(
            key(&a, 0),
            step_cache_key(&a, "r", "c", &[], "sub", false, 0)
        );
        assert_eq!(key(&a, 0).len(), 64);
    }
}
