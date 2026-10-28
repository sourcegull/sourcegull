//! Repository predicates evaluated on the repo nodes: `rev:at.time()`,
//! `repo:has.commit.after()` and `repo:has.file()` with its short forms `repo:has.path()`
//! and `repo:has.content()`.
//!
//! [`RepoMatcher`](sourcegull_search::RepoMatcher) resolves names, metadata and permissions
//! from the catalog and leaves these in its `deferred()` list. [`filter_repos`] then runs
//! them over a page of resolved repositories, in this order:
//!
//! 1. `rev:at.time(time[, rev])` adds, per repository, the newest first-parent ancestor of
//!    `rev` (default `HEAD`) committed at or before `time` to the revisions searched, as
//!    `git log --first-parent --before=time -n1 rev`. A repository with no revision left
//!    is dropped.
//! 2. `repo:has.commit.after(date)` keeps the revisions with a commit after `date`
//!    (negated: without one).
//! 3. `repo:has.file(path:… content:…)` keeps the revisions with a file whose path and
//!    contents match the (case-insensitive) regexps; negated, without one. Indexed
//!    branches are asked in one `SearchIndexed` call per node and predicate with
//!    `repos_only` (the index stops at the first match per branch); the rest are searched
//!    unindexed, one file per revision.
//!
//! A repository keeps the revisions that pass every predicate and is dropped when none do.
//! Only the repositories handed in are searched, so permissions apply as resolved: an
//! unreadable repository never reaches the nodes. Repositories that could not be checked
//! (not cloned, unknown revision, node failure) come back as skipped.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use sourcegull_query::Predicate;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::tonic::Code;
use sourcegull_rpc::{ErrorKind, RepoClients};
use sourcegull_search::stream::{Skipped, SkippedReason};
use sourcegull_search::{FileFilter, PatternSpec, RepoRevs, YesNoOnly, parse_date};
use sourcegull_types::RepoId;

use crate::fanout::{index_branch, indexed_batches, skip_reason, skipped_entry, with_deadline};
use crate::wire::{path_filter_to_pb, spec_to_pb};

/// Repositories checked at once.
const CONCURRENCY: usize = 32;

/// `repo:has.file(…)`, `repo:has.path(…)` or `repo:has.content(…)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HasFile {
    pub negated: bool,
    /// Regexp on the path.
    pub path: Option<String>,
    /// Regexp on the contents.
    pub content: Option<String>,
}

/// The deferred predicates of one repository filter, compiled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoPredicates {
    /// `rev:at.time()`: (seconds since the epoch, revision; empty for `HEAD`).
    pub at_time: Vec<(i64, String)>,
    /// `repo:has.commit.after()`: (negated, seconds since the epoch).
    pub commit_after: Vec<(bool, i64)>,
    pub has_file: Vec<HasFile>,
}

