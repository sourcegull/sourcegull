//! On-disk layout of a repo node.
//!
//! ```text
//! <root>/repos/<shard>/<repo_id>.git   bare repositories
//! <root>/tmp/                          clones in progress, trash, askpass helper
//! <root>/index/                        index shards
//! <root>/cache/                        highlight/symbol caches
//! ```
//!
//! Repos are named by id, so a rename on the code host is O(1). The shard
//! directory (`id % 256`, two hex digits) keeps directories small on nodes
//! with hundreds of thousands of repos.

use std::path::{Path, PathBuf};

use sourcegull_types::RepoId;

/// Paths under a repo node's data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// A layout rooted at `root` (e.g. `/data`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Creates the top-level directories if they are missing.
    pub fn create_dirs(&self) -> std::io::Result<()> {
        for dir in [
            self.repos_dir(),
            self.tmp_dir(),
            self.index_dir(),
            self.cache_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// The data directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Parent of all shard directories.
    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    /// Temporary directory on the same filesystem as the repos, so a finished
    /// clone can be renamed into place atomically.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Index shards.
    pub fn index_dir(&self) -> PathBuf {
        self.root.join("index")
    }

    /// Content-addressed caches.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The shard directory name for `repo`.
    pub fn shard_of(repo: RepoId) -> String {
        format!("{:02x}", (repo.get() as u32) % 256)
    }

    /// The bare repository directory for `repo`.
    pub fn repo_dir(&self, repo: RepoId) -> PathBuf {
        self.repos_dir()
            .join(Self::shard_of(repo))
            .join(format!("{}.git", repo.get()))
    }

    /// Parses a repo id back from a repo directory path (`…/<id>.git`).
    pub fn repo_id_from_dir(path: &Path) -> Option<RepoId> {
        let name = path.file_name()?.to_str()?;
        name.strip_suffix(".git")?.parse().ok().map(RepoId)
    }

    /// Every repo directory on disk, in no particular order.
    pub fn list_repos(&self) -> std::io::Result<Vec<(RepoId, PathBuf)>> {
        let mut out = Vec::new();
        let shards = match std::fs::read_dir(self.repos_dir()) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let path = entry?.path();
                if let Some(id) = Self::repo_id_from_dir(&path) {
                    out.push((id, path));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_are_sharded_by_id() {
        let l = Layout::new("/data");
        assert_eq!(l.repo_dir(RepoId(1)), PathBuf::from("/data/repos/01/1.git"));
        assert_eq!(
            l.repo_dir(RepoId(511)),
            PathBuf::from("/data/repos/ff/511.git")
        );
        assert_eq!(
            Layout::repo_id_from_dir(&l.repo_dir(RepoId(511))),
            Some(RepoId(511))
        );
        assert_eq!(Layout::repo_id_from_dir(Path::new("/x/tmp-1")), None);
    }

    #[test]
    fn list_repos_finds_repo_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let l = Layout::new(dir.path());
        assert!(l.list_repos().unwrap().is_empty());
        l.create_dirs().unwrap();
        for id in [3, 259, 7] {
            std::fs::create_dir_all(l.repo_dir(RepoId(id))).unwrap();
        }
        std::fs::write(l.repos_dir().join("stray-file"), b"").unwrap();
        let mut ids: Vec<i32> = l
            .list_repos()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id.get())
            .collect();
        ids.sort();
        assert_eq!(ids, [3, 7, 259]);
    }
}
