//! Watch queries, compiled for evaluation, and the in-memory index from repositories to the
//! watches watching them.
//!
//! A watch's query must be a commit or diff search (`type:commit` / `type:diff`).
//! [`CompiledMonitor`] holds each part of the query (a commit search and
//! its repository filter, compiled with the owner's repository permissions and `context:`).
//! [`MonitorIndex`] maps every repository of the catalog to the watches whose query matches
//! it and the revisions they watch (the default branch unless the query names revisions), so
//! a ref update only costs the watches that care about that repository.
//!
//! The repository predicates evaluated on the repo nodes (`repo:has.file()`, `has.path()`,
//! `has.content()`, `has.commit.after()`, `rev:at.time()`) apply as in any search, since a
//! watch's commit search runs under the same repository resolution: the
//! trigger job checks `repo:has.*` on each range's new head through
//! [`sourcegull_search_client::repo_predicates`] and searches only when the repository passes.
//! `rev:at.time()` makes the searched revision a fixed commit, which never moves, so a part
//! with it watches nothing (that commit is recorded and no new ones are found).
//!
//! [`plan_evaluation`] is the pure core of an evaluation: given the resolved heads of the
//! watched revisions and the last commits searched, it says which commit ranges to search
//! and which heads to record. A pair seen for the first time is recorded without a search, so
//! a new watch (or a repository that newly matches) does not notify about old commits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sourcegull_query::PatternType;
use sourcegull_search::{
    CatalogRepo, ContextScope, DEFAULT_PAGE_SIZE, Job, Limits, PatternSpec, RepoCatalog,
    RepoMatcher, SelectPath, plan_query,
};
use sourcegull_search_client::ContentSearch;
use sourcegull_search_client::repo_predicates::RepoPredicates;
use sourcegull_types::{RepoAccess, RepoId};

use crate::store::{CommitRange, HeadUpdate, LastSearched};

/// Results kept per trigger job.
pub const MAX_RESULTS: usize = 500;

/// The error for queries that are not commit or diff searches.
pub const TYPE_REQUIRED: &str =
    "Watches require queries to specify either `type:commit` or `type:diff`.";

/// Repositories one part of a query searches at once.
const REPO_CONCURRENCY: usize = 8;

/// One part of a watch's query: a commit or diff search over the repositories its filter
/// matches.
#[derive(Debug)]
pub struct CommitUnit {
    pub search: ContentSearch,
    pub matcher: RepoMatcher,
    /// The filter's predicates evaluated on the repo nodes, without `rev:at.time()`
    /// (see [`CommitUnit::fixed_revision`]).
    pub predicates: RepoPredicates,
    /// The filter has `rev:at.time()`: its revision is a fixed commit, so it watches
    /// nothing.
    pub fixed_revision: bool,
}

/// A commit or diff search of a query, before repository resolution.
struct PlannedUnit {
    search: ContentSearch,
    repos: sourcegull_search::RepoFilter,
}

fn collect_units(
    job: &Job,
    diff_lines: Option<char>,
    out: &mut Vec<PlannedUnit>,
) -> Result<(), String> {
    match job {
        Job::Timeout { child, .. } | Job::Limit { child, .. } => {
            collect_units(child, diff_lines, out)
        }
        Job::Parallel(jobs) => jobs
            .iter()
            .try_for_each(|j| collect_units(j, diff_lines, out)),
        Job::Select { path, child } => match SelectPath::parse(path) {
            Some(SelectPath::CommitDiff(sign)) => collect_units(child, Some(sign), out),
            Some(SelectPath::Commit) => collect_units(child, diff_lines, out),
            _ => Err(format!("`select:{path}` is not supported in watches.")),
        },
        Job::Commit {
            pattern,
            repos,
            files,
            filter,
            diff,
        } => {
            let spec = pattern.as_ref().map(PatternSpec::from_pattern).transpose();
            out.push(PlannedUnit {
                search: ContentSearch {
                    pattern: spec.map_err(|e| e.to_string())?,
                    path_pattern: None,
                    case_sensitive: pattern.as_ref().is_some_and(|p| p.case_sensitive),
                    paths: files.clone(),
                    limit: MAX_RESULTS,
                    max_line_len: 0,
                    concurrency: REPO_CONCURRENCY,
                    commits: Some(filter.into()),
                    diff: *diff,
                    index: sourcegull_search::YesNoOnly::No,
                    symbols: false,
                    bm25: false,
                    select: None,
                    diff_lines: diff_lines.filter(|_| *diff),
                    owners: None,
                    deadline: None,
                },
                repos: repos.clone(),
            });
            Ok(())
        }
        Job::Content { .. } | Job::Path { .. } | Job::Symbol { .. } | Job::Repo { .. } => {
            Err(TYPE_REQUIRED.to_owned())
        }
    }
}

