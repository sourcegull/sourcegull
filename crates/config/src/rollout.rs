//! Bulk changes rollout windows (`batchChanges.rolloutWindows`).
//!
//! The config is a list of windows, each with optional `days` (lower-case weekday names; all
//! days when empty), an optional `start`/`end` time of day (`hh:mm`, UTC; the whole day when
//! both are absent, `end` may be `24:00`) and a `rate`: `"unlimited"`, `0`/`"0"` (publish
//! nothing) or `"<n>/<unit>"` with unit `second|sec|s`, `minute|min|m` or `hour|hr|h`.
//! Outside every window nothing is published; where windows overlap, the one
//! listed last wins.
//!
//! [`RolloutWindows`] is the parsed form: [`RolloutWindows::at`] answers which rate applies
//! at an instant and until when, and [`RolloutWindows::estimate`] when the changeset at a
//! position in the schedule queue would be released if every token went to the queue in
//! order. Validation ([`RolloutWindows::parse`]) reports issues with JSON pointers below the
//! `batchChanges.rolloutWindows` key.

use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveTime, TimeZone, Utc, Weekday};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::validate::ConfigIssue;

/// The site config key.
pub const ROLLOUT_WINDOWS_KEY: &str = "batchChanges.rolloutWindows";

/// Minutes in a day.
const DAY_MINUTES: u32 = 24 * 60;

/// How far [`RolloutWindows::estimate`] looks ahead.
const ESTIMATE_HORIZON_DAYS: i64 = 400;

/// A weekday in a rollout window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RolloutDay {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl RolloutDay {
    fn weekday(self) -> Weekday {
        match self {
            Self::Monday => Weekday::Mon,
            Self::Tuesday => Weekday::Tue,
            Self::Wednesday => Weekday::Wed,
            Self::Thursday => Weekday::Thu,
            Self::Friday => Weekday::Fri,
            Self::Saturday => Weekday::Sat,
            Self::Sunday => Weekday::Sun,
        }
    }
}

/// A `rate` as written: a number (only `0` is meaningful) or a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RolloutRateValue {
    Number(u64),
    Text(String),
}

/// One entry of `batchChanges.rolloutWindows`, as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RolloutWindowConfig {
    /// The days the window applies to; every day when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<RolloutDay>,
    /// Start time of day as `hh:mm` in UTC; requires `end`. The whole day when both are
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// End time of day as `hh:mm` in UTC (exclusive, `24:00` for midnight); requires `start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// How fast changesets are published in the window: `"10/hour"`, `"1/minute"`,
    /// `"unlimited"`, or `0` to publish nothing.
    pub rate: RolloutRateValue,
}

/// The unit of a limited rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RatePeriod {
    Second,
    Minute,
    Hour,
}

impl RatePeriod {
    /// The period's length.
    pub const fn duration(self) -> Duration {
        match self {
            Self::Second => Duration::from_secs(1),
            Self::Minute => Duration::from_secs(60),
            Self::Hour => Duration::from_secs(3600),
        }
    }
}

/// A window's publication rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RolloutRate {
    /// As fast as the reconciler goes.
    Unlimited,
    /// Nothing is published.
    Paused,
    /// `count` changesets per `per`, evenly spaced.
    Limited { count: u32, per: RatePeriod },
}

impl RolloutRate {
    /// Parses `"unlimited"`, `"0"` or `"<n>/<unit>"`.
    pub fn parse(value: &RolloutRateValue) -> Result<Self, String> {
        let text = match value {
            RolloutRateValue::Number(0) => return Ok(Self::Paused),
            RolloutRateValue::Number(n) => {
                return Err(format!(
                    "rate {n} needs a unit, e.g. \"{n}/hour\" (only 0 may be a bare number)"
                ));
            }
            RolloutRateValue::Text(t) => t.trim(),
        };
        if text.eq_ignore_ascii_case("unlimited") {
            return Ok(Self::Unlimited);
        }
        if text == "0" {
            return Ok(Self::Paused);
        }
        let bad = || {
            format!(
                "invalid rate {text:?}: expected \"unlimited\", 0 or \"<n>/<unit>\" with unit \
                 second, minute or hour, e.g. \"10/hour\""
            )
        };
        let (n, unit) = text.split_once('/').ok_or_else(bad)?;
        let count: u32 = n.trim().parse().map_err(|_| bad())?;
        let per = match unit.trim().to_ascii_lowercase().as_str() {
            "second" | "sec" | "s" => RatePeriod::Second,
            "minute" | "min" | "m" => RatePeriod::Minute,
            "hour" | "hr" | "h" => RatePeriod::Hour,
            _ => return Err(bad()),
        };
        Ok(if count == 0 {
            Self::Paused
        } else {
            Self::Limited { count, per }
        })
    }

