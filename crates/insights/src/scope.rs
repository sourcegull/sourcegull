//! The repositories a series counts: every repository, a list, or those a
//! repository query matches, intersected with the series query's own `repo:` filters.
//!
//! Series are counted for the whole instance, with no viewer's permissions: permissions
//! apply when points are read ([`crate::read`]). Blocked repositories and those the
//! repo nodes report as not cloned are left out.

use std::collections::HashSet;

use sourcegull_query::{PatternType, Predicate};
use sourcegull_search::{
    ContextScope, Job, Limits, RepoCatalog, RepoFilter, RepoMatcher, plan_query,
};
use sourcegull_types::RepoAccess;

use crate::compile::CompiledQuery;
use crate::series::RepoScope;

/// A repository a series counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRepo {
    pub id: i32,
    pub name: String,
}

/// The repository filters of a repository query (scope [`RepoScope::Query`]): one per
/// `OR`ed group. Only repository filters are allowed (`repo:`, `-repo:`, `fork:`,
/// `archived:`, `visibility:`, `repo:has.description`, `repo:has.topic`).
pub fn criteria_filters(criteria: &str) -> Result<Vec<RepoFilter>, String> {
    let job = plan_query(
        criteria,
        PatternType::default(),
        &Limits::default(),
        chrono::Utc::now(),
    )
    .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    collect(&job, &mut out)?;
    if out.is_empty() {
        return Err("The repository query has no repository filters.".into());
    }
    Ok(out)
}

fn collect(job: &Job, out: &mut Vec<RepoFilter>) -> Result<(), String> {
    const ONLY: &str = "A repository query may only hold repository filters (repo:, fork:, \
                        archived:, visibility:, repo:has.description, repo:has.topic).";
    match job {
        Job::Timeout { child, .. } | Job::Limit { child, .. } => collect(child, out),
        Job::Parallel(jobs) => jobs.iter().try_for_each(|j| collect(j, out)),
        Job::Repo {
            pattern: None,
            repos,
        } => {
            if repos.predicates.iter().any(|(_, p)| {
                !matches!(
                    p,
                    Predicate::RepoHasDescription(_) | Predicate::RepoHasTopic(_)
                )
            }) || !repos.revs.is_empty()
                || repos.context.is_some()
            {
                return Err(ONLY.into());
            }
            if !out.contains(repos) {
                out.push(repos.clone());
            }
            Ok(())
        }
        _ => Err(ONLY.into()),
    }
}

/// The repositories of `catalog` a repository query matches (`previewRepositoriesFromQuery`),
/// in catalog order. Blocked repositories are left out.
pub fn criteria_repos(catalog: &RepoCatalog, criteria: &str) -> Result<Vec<ScopedRepo>, String> {
    let matchers = criteria_filters(criteria)?
        .iter()
        .map(|f| RepoMatcher::new(f, &ContextScope::Global, RepoAccess::All))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(catalog
        .repos()
        .iter()
        .filter(|r| !r.blocked && matchers.iter().any(|m| m.matches(r)))
        .map(|r| ScopedRepo {
            id: r.id,
            name: r.name.clone(),
        })
        .collect())
}

/// The repositories of `catalog` that a series with `scope` and `query` counts, in catalog
/// order (most stars first).
pub fn resolve(
    catalog: &RepoCatalog,
    scope: &RepoScope,
    query: &CompiledQuery,
) -> Result<Vec<ScopedRepo>, String> {
    let all = || RepoMatcher::new(&query.repos, &ContextScope::Global, RepoAccess::All);
    let matcher = all().map_err(|e| e.to_string())?;
    let criteria = match scope {
        RepoScope::Query(q) => criteria_filters(q)?
            .iter()
            .map(|f| RepoMatcher::new(f, &ContextScope::Global, RepoAccess::All))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?,
        _ => Vec::new(),
    };
    let listed: Option<HashSet<&str>> = match scope {
        RepoScope::List(names) => Some(names.iter().map(String::as_str).collect()),
        _ => None,
    };
    Ok(catalog
        .repos()
        .iter()
        .filter(|r| matcher.matches(r))
        .filter(|r| listed.as_ref().is_none_or(|l| l.contains(r.name.as_str())))
        .filter(|r| criteria.is_empty() || criteria.iter().any(|m| m.matches(r)))
        .map(|r| ScopedRepo {
            id: r.id,
            name: r.name.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::series::GenerationMethod;
    use sourcegull_search::CatalogRepo;

    fn catalog() -> RepoCatalog {
        RepoCatalog::new(
            ["acme/web", "acme/api", "other/lib"]
                .iter()
                .enumerate()
                .map(|(i, n)| CatalogRepo {
                    id: i as i32 + 1,
                    name: (*n).into(),
                    ..CatalogRepo::default()
                })
                .collect(),
        )
    }

    fn names(scope: &RepoScope, q: &str) -> Vec<String> {
        let q = compile(q, GenerationMethod::Search, None, chrono::Utc::now()).unwrap();
        let mut n: Vec<_> = resolve(&catalog(), scope, &q)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        n.sort();
        n
    }

    #[test]
    fn scopes_intersect_the_query_filters() {
        assert_eq!(names(&RepoScope::All, "x").len(), 3);
        assert_eq!(
            names(&RepoScope::All, "repo:^acme/ x"),
            ["acme/api", "acme/web"]
        );
        assert_eq!(
            names(
                &RepoScope::List(vec!["acme/web".into(), "other/lib".into()]),
                "repo:acme x"
            ),
            ["acme/web"]
        );
        assert_eq!(
            names(&RepoScope::Query("repo:lib$ or repo:api$".into()), "x"),
            ["acme/api", "other/lib"]
        );
        assert!(criteria_filters("repo:x TODO").is_err());
        assert!(criteria_filters("repo:has.file(go.mod)").is_err());
    }
}