fn plan(query: &str, now: DateTime<Utc>) -> Result<Vec<PlannedUnit>, String> {
    let job = plan_query(query, PatternType::default(), &Limits::default(), now)
        .map_err(|e| e.to_string())?;
    let mut units = Vec::new();
    collect_units(&job, None, &mut units)?;
    if units.is_empty() {
        return Err(TYPE_REQUIRED.to_owned());
    }
    // Invalid predicate arguments fail as they do in a search.
    for u in &units {
        RepoPredicates::compile(&u.repos.predicates, now)?;
    }
    Ok(units)
}

/// Checks a watch query on create and update: it must plan, and every part must be a
/// commit or diff search.
pub fn validate_query(query: &str) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("The query is empty.".to_owned());
    }
    plan(query, Utc::now()).map(|_| ())
}

/// A watch's query, compiled with its owner's permissions.
#[derive(Debug)]
pub struct CompiledMonitor {
    pub id: i64,
    pub units: Vec<CommitUnit>,
}

/// The revisions to watch for a filter's revisions: the default branch (`""`) when none.
fn watched(revs: Vec<String>) -> Vec<String> {
    // `^rev` exclusions only bound searches; they are not watched.
    let revs: Vec<String> = revs.into_iter().filter(|r| !r.starts_with('^')).collect();
    if revs.is_empty() {
        vec![String::new()]
    } else {
        revs
    }
}

impl CompiledMonitor {
    /// Compiles `query` (with any query-defined `context:` already substituted) within
    /// `scope` for an owner who reads `access`.
    pub fn compile(
        id: i64,
        query: &str,
        scope: &ContextScope,
        access: RepoAccess,
        now: DateTime<Utc>,
    ) -> Result<CompiledMonitor, String> {
        let units = plan(query, now)?
            .into_iter()
            .map(|u| {
                let matcher =
                    RepoMatcher::new(&u.repos, scope, access.clone()).map_err(|e| e.to_string())?;
                let predicates = RepoPredicates::compile(matcher.deferred(), now)?;
                Ok(CommitUnit {
                    search: u.search,
                    matcher,
                    fixed_revision: !predicates.at_time.is_empty(),
                    predicates: predicates.without_at_time(),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(CompiledMonitor { id, units })
    }

    /// The revisions of `repo` that unit `unit` watches, or `None` when it does not match
    /// the repository.
    pub fn unit_revs(&self, unit: usize, repo: &CatalogRepo) -> Option<Vec<String>> {
        let u = self.units.get(unit)?;
        if u.fixed_revision || !u.matcher.matches(repo) {
            return None;
        }
        let one = RepoCatalog::new(vec![repo.clone()]);
        let page = u.matcher.page(&one, 0, 1);
        page.repos.into_iter().next().map(|r| watched(r.revs))
    }
}

/// A watch watching a repository, and the revisions it watches there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub monitor: i64,
    pub revs: Vec<String>,
}

/// Repository → the watches whose query matches it.
#[derive(Debug, Default)]
pub struct MonitorIndex {
    by_repo: HashMap<i32, Vec<Target>>,
    names: HashMap<i32, String>,
    monitors: HashMap<i64, Arc<CompiledMonitor>>,
}

impl MonitorIndex {
    /// Indexes `monitors` over `catalog`.
    pub fn build(monitors: Vec<CompiledMonitor>, catalog: &RepoCatalog) -> MonitorIndex {
        let mut by_repo: HashMap<i32, BTreeMap<i64, BTreeSet<String>>> = HashMap::new();
        let mut names = HashMap::new();
        for m in &monitors {
            for u in m.units.iter().filter(|u| !u.fixed_revision) {
                for page in u.matcher.pages(catalog, DEFAULT_PAGE_SIZE) {
                    for r in page {
                        names.entry(r.id).or_insert_with(|| r.name.clone());
                        by_repo
                            .entry(r.id)
                            .or_default()
                            .entry(m.id)
                            .or_default()
                            .extend(watched(r.revs));
                    }
                }
            }
        }
        MonitorIndex {
            by_repo: by_repo
                .into_iter()
                .map(|(repo, ms)| {
                    let targets = ms
                        .into_iter()
                        .map(|(monitor, revs)| Target {
                            monitor,
                            revs: revs.into_iter().collect(),
                        })
                        .collect();
                    (repo, targets)
                })
                .collect(),
            names,
            monitors: monitors.into_iter().map(|m| (m.id, Arc::new(m))).collect(),
        }
    }

    /// The watches watching `repo`, by watch id.
    pub fn targets(&self, repo: i32) -> &[Target] {
        self.by_repo.get(&repo).map_or(&[], Vec::as_slice)
    }

    /// The watched repositories.
    pub fn repos(&self) -> impl Iterator<Item = i32> + '_ {
        self.by_repo.keys().copied()
    }

    /// A watched repository's name.
    pub fn repo_name(&self, repo: i32) -> Option<&str> {
        self.names.get(&repo).map(String::as_str)
    }

    /// The indexed watches' ids.
    pub fn monitor_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.monitors.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    pub fn monitor(&self, id: i64) -> Option<&Arc<CompiledMonitor>> {
        self.monitors.get(&id)
    }

    /// Number of (repository, watch) pairs.
    pub fn pairs(&self) -> usize {
        self.by_repo.values().map(Vec::len).sum()
    }
}

/// The resolved head of a watched revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub repo_id: i32,
    pub repo: String,
    pub monitor: i64,
    pub rev: String,
    pub oid: String,
}

/// What an evaluation does: the heads to record, the pairs found unchanged, and the ranges
/// to search per watch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    pub heads: Vec<HeadUpdate>,
    pub unchanged: Vec<(i64, RepoId)>,
    pub ranges: BTreeMap<i64, Vec<CommitRange>>,
}