impl RepoPredicates {
    /// Compiles `deferred` (from `RepoMatcher::deferred`), resolving dates against `now`.
    /// Errors are messages for an "Unable To Process Query" alert.
    pub fn compile(
        deferred: &[(bool, Predicate)],
        now: DateTime<Utc>,
    ) -> Result<RepoPredicates, String> {
        let mut out = RepoPredicates::default();
        let regex = |name: &str, re: &str| {
            regex::RegexBuilder::new(re)
                .case_insensitive(true)
                .build()
                .map(|_| re.to_owned())
                .map_err(|e| format!("invalid regular expression in {name}: {e}"))
        };
        for (negated, p) in deferred {
            let negated = *negated;
            match p {
                Predicate::RepoHasFile { path, content } => out.has_file.push(HasFile {
                    negated,
                    path: path
                        .as_deref()
                        .map(|p| regex("repo:has.file()", p))
                        .transpose()?,
                    content: content
                        .as_deref()
                        .map(|c| regex("repo:has.file()", c))
                        .transpose()?,
                }),
                Predicate::RepoHasPath(path) => out.has_file.push(HasFile {
                    negated,
                    path: Some(regex("repo:has.path()", path)?),
                    content: None,
                }),
                Predicate::RepoHasContent(content) => out.has_file.push(HasFile {
                    negated,
                    path: None,
                    content: Some(regex("repo:has.content()", content)?),
                }),
                Predicate::RepoHasCommitAfter(expr) => {
                    let t = parse_date(expr, now)
                        .map_err(|e| format!("repo:has.commit.after(): {e}"))?;
                    out.commit_after.push((negated, t.timestamp()));
                }
                Predicate::RevAtTime { time, rev } => {
                    if negated {
                        return Err(
                            "search predicate \"at.time\" does not support negation".to_owned()
                        );
                    }
                    let t = parse_date(time, now).map_err(|e| format!("rev:at.time(): {e}"))?;
                    let rev = rev.clone().filter(|r| r != "HEAD").unwrap_or_default();
                    out.at_time.push((t.timestamp(), rev));
                }
                // Metadata predicates are the matcher's; file predicates never get here.
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.at_time.is_empty() && self.commit_after.is_empty() && self.has_file.is_empty()
    }

    /// These predicates without `rev:at.time()`, for a search whose revision is fixed
    /// already (it was resolved through them).
    pub fn without_at_time(&self) -> RepoPredicates {
        RepoPredicates {
            at_time: Vec::new(),
            ..self.clone()
        }
    }
}

/// What [`filter_repos`] kept.
#[derive(Debug, Clone, Default)]
pub struct Filtered {
    /// The repositories that pass, in input order, with the revisions that pass.
    pub repos: Vec<RepoRevs>,
    /// Repositories that could not be checked, one entry per reason.
    pub skipped: Vec<Skipped>,
}

/// Keeps the repositories and revisions of `repos` that pass `preds` (see the module
/// documentation). `index` is the search's `index:` (`no` checks `repo:has.file()`
/// unindexed only; `only` drops revisions the index lacks). The `repo:has.file()` searches
/// carry `deadline`, so the nodes stop them then; repositories not checked by then are
/// skipped as timed out.
pub async fn filter_repos(
    clients: Arc<RepoClients>,
    repos: Vec<RepoRevs>,
    preds: &RepoPredicates,
    index: YesNoOnly,
    deadline: Option<Instant>,
) -> Filtered {
    if preds.is_empty() || repos.is_empty() {
        return Filtered {
            repos,
            skipped: Vec::new(),
        };
    }
    let mut skipped: Vec<(String, SkippedReason)> = Vec::new();
    let mut current = repos;
    // The futures below own what they use, so that callers' futures stay `Send`.
    let preds = Arc::new(preds.clone());
    if !preds.at_time.is_empty() || !preds.commit_after.is_empty() {
        let results: Vec<_> = futures::stream::iter(current)
            .map(|repo| {
                let clients = clients.clone();
                let preds = preds.clone();
                async move {
                    let name = repo.name.clone();
                    (name, resolve_revs(&clients, repo, &preds).await)
                }
            })
            .buffered(CONCURRENCY)
            .collect()
            .await;
        current = Vec::new();
        for (name, r) in results {
            match r {
                Ok(Some(repo)) => current.push(repo),
                Ok(None) => {}
                Err(reason) => skipped.push((name, reason)),
            }
        }
    }
    if !preds.has_file.is_empty() && !current.is_empty() {
        let has: Arc<[HasFile]> = preds.has_file.clone().into();
        let (kept, mut failed) = has_file(&clients, current, has, index, deadline).await;
        current = kept;
        skipped.append(&mut failed);
    }
    let mut by_reason: Vec<(SkippedReason, Vec<String>)> = Vec::new();
    for (name, reason) in skipped {
        match by_reason.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, names)) => names.push(name),
            None => by_reason.push((reason, vec![name])),
        }
    }
    Filtered {
        repos: current,
        skipped: by_reason
            .into_iter()
            .map(|(reason, names)| skipped_entry(reason, &names))
            .collect(),
    }
}

/// The revisions to check: the requested ones ("" for the default branch), with
/// `^excluded` ones (commit search only) set apart.
fn split_revs(revs: &[String]) -> (Vec<String>, Vec<String>) {
    let (excluded, mut revs): (Vec<String>, Vec<String>) =
        revs.iter().cloned().partition(|r| r.starts_with('^'));
    if revs.is_empty() {
        revs.push(String::new());
    }
    (excluded, revs)
}

/// Back to [`RepoRevs::revs`] form: only the default branch is no revisions.
fn join_revs(mut revs: Vec<String>, excluded: Vec<String>) -> Vec<String> {
    if revs.len() == 1 && revs[0].is_empty() {
        revs.clear();
    }
    revs.extend(excluded);
    revs
}

