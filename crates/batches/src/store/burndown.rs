//! The burndown chart of a bulk change: daily changeset counts by
//! state, reconstructed from each changeset's timeline in `changeset_events`.
//!
//! Rules:
//! - A changeset counts from the time its merge request was opened on the code host (the
//!   synced merge request's `created_at`); only published changesets are passed in.
//! - Its external state starts as open or draft: the first `MARKED_READY` event means it was
//!   opened as a draft, the first `MARKED_DRAFT` that it was opened ready; without either, its
//!   current draft flag decides.
//! - `CLOSED`, `REOPENED`, `MERGED`, `MARKED_DRAFT` and `MARKED_READY` change the external
//!   state; merged is final. A reopened draft is a draft again, and a
//!   changeset marked as draft while closed stays closed.
//! - Reviews: each author's last `APPROVED` or `CHANGES_REQUESTED` counts (`UNAPPROVED`
//!   withdraws it, like a dismissed review); any changes requested wins over approvals, else
//!   any approval, else pending. Events without an author are skipped.
//! - A merge request deleted on the code host counts as closed from its deletion.
//! - Points are every 24 hours walking back from `end` to `start`, so the last point is `end`.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::{Changeset, ChangesetEvent, ExternalState};

/// The most points a burndown has (ten years of days).
pub const MAX_BURNDOWN_POINTS: usize = 3660;

/// Changeset counts at one point of the burndown chart (`ChangesetCounts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DailyChangesetCounts {
    pub date: DateTime<Utc>,
    /// Changesets opened by then.
    pub total: i32,
    pub merged: i32,
    pub closed: i32,
    pub draft: i32,
    pub open: i32,
    pub open_approved: i32,
    pub open_changes_requested: i32,
    pub open_pending: i32,
}

/// The external state of a changeset in its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryState {
    Open,
    Draft,
    Closed,
    Merged,
}

/// The review state of a changeset in its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryReview {
    Pending,
    Approved,
    ChangesRequested,
}

/// One timeline event as the history needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEvent<'a> {
    pub at: DateTime<Utc>,
    /// The `changeset_events.kind`, e.g. `APPROVED`.
    pub kind: &'a str,
    /// Who did it (the code host username), for reviews.
    pub author: Option<&'a str>,
}

/// The states of a changeset from `at` until the next entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    pub state: HistoryState,
    pub review: HistoryReview,
}

/// What the history of one changeset starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryStart {
    /// When the merge request was opened.
    pub opened_at: DateTime<Utc>,
    /// Whether it is a draft now (used when no event tells how it started).
    pub draft_now: bool,
    /// When it was found deleted on the code host.
    pub deleted_at: Option<DateTime<Utc>>,
}

fn reduce_reviews(by_author: &HashMap<&str, HistoryReview>) -> HistoryReview {
    if by_author
        .values()
        .any(|r| *r == HistoryReview::ChangesRequested)
    {
        HistoryReview::ChangesRequested
    } else if by_author.values().any(|r| *r == HistoryReview::Approved) {
        HistoryReview::Approved
    } else {
        HistoryReview::Pending
    }
}