/// Plans an evaluation of `heads` against the `last` commits searched. Watches in
/// `search_all` search each head's whole history (`resetTriggerQueryTimestamps`); otherwise a
/// pair without a last commit only records its head.
pub fn plan_evaluation(
    heads: &[Head],
    last: &[LastSearched],
    search_all: &HashSet<i64>,
) -> Evaluation {
    let last: HashMap<(i64, i32, &str), &str> = last
        .iter()
        .map(|l| {
            (
                (l.monitor_id, l.repo_id.get(), l.rev.as_str()),
                l.commit_oid.as_str(),
            )
        })
        .collect();
    let mut out = Evaluation::default();
    let mut unchanged = BTreeSet::new();
    for h in heads {
        let prev = last.get(&(h.monitor, h.repo_id, h.rev.as_str())).copied();
        let all = search_all.contains(&h.monitor);
        let from = match (prev, all) {
            (_, true) => Some(None),
            (Some(p), false) if p == h.oid => None,
            (Some(p), false) => Some(Some(p.to_owned())),
            // First sight: record only.
            (None, false) => {
                out.heads.push(HeadUpdate {
                    monitor_id: h.monitor,
                    repo_id: RepoId(h.repo_id),
                    rev: h.rev.clone(),
                    commit_oid: h.oid.clone(),
                });
                continue;
            }
        };
        match from {
            None => {
                unchanged.insert((h.monitor, RepoId(h.repo_id)));
            }
            Some(from) => {
                out.heads.push(HeadUpdate {
                    monitor_id: h.monitor,
                    repo_id: RepoId(h.repo_id),
                    rev: h.rev.clone(),
                    commit_oid: h.oid.clone(),
                });
                out.ranges.entry(h.monitor).or_default().push(CommitRange {
                    repo_id: h.repo_id,
                    repo: h.repo.clone(),
                    rev: h.rev.clone(),
                    from,
                    to: h.oid.clone(),
                });
            }
        }
    }
    out.unchanged = unchanged.into_iter().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(id: i32, name: &str) -> CatalogRepo {
        CatalogRepo {
            id,
            name: name.into(),
            ..CatalogRepo::default()
        }
    }

    fn catalog() -> RepoCatalog {
        let mut private = repo(4, "gitlab.example.com/acme/secret");
        private.private = true;
        RepoCatalog::new(vec![
            repo(1, "gitlab.example.com/acme/web"),
            repo(2, "gitlab.example.com/acme/api"),
            repo(3, "gitlab.example.com/other/tool"),
            private,
        ])
    }

    fn compile(id: i64, query: &str, access: RepoAccess) -> CompiledMonitor {
        CompiledMonitor::compile(id, query, &ContextScope::Global, access, Utc::now()).unwrap()
    }

    #[test]
    fn queries_must_be_commit_or_diff_searches() {
        assert!(validate_query("type:diff TODO").is_ok());
        assert!(validate_query("type:commit repo:acme fix").is_ok());
        assert!(validate_query("type:diff select:commit.diff.added TODO").is_ok());
        assert_eq!(validate_query("TODO").unwrap_err(), TYPE_REQUIRED);
        assert_eq!(validate_query("type:file TODO").unwrap_err(), TYPE_REQUIRED);
        assert!(validate_query("(type:diff a) or (type:file b)").is_err());
        assert!(validate_query("  ").is_err());
    }

    #[test]
    fn index_maps_repos_to_monitors_and_revisions() {
        let monitors = vec![
            compile(10, "type:diff repo:acme/ TODO", RepoAccess::All),
            compile(
                11,
                "type:commit repo:acme/web@main:dev fix",
                RepoAccess::All,
            ),
            compile(12, "type:commit fix", RepoAccess::All),
            // An owner who reads only repository 3: the private repository stays unwatched.
            compile(
                13,
                "type:commit fix",
                RepoAccess::Restricted {
                    user_id: None,
                    user_mapping: false,
                    unrestricted: Arc::new([3u32].into_iter().collect::<roaring::RoaringBitmap>()),
                    granted: Arc::default(),
                },
            ),
        ];
        let index = MonitorIndex::build(monitors, &catalog());
        let t = |r| -> Vec<(i64, Vec<&str>)> {
            index
                .targets(r)
                .iter()
                .map(|t| (t.monitor, t.revs.iter().map(String::as_str).collect()))
                .collect()
        };
        assert_eq!(
            t(1),
            vec![(10, vec![""]), (11, vec!["dev", "main"]), (12, vec![""])]
        );
        assert_eq!(t(2), vec![(10, vec![""]), (12, vec![""])]);
        assert_eq!(t(3), vec![(12, vec![""]), (13, vec![""])]);
        assert_eq!(t(4), vec![(10, vec![""]), (12, vec![""])]);
        assert!(index.targets(99).is_empty());
        assert_eq!(index.repo_name(3), Some("gitlab.example.com/other/tool"));
        assert_eq!(index.pairs(), 9);
        assert_eq!(index.monitor_ids(), vec![10, 11, 12, 13]);

        let m = index.monitor(11).unwrap();
        let web = catalog().get(1).unwrap().clone();
        assert_eq!(
            m.unit_revs(0, &web),
            Some(vec!["main".into(), "dev".into()])
        );
        assert_eq!(m.unit_revs(0, catalog().get(2).unwrap()), None);
    }

    fn head(monitor: i64, repo_id: i32, rev: &str, oid: &str) -> Head {
        Head {
            repo_id,
            repo: format!("r{repo_id}"),
            monitor,
            rev: rev.into(),
            oid: oid.into(),
        }
    }

    fn last(monitor: i64, repo_id: i32, rev: &str, oid: &str) -> LastSearched {
        LastSearched {
            monitor_id: monitor,
            repo_id: RepoId(repo_id),
            rev: rev.into(),
            commit_oid: oid.into(),
            checked_at: Utc::now(),
        }
    }

    #[test]
    fn evaluation_searches_moved_heads_and_records_new_ones() {
        let heads = vec![
            head(1, 7, "", "b"),     // moved from a
            head(1, 8, "", "x"),     // unchanged
            head(1, 9, "", "n"),     // never seen: record only
            head(2, 7, "dev", "d2"), // moved
        ];
        let lasts = vec![
            last(1, 7, "", "a"),
            last(1, 8, "", "x"),
            last(2, 7, "dev", "d1"),
        ];
        let e = plan_evaluation(&heads, &lasts, &HashSet::new());
        let range = |repo_id, rev: &str, from: Option<&str>, to: &str| CommitRange {
            repo_id,
            repo: format!("r{repo_id}"),
            rev: rev.into(),
            from: from.map(str::to_owned),
            to: to.into(),
        };
        assert_eq!(e.ranges[&1], vec![range(7, "", Some("a"), "b")]);
        assert_eq!(e.ranges[&2], vec![range(7, "dev", Some("d1"), "d2")]);
        let recorded: Vec<(i64, i32, &str)> = e
            .heads
            .iter()
            .map(|h| (h.monitor_id, h.repo_id.get(), h.commit_oid.as_str()))
            .collect();
        assert_eq!(recorded, vec![(1, 7, "b"), (1, 9, "n"), (2, 7, "d2")]);
        assert_eq!(e.unchanged, vec![(1, RepoId(8))]);

        // A reset watch searches whole histories, seen or not.
        let e = plan_evaluation(&heads[..3], &lasts, &[1].into_iter().collect());
        assert_eq!(
            e.ranges[&1],
            vec![
                range(7, "", None, "b"),
                range(8, "", None, "x"),
                range(9, "", None, "n")
            ]
        );
        assert!(e.unchanged.is_empty());
    }
}
