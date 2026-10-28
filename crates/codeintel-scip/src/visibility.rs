//! Which uploads serve a commit: per root and
//! indexer, the completed upload at the commit or at the closest ancestor (up to
//! [`MAX_DISTANCE`] commits back), else at the closest descendant among the repository's
//! recent uploads. Precise navigation reads from these uploads, and data retention keeps the
//! uploads that serve the commits its policies protect.
//!
//! Nothing is precomputed: a commit's history comes from the repository node (through
//! [`History`], which callers cache), and the uploads at those commits from one query.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::store::{CodeIntelStore, Upload};
use async_trait::async_trait;
use sourcegull_db::DbError;
use sourcegull_types::RepoId;

/// How far back (in commits) the nearest upload is looked for.
pub const MAX_DISTANCE: u32 = 1000;
/// Recent uploads checked for being descendants of the viewed commit.
const DESCENDANT_CANDIDATES: i64 = 50;
/// Descendant candidates checked per root and indexer.
const DESCENDANT_CHECKS: usize = 3;

/// An upload serving a commit.
#[derive(Debug, Clone)]
pub struct Visible {
    pub upload: Upload,
    /// Commits between the upload's and the viewed one (0: the same commit).
    pub distance: u32,
}

/// The history of one repository.
#[async_trait]
pub trait History: Send + Sync {
    type Error: From<DbError> + std::fmt::Display + Send;

    /// The first [`MAX_DISTANCE`] commits of `commit`'s history, itself first.
    async fn ancestors(&self, commit: &str) -> Result<Arc<Vec<String>>, Self::Error>;
}

/// The uploads serving `commit` of `repo`, nearest first (then deeper roots, then older).
pub async fn nearest_uploads<H: History>(
    store: &CodeIntelStore,
    repo: RepoId,
    commit: &str,
    history: &H,
) -> Result<Vec<Visible>, H::Error> {
    let ancestors = history.ancestors(commit).await?;
    let at: HashMap<&str, u32> = ancestors
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), u32::try_from(i).unwrap_or(u32::MAX)))
        .collect();
    let mut best: BTreeMap<(String, String), Visible> = BTreeMap::new();
    for u in store.completed_at_commits(repo, &ancestors).await? {
        let distance = at.get(u.commit.as_str()).copied().unwrap_or(u32::MAX);
        let key = (u.root.clone(), u.indexer.clone());
        // Ids come newest first, so a tie keeps the newer upload.
        if best.get(&key).is_none_or(|b| distance < b.distance) {
            best.insert(
                key,
                Visible {
                    upload: u,
                    distance,
                },
            );
        }
    }
    // Descendants, for the roots and indexers no ancestor has: the oldest recent uploads
    // are the likeliest to be close.
    let mut candidates: BTreeMap<(String, String), Vec<Upload>> = BTreeMap::new();
    let mut recent = store
        .completed_for_repo(repo, DESCENDANT_CANDIDATES)
        .await?;
    recent.reverse();
    for u in recent {
        let key = (u.root.clone(), u.indexer.clone());
        if best.contains_key(&key) || at.contains_key(u.commit.as_str()) {
            continue;
        }
        let list = candidates.entry(key).or_default();
        if list.len() < DESCENDANT_CHECKS && !list.iter().any(|c| c.commit == u.commit) {
            list.push(u);
        }
    }
    for (key, list) in candidates {
        for u in list {
            let history = match history.ancestors(&u.commit).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!(upload = u.id, error = %e, "reading an upload's history");
                    continue;
                }
            };
            if let Some(i) = history.iter().position(|c| c == commit) {
                let distance = u32::try_from(i).unwrap_or(u32::MAX);
                if best.get(&key).is_none_or(|b| distance < b.distance) {
                    best.insert(
                        key.clone(),
                        Visible {
                            upload: u,
                            distance,
                        },
                    );
                }
            }
        }
    }
    let mut out: Vec<Visible> = best.into_values().collect();
    out.sort_by(|a, b| {
        (
            a.distance,
            std::cmp::Reverse(a.upload.root.len()),
            a.upload.id,
        )
            .cmp(&(
                b.distance,
                std::cmp::Reverse(b.upload.root.len()),
                b.upload.id,
            ))
    });
    Ok(out)
}
