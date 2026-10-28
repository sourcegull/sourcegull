//! Language statistics of one revision (Trends pie charts): lines and bytes per
//! language of the text files.
//!
//! An indexed branch head is answered from the shard's per-doc metadata (`lang`,
//! `line_count`, `size`) without reading any content ([`SearchIndex::language_stats`]). Any
//! other revision is counted from its tree: languages are detected from paths as the indexer
//! detects them, and each blob's line count is cached by blob OID ([`LocCache`]), so files
//! shared between revisions and repositories are read once. Both paths count the same files:
//! text blobs no larger than [`DEFAULT_MAX_FILE_SIZE`] with a detected language.

use std::collections::HashMap;
use std::ops::ControlFlow;

use sourcegull_gitstore::read::{
    DEFAULT_MAX_FILE_SIZE, ListTreeOptions, is_binary, list_tree, resolve_revision,
};
use sourcegull_gitstore::{GitError, ObjectId};
use sourcegull_rpc::ErrorKind;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search::index::{Cancel, DEFAULT_BRANCH, LanguageCount, SearchIndex};
use sourcegull_types::RepoId;

/// Line counts of text blobs by OID (`None`: binary or too large, not counted).
#[derive(Clone)]
pub struct LocCache {
    cache: moka::sync::Cache<ObjectId, Option<u32>>,
}

impl LocCache {
    /// A cache of at most `entries` blobs.
    pub fn new(entries: u64) -> Self {
        Self {
            cache: moka::sync::Cache::new(entries),
        }
    }
}

/// Lines of `content` as the index counts them: newlines, plus a last line without one.
pub fn count_lines(content: &[u8]) -> u32 {
    let n = memchr::memchr_iter(b'\n', content).count();
    let last = content.last().is_some_and(|&b| b != b'\n');
    u32::try_from(n + usize::from(last)).unwrap_or(u32::MAX)
}

/// Tree entries looked at between checks of the cancel flag.
const CANCEL_EVERY: usize = 1024;

/// Counts the tree of `commit`. Returns the counts and how many blobs were read (cache
/// misses). Stops with [`GitError::Cancelled`] once `cancel` is canceled, checked per blob
/// read and per batch of tree entries.
pub fn tree_language_stats(
    repo: &gix::Repository,
    commit: ObjectId,
    cache: &LocCache,
    cancel: &Cancel,
) -> Result<(Vec<LanguageCount>, u64), GitError> {
    let opts = ListTreeOptions {
        recursive: true,
        include_sizes: true,
        limit: usize::MAX,
        ..ListTreeOptions::default()
    };
    let mut by_lang: HashMap<&'static str, (u64, u64)> = HashMap::new();
    let mut read = 0u64;
    let mut failure = None;
    let mut seen = 0usize;
    list_tree(repo, commit, &opts, |e| {
        seen += 1;
        if seen % CANCEL_EVERY == 0 && cancel.is_canceled() {
            failure = Some(GitError::Cancelled("language stats".into()));
            return ControlFlow::Break(());
        }
        if !e.kind.is_blob() {
            return ControlFlow::Continue(());
        }
        let Some(lang) = sourcegull_languages::detect(&e.path, None) else {
            return ControlFlow::Continue(());
        };
        let size = e.size.unwrap_or(0);
        if size > DEFAULT_MAX_FILE_SIZE {
            return ControlFlow::Continue(());
        }
        let lines = match cache.cache.get(&e.oid) {
            Some(l) => l,
            None => {
                if cancel.is_canceled() {
                    failure = Some(GitError::Cancelled("language stats".into()));
                    return ControlFlow::Break(());
                }
                read += 1;
                let blob = match repo.find_blob(e.oid) {
                    Ok(b) => b,
                    Err(err) => {
                        failure = Some(GitError::from(err));
                        return ControlFlow::Break(());
                    }
                };
                let l = (!is_binary(&blob.data)).then(|| count_lines(&blob.data));
                cache.cache.insert(e.oid, l);
                l
            }
        };
        if let Some(lines) = lines {
            let c = by_lang.entry(lang.id).or_default();
            c.0 += u64::from(lines);
            c.1 += size;
        }
        ControlFlow::Continue(())
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    let mut out: Vec<LanguageCount> = by_lang
        .into_iter()
        .map(|(lang, (lines, bytes))| LanguageCount {
            lang: lang.to_owned(),
            lines,
            bytes,
        })
        .collect();
    out.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.lang.cmp(&b.lang)));
    Ok((out, read))
}

/// The indexed branch of `repo` whose head is `commit` (the default branch first).
fn indexed_branch(index: &dyn SearchIndex, repo: RepoId, commit: ObjectId) -> Option<String> {
    let hex = commit.to_string();
    let mut branches = index.branches(repo)?;
    branches.sort_by_key(|b| b.name != DEFAULT_BRANCH);
    branches
        .into_iter()
        .find(|b| b.commit == hex)
        .map(|b| b.name)
}

/// Serves a `LanguageStats` request for an opened repository.
pub(crate) fn language_stats_request(
    repo: &gix::Repository,
    repo_id: RepoId,
    rev: &str,
    index: Option<&dyn SearchIndex>,
    cache: &LocCache,
    cancel: &Cancel,
) -> Result<pb::LanguageStatsResponse, tonic::Status> {
    let commit = match resolve_revision(repo, rev) {
        Ok(c) => c,
        // An empty repository has no languages.
        Err(GitError::RevisionNotFound(_)) if rev.is_empty() => {
            return Ok(pb::LanguageStatsResponse::default());
        }
        Err(e) => return Err(e.into()),
    };
    let from_index = index.and_then(|ix| {
        let branch = indexed_branch(ix, repo_id, commit)?;
        ix.language_stats(repo_id, &branch)
    });
    let indexed = from_index.is_some();
    let counts = match from_index {
        Some(c) => c,
        None => {
            let (c, read) =
                tree_language_stats(repo, commit, cache, cancel).map_err(|e| match e {
                    GitError::Cancelled(_) => ErrorKind::Timeout
                        .status("counting was canceled or reached the call's deadline"),
                    e => e.into(),
                })?;
            tracing::debug!(repo = %repo_id, blobs_read = read, "language stats counted");
            c
        }
    };
    Ok(pb::LanguageStatsResponse {
        languages: counts
            .into_iter()
            .map(|c| pb::LanguageStat {
                name: sourcegull_languages::by_id(&c.lang)
                    .map_or_else(|| c.lang.clone(), |l| l.name.to_owned()),
                total_lines: c.lines,
                total_bytes: c.bytes,
            })
            .collect(),
        commit_oid: commit.to_string(),
        indexed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_counted_as_the_index_counts_them() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"\n\n"), 2);
    }
}
