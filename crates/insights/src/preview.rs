//! Live previews (`searchInsightPreview`): a series counted once over the viewer's
//! readable repositories, with nothing stored, for the creation form to show what a series
//! would look like. It is the backfill's counting (each repository's history counted
//! incrementally on its node) run inline and summed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sourcegull_search_exec::series::counts_from_pb;
use sourcegull_search_web::SearchBackend;
use sourcegull_types::{RepoAccess, RepoId};

use crate::compile::compile;
use crate::jobs::{count_request, time};
use crate::scope::resolve;
use crate::store::{InsightStore, MAX_SERIES_VALUES, NewSeries, OTHER, top_values};

/// Repositories a preview may count; larger scopes are refused (create the trend instead).
pub const PREVIEW_MAX_REPOS: usize = 100;

/// How long a preview's counts may take, over all its repositories.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(60);

/// One previewed series: the count of a search series, or one captured or grouped value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSeries {
    /// `None` for a search series' count and for the values past the top ones ("other").
    pub value: Option<String>,
    /// Oldest first.
    pub points: Vec<(DateTime<Utc>, i64)>,
}

/// Counts `series` at `points` grid times ending at `now` over the repositories `access`
/// may read. Errors are messages for the user.
pub async fn preview(
    backend: &SearchBackend,
    access: &RepoAccess,
    series: &NewSeries,
    points: usize,
    now: DateTime<Utc>,
) -> Result<Vec<PreviewSeries>, String> {
    let q = compile(&series.query, series.method, series.group_by, now)?;
    let catalog = backend.catalog.get(&backend.db).await?;
    let mut repos = resolve(&catalog, &series.scope, &q)?;
    let readable = access.readable(
        &repos
            .iter()
            .filter_map(|r| u32::try_from(r.id).ok())
            .collect(),
    );
    repos.retain(|r| u32::try_from(r.id).is_ok_and(|id| readable.contains(id)));
    let store = InsightStore::new(&backend.db);
    let ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
    let not_cloned = store.not_cloned(&ids).await.map_err(|e| e.to_string())?;
    repos.retain(|r| !not_cloned.contains(&r.id));
    if repos.len() > PREVIEW_MAX_REPOS {
        return Err(format!(
            "A preview counts at most {PREVIEW_MAX_REPOS} repositories and this series covers \
             {}; narrow the repositories or create the insight to see its data.",
            repos.len()
        ));
    }
    // Points are counted at second precision.
    let now =
        chrono::DurationRound::duration_trunc(now, chrono::TimeDelta::seconds(1)).unwrap_or(now);
    let times = series.interval.backfill_times(now, points);
    let mut sums: BTreeMap<String, BTreeMap<DateTime<Utc>, i64>> = BTreeMap::new();
    let deadline = Instant::now() + PREVIEW_TIMEOUT;
    for repo in &repos {
        let mut client = backend
            .repo_clients
            .search_for_repo(RepoId(repo.id))
            .map_err(|e| e.to_string())?;
        // The nodes stop counting at the deadline (and when this call is dropped).
        let mut req =
            sourcegull_rpc::tonic::Request::new(count_request(repo, &q, &times, false, None));
        req.set_timeout(deadline.saturating_duration_since(Instant::now()));
        let resp = client
            .count_series(req)
            .await
            .map_err(|s| format!("counting {}: {}", repo.name, s.message()))?
            .into_inner();
        for p in resp.points {
            for (value, count) in counts_from_pb(&p.counts).0 {
                *sums
                    .entry(value)
                    .or_default()
                    .entry(time(p.time))
                    .or_default() += i64::try_from(count).unwrap_or(i64::MAX);
            }
        }
    }
    // The top values over all points (as every stored point keeps its own top values), the
    // rest folded into "other".
    let latest = sums
        .iter()
        .map(|(v, by_time)| (v.clone(), by_time.values().sum::<i64>()));
    let mut keep: Vec<String> = top_values(latest, MAX_SERIES_VALUES)
        .into_iter()
        .map(|(v, _)| v)
        .filter(|v| v != OTHER)
        .collect();
    keep.sort();
    let mut other: BTreeMap<DateTime<Utc>, i64> = BTreeMap::new();
    let mut out = Vec::new();
    for (value, by_time) in sums {
        if value != OTHER && keep.binary_search(&value).is_ok() {
            out.push(PreviewSeries {
                value: Some(value),
                points: times
                    .iter()
                    .map(|t| (*t, by_time.get(t).copied().unwrap_or(0)))
                    .collect(),
            });
        } else {
            for (t, c) in by_time {
                *other.entry(t).or_default() += c;
            }
        }
    }
    if out.is_empty() || other.values().any(|c| *c > 0) {
        out.push(PreviewSeries {
            value: None,
            points: times
                .iter()
                .map(|t| (*t, other.get(t).copied().unwrap_or(0)))
                .collect(),
        });
    }
    Ok(out)
}
