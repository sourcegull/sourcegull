//! The read path: a series' points as a viewer may see them.
//!
//! Series are counted for the whole instance; repository permissions apply here, at read
//! time. When the viewer can read
//! every repository with points (bitmap containment, the common case) the pre-summed
//! aggregate is returned; otherwise the per-repository points of the readable repositories
//! are summed, and the top values picked again. Unreadable repositories never show, not even
//! through the "other" value.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use roaring::RoaringBitmap;
use sourcegull_db::Result;
use sourcegull_types::RepoAccess;

use crate::store::{InsightStore, MAX_SERIES_VALUES, OTHER, PointValue, Series, top_values};

/// How a series' points were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPath {
    Aggregate,
    PerRepository,
}

/// A series' points for one viewer: every recorded time, oldest first, with its values (at
/// least one per time: [`OTHER`] with 0 when nothing matched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesPoints {
    pub path: ReadPath,
    pub points: Vec<PointValue>,
}

impl SeriesPoints {
    /// The total count per time.
    pub fn totals(&self) -> Vec<(DateTime<Utc>, i64)> {
        let mut out: BTreeMap<DateTime<Utc>, i64> = BTreeMap::new();
        for p in &self.points {
            *out.entry(p.time).or_default() += p.count;
        }
        out.into_iter().collect()
    }
}

/// The points of `series` as a viewer with `access` may see them.
pub async fn points(
    store: &InsightStore,
    series: &Series,
    access: &RepoAccess,
) -> Result<SeriesPoints> {
    points_in(store, series, access, None).await
}

/// [`points`] counting only the repositories in `only` (a view's filters,
/// [`crate::filters`]) when given.
pub async fn points_in(
    store: &InsightStore,
    series: &Series,
    access: &RepoAccess,
    only: Option<&RoaringBitmap>,
) -> Result<SeriesPoints> {
    let times = store.times(series.id).await?;
    let all = match only {
        None => true,
        Some(o) => series.repos.is_subset(o),
    };
    if all && access.can_read_all(&series.repos) {
        let mut rows = store.aggregate_points(series.id).await?;
        rows.retain(|p| times.binary_search(&p.time).is_ok());
        return Ok(SeriesPoints {
            path: ReadPath::Aggregate,
            points: fill(&times, rows),
        });
    }
    let mut readable = access.readable(&series.repos);
    if let Some(o) = only {
        readable &= o;
    }
    let readable: Vec<i32> = readable
        .iter()
        .filter_map(|r| i32::try_from(r).ok())
        .collect();
    let rows = if readable.is_empty() {
        Vec::new()
    } else {
        store.repo_sums(series.id, &readable).await?
    };
    let mut by_time: BTreeMap<DateTime<Utc>, Vec<(String, i64)>> = BTreeMap::new();
    for p in rows {
        by_time.entry(p.time).or_default().push((p.value, p.count));
    }
    let rows = by_time
        .into_iter()
        .filter(|(t, _)| times.binary_search(t).is_ok())
        .flat_map(|(time, vs)| {
            top_values(vs, MAX_SERIES_VALUES)
                .into_iter()
                .map(move |(value, count)| PointValue { time, value, count })
        })
        .collect();
    Ok(SeriesPoints {
        path: ReadPath::PerRepository,
        points: fill(&times, rows),
    })
}

/// Adds a zero [`OTHER`] value for every time without rows; sorted by time, then value.
fn fill(times: &[DateTime<Utc>], mut rows: Vec<PointValue>) -> Vec<PointValue> {
    for t in times {
        if !rows.iter().any(|p| p.time == *t) {
            rows.push(PointValue {
                time: *t,
                value: OTHER.to_owned(),
                count: 0,
            });
        }
    }
    rows.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.value.cmp(&b.value)));
    rows
}