fn repo_ref(repo: &RepoRevs) -> pb::RepoRef {
    pb::RepoRef {
        id: repo.id,
        name: repo.name.clone(),
    }
}

/// The commit `rev` names in `repo`; `Ok(None)` when it names none.
async fn resolve(
    clients: &RepoClients,
    repo: &RepoRevs,
    rev: &str,
) -> Result<Option<String>, SkippedReason> {
    let mut git = clients
        .for_repo(RepoId(repo.id))
        .map_err(|_| SkippedReason::Error)?;
    match git
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: Some(repo_ref(repo)),
            rev: rev.to_owned(),
        })
        .await
    {
        Ok(r) => Ok(Some(r.into_inner().commit_oid)),
        Err(s) if ErrorKind::of(&s) == Some(ErrorKind::RevisionNotFound) => Ok(None),
        Err(s) => Err(skip_reason(&s)),
    }
}

/// Steps 1 and 2: `rev:at.time()` and `repo:has.commit.after()` for one repository.
/// `Ok(None)` when no revision is left.
async fn resolve_revs(
    clients: &RepoClients,
    repo: RepoRevs,
    preds: &RepoPredicates,
) -> Result<Option<RepoRevs>, SkippedReason> {
    let (excluded, mut revs) = if preds.at_time.is_empty() {
        split_revs(&repo.revs)
    } else {
        // `rev:at.time()` is a revision of its own: the requested ones stay, and without
        // any the default branch is not searched besides it.
        let (excluded, revs): (Vec<String>, Vec<String>) =
            repo.revs.iter().cloned().partition(|r| r.starts_with('^'));
        let mut revs = revs;
        for (time, rev) in &preds.at_time {
            let Some(head) = resolve(clients, &repo, rev).await? else {
                // An unknown revision is missing, an empty repository
                // has no commit at any time.
                if rev.is_empty() {
                    continue;
                }
                return Err(SkippedReason::RepositoryMissing);
            };
            if let Some(oid) = commit_at(clients, &repo, &head, *time).await?
                && !revs.contains(&oid)
            {
                revs.push(oid);
            }
        }
        (excluded, revs)
    };
    if !preds.commit_after.is_empty() {
        let mut kept = Vec::with_capacity(revs.len());
        for rev in revs {
            // An unknown revision or an empty repository has no commits after anything.
            let oid = resolve(clients, &repo, &rev).await?;
            let mut pass = true;
            for (negated, after) in &preds.commit_after {
                let has = match &oid {
                    Some(oid) => has_commit_after(clients, &repo, oid, *after).await?,
                    None => false,
                };
                if has == *negated {
                    pass = false;
                    break;
                }
            }
            if pass {
                kept.push(rev);
            }
        }
        revs = kept;
    }
    if revs.is_empty() {
        return Ok(None);
    }
    Ok(Some(RepoRevs {
        revs: join_revs(revs, excluded),
        ..repo
    }))
}

/// The newest first-parent ancestor of `head` (inclusive) committed at or before `time`.
async fn commit_at(
    clients: &RepoClients,
    repo: &RepoRevs,
    head: &str,
    time: i64,
) -> Result<Option<String>, SkippedReason> {
    let mut git = clients
        .for_repo(RepoId(repo.id))
        .map_err(|_| SkippedReason::Error)?;
    let mut stream = git
        .commit_log(pb::CommitLogRequest {
            repo: Some(repo_ref(repo)),
            commit_oid: head.to_owned(),
            limit: 1,
            first_parent: true,
            before: Some(time),
            ..Default::default()
        })
        .await
        .map_err(|s| skip_reason(&s))?
        .into_inner();
    while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
        if let Some(c) = msg.commits.into_iter().next() {
            return Ok(Some(c.oid));
        }
    }
    Ok(None)
}

