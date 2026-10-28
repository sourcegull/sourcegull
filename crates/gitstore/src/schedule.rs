//! Fetch scheduling.
//!
//! - [`next_fetch`] is the policy, a pure function: given what is known about
//!   a repo's fetch history it returns when to fetch next. The interval adapts
//!   to how often the repo changes, `clamp(2 × mean change interval, 45 s, 8 h)`;
//!   without a mean it falls back to half the time since the last change.
//!   Failures back off exponentially. Deterministic jitter (from the repo id)
//!   spreads fetches so restarts do not cause a thundering herd.
//! - [`UpdateIntervalRules`] implements `gitUpdateInterval` overrides.
//! - [`FetchQueue`] is the priority queue: webhook signals first, then manual
//!   "update now", then scheduled fetches, each in due-time order.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_types::RepoId;

/// Tunables for [`next_fetch`].
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulePolicy {
    /// Shortest interval between fetches.
    pub min_interval: Duration,
    /// Longest interval between fetches.
    pub max_interval: Duration,
    /// Delay after the first failure; doubles per consecutive failure.
    pub error_backoff_base: Duration,
    /// Longest delay after failures.
    pub error_backoff_max: Duration,
    /// Relative jitter, e.g. 0.1 for ±10 %.
    pub jitter: f64,
}

impl Default for SchedulePolicy {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(45),
            max_interval: Duration::from_secs(8 * 3600),
            error_backoff_base: Duration::from_secs(60),
            error_backoff_max: Duration::from_secs(8 * 3600),
            jitter: 0.1,
        }
    }
}

/// What is known about a repo's fetches (from `gitserver_repos`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchHistory {
    /// Last successful fetch or clone.
    pub last_fetched: Option<DateTime<Utc>>,
    /// Last time a fetch changed any ref.
    pub last_changed: Option<DateTime<Utc>>,
    /// Moving average of the time between changes (see [`update_mean_change_interval`]).
    pub mean_change_interval: Option<Duration>,
    /// Failed attempts since the last success.
    pub consecutive_failures: u32,
    /// Last attempt, successful or not.
    pub last_attempt: Option<DateTime<Utc>>,
}

/// When to fetch next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextFetch {
    /// Immediately (never cloned).
    Now,
    /// At this time.
    At(DateTime<Utc>),
    /// Not automatically (`disableAutoGitUpdates`).
    Never,
}

/// The fetch policy. `interval_override` comes from
/// [`UpdateIntervalRules::interval_for`]; `auto_updates` is false when
/// `disableAutoGitUpdates` is set.
pub fn next_fetch(
    repo: RepoId,
    h: &FetchHistory,
    policy: &SchedulePolicy,
    interval_override: Option<Duration>,
    auto_updates: bool,
) -> NextFetch {
    if !auto_updates {
        return NextFetch::Never;
    }
    let Some(base) = h.last_attempt.max(h.last_fetched) else {
        return NextFetch::Now;
    };
    if h.consecutive_failures > 0 {
        let exp = (h.consecutive_failures - 1).min(30);
        let delay = policy
            .error_backoff_base
            .saturating_mul(1u32 << exp)
            .min(policy.error_backoff_max);
        return NextFetch::At(base + to_chrono(jitter(repo, h, delay, policy.jitter)));
    }
    let interval = interval_override.unwrap_or_else(|| adaptive_interval(h, policy));
    let from = h.last_fetched.unwrap_or(base);
    NextFetch::At(from + to_chrono(jitter(repo, h, interval, policy.jitter)))
}

/// `clamp(2 × mean change interval, min, max)`, or half the time between the
/// last change and the last fetch when no mean is known yet.
pub fn adaptive_interval(h: &FetchHistory, policy: &SchedulePolicy) -> Duration {
    let raw = match (h.mean_change_interval, h.last_fetched, h.last_changed) {
        (Some(mean), _, _) => mean.saturating_mul(2),
        (None, Some(fetched), Some(changed)) => {
            (fetched - changed).to_std().unwrap_or_default() / 2
        }
        _ => policy.max_interval,
    };
    raw.clamp(policy.min_interval, policy.max_interval)
}