    /// The time between two releases (`None` unless limited).
    pub fn interval(self) -> Option<Duration> {
        match self {
            Self::Limited { count, per } => Some(per.duration() / count),
            _ => None,
        }
    }
}

/// A parsed rollout window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutWindow {
    /// The weekdays it applies to; every day when empty.
    pub days: Vec<Weekday>,
    /// Start, in minutes after midnight UTC.
    pub start: u32,
    /// End (exclusive), in minutes after midnight UTC; up to 1440.
    pub end: u32,
    pub rate: RolloutRate,
}

impl RolloutWindow {
    fn applies_on(&self, day: Weekday) -> bool {
        self.days.is_empty() || self.days.contains(&day)
    }
}

/// What applies at an instant: the rate of the window in effect (`None` outside every
/// window: nothing is published) and when that stops being true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutSegment {
    pub rate: Option<RolloutRate>,
    /// The index of the window in effect.
    pub window: Option<usize>,
    pub until: DateTime<Utc>,
}

impl RolloutSegment {
    /// Whether changesets are released in this segment.
    pub fn publishes(&self) -> bool {
        matches!(
            self.rate,
            Some(RolloutRate::Unlimited | RolloutRate::Limited { .. })
        )
    }
}

/// The parsed `batchChanges.rolloutWindows`. Empty: no rollout windows, changesets are
/// published right away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolloutWindows {
    pub windows: Vec<RolloutWindow>,
}