/// Whether `oid` or an ancestor was committed at or after `after`, as
/// `git log -n1 --after=<after> <oid>`: the log is newest first by
/// committer time and stops at the first older commit, so a stale repository costs one
/// step, not a walk of its history.
async fn has_commit_after(
    clients: &RepoClients,
    repo: &RepoRevs,
    oid: &str,
    after: i64,
) -> Result<bool, SkippedReason> {
    let mut git = clients
        .for_repo(RepoId(repo.id))
        .map_err(|_| SkippedReason::Error)?;
    let mut stream = git
        .commit_log(pb::CommitLogRequest {
            repo: Some(repo_ref(repo)),
            commit_oid: oid.to_owned(),
            limit: 1,
            after: Some(after),
            ..Default::default()
        })
        .await
        .map_err(|s| skip_reason(&s))?
        .into_inner();
    while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
        if !msg.commits.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The pattern and path filter of a `repo:has.file()` sub-search.
fn sub_search(p: &HasFile) -> (Option<pb::PatternNode>, pb::PathFilter) {
    let pattern = p.content.as_ref().map(|c| {
        spec_to_pb(&PatternSpec::Atom {
            regex: c.clone(),
            negated: false,
        })
    });
    let paths = FileFilter {
        include: p.path.iter().cloned().collect(),
        ..FileFilter::default()
    };
    (pattern, path_filter_to_pb(&paths))
}

/// (repository id, revision as requested).
type RepoRev = (i32, String);

/// Step 3: `repo:has.file()` predicates. Returns the repositories and revisions that pass
/// and the repositories that failed.
async fn has_file(
    clients: &Arc<RepoClients>,
    repos: Vec<RepoRevs>,
    preds: Arc<[HasFile]>,
    index: YesNoOnly,
    deadline: Option<Instant>,
) -> (Vec<RepoRevs>, Vec<(String, SkippedReason)>) {
    let mut failed: HashMap<i32, SkippedReason> = HashMap::new();
    let split: Vec<(RepoRevs, Vec<String>, Vec<String>)> = repos
        .into_iter()
        .map(|r| {
            let (excluded, revs) = split_revs(&r.revs);
            (r, excluded, revs)
        })
        .collect();
    // hits[i]: the revisions with a file matching predicate i.
    let mut hits: Vec<HashSet<RepoRev>> = vec![HashSet::new(); preds.len()];
    // The (revision, predicate) pairs the index could not answer.
    let mut unindexed: Vec<(RepoRev, usize)> = Vec::new();
    // Revisions dropped because `index:only` cannot search them.
    let mut dropped: HashSet<RepoRev> = HashSet::new();
    let all_pairs = |i: usize| {
        split
            .iter()
            .flat_map(move |(r, _, revs)| revs.iter().map(move |v| ((r.id, v.clone()), i)))
    };
    if index == YesNoOnly::No {
        for i in 0..preds.len() {
            unindexed.extend(all_pairs(i));
        }
    } else {
        let requested: Vec<RepoRevs> = split
            .iter()
            .map(|(r, _, revs)| RepoRevs {
                revs: revs.clone(),
                ..r.clone()
            })
            .collect();
        let (batches, unrouted) = indexed_batches(clients, requested);
        let names: HashMap<&str, i32> = split
            .iter()
            .map(|(r, ..)| (r.name.as_str(), r.id))
            .collect();
        for name in unrouted {
            if let Some(&id) = names.get(name.as_str()) {
                failed.insert(id, SkippedReason::Error);
            }
        }
        let calls: Vec<(String, Arc<Vec<RepoRevs>>, usize)> = batches
            .into_iter()
            .flat_map(|(node, batch)| {
                let batch = Arc::new(batch);
                (0..preds.len()).map(move |i| (node.clone(), batch.clone(), i))
            })
            .collect();
        let results: Vec<_> = futures::stream::iter(calls)
            .map(|(node, batch, i)| {
                let clients = clients.clone();
                let preds = preds.clone();
                async move {
                    let r = indexed_hits(&clients, &node, &batch, &preds[i], deadline).await;
                    (batch, i, r)
                }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        for (batch, i, r) in results {
            match r {
                Ok(Indexed::Answered { found, missing }) => {
                    hits[i].extend(found);
                    for pair in missing {
                        if index == YesNoOnly::Only {
                            dropped.insert(pair);
                        } else {
                            unindexed.push((pair, i));
                        }
                    }
                }
                Ok(Indexed::NoIndex) => {
                    for r in batch.iter() {
                        for v in &r.revs {
                            if index == YesNoOnly::Only {
                                dropped.insert((r.id, v.clone()));
                            } else {
                                unindexed.push(((r.id, v.clone()), i));
                            }
                        }
                    }
                }
                Err(reason) => {
                    for r in batch.iter() {
                        failed.insert(r.id, reason);
                    }
                }
            }
        }
    }
    // Unindexed: resolve each revision once, then one search per predicate it needs.
    let mut needs: HashMap<RepoRev, Vec<usize>> = HashMap::new();
    for (pair, i) in unindexed {
        if !failed.contains_key(&pair.0) {
            needs.entry(pair).or_default().push(i);
        }
    }
    let by_id: HashMap<i32, &RepoRevs> = split.iter().map(|(r, ..)| (r.id, r)).collect();
    let checks: Vec<_> = needs
        .into_iter()
        .map(|(pair, wanted)| (by_id[&pair.0].clone(), pair, wanted))
        .collect();
    let results: Vec<_> = futures::stream::iter(checks)
        .map(|(repo, (id, rev), wanted)| {
            let clients = clients.clone();
            let preds = preds.clone();
            async move {
                let r = unindexed_hits(&clients, &repo, &rev, &preds, &wanted, deadline).await;
                ((id, rev), r)
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for (pair, r) in results {
        match r {
            Ok(found) => {
                for i in found {
                    hits[i].insert(pair.clone());
                }
            }
            Err(reason) => {
                failed.entry(pair.0).or_insert(reason);
            }
        }
    }
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for (repo, excluded, revs) in split {
        if let Some(&reason) = failed.get(&repo.id) {
            skipped.push((repo.name, reason));
            continue;
        }
        let revs: Vec<String> = revs
            .into_iter()
            .filter(|v| {
                let pair = (repo.id, v.clone());
                !dropped.contains(&pair)
                    && preds
                        .iter()
                        .zip(&hits)
                        .all(|(p, h)| h.contains(&pair) != p.negated)
            })
            .collect();
        if !revs.is_empty() {
            kept.push(RepoRevs {
                revs: join_revs(revs, excluded),
                ..repo
            });
        }
    }
    (kept, skipped)
}

enum Indexed {
    /// The revisions with a matching file, and those the index does not hold.
    Answered {
        found: Vec<RepoRev>,
        missing: Vec<RepoRev>,
    },
    /// The node has no index.
    NoIndex,
}

/// Asks `node`'s index which revisions of `batch` have a file matching `p`.
async fn indexed_hits(
    clients: &RepoClients,
    node: &str,
    batch: &[RepoRevs],
    p: &HasFile,
    deadline: Option<Instant>,
) -> Result<Indexed, SkippedReason> {
    let (pattern, paths) = sub_search(p);
    let req = pb::SearchIndexedRequest {
        repos: batch
            .iter()
            .map(|r| pb::IndexedRepoBranches {
                repo: Some(repo_ref(r)),
                branches: r.revs.iter().map(|v| index_branch(v).to_owned()).collect(),
            })
            .collect(),
        pattern,
        case_sensitive: false,
        paths: Some(paths),
        repos_only: true,
        ..Default::default()
    };
    let by_id: HashMap<i32, &RepoRevs> = batch.iter().map(|r| (r.id, r)).collect();
    // Index branch names back to the revisions as requested.
    let revs_of = |id: i32, branch: &str| -> Vec<RepoRev> {
        by_id
            .get(&id)
            .map(|r| {
                r.revs
                    .iter()
                    .filter(|v| index_branch(v) == branch)
                    .map(|v| (id, v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut client = clients
        .search_for_node(node)
        .map_err(|_| SkippedReason::Error)?;
    let mut stream = match client.search_indexed(with_deadline(req, deadline)).await {
        Ok(r) => r.into_inner(),
        Err(s) if s.code() == Code::Unimplemented => return Ok(Indexed::NoIndex),
        Err(s) => return Err(skip_reason(&s)),
    };
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
        if msg.stats.is_some_and(|st| st.canceled) {
            return Err(SkippedReason::ShardTimeout);
        }
        for m in msg.missing {
            let Some(id) = m.repo.map(|r| r.id) else {
                continue;
            };
            for b in &m.branches {
                missing.extend(revs_of(id, b));
            }
        }
        for f in msg.files {
            for b in &f.branches {
                found.extend(revs_of(f.repo_id, &b.name));
            }
        }
    }
    Ok(Indexed::Answered { found, missing })
}

/// Which of the predicates `wanted` have a matching file at `rev` of `repo`, searched
/// unindexed.
async fn unindexed_hits(
    clients: &RepoClients,
    repo: &RepoRevs,
    rev: &str,
    preds: &[HasFile],
    wanted: &[usize],
    deadline: Option<Instant>,
) -> Result<Vec<usize>, SkippedReason> {
    let Some(commit) = resolve(clients, repo, rev).await? else {
        // An empty repository has no files; another unknown revision is missing.
        if rev.is_empty() {
            return Ok(Vec::new());
        }
        return Err(SkippedReason::RepositoryMissing);
    };
    let mut found = Vec::new();
    for &i in wanted {
        let (pattern, paths) = sub_search(&preds[i]);
        let mut search = clients
            .search_for_repo(RepoId(repo.id))
            .map_err(|_| SkippedReason::Error)?;
        let req = pb::SearchCommitRequest {
            repo: Some(repo_ref(repo)),
            commit_oid: commit.clone(),
            pattern,
            case_sensitive: false,
            paths: Some(paths),
            file_limit: 1,
            ..Default::default()
        };
        let mut stream = search
            .search_commit(with_deadline(req, deadline))
            .await
            .map_err(|s| skip_reason(&s))?
            .into_inner();
        while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
            if !msg.files.is_empty() {
                found.push(i);
                break;
            }
            if msg.stats.is_some_and(|st| st.canceled) {
                return Err(SkippedReason::ShardTimeout);
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn compile(q: &[(bool, Predicate)]) -> Result<RepoPredicates, String> {
        RepoPredicates::compile(q, Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap())
    }

    #[test]
    fn compiles_predicates() {
        let p = compile(&[
            (
                false,
                Predicate::RepoHasFile {
                    path: Some("go\\.mod$".into()),
                    content: Some("module".into()),
                },
            ),
            (true, Predicate::RepoHasPath("vendor/".into())),
            (false, Predicate::RepoHasContent("TODO".into())),
            (false, Predicate::RepoHasCommitAfter("1 month ago".into())),
            (
                false,
                Predicate::RevAtTime {
                    time: "2025-06-01".into(),
                    rev: Some("dev".into()),
                },
            ),
            (
                false,
                Predicate::RevAtTime {
                    time: "2025-06-01T12:00:00Z".into(),
                    rev: Some("HEAD".into()),
                },
            ),
        ])
        .unwrap();
        assert_eq!(p.has_file.len(), 3);
        assert!(p.has_file[1].negated);
        assert_eq!(p.has_file[1].content, None);
        assert_eq!(p.has_file[2].path, None);
        assert_eq!(
            p.commit_after,
            [(
                false,
                Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0)
                    .unwrap()
                    .timestamp()
            )]
        );
        assert_eq!(
            p.at_time,
            [
                (
                    Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0)
                        .unwrap()
                        .timestamp(),
                    "dev".to_owned()
                ),
                (
                    Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0)
                        .unwrap()
                        .timestamp(),
                    String::new()
                ),
            ]
        );
        assert!(p.without_at_time().at_time.is_empty());
        assert!(!p.is_empty());
        assert!(compile(&[]).unwrap().is_empty());
    }

    #[test]
    fn compile_errors() {
        let err = |p: Predicate, negated: bool| compile(&[(negated, p)]).unwrap_err();
        assert!(err(Predicate::RepoHasPath("(".into()), false).contains("repo:has.path()"));
        assert!(err(Predicate::RepoHasCommitAfter("soon".into()), false).contains("invalid date"));
        assert!(
            err(
                Predicate::RevAtTime {
                    time: "yesterday".into(),
                    rev: None
                },
                true
            )
            .contains("does not support negation")
        );
        assert!(
            err(
                Predicate::RevAtTime {
                    time: "whenever".into(),
                    rev: None
                },
                false
            )
            .contains("rev:at.time()")
        );
    }

    #[test]
    fn revs_round_trip() {
        let (ex, revs) = split_revs(&[]);
        assert_eq!((ex.clone(), revs.clone()), (vec![], vec![String::new()]));
        assert!(join_revs(revs, ex).is_empty());
        let (ex, revs) = split_revs(&["main".into(), "^old".into()]);
        assert_eq!(revs, ["main"]);
        assert_eq!(join_revs(revs, ex), ["main", "^old"]);
    }
}