/// Updates the moving average of the time between changes after a fetch
/// that changed refs at `now` (exponential moving average, weight 0.3).
pub fn update_mean_change_interval(
    previous_mean: Option<Duration>,
    previous_change: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Duration> {
    let sample = (now - previous_change?).to_std().ok()?;
    Some(match previous_mean {
        Some(m) => Duration::from_secs_f64(m.as_secs_f64() * 0.7 + sample.as_secs_f64() * 0.3),
        None => sample,
    })
}

fn jitter(repo: RepoId, h: &FetchHistory, d: Duration, amount: f64) -> Duration {
    if amount <= 0.0 {
        return d;
    }
    // Deterministic in (repo, attempt) so the function stays pure.
    let seed = (repo.get() as u32 as u64)
        ^ (u64::from(h.consecutive_failures) << 32)
        ^ h.last_attempt
            .or(h.last_fetched)
            .map_or(0, |t| t.timestamp() as u64);
    let unit = (mix(seed) >> 11) as f64 / (1u64 << 53) as f64; // [0, 1)
    d.mul_f64(1.0 + amount * (unit * 2.0 - 1.0))
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn to_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

/// Error returned for an invalid `gitUpdateInterval` pattern.
#[derive(Debug, thiserror::Error)]
#[error("invalid gitUpdateInterval pattern {pattern:?}: {message}")]
pub struct InvalidRule {
    /// The pattern.
    pub pattern: String,
    /// Regex error.
    pub message: String,
}

/// `gitUpdateInterval` rules: the first rule whose regex matches the repo
/// name sets a fixed fetch interval.
#[derive(Debug, Clone, Default)]
pub struct UpdateIntervalRules {
    rules: Vec<(regex::Regex, Duration)>,
}

impl UpdateIntervalRules {
    /// Compiles `(pattern, interval)` rules in order.
    pub fn new<'a>(
        rules: impl IntoIterator<Item = (&'a str, Duration)>,
    ) -> Result<Self, InvalidRule> {
        let rules = rules
            .into_iter()
            .map(|(p, d)| {
                regex::Regex::new(p)
                    .map(|r| (r, d))
                    .map_err(|e| InvalidRule {
                        pattern: p.to_owned(),
                        message: e.to_string(),
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { rules })
    }

    /// The fixed interval for `repo_name`, if a rule matches.
    pub fn interval_for(&self, repo_name: &str) -> Option<Duration> {
        self.rules
            .iter()
            .find(|(r, _)| r.is_match(repo_name))
            .map(|(_, d)| *d)
    }
}

/// Why a fetch was queued; lower sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FetchPriority {
    /// A webhook or `last_activity_at` signal: the repo is known to have changed.
    Signal,
    /// A user or admin asked for an update.
    Manual,
    /// Periodic fetch from [`next_fetch`].
    Scheduled,
}

/// A due-time priority queue of repos to fetch, one entry per repo.
///
/// Pushing a repo that is already queued keeps the more urgent of the two
/// (higher priority, then earlier due time).
#[derive(Debug, Default)]
pub struct FetchQueue {
    entries: HashMap<RepoId, Queued>,
    by_due: BinaryHeap<Reverse<(DateTime<Utc>, u64, RepoId)>>,
    ready: BinaryHeap<Reverse<ReadyKey>>,
    seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct Queued {
    priority: FetchPriority,
    due: DateTime<Utc>,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadyKey {
    priority: FetchPriority,
    due: DateTime<Utc>,
    seq: u64,
    repo: RepoId,
}

impl Ord for ReadyKey {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.priority, self.due, self.seq).cmp(&(o.priority, o.due, o.seq))
    }
}

impl PartialOrd for ReadyKey {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl FetchQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of queued repos.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Queues `repo`, or makes its entry more urgent.
    pub fn push(&mut self, repo: RepoId, priority: FetchPriority, due: DateTime<Utc>) {
        if let Some(q) = self.entries.get(&repo) {
            if (q.priority, q.due) <= (priority, due) {
                return;
            }
        }
        self.seq += 1;
        let q = Queued {
            priority,
            due,
            seq: self.seq,
        };
        self.entries.insert(repo, q);
        self.by_due.push(Reverse((due, q.seq, repo)));
    }

    /// Removes `repo` from the queue.
    pub fn remove(&mut self, repo: RepoId) -> bool {
        // Heap entries become stale and are skipped lazily.
        self.entries.remove(&repo).is_some()
    }

    /// The earliest due time, for sleeping until there is work.
    pub fn next_due(&mut self) -> Option<DateTime<Utc>> {
        if self
            .ready
            .iter()
            .any(|Reverse(k)| self.is_live(k.repo, k.seq))
        {
            return self.entries.values().map(|q| q.due).min();
        }
        while let Some(Reverse((due, seq, repo))) = self.by_due.peek().copied() {
            if self.is_live(repo, seq) {
                return Some(due);
            }
            self.by_due.pop();
        }
        None
    }

    /// Pops the most urgent repo that is due at `now`.
    pub fn pop_due(&mut self, now: DateTime<Utc>) -> Option<(RepoId, FetchPriority)> {
        while let Some(Reverse((due, seq, repo))) = self.by_due.peek().copied() {
            if due > now {
                break;
            }
            self.by_due.pop();
            if let Some(q) = self.entries.get(&repo).filter(|q| q.seq == seq) {
                self.ready.push(Reverse(ReadyKey {
                    priority: q.priority,
                    due,
                    seq,
                    repo,
                }));
            }
        }
        while let Some(Reverse(k)) = self.ready.pop() {
            if self.is_live(k.repo, k.seq) {
                self.entries.remove(&k.repo);
                return Some((k.repo, k.priority));
            }
        }
        None
    }

    fn is_live(&self, repo: RepoId, seq: u64) -> bool {
        self.entries.get(&repo).is_some_and(|q| q.seq == seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    fn no_jitter() -> SchedulePolicy {
        SchedulePolicy {
            jitter: 0.0,
            ..Default::default()
        }
    }

    #[test]
    fn never_fetched_is_now_and_disabled_is_never() {
        let p = SchedulePolicy::default();
        let h = FetchHistory::default();
        assert_eq!(next_fetch(RepoId(1), &h, &p, None, true), NextFetch::Now);
        assert_eq!(next_fetch(RepoId(1), &h, &p, None, false), NextFetch::Never);
    }

    #[test]
    fn interval_adapts_to_change_frequency() {
        let p = no_jitter();
        // Changes every 10 minutes on average → fetch every 20 minutes.
        let h = FetchHistory {
            last_fetched: Some(t(0)),
            last_attempt: Some(t(0)),
            last_changed: Some(t(-60)),
            mean_change_interval: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        assert_eq!(
            next_fetch(RepoId(1), &h, &p, None, true),
            NextFetch::At(t(1200))
        );
        // Busy repo is clamped to 45 s.
        let busy = FetchHistory {
            mean_change_interval: Some(Duration::from_secs(5)),
            ..h.clone()
        };
        assert_eq!(
            next_fetch(RepoId(1), &busy, &p, None, true),
            NextFetch::At(t(45))
        );
        // No mean: half the time since the last change (2 h → 1 h).
        let fallback = FetchHistory {
            mean_change_interval: None,
            last_changed: Some(t(-7200)),
            ..h.clone()
        };
        assert_eq!(
            next_fetch(RepoId(1), &fallback, &p, None, true),
            NextFetch::At(t(3600))
        );
        // Dormant repo is clamped to 8 h.
        let dormant = FetchHistory {
            mean_change_interval: None,
            last_changed: Some(t(-86400 * 365)),
            ..h.clone()
        };
        assert_eq!(
            next_fetch(RepoId(1), &dormant, &p, None, true),
            NextFetch::At(t(8 * 3600))
        );
        // Override wins.
        assert_eq!(
            next_fetch(RepoId(1), &h, &p, Some(Duration::from_secs(300)), true),
            NextFetch::At(t(300))
        );
    }

    #[test]
    fn failures_back_off_exponentially_and_cap() {
        let p = no_jitter();
        let h = |failures| FetchHistory {
            last_fetched: Some(t(-1000)),
            last_attempt: Some(t(0)),
            consecutive_failures: failures,
            ..Default::default()
        };
        assert_eq!(
            next_fetch(RepoId(1), &h(1), &p, None, true),
            NextFetch::At(t(60))
        );
        assert_eq!(
            next_fetch(RepoId(1), &h(3), &p, None, true),
            NextFetch::At(t(240))
        );
        assert_eq!(
            next_fetch(RepoId(1), &h(40), &p, None, true),
            NextFetch::At(t(8 * 3600))
        );
    }

    #[test]
    fn jitter_is_bounded_and_deterministic() {
        let p = SchedulePolicy::default();
        let h = FetchHistory {
            last_fetched: Some(t(0)),
            last_attempt: Some(t(0)),
            mean_change_interval: Some(Duration::from_secs(1000)),
            ..Default::default()
        };
        let mut distinct = std::collections::HashSet::new();
        for id in 0..200 {
            let NextFetch::At(at) = next_fetch(RepoId(id), &h, &p, None, true) else {
                panic!()
            };
            let secs = (at - t(0)).num_milliseconds() as f64 / 1000.0;
            assert!((1800.0..=2200.0).contains(&secs), "{secs}");
            assert_eq!(
                next_fetch(RepoId(id), &h, &p, None, true),
                NextFetch::At(at)
            );
            distinct.insert(at);
        }
        assert!(distinct.len() > 150);
    }

    #[test]
    fn mean_change_interval_is_a_moving_average() {
        assert_eq!(update_mean_change_interval(None, None, t(0)), None);
        assert_eq!(
            update_mean_change_interval(None, Some(t(-100)), t(0)),
            Some(Duration::from_secs(100))
        );
        assert_eq!(
            update_mean_change_interval(Some(Duration::from_secs(100)), Some(t(-200)), t(0)),
            Some(Duration::from_secs(130))
        );
    }

    #[test]
    fn interval_rules_first_match_wins() {
        let rules = UpdateIntervalRules::new([
            ("^gitlab.com/big/", Duration::from_secs(3600)),
            ("^gitlab.com/", Duration::from_secs(600)),
        ])
        .unwrap();
        assert_eq!(
            rules.interval_for("gitlab.com/big/mono"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            rules.interval_for("gitlab.com/x/y"),
            Some(Duration::from_secs(600))
        );
        assert_eq!(rules.interval_for("other/x"), None);
        assert!(UpdateIntervalRules::new([("(", Duration::ZERO)]).is_err());
    }

    #[test]
    fn queue_orders_by_priority_among_due_entries() {
        let mut q = FetchQueue::new();
        q.push(RepoId(1), FetchPriority::Scheduled, t(0));
        q.push(RepoId(2), FetchPriority::Signal, t(5));
        q.push(RepoId(3), FetchPriority::Manual, t(1));
        q.push(RepoId(4), FetchPriority::Scheduled, t(100));
        assert_eq!(q.len(), 4);
        assert_eq!(q.next_due(), Some(t(0)));
        // At t=10, three are due: signal, then manual, then scheduled.
        assert_eq!(q.pop_due(t(10)), Some((RepoId(2), FetchPriority::Signal)));
        assert_eq!(q.pop_due(t(10)), Some((RepoId(3), FetchPriority::Manual)));
        assert_eq!(
            q.pop_due(t(10)),
            Some((RepoId(1), FetchPriority::Scheduled))
        );
        assert_eq!(q.pop_due(t(10)), None);
        assert_eq!(q.next_due(), Some(t(100)));
        // A webhook for a scheduled repo upgrades it; a less urgent push is ignored.
        q.push(RepoId(4), FetchPriority::Signal, t(10));
        q.push(RepoId(4), FetchPriority::Scheduled, t(5));
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop_due(t(10)), Some((RepoId(4), FetchPriority::Signal)));
        assert!(q.is_empty());
        assert_eq!(q.pop_due(t(1000)), None);
        // Removal.
        q.push(RepoId(5), FetchPriority::Manual, t(0));
        assert!(q.remove(RepoId(5)));
        assert_eq!(q.next_due(), None);
        assert_eq!(q.pop_due(t(1000)), None);
    }
}
