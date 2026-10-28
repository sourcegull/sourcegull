//! View filters: which repositories a view's points count, applied when points are read.
//!
//! - `includeRepoRegex` / `excludeRepoRegex`: regular expressions on the repository name.
//! - `searchContexts`: only repositories in one of the contexts. A query-defined context
//!   contributes the repositories its `repo:` / `-repo:` filters select (unwrapped into
//!   include and exclude patterns); a listed context its repositories.
//!   Contexts are resolved as the viewer sees them; an unknown one is an error.
//!
//! The result is a bitmap of repository ids intersected with the series' repositories and
//! the viewer's readable ones by the read path ([`crate::read::points_in`]).

use regex::Regex;
use roaring::RoaringBitmap;
use sourcegull_db::Db;
use sourcegull_query::PatternType;
use sourcegull_search::{ContextScope, Job, Limits, RepoCatalog, RepoFilter, plan_query};
use sourcegull_search_web::{ResolvedContext, resolve_context};
use sourcegull_types::UserId;

use crate::views::ViewFilters;

/// Compiles an include or exclude filter; the error is the message for the user.
fn compile(field: &str, re: Option<&str>) -> Result<Option<Regex>, String> {
    match re.filter(|r| !r.is_empty()) {
        None => Ok(None),
        Some(r) => Regex::new(r)
            .map(Some)
            .map_err(|e| format!("Invalid {field} {r:?}: {e}")),
    }
}

/// Checks the filters' regular expressions; the error is the message for the user.
pub fn validate(filters: &ViewFilters) -> Result<(), String> {
    compile("includeRepoRegex", filters.include_repo_regex.as_deref())?;
    compile("excludeRepoRegex", filters.exclude_repo_regex.as_deref())?;
    Ok(())
}

/// The repository filters of a planned context query, one per `OR`ed part.
fn repo_filters(job: &Job, out: &mut Vec<RepoFilter>) {
    match job {
        Job::Timeout { child, .. } | Job::Limit { child, .. } | Job::Select { child, .. } => {
            repo_filters(child, out)
        }
        Job::Parallel(jobs) => jobs.iter().for_each(|j| repo_filters(j, out)),
        Job::Content { repos, .. }
        | Job::Path { repos, .. }
        | Job::Repo { repos, .. }
        | Job::Symbol { repos, .. }
        | Job::Commit { repos, .. } => {
            if !out.contains(repos) {
                out.push(repos.clone());
            }
        }
    }
}

/// The repositories of `catalog` in context `spec` as `viewer` sees contexts.
async fn context_repos(
    db: &Db,
    catalog: &RepoCatalog,
    spec: &str,
    viewer: Option<UserId>,
) -> Result<Option<RoaringBitmap>, String> {
    let resolved = resolve_context(db, &format!("context:{spec}"), PatternType::Keyword, viewer)
        .await
        .map_err(|e| e.to_string())?;
    let ids = |f: &dyn Fn(&sourcegull_search::CatalogRepo) -> bool| -> RoaringBitmap {
        catalog
            .repos()
            .iter()
            .filter(|r| f(r))
            .filter_map(|r| u32::try_from(r.id).ok())
            .collect()
    };
    match resolved {
        None => Err(format!("The search context {spec:?} does not exist.")),
        Some(ResolvedContext::Global) => Ok(None),
        Some(ResolvedContext::Listed(ContextScope::Listed { repos, .. })) => {
            Ok(Some(ids(&|r| repos.contains_key(&r.id))))
        }
        Some(ResolvedContext::Listed(ContextScope::Global)) => Ok(None),
        Some(ResolvedContext::Query(q)) => {
            let job = plan_query(
                &q,
                PatternType::Keyword,
                &Limits::default(),
                chrono::Utc::now(),
            )
            .map_err(|e| format!("The search context {spec:?}: {e}"))?;
            let mut parts = Vec::new();
            repo_filters(&job, &mut parts);
            let mut compiled = Vec::new();
            for p in &parts {
                let inc = p
                    .include
                    .iter()
                    .map(|r| Regex::new(r))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                let exc = p
                    .exclude
                    .iter()
                    .map(|r| Regex::new(r))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                compiled.push((inc, exc));
            }
            Ok(Some(ids(&|r| {
                compiled.iter().any(|(inc, exc)| {
                    inc.iter().all(|re| re.is_match(&r.name))
                        && !exc.iter().any(|re| re.is_match(&r.name))
                })
            })))
        }
    }
}

/// The repositories of `catalog` that `filters` keep, or `None` when they keep every one.
pub async fn resolve(
    db: &Db,
    catalog: &RepoCatalog,
    filters: &ViewFilters,
    viewer: Option<UserId>,
) -> Result<Option<RoaringBitmap>, String> {
    if filters.is_empty() {
        return Ok(None);
    }
    let include = compile("includeRepoRegex", filters.include_repo_regex.as_deref())?;
    let exclude = compile("excludeRepoRegex", filters.exclude_repo_regex.as_deref())?;
    let mut contexts: Option<RoaringBitmap> = None;
    for spec in &filters.search_contexts {
        match context_repos(db, catalog, spec, viewer).await? {
            // `global` keeps every repository.
            None => {
                contexts = None;
                break;
            }
            Some(ids) => *contexts.get_or_insert_with(RoaringBitmap::new) |= ids,
        }
    }
    Ok(Some(
        catalog
            .repos()
            .iter()
            .filter(|r| include.as_ref().is_none_or(|re| re.is_match(&r.name)))
            .filter(|r| exclude.as_ref().is_none_or(|re| !re.is_match(&r.name)))
            .filter_map(|r| u32::try_from(r.id).ok())
            .filter(|id| contexts.as_ref().is_none_or(|c| c.contains(*id)))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regexes_are_checked() {
        assert!(
            validate(&ViewFilters {
                include_repo_regex: Some("acme/(web".into()),
                ..ViewFilters::default()
            })
            .unwrap_err()
            .contains("includeRepoRegex")
        );
        assert!(
            validate(&ViewFilters {
                exclude_repo_regex: Some("^acme/".into()),
                ..ViewFilters::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn context_queries_yield_their_repository_filters() {
        let job = plan_query(
            "repo:^acme/ -repo:secret or repo:lib$",
            PatternType::Keyword,
            &Limits::default(),
            chrono::Utc::now(),
        )
        .unwrap();
        let mut parts = Vec::new();
        repo_filters(&job, &mut parts);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].include, ["^acme/"]);
        assert_eq!(parts[0].exclude, ["secret"]);
        assert_eq!(parts[1].include, ["lib$"]);
    }
}