/// Parses `hh:mm` (hours 0–23, or 24:00 when `end`) into minutes after midnight.
fn parse_time(s: &str, end: bool) -> Result<u32, String> {
    let bad = || format!("invalid time {s:?}: expected hh:mm in UTC, e.g. \"18:30\"");
    let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: u32 = h.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    if m > 59 || h > 24 || (h == 24 && (m != 0 || !end)) {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

impl RolloutWindows {
    /// Parses and validates the config. Issues carry pointers like `/2/end`, relative to the
    /// list.
    pub fn parse(config: &[RolloutWindowConfig]) -> Result<Self, Vec<ConfigIssue>> {
        let mut windows = Vec::with_capacity(config.len());
        let mut issues = Vec::new();
        for (i, w) in config.iter().enumerate() {
            match Self::parse_one(w) {
                Ok(w) => windows.push(w),
                Err((field, message)) => issues.push(ConfigIssue {
                    pointer: format!("/{i}{field}"),
                    message,
                }),
            }
        }
        if issues.is_empty() {
            Ok(Self { windows })
        } else {
            Err(issues)
        }
    }

    /// Like [`parse`](Self::parse), leaving invalid windows out (the stored config was
    /// validated when saved; this only guards against older documents).
    pub fn lenient(config: &[RolloutWindowConfig]) -> Self {
        Self {
            windows: config
                .iter()
                .filter_map(|w| Self::parse_one(w).ok())
                .collect(),
        }
    }

    fn parse_one(w: &RolloutWindowConfig) -> Result<RolloutWindow, (&'static str, String)> {
        let rate = RolloutRate::parse(&w.rate).map_err(|e| ("/rate", e))?;
        let (start, end) = match (&w.start, &w.end) {
            (None, None) => (0, DAY_MINUTES),
            (Some(s), Some(e)) => {
                let start = parse_time(s, false).map_err(|m| ("/start", m))?;
                let end = parse_time(e, true).map_err(|m| ("/end", m))?;
                if end <= start {
                    return Err(("/end", format!("end {e:?} must be after start {s:?}")));
                }
                (start, end)
            }
            (Some(_), None) => return Err(("", "start is set without end".into())),
            (None, Some(_)) => return Err(("", "end is set without start".into())),
        };
        let mut days: Vec<Weekday> = w.days.iter().map(|d| d.weekday()).collect();
        days.dedup();
        Ok(RolloutWindow {
            days,
            start,
            end,
            rate,
        })
    }

    /// Whether any windows are configured.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// The rate in effect at `at` and until when.
    pub fn at(&self, at: DateTime<Utc>) -> RolloutSegment {
        let day = at.weekday();
        let midnight = Utc.from_utc_datetime(&at.date_naive().and_time(NaiveTime::MIN));
        let minute = u32::try_from((at - midnight).num_minutes()).unwrap_or(0);
        // Boundaries of the day: within two consecutive ones, coverage does not change.
        let mut bounds = vec![0, DAY_MINUTES];
        for w in self.windows.iter().filter(|w| w.applies_on(day)) {
            bounds.push(w.start);
            bounds.push(w.end);
        }
        bounds.sort_unstable();
        bounds.dedup();
        let lo = bounds.iter().copied().rfind(|b| *b <= minute).unwrap_or(0);
        let window = self
            .windows
            .iter()
            .rposition(|w| w.applies_on(day) && w.start <= lo && lo < w.end);
        // Extend over following boundaries where the same window stays in effect.
        let mut hi = DAY_MINUTES;
        for &b in bounds.iter().filter(|b| **b > lo) {
            if b == DAY_MINUTES {
                break;
            }
            let next = self
                .windows
                .iter()
                .rposition(|w| w.applies_on(day) && w.start <= b && b < w.end);
            if next != window {
                hi = b;
                break;
            }
        }
        RolloutSegment {
            rate: window.map(|i| self.windows[i].rate),
            window,
            until: midnight + chrono::Duration::minutes(i64::from(hi)),
        }
    }

    /// When the scheduled changeset at `position` (0 = released next) is expected to be
    /// released, if every token from `now` on goes to the queue in order. `None` without
    /// windows or when no window within about a year would release it.
    pub fn estimate(&self, now: DateTime<Utc>, position: u64) -> Option<DateTime<Utc>> {
        if self.is_empty() {
            return None;
        }
        let horizon = now + chrono::Duration::days(ESTIMATE_HORIZON_DAYS);
        let mut left = position;
        let mut t = now;
        // The next token of the window that was in effect, carried over when the same
        // window continues past a day boundary.
        let mut carry: Option<(usize, DateTime<Utc>)> = None;
        while t < horizon {
            let seg = self.at(t);
            match (seg.rate, seg.window) {
                (Some(RolloutRate::Unlimited), _) => return Some(t),
                (Some(rate @ RolloutRate::Limited { .. }), Some(w)) => {
                    let step = chrono::Duration::from_std(rate.interval()?).ok()?;
                    let first = match carry {
                        Some((cw, next)) if cw == w && next > t => next,
                        _ => t,
                    };
                    if first < seg.until {
                        let span = (seg.until - first).num_nanoseconds()?;
                        let step_ns = step.num_nanoseconds()?.max(1);
                        let fits = u64::try_from((span + step_ns - 1) / step_ns).unwrap_or(0);
                        if left < fits {
                            let k = i32::try_from(left).ok()?;
                            return Some(first + step * k);
                        }
                        left -= fits;
                        let k = i32::try_from(fits).ok()?;
                        carry = Some((w, first + step * k));
                    } else {
                        carry = Some((w, first));
                    }
                }
                _ => carry = None,
            }
            t = seg.until;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: serde_json::Value) -> Vec<RolloutWindowConfig> {
        serde_json::from_value(v).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn rates() {
        use RolloutRate::*;
        let text = |s: &str| RolloutRate::parse(&RolloutRateValue::Text(s.into()));
        let table = [
            ("unlimited", Ok(Unlimited)),
            ("UNLIMITED", Ok(Unlimited)),
            ("0", Ok(Paused)),
            ("0/hour", Ok(Paused)),
            (
                "10/hour",
                Ok(Limited {
                    count: 10,
                    per: RatePeriod::Hour,
                }),
            ),
            (
                "1/min",
                Ok(Limited {
                    count: 1,
                    per: RatePeriod::Minute,
                }),
            ),
            (
                "5/s",
                Ok(Limited {
                    count: 5,
                    per: RatePeriod::Second,
                }),
            ),
            (
                " 3 / hr ",
                Ok(Limited {
                    count: 3,
                    per: RatePeriod::Hour,
                }),
            ),
        ];
        for (s, want) in table {
            assert_eq!(text(s), want, "{s}");
        }
        for s in [
            "", "10", "10/day", "x/hour", "-1/hour", "10/", "/hour", "fast",
        ] {
            assert!(text(s).is_err(), "{s}");
        }
        assert_eq!(RolloutRate::parse(&RolloutRateValue::Number(0)), Ok(Paused));
        assert!(RolloutRate::parse(&RolloutRateValue::Number(5)).is_err());
        assert_eq!(
            text("10/hour").unwrap().interval(),
            Some(Duration::from_secs(360))
        );
        assert_eq!(
            text("4/second").unwrap().interval(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(Unlimited.interval(), None);
    }

    #[test]
    fn parse_windows() {
        let ok = RolloutWindows::parse(&cfg(serde_json::json!([
            {"rate": "10/hour"},
            {"days": ["saturday", "sunday"], "rate": 0},
            {"days": ["monday"], "start": "9:00", "end": "17:30", "rate": "unlimited"},
            {"start": "22:00", "end": "24:00", "rate": "1/minute"},
        ])))
        .unwrap();
        assert_eq!(ok.windows.len(), 4);
        assert_eq!((ok.windows[0].start, ok.windows[0].end), (0, 1440));
        assert_eq!(ok.windows[1].days, vec![Weekday::Sat, Weekday::Sun]);
        assert_eq!((ok.windows[2].start, ok.windows[2].end), (540, 1050));
        assert_eq!(ok.windows[3].end, 1440);

        let table: &[(serde_json::Value, &str, &str)] = &[
            (
                serde_json::json!({"rate": "10/day"}),
                "/0/rate",
                "invalid rate",
            ),
            (serde_json::json!({"rate": 3}), "/0/rate", "needs a unit"),
            (
                serde_json::json!({"start": "10:00", "rate": "unlimited"}),
                "/0",
                "without end",
            ),
            (
                serde_json::json!({"end": "10:00", "rate": "unlimited"}),
                "/0",
                "without start",
            ),
            (
                serde_json::json!({"start": "25:00", "end": "26:00", "rate": "0"}),
                "/0/start",
                "invalid time",
            ),
            (
                serde_json::json!({"start": "24:00", "end": "24:00", "rate": "0"}),
                "/0/start",
                "invalid time",
            ),
            (
                serde_json::json!({"start": "10:60", "end": "11:00", "rate": "0"}),
                "/0/start",
                "invalid time",
            ),
            (
                serde_json::json!({"start": "10:00", "end": "9:59", "rate": "0"}),
                "/0/end",
                "must be after start",
            ),
            (
                serde_json::json!({"start": "10:00", "end": "10:00", "rate": "0"}),
                "/0/end",
                "must be after start",
            ),
            (
                serde_json::json!({"start": "1000", "end": "11:00", "rate": "0"}),
                "/0/start",
                "invalid time",
            ),
        ];
        for (w, pointer, msg) in table {
            let issues = RolloutWindows::parse(&cfg(serde_json::json!([w]))).unwrap_err();
            assert_eq!(issues.len(), 1, "{w}");
            assert_eq!(issues[0].pointer, *pointer, "{w}");
            assert!(
                issues[0].message.contains(msg),
                "{w}: {}",
                issues[0].message
            );
        }
        // Lenient parsing drops the bad ones.
        let w = RolloutWindows::lenient(&cfg(serde_json::json!([
            {"rate": "bad"}, {"rate": "unlimited"}
        ])));
        assert_eq!(w.windows.len(), 1);
    }

    #[test]
    fn segments() {
        // 2026-10-19 is a Monday.
        let w = RolloutWindows::parse(&cfg(serde_json::json!([
            {"days": ["monday", "tuesday"], "start": "09:00", "end": "17:00", "rate": "10/hour"},
            {"days": ["monday"], "start": "12:00", "end": "13:00", "rate": 0},
            {"days": ["tuesday"], "start": "17:00", "end": "24:00", "rate": "10/hour"},
        ])))
        .unwrap();
        let seg = w.at(at("2026-10-19T08:00:00Z"));
        assert_eq!((seg.rate, seg.window), (None, None));
        assert_eq!(seg.until, at("2026-10-19T09:00:00Z"));
        let seg = w.at(at("2026-10-19T09:30:10Z"));
        assert_eq!(seg.window, Some(0));
        assert!(seg.publishes());
        assert_eq!(seg.until, at("2026-10-19T12:00:00Z"));
        // The later window wins where they overlap.
        let seg = w.at(at("2026-10-19T12:59:59Z"));
        assert_eq!(
            (seg.rate, seg.until),
            (Some(RolloutRate::Paused), at("2026-10-19T13:00:00Z"))
        );
        assert!(!seg.publishes());
        let seg = w.at(at("2026-10-19T13:00:00Z"));
        assert_eq!(
            (seg.window, seg.until),
            (Some(0), at("2026-10-19T17:00:00Z"))
        );
        let seg = w.at(at("2026-10-19T17:00:00Z"));
        assert_eq!((seg.window, seg.until), (None, at("2026-10-20T00:00:00Z")));
        // Tuesday: window 0 then window 2 (different windows, same rate).
        let seg = w.at(at("2026-10-20T10:00:00Z"));
        assert_eq!(
            (seg.window, seg.until),
            (Some(0), at("2026-10-20T17:00:00Z"))
        );
        let seg = w.at(at("2026-10-20T17:00:00Z"));
        assert_eq!(
            (seg.window, seg.until),
            (Some(2), at("2026-10-21T00:00:00Z"))
        );
        // Wednesday: nothing all day.
        let seg = w.at(at("2026-10-21T10:00:00Z"));
        assert_eq!((seg.window, seg.until), (None, at("2026-10-22T00:00:00Z")));
        // An all-day window covers the whole day.
        let all = RolloutWindows::parse(&cfg(serde_json::json!([{"rate": "unlimited"}]))).unwrap();
        let seg = all.at(at("2026-10-21T10:00:00Z"));
        assert_eq!(
            (seg.window, seg.until),
            (Some(0), at("2026-10-22T00:00:00Z"))
        );
    }

    #[test]
    fn estimates() {
        let none = RolloutWindows::default();
        assert_eq!(none.estimate(at("2026-10-19T08:00:00Z"), 0), None);

        let w = RolloutWindows::parse(&cfg(serde_json::json!([
            {"days": ["monday"], "start": "09:00", "end": "10:00", "rate": "10/hour"},
        ])))
        .unwrap();
        // Before the window: at its start, then every 6 minutes.
        let now = at("2026-10-19T08:00:00Z");
        assert_eq!(w.estimate(now, 0), Some(at("2026-10-19T09:00:00Z")));
        assert_eq!(w.estimate(now, 3), Some(at("2026-10-19T09:18:00Z")));
        assert_eq!(w.estimate(now, 9), Some(at("2026-10-19T09:54:00Z")));
        // The 11th waits a week.
        assert_eq!(w.estimate(now, 10), Some(at("2026-10-26T09:00:00Z")));
        // Inside the window: from now.
        let now = at("2026-10-19T09:30:00Z");
        assert_eq!(w.estimate(now, 0), Some(now));
        assert_eq!(w.estimate(now, 4), Some(at("2026-10-19T09:54:00Z")));
        assert_eq!(w.estimate(now, 5), Some(at("2026-10-26T09:00:00Z")));

        // A window over midnight keeps its pace across the day boundary.
        let w = RolloutWindows::parse(&cfg(serde_json::json!([{"rate": "7/hour"}]))).unwrap();
        let now = at("2026-10-19T23:50:00Z");
        let step = chrono::Duration::from_std(Duration::from_secs(3600) / 7).unwrap();
        assert_eq!(w.estimate(now, 2), Some(now + step * 2));
        assert_eq!(w.estimate(now, 100), Some(now + step * 100));

        // Unlimited: right away, or at the window's start.
        let w = RolloutWindows::parse(&cfg(serde_json::json!([
            {"start": "12:00", "end": "13:00", "rate": "unlimited"},
        ])))
        .unwrap();
        assert_eq!(
            w.estimate(at("2026-10-19T08:00:00Z"), 5000),
            Some(at("2026-10-19T12:00:00Z"))
        );
        // Only paused windows: never.
        let w = RolloutWindows::parse(&cfg(serde_json::json!([{"rate": 0}]))).unwrap();
        assert_eq!(w.estimate(at("2026-10-19T08:00:00Z"), 0), None);
    }
}
