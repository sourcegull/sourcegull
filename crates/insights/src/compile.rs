//! Series queries compiled for counting: the query is planned like a search, then
//! each part becomes a [`SeriesLeaf`] that repo nodes count per file or commit.
//!
//! What counts, per file or commit: content matches (a file
//! matched only through negations counts once), one per file for path results, matching
//! symbol definitions, and one per commit for commit and diff results. Repository name
//! results (a query without `type:` also searches names) are not counted.
//!
//! Not supported (the error is the message for the user): `select:`, `AND`/`OR` between
//! groups of filters with different repository filters, the predicates `repo:has.commit.after`,
//! `repo:has.meta`, `rev:at.time`, `file:has.owner` and `file:has.contributor`, and
//! `context:` other than `global`.

use chrono::{DateTime, Utc};
use sourcegull_query::{PatternType, Predicate};
use sourcegull_search::{FileFilter, Job, Limits, PatternSpec, RepoFilter, plan_query};
use sourcegull_search_client::CommitFilterSpec;
use sourcegull_search_exec::series::{Group, LeafKind, RepoPredicate, SeriesLeaf, SeriesSpec};

use crate::series::{GenerationMethod, GroupBy};

/// A series query, ready to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledQuery {
    pub spec: SeriesSpec,
    /// The query's repository filters (predicates moved into the spec).
    pub repos: RepoFilter,
    /// The revision counted; empty for the default branch.
    pub rev: String,
}

/// Compiles `query` for a series of `method` (with `group_by` for `search-compute`).
pub fn compile(
    query: &str,
    method: GenerationMethod,
    group_by: Option<GroupBy>,
    now: DateTime<Utc>,
) -> Result<CompiledQuery, String> {
    if query.trim().is_empty() {
        return Err("The query is empty.".into());
    }
    // Capture groups need a regular expression.
    let pattern_type = match method {
        GenerationMethod::MappingCompute => PatternType::Regexp,
        _ => PatternType::default(),
    };
    let job =
        plan_query(query, pattern_type, &Limits::default(), now).map_err(|e| e.to_string())?;
    let mut leaves = Vec::new();
    let mut repos: Option<RepoFilter> = None;
    collect(&job, &mut leaves, &mut repos)?;
    let mut repos = repos.ok_or("The query has nothing to count.")?;
    if let Some(ctx) = repos.context.as_deref()
        && ctx != sourcegull_search::GLOBAL_CONTEXT
    {
        return Err("Trends do not support search contexts yet; use repo: filters.".into());
    }
    let mut repo_predicates = Vec::new();
    let mut kept = Vec::new();
    for (negated, p) in std::mem::take(&mut repos.predicates) {
        match p {
            Predicate::RepoHasFile { path, content } => repo_predicates.push(RepoPredicate {
                negated,
                path,
                content,
            }),
            Predicate::RepoHasPath(path) => repo_predicates.push(RepoPredicate {
                negated,
                path: Some(path),
                content: None,
            }),
            Predicate::RepoHasContent(content) => repo_predicates.push(RepoPredicate {
                negated,
                path: None,
                content: Some(content),
            }),
            p @ (Predicate::RepoHasDescription(_) | Predicate::RepoHasTopic(_)) => {
                kept.push((negated, p))
            }
            other => return Err(unsupported(&other)),
        }
    }
    repos.predicates = kept;
    let revs: Vec<&String> = repos.revs.iter().filter(|r| !r.starts_with('^')).collect();
    let rev = match revs.as_slice() {
        [] => String::new(),
        [r] => (*r).clone(),
        _ => return Err("Trends count one revision per repository.".into()),
    };
    let (group, capture) = match (method, group_by) {
        (GenerationMethod::Search, _) => (Group::None, None),
        (GenerationMethod::MappingCompute, _) => {
            leaves.retain(|l| l.kind == LeafKind::Content);
            let [leaf] = leaves.as_slice() else {
                return Err(
                    "A capture group insight needs one regular expression searched in file \
                     contents."
                        .into(),
                );
            };
            let capture = capture_regex(leaf.pattern.as_ref())?;
            (Group::Capture, Some(capture))
        }
        (GenerationMethod::SearchCompute, None) => {
            return Err("A grouped trend needs a group (repo, lang, path, author or date).".into());
        }
        (GenerationMethod::SearchCompute, Some(g)) => {
            let commits = leaves.iter().filter(|l| is_commit(l.kind)).count();
            match g {
                GroupBy::Author | GroupBy::Date if commits != leaves.len() => {
                    return Err(format!(
                        "Grouping by {} needs a commit or diff search (type:commit or \
                         type:diff).",
                        g.as_str()
                    ));
                }
                GroupBy::Lang | GroupBy::Path if commits > 0 => {
                    return Err(format!(
                        "Grouping by {} is not supported for commit and diff searches.",
                        g.as_str()
                    ));
                }
                _ => {}
            }
            let group = match g {
                GroupBy::Repo => Group::Repo,
                GroupBy::Lang => Group::Lang,
                GroupBy::Path => Group::Path,
                GroupBy::Author => Group::Author,
                GroupBy::Date => Group::Date,
            };
            (group, None)
        }
    };
    if leaves.is_empty() {
        return Err("The query has nothing to count.".into());
    }
    Ok(CompiledQuery {
        spec: SeriesSpec {
            leaves,
            group,
            capture,
            capture_case_sensitive: false,
            repo_predicates,
        },
        repos,
        rev,
    })
}