/// The history of one changeset. `events` must be in time order.
pub fn changeset_history(start: HistoryStart, events: &[HistoryEvent<'_>]) -> Vec<HistoryEntry> {
    let initially_draft = events
        .iter()
        .find_map(|e| match e.kind {
            "MARKED_READY" => Some(true),
            "MARKED_DRAFT" => Some(false),
            _ => None,
        })
        .unwrap_or(start.draft_now);
    let mut draft = initially_draft;
    let mut state = if draft {
        HistoryState::Draft
    } else {
        HistoryState::Open
    };
    let mut review = HistoryReview::Pending;
    let mut by_author: HashMap<&str, HistoryReview> = HashMap::new();
    let mut out = vec![HistoryEntry {
        at: start.opened_at,
        state,
        review,
    }];
    let open_state = |draft: bool| {
        if draft {
            HistoryState::Draft
        } else {
            HistoryState::Open
        }
    };
    for e in events {
        // Events dated before the merge request (clock skew) apply when it opens.
        let at = e.at.max(start.opened_at);
        let merged = state == HistoryState::Merged;
        let changed = match e.kind {
            "MERGED" => {
                state = HistoryState::Merged;
                !merged
            }
            _ if merged => false,
            "CLOSED" => {
                let changed = state != HistoryState::Closed;
                state = HistoryState::Closed;
                changed
            }
            "REOPENED" => {
                let before = state;
                state = open_state(draft);
                before != state
            }
            "MARKED_DRAFT" | "MARKED_READY" => {
                draft = e.kind == "MARKED_DRAFT";
                if matches!(state, HistoryState::Open | HistoryState::Draft) {
                    let before = state;
                    state = open_state(draft);
                    before != state
                } else {
                    false
                }
            }
            kind @ ("APPROVED" | "CHANGES_REQUESTED" | "UNAPPROVED") => {
                let Some(author) = e.author.filter(|a| !a.is_empty()) else {
                    continue;
                };
                match kind {
                    "APPROVED" => by_author.insert(author, HistoryReview::Approved),
                    "CHANGES_REQUESTED" => {
                        by_author.insert(author, HistoryReview::ChangesRequested)
                    }
                    _ => by_author.remove(author),
                };
                let before = review;
                review = reduce_reviews(&by_author);
                before != review
            }
            _ => false,
        };
        if changed {
            out.push(HistoryEntry { at, state, review });
        }
    }
    if let Some(deleted) = start.deleted_at
        && state != HistoryState::Merged
    {
        state = HistoryState::Closed;
        out.push(HistoryEntry {
            at: deleted.max(start.opened_at),
            state,
            review,
        });
    }
    out
}

/// The entry in effect at `t`, or `None` before the changeset was opened.
pub fn state_at(history: &[HistoryEntry], t: DateTime<Utc>) -> Option<HistoryEntry> {
    history.iter().take_while(|e| e.at <= t).last().copied()
}

/// The points of a burndown: every 24 hours from `end` back to `start`, oldest first.
pub fn burndown_points(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut points = Vec::new();
    let mut t = end;
    while t >= start && points.len() < MAX_BURNDOWN_POINTS {
        points.push(t);
        t -= Duration::days(1);
    }
    points.reverse();
    points
}

/// Counts the histories at each point.
pub fn count_histories(
    points: &[DateTime<Utc>],
    histories: &[Vec<HistoryEntry>],
) -> Vec<DailyChangesetCounts> {
    points
        .iter()
        .map(|&date| {
            let mut c = DailyChangesetCounts {
                date,
                ..Default::default()
            };
            for h in histories {
                let Some(e) = state_at(h, date) else { continue };
                c.total += 1;
                match e.state {
                    HistoryState::Draft => c.draft += 1,
                    HistoryState::Open => {
                        c.open += 1;
                        match e.review {
                            HistoryReview::Pending => c.open_pending += 1,
                            HistoryReview::Approved => c.open_approved += 1,
                            HistoryReview::ChangesRequested => c.open_changes_requested += 1,
                        }
                    }
                    HistoryState::Merged => c.merged += 1,
                    HistoryState::Closed => c.closed += 1,
                }
            }
            c
        })
        .collect()
}

/// The raw merge request in a changeset's synced metadata.
fn merge_request(metadata: &serde_json::Value) -> &serde_json::Value {
    match metadata.get("mergeRequest") {
        Some(mr) if mr.is_object() => mr,
        _ => metadata,
    }
}

/// The username of an event's author (`metadata.author`, a username or a user object).
fn event_author(e: &ChangesetEvent) -> Option<&str> {
    let m = &e.metadata.0;
    let a = m.get("author").or_else(|| m.get("user"))?;
    a.as_str()
        .or_else(|| a.get("username").and_then(serde_json::Value::as_str))
}

/// Where the history of a stored changeset starts: when its merge request was opened (from
/// the synced merge request, else its first event, else when the changeset was created).
pub fn history_start(c: &Changeset, events: &[&ChangesetEvent]) -> HistoryStart {
    let opened_at = merge_request(&c.metadata)
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| events.iter().map(|e| e.created_at).min())
        .unwrap_or(c.created_at);
    HistoryStart {
        opened_at,
        draft_now: c.external_state == Some(ExternalState::Draft),
        deleted_at: c.external_deleted_at,
    }
}

