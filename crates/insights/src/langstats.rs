//! Language statistics of repositories (pie chart trends): each repository's node
//! answers `LanguageStats` for its default branch (from the index's metadata when the head
//! is indexed, else a blob-OID-cached line count), and the counts are summed.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sourcegull_rpc::RepoClients;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_types::RepoId;

/// Lines and bytes of one language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageStat {
    pub name: String,
    pub total_lines: u64,
    pub total_bytes: u64,
}

/// Summed language statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageStats {
    /// Most lines first.
    pub languages: Vec<LanguageStat>,
    /// Whether every repository was answered from the index.
    pub indexed: bool,
}

/// How long counting the language statistics of a set of repositories may take.
const LANGUAGE_STATS_TIMEOUT: Duration = Duration::from_secs(60);

/// The language statistics of `repos` (id, name) at their default branch, summed.
pub async fn language_stats(
    clients: &RepoClients,
    repos: &[(i32, String)],
) -> Result<LanguageStats, String> {
    let mut by_name: HashMap<String, (u64, u64)> = HashMap::new();
    let mut indexed = true;
    let deadline = Instant::now() + LANGUAGE_STATS_TIMEOUT;
    for (id, name) in repos {
        let mut client = clients
            .search_for_repo(RepoId(*id))
            .map_err(|e| e.to_string())?;
        // The nodes stop counting at the deadline (and when this call is dropped).
        let mut req = sourcegull_rpc::tonic::Request::new(pb::LanguageStatsRequest {
            repo: Some(pb::RepoRef {
                id: *id,
                name: name.clone(),
            }),
            rev: String::new(),
        });
        req.set_timeout(deadline.saturating_duration_since(Instant::now()));
        let resp = client
            .language_stats(req)
            .await
            .map_err(|s| format!("language statistics of {name}: {}", s.message()))?
            .into_inner();
        indexed &= resp.indexed;
        for l in resp.languages {
            let e = by_name.entry(l.name).or_default();
            e.0 += l.total_lines;
            e.1 += l.total_bytes;
        }
    }
    let mut languages: Vec<LanguageStat> = by_name
        .into_iter()
        .map(|(name, (total_lines, total_bytes))| LanguageStat {
            name,
            total_lines,
            total_bytes,
        })
        .collect();
    languages.sort_by(|a, b| {
        b.total_lines
            .cmp(&a.total_lines)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(LanguageStats {
        languages,
        indexed: indexed && !repos.is_empty(),
    })
}
