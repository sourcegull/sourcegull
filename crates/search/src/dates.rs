//! Date expressions for `before:`, `after:` and `repo:has.commit.after()`.
//!
//! Accepts what users type in practice: `now`, `today`, `yesterday`,
//! `3 days ago`, `a week ago`, `2 weeks` (implicit "ago"), `last month`,
//! ISO dates (`2024-01-31`), date and time (`2024-01-31 12:00`) and RFC 3339
//! (`2024-01-31T12:00:00Z`). Dates without a zone are UTC.

use chrono::{DateTime, Days, Months, NaiveDate, NaiveDateTime, TimeDelta, Utc};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid date {0:?}: try \"2 weeks ago\", \"yesterday\" or \"2024-01-31\"")]
pub struct InvalidDate(pub String);

/// Resolves `expr` relative to `now`.
pub fn parse_date(expr: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, InvalidDate> {
    let invalid = || InvalidDate(expr.to_string());
    let s = expr.trim().to_ascii_lowercase();
    let start_of_day = |t: DateTime<Utc>| {
        t.date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
    };
    match s.as_str() {
        "now" => return Ok(now),
        "today" => return Ok(start_of_day(now)),
        "yesterday" => return Ok(start_of_day(now - TimeDelta::days(1))),
        _ => {}
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(expr.trim()) {
        return Ok(t.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(&s, format) {
            return Ok(t.and_utc());
        }
    }
    for format in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(&s, format) {
            return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
        }
    }

    // "<n> <unit>[s] [ago]" and "last <unit>".
    let words: Vec<&str> = s.split_whitespace().collect();
    let (n, unit) = match words.as_slice() {
        ["last", unit] => (1, *unit),
        [n, unit] | [n, unit, "ago"] => {
            let n = match *n {
                "a" | "an" | "one" => 1,
                n => n.parse::<u32>().map_err(|_| invalid())?,
            };
            (n, *unit)
        }
        _ => return Err(invalid()),
    };
    let unit = unit.strip_suffix('s').unwrap_or(unit);
    let t = match unit {
        "second" | "sec" => now.checked_sub_signed(TimeDelta::seconds(n.into())),
        "minute" | "min" => now.checked_sub_signed(TimeDelta::minutes(n.into())),
        "hour" | "hr" => now.checked_sub_signed(TimeDelta::hours(n.into())),
        "day" => now.checked_sub_days(Days::new(n.into())),
        "week" => now.checked_sub_days(Days::new(u64::from(n) * 7)),
        "month" => now.checked_sub_months(Months::new(n)),
        "year" => n
            .checked_mul(12)
            .and_then(|m| now.checked_sub_months(Months::new(m))),
        _ => None,
    };
    t.ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 3, 31, 15, 30, 0).unwrap()
    }

    fn d(s: &str) -> String {
        parse_date(s, now()).unwrap().to_rfc3339()
    }

    #[test]
    fn keywords() {
        assert_eq!(d("now"), "2024-03-31T15:30:00+00:00");
        assert_eq!(d("Today"), "2024-03-31T00:00:00+00:00");
        assert_eq!(d("yesterday"), "2024-03-30T00:00:00+00:00");
    }

    #[test]
    fn relative() {
        assert_eq!(d("3 days ago"), "2024-03-28T15:30:00+00:00");
        assert_eq!(d("a week ago"), "2024-03-24T15:30:00+00:00");
        assert_eq!(d("2 weeks"), "2024-03-17T15:30:00+00:00");
        // Month arithmetic clamps to the end of the month.
        assert_eq!(d("1 month ago"), "2024-02-29T15:30:00+00:00");
        assert_eq!(d("last year"), "2023-03-31T15:30:00+00:00");
        assert_eq!(d("90 minutes ago"), "2024-03-31T14:00:00+00:00");
    }

    #[test]
    fn absolute() {
        assert_eq!(d("2024-01-31"), "2024-01-31T00:00:00+00:00");
        assert_eq!(d("2024-01-31 08:15"), "2024-01-31T08:15:00+00:00");
        assert_eq!(d("2024-01-31T08:15:00+02:00"), "2024-01-31T06:15:00+00:00");
    }

    #[test]
    fn invalid() {
        for s in [
            "",
            "soon",
            "3 fortnights ago",
            "x days ago",
            "2024-13-01",
            "last",
        ] {
            assert!(parse_date(s, now()).is_err(), "{s}");
        }
    }
}