/// The burndown of `changesets` (published ones) from their `events` between `start` and
/// `end`.
pub fn changeset_counts_over_time(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    changesets: &[Changeset],
    events: &[ChangesetEvent],
) -> Vec<DailyChangesetCounts> {
    let mut by_changeset: HashMap<i64, Vec<&ChangesetEvent>> = HashMap::new();
    for e in events {
        by_changeset.entry(e.changeset_id).or_default().push(e);
    }
    let histories: Vec<Vec<HistoryEntry>> = changesets
        .iter()
        .map(|c| {
            let mut evs = by_changeset.remove(&c.id).unwrap_or_default();
            evs.sort_by_key(|e| (e.created_at, e.id));
            let start = history_start(c, &evs);
            let hist: Vec<HistoryEvent<'_>> = evs
                .iter()
                .map(|e| HistoryEvent {
                    at: e.created_at,
                    kind: e.kind.as_str(),
                    author: event_author(e),
                })
                .collect();
            changeset_history(start, &hist)
        })
        .collect();
    count_histories(&burndown_points(start, end), &histories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn day(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, d, 12, 0, 0).unwrap()
    }

    fn ev(d: u32, kind: &'static str, author: Option<&'static str>) -> HistoryEvent<'static> {
        HistoryEvent {
            at: day(d) + Duration::hours(1),
            kind,
            author,
        }
    }

    fn start(d: u32) -> HistoryStart {
        HistoryStart {
            opened_at: day(d),
            draft_now: false,
            deleted_at: None,
        }
    }

    /// (open, draft, merged, closed, open_approved, open_changes_requested) per day 1..=n.
    type Row = (i32, i32, i32, i32, i32, i32);

    fn rows(counts: &[DailyChangesetCounts]) -> Vec<Row> {
        counts
            .iter()
            .map(|c| {
                assert_eq!(c.total, c.open + c.draft + c.merged + c.closed, "{c:?}");
                assert_eq!(
                    c.open,
                    c.open_approved + c.open_changes_requested + c.open_pending
                );
                (
                    c.open,
                    c.draft,
                    c.merged,
                    c.closed,
                    c.open_approved,
                    c.open_changes_requested,
                )
            })
            .collect()
    }

    /// Daily counts (at noon, days 1..=6) of changesets given by their start and events.
    fn burndown(changesets: &[(HistoryStart, Vec<HistoryEvent<'static>>)]) -> Vec<Row> {
        let histories: Vec<_> = changesets
            .iter()
            .map(|(s, e)| changeset_history(*s, e))
            .collect();
        rows(&count_histories(
            &burndown_points(day(1), day(6)),
            &histories,
        ))
    }

    #[test]
    fn points_walk_back_from_end() {
        let pts = burndown_points(day(1), day(3));
        assert_eq!(pts, vec![day(1), day(2), day(3)]);
        let pts = burndown_points(day(1) + Duration::hours(1), day(3));
        assert_eq!(pts, vec![day(2), day(3)]);
        assert!(burndown_points(day(3), day(1)).is_empty());
        let far = burndown_points(day(1) - Duration::days(100_000), day(1));
        assert_eq!(far.len(), MAX_BURNDOWN_POINTS);
        assert_eq!(far.last(), Some(&day(1)));
    }

    /// A named table case: changesets (start and events) and the expected rows.
    type Case = (
        &'static str,
        Vec<(HistoryStart, Vec<HistoryEvent<'static>>)>,
        Vec<Row>,
    );

    #[test]
    fn burndown_tables() {
        let none: Vec<HistoryEvent<'static>> = vec![];
        let cases: Vec<Case> = vec![
            (
                "opened on day 2, nothing else",
                vec![(start(2), none.clone())],
                vec![
                    (0, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                ],
            ),
            (
                "approved, changes requested by another, merged",
                vec![(
                    start(1),
                    vec![
                        ev(1, "APPROVED", Some("bob")),
                        ev(2, "CHANGES_REQUESTED", Some("carol")),
                        ev(3, "APPROVED", Some("carol")),
                        ev(4, "MERGED", None),
                        ev(5, "CLOSED", None),
                    ],
                )],
                vec![
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 1, 0),
                    (1, 0, 0, 0, 0, 1),
                    (1, 0, 0, 0, 1, 0),
                    (0, 0, 1, 0, 0, 0),
                    (0, 0, 1, 0, 0, 0),
                ],
            ),
            (
                "unapproved withdraws; reviews without author are skipped",
                vec![(
                    start(1),
                    vec![
                        ev(1, "APPROVED", Some("bob")),
                        ev(2, "CHANGES_REQUESTED", None),
                        ev(3, "UNAPPROVED", Some("bob")),
                    ],
                )],
                vec![
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 1, 0),
                    (1, 0, 0, 0, 1, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                ],
            ),
            (
                "opened as draft (first event marks it ready), closed, reopened",
                vec![(
                    start(1),
                    vec![
                        ev(2, "MARKED_READY", None),
                        ev(3, "CLOSED", None),
                        ev(4, "REOPENED", None),
                        ev(5, "MARKED_DRAFT", None),
                    ],
                )],
                vec![
                    (0, 1, 0, 0, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (0, 0, 0, 1, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                ],
            ),
            (
                "a draft now without events was always a draft; a closed draft reopens as one",
                vec![(
                    HistoryStart {
                        draft_now: true,
                        ..start(1)
                    },
                    vec![ev(2, "CLOSED", None), ev(3, "REOPENED", None)],
                )],
                vec![
                    (0, 1, 0, 0, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                    (0, 0, 0, 1, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                    (0, 1, 0, 0, 0, 0),
                ],
            ),
            (
                "deleted on the code host: closed",
                vec![(
                    HistoryStart {
                        deleted_at: Some(day(3) + Duration::hours(2)),
                        ..start(1)
                    },
                    none.clone(),
                )],
                vec![
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (0, 0, 0, 1, 0, 0),
                    (0, 0, 0, 1, 0, 0),
                    (0, 0, 0, 1, 0, 0),
                ],
            ),
            (
                "several changesets",
                vec![
                    (start(1), vec![ev(2, "MERGED", None)]),
                    (start(3), vec![ev(4, "CLOSED", None)]),
                    (start(5), vec![ev(5, "APPROVED", Some("bob"))]),
                ],
                vec![
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 0, 0, 0, 0),
                    (1, 0, 1, 0, 0, 0),
                    (1, 0, 1, 0, 0, 0),
                    (1, 0, 1, 1, 0, 0),
                    (1, 0, 1, 1, 1, 0),
                ],
            ),
        ];
        for (name, changesets, want) in cases {
            assert_eq!(burndown(&changesets), want, "{name}");
        }
    }

    #[test]
    fn events_before_opening_apply_at_opening() {
        let h = changeset_history(start(3), &[ev(1, "MERGED", None)]);
        assert_eq!(state_at(&h, day(2)), None);
        assert_eq!(state_at(&h, day(3)).unwrap().state, HistoryState::Merged);
    }
}
