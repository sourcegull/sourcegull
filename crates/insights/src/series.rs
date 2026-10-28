//! Series definitions: what a series counts, over which repositories, and how often.

use chrono::{DateTime, Months, TimeDelta, Utc};

/// How a series turns matches into values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenerationMethod {
    /// The match count (tracked search series).
    Search,
    /// One value per string captured by the query's first capture group.
    MappingCompute,
    /// One value per repository, language, path, commit author or commit date
    /// ([`GroupBy`]).
    SearchCompute,
}

impl GenerationMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::MappingCompute => "mapping-compute",
            Self::SearchCompute => "search-compute",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "search" => Self::Search,
            "mapping-compute" => Self::MappingCompute,
            "search-compute" => Self::SearchCompute,
            _ => return None,
        })
    }
}

/// What a `search-compute` series groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupBy {
    Repo,
    Lang,
    Path,
    /// Commit and diff searches only.
    Author,
    /// The commit's date (committer date, `YYYY-MM-DD` in UTC); commit and diff searches
    /// only.
    Date,
}

impl GroupBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Lang => "lang",
            Self::Path => "path",
            Self::Author => "author",
            Self::Date => "date",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "repo" => Self::Repo,
            "lang" => Self::Lang,
            "path" => Self::Path,
            "author" => Self::Author,
            "date" => Self::Date,
            _ => return None,
        })
    }
}

/// The repositories a series counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoScope {
    /// Every repository (the query's own `repo:` filters still apply).
    All,
    /// Repositories listed by name.
    List(Vec<String>),
    /// Repositories matched by the repository filters of a search query, re-evaluated at
    /// every recording.
    Query(String),
}

impl RepoScope {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::List(_) => "list",
            Self::Query(_) => "query",
        }
    }
}

/// The unit of a sample interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntervalUnit {
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl IntervalUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "HOUR",
            Self::Day => "DAY",
            Self::Week => "WEEK",
            Self::Month => "MONTH",
            Self::Year => "YEAR",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "HOUR" => Self::Hour,
            "DAY" => Self::Day,
            "WEEK" => Self::Week,
            "MONTH" => Self::Month,
            "YEAR" => Self::Year,
            _ => return None,
        })
    }
}

/// How often a series records a point: every `value` `unit`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interval {
    pub unit: IntervalUnit,
    pub value: u32,
}

impl Interval {
    pub fn new(unit: IntervalUnit, value: u32) -> Self {
        Self {
            unit,
            value: value.max(1),
        }
    }

    /// `anchor` moved by `k` intervals (negative goes back). Months and years are calendar
    /// steps from the anchor, so points do not drift (a month after Jan 31 is Feb 28/29).
    pub fn step(&self, anchor: DateTime<Utc>, k: i64) -> Option<DateTime<Utc>> {
        let n = k.checked_mul(i64::from(self.value))?;
        let fixed = |secs: i64| {
            TimeDelta::try_seconds(n.checked_mul(secs)?).and_then(|d| anchor.checked_add_signed(d))
        };
        let months = |per: i64| {
            let m = n.checked_mul(per)?;
            let abs = Months::new(u32::try_from(m.unsigned_abs()).ok()?);
            if m >= 0 {
                anchor.checked_add_months(abs)
            } else {
                anchor.checked_sub_months(abs)
            }
        };
        match self.unit {
            IntervalUnit::Hour => fixed(3600),
            IntervalUnit::Day => fixed(86_400),
            IntervalUnit::Week => fixed(7 * 86_400),
            IntervalUnit::Month => months(1),
            IntervalUnit::Year => months(12),
        }
    }

    /// The `n` points of a backfill ending at `anchor`, oldest first.
    pub fn backfill_times(&self, anchor: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let n = n.max(1) as i64;
        (0..n).rev().filter_map(|k| self.step(anchor, -k)).collect()
    }

    /// The newest grid time (`anchor` + k intervals, k ≥ 1) after `last` that is not after
    /// `now`, or `None` when the next point is not due yet.
    pub fn due(
        &self,
        anchor: DateTime<Utc>,
        last: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let mut due = None;
        // Grid points from the anchor: skip quickly to around `last`, then walk.
        let mut k = 1;
        while let Some(t) = self.step(anchor, k) {
            if t > now {
                break;
            }
            if t > last {
                due = Some(t);
            }
            k += 1;
            // Months and years have few steps; hours across years may have many, so jump.
            if t <= last
                && let Some(skip) = self.estimate_steps(t, last)
                && skip > 1
            {
                k += skip - 1;
            }
        }
        due
    }

    /// Roughly how many intervals fit between `from` and `to` (an underestimate).
    fn estimate_steps(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<i64> {
        let secs = (to - from).num_seconds();
        let per = match self.unit {
            IntervalUnit::Hour => 3600,
            IntervalUnit::Day => 86_400,
            IntervalUnit::Week => 7 * 86_400,
            IntervalUnit::Month => 31 * 86_400,
            IntervalUnit::Year => 366 * 86_400,
        } * i64::from(self.value);
        Some(secs / per)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn backfill_times_end_at_the_anchor() {
        let month = Interval::new(IntervalUnit::Month, 1);
        assert_eq!(
            month.backfill_times(at(2026, 3, 31, 12), 3),
            vec![
                at(2026, 1, 31, 12),
                at(2026, 2, 28, 12),
                at(2026, 3, 31, 12)
            ]
        );
        let week = Interval::new(IntervalUnit::Week, 2);
        assert_eq!(
            week.backfill_times(at(2026, 3, 29, 0), 2),
            vec![at(2026, 3, 15, 0), at(2026, 3, 29, 0)]
        );
        assert_eq!(
            Interval::new(IntervalUnit::Year, 1).backfill_times(at(2026, 1, 1, 0), 1),
            vec![at(2026, 1, 1, 0)]
        );
    }

    #[test]
    fn due_is_the_newest_grid_time_after_the_last_point() {
        let day = Interval::new(IntervalUnit::Day, 1);
        let anchor = at(2026, 1, 1, 6);
        assert_eq!(day.due(anchor, anchor, at(2026, 1, 2, 5)), None);
        assert_eq!(
            day.due(anchor, anchor, at(2026, 1, 2, 6)),
            Some(at(2026, 1, 2, 6))
        );
        // Missed points: only the newest is recorded.
        assert_eq!(
            day.due(anchor, at(2026, 1, 2, 6), at(2026, 1, 5, 7)),
            Some(at(2026, 1, 5, 6))
        );
        let hour = Interval::new(IntervalUnit::Hour, 1);
        assert_eq!(hour.due(anchor, at(2026, 6, 1, 0), at(2026, 6, 1, 0)), None);
        assert_eq!(
            hour.due(anchor, at(2026, 6, 1, 0), at(2026, 6, 1, 1)),
            Some(at(2026, 6, 1, 1))
        );
    }
}