fn is_commit(k: LeafKind) -> bool {
    matches!(k, LeafKind::Commit | LeafKind::Diff)
}

fn unsupported(p: &Predicate) -> String {
    let name = match p {
        Predicate::RepoHasCommitAfter(_) => "repo:has.commit.after",
        Predicate::RepoHasMeta { .. } => "repo:has.meta",
        Predicate::RevAtTime { .. } => "rev:at.time",
        Predicate::FileHasOwner(_) => "file:has.owner",
        Predicate::FileHasContributor(_) => "file:has.contributor",
        _ => "this predicate",
    };
    format!("Trends do not support {name}.")
}

/// The capture regexp of a capture group series: its one non-negated regexp.
fn capture_regex(pattern: Option<&PatternSpec>) -> Result<String, String> {
    fn atoms<'p>(p: &'p PatternSpec, out: &mut Vec<&'p str>) {
        match p {
            PatternSpec::Atom {
                regex,
                negated: false,
            } => out.push(regex),
            PatternSpec::And(ps) | PatternSpec::Or(ps) => ps.iter().for_each(|p| atoms(p, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    if let Some(p) = pattern {
        atoms(p, &mut found);
    }
    match found.as_slice() {
        [re] => Ok((*re).to_owned()),
        _ => Err(
            "A capture group insight needs exactly one regular expression with a capture \
             group, e.g. `TODO\\((\\w+)\\)`."
                .into(),
        ),
    }
}

/// The leaves of `job`; every part must share one repository filter.
fn collect(
    job: &Job,
    out: &mut Vec<SeriesLeaf>,
    repos: &mut Option<RepoFilter>,
) -> Result<(), String> {
    let mut same_repos = |r: &RepoFilter| -> Result<(), String> {
        match repos {
            None => {
                *repos = Some(r.clone());
                Ok(())
            }
            Some(have) if have == r => Ok(()),
            Some(_) => Err(
                "Trends do not support AND/OR between groups of filters with \
                            different repo: filters; use one series per group."
                    .into(),
            ),
        }
    };
    let pattern = |info: &sourcegull_search::PatternInfo| {
        PatternSpec::from_pattern(info).map_err(|e| e.to_string())
    };
    match job {
        Job::Timeout { child, .. } | Job::Limit { child, .. } => collect(child, out, repos),
        Job::Parallel(jobs) => jobs.iter().try_for_each(|j| collect(j, out, repos)),
        Job::Select { .. } => Err("Trends do not support select:.".into()),
        Job::Repo { repos: r, .. } => same_repos(r),
        Job::Content {
            pattern: p,
            repos: r,
            files,
            ..
        } => {
            same_repos(r)?;
            let mut leaf = SeriesLeaf::new(LeafKind::Content, Some(pattern(p)?));
            leaf.case_sensitive = p.case_sensitive;
            set_files(&mut leaf, files)?;
            out.push(leaf);
            Ok(())
        }
        Job::Path {
            pattern: p,
            repos: r,
            files,
            ..
        }
        | Job::Symbol {
            pattern: p,
            repos: r,
            files,
            ..
        } => {
            same_repos(r)?;
            let kind = if matches!(job, Job::Path { .. }) {
                LeafKind::Path
            } else {
                LeafKind::Symbol
            };
            let mut leaf = SeriesLeaf::new(kind, p.as_ref().map(pattern).transpose()?);
            leaf.case_sensitive = p.as_ref().is_some_and(|p| p.case_sensitive);
            set_files(&mut leaf, files)?;
            out.push(leaf);
            Ok(())
        }
        Job::Commit {
            pattern: p,
            repos: r,
            files,
            filter,
            diff,
        } => {
            same_repos(r)?;
            let kind = if *diff {
                LeafKind::Diff
            } else {
                LeafKind::Commit
            };
            let mut leaf = SeriesLeaf::new(kind, p.as_ref().map(pattern).transpose()?);
            leaf.case_sensitive = p.as_ref().is_some_and(|p| p.case_sensitive);
            leaf.commit_filters = CommitFilterSpec::from(filter);
            set_files(&mut leaf, files)?;
            out.push(leaf);
            Ok(())
        }
    }
}

/// Copies `files` into `leaf`, turning `file:has.content` into a per-file content check.
fn set_files(leaf: &mut SeriesLeaf, files: &FileFilter) -> Result<(), String> {
    let mut f = files.clone();
    for (negated, p) in std::mem::take(&mut f.predicates) {
        match p {
            Predicate::FileHasContent(re) => leaf.file_contents.push((negated, re)),
            other => return Err(unsupported(&other)),
        }
    }
    leaf.paths = f;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(q: &str, m: GenerationMethod, g: Option<GroupBy>) -> Result<CompiledQuery, String> {
        compile(q, m, g, Utc::now())
    }

    #[test]
    fn search_series_count_contents_and_paths() {
        let q = c("repo:^acme/ TODO lang:go", GenerationMethod::Search, None).unwrap();
        assert_eq!(q.repos.include, vec!["^acme/"]);
        assert_eq!(q.spec.group, Group::None);
        let kinds: Vec<_> = q.spec.leaves.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, vec![LeafKind::Content]);
        assert_eq!(q.spec.leaves[0].paths.langs, vec!["go"]);
        let q = c("TODO", GenerationMethod::Search, None).unwrap();
        let kinds: Vec<_> = q.spec.leaves.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, vec![LeafKind::Content, LeafKind::Path]);
        let q = c("type:symbol Foo", GenerationMethod::Search, None).unwrap();
        assert_eq!(q.spec.leaves[0].kind, LeafKind::Symbol);
        let q = c("type:diff author:ann fix", GenerationMethod::Search, None).unwrap();
        assert_eq!(q.spec.leaves[0].kind, LeafKind::Diff);
        assert_eq!(q.spec.leaves[0].commit_filters.authors, vec!["ann"]);
    }

    #[test]
    fn predicates_and_revisions() {
        let q = c(
            "repo:has.file(path:go\\.mod) file:has.content(foo) bar repo:x@dev",
            GenerationMethod::Search,
            None,
        )
        .unwrap();
        assert_eq!(q.spec.repo_predicates.len(), 1);
        assert_eq!(
            q.spec.leaves[0].file_contents,
            vec![(false, "foo".to_owned())]
        );
        assert_eq!(q.rev, "dev");
        let e = c(
            "repo:has.commit.after(2024-01-01) x",
            GenerationMethod::Search,
            None,
        );
        assert!(
            e.as_ref().unwrap_err().contains("has.commit.after"),
            "{e:?}"
        );
        assert!(c("x select:repo", GenerationMethod::Search, None).is_err());
    }

    #[test]
    fn capture_and_group_series() {
        let q = c(r"TODO\((\w+)\)", GenerationMethod::MappingCompute, None).unwrap();
        assert_eq!(q.spec.group, Group::Capture);
        assert_eq!(q.spec.capture.as_deref(), Some(r"TODO\((\w+)\)"));
        assert_eq!(q.spec.leaves.len(), 1);
        let q = c("TODO", GenerationMethod::SearchCompute, Some(GroupBy::Lang)).unwrap();
        assert_eq!(q.spec.group, Group::Lang);
        assert!(
            c(
                "TODO",
                GenerationMethod::SearchCompute,
                Some(GroupBy::Author)
            )
            .is_err()
        );
        let q = c(
            "type:commit fix",
            GenerationMethod::SearchCompute,
            Some(GroupBy::Author),
        )
        .unwrap();
        assert_eq!(q.spec.group, Group::Author);
        let q = c(
            "type:diff fix",
            GenerationMethod::SearchCompute,
            Some(GroupBy::Date),
        )
        .unwrap();
        assert_eq!(q.spec.group, Group::Date);
        let e = c("TODO", GenerationMethod::SearchCompute, Some(GroupBy::Date)).unwrap_err();
        assert!(e.contains("Grouping by date"), "{e}");
        assert!(c("TODO", GenerationMethod::SearchCompute, None).is_err());
    }
}
