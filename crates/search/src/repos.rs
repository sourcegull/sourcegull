//! Repo resolution: [`RepoFilter`] → repositories and revisions.
//!
//! Resolution runs over an in-memory [`RepoCatalog`], a compact copy of the
//! `repo` table ordered by stars so the most relevant repositories are
//! searched first. It hands out pages ([`DEFAULT_PAGE_SIZE`] repos), so
//! streaming can start before resolution finishes.
//!
//! Predicates on repository metadata (`has.description`, `has.topic`,
//! `has.meta` and its forms `has.key`, `has.tag` and `has`) are evaluated here. Predicates on contents or history (`has.file`,
//! `has.path`, `has.content`, `has.commit.after`) and `rev:at.time()` need the
//! repo nodes; they are returned in [`RepoMatcher::deferred`], and the search
//! runs them over the resolved repositories
//! (`sourcegull_search_exec::repo_predicates`).

use regex::{Regex, RegexBuilder};
use sourcegull_query::{MetaMatch, Predicate};

use crate::context::{ContextScope, ContextSpec};
use crate::plan::{RepoFilter, Visibility, YesNoOnly};

/// Repositories per page.
pub const DEFAULT_PAGE_SIZE: usize = 500;

/// One repository as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogRepo {
    pub id: i32,
    pub name: String,
    pub description: String,
    pub fork: bool,
    pub archived: bool,
    pub private: bool,
    pub stars: i32,
    pub topics: Vec<String>,
    /// Key-value metadata, ordered by key; a `None` value is a tag.
    pub kvps: Vec<(String, Option<String>)>,
    /// Blocked repositories are never searched.
    pub blocked: bool,
}

/// The repositories a search can see, ordered by stars, then name.
#[derive(Debug, Clone, Default)]
pub struct RepoCatalog {
    repos: Vec<CatalogRepo>,
    /// Repository id → index in `repos`.
    by_id: std::collections::HashMap<i32, usize>,
}

impl RepoCatalog {
    pub fn new(mut repos: Vec<CatalogRepo>) -> Self {
        repos.retain(|r| !r.blocked);
        repos.sort_by(|a, b| b.stars.cmp(&a.stars).then_with(|| a.name.cmp(&b.name)));
        let by_id = repos.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
        RepoCatalog { repos, by_id }
    }

    /// The repository with id `id`.
    pub fn get(&self, id: i32) -> Option<&CatalogRepo> {
        self.by_id.get(&id).map(|&i| &self.repos[i])
    }

    /// All repositories, in catalog order.
    pub fn repos(&self) -> &[CatalogRepo] {
        &self.repos
    }

    pub fn len(&self) -> usize {
        self.repos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }
}

/// A resolved repository and the revisions to search (empty: default branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRevs {
    pub id: i32,
    pub name: String,
    pub revs: Vec<String>,
    /// For ranking.
    pub stars: i32,
}

/// One page of resolved repositories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPage {
    pub repos: Vec<RepoRevs>,
    /// Cursor for the next page; `None` when the catalog is exhausted.
    pub next: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("invalid regexp {pattern:?} in repo: {message}")]
    InvalidRegexp { pattern: String, message: String },
    #[error("search context {0:?} not found")]
    UnknownContext(String),
}

/// A compiled [`RepoFilter`].
#[derive(Debug)]
pub struct RepoMatcher {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    revs: Vec<String>,
    fork: YesNoOnly,
    archived: YesNoOnly,
    visibility: Visibility,
    /// A listed context's repositories and revisions.
    context: Option<std::sync::Arc<std::collections::HashMap<i32, Vec<String>>>>,
    /// (negated, matcher) for metadata predicates.
    metadata: Vec<(bool, MetadataPredicate)>,
    deferred: Vec<(bool, Predicate)>,
    /// The viewer's readable repositories.
    access: sourcegull_types::RepoAccess,
}

#[derive(Debug)]
enum MetadataPredicate {
    Description(Regex),
    Topic(String),
    /// A key, and the value (`None`: any value, `Some(None)`: no value).
    Meta(MetaMatcher, Option<Option<MetaMatcher>>),
}

/// A `has.meta` key or value; matches are case-sensitive.
#[derive(Debug)]
enum MetaMatcher {
    Exact(String),
    Regexp(Regex),
}

impl MetaMatcher {
    fn compile(m: &MetaMatch) -> Result<MetaMatcher, ResolveError> {
        match m {
            MetaMatch::Exact(s) => Ok(MetaMatcher::Exact(s.clone())),
            MetaMatch::Regexp(re) => {
                Regex::new(re)
                    .map(MetaMatcher::Regexp)
                    .map_err(|e| ResolveError::InvalidRegexp {
                        pattern: re.clone(),
                        message: e.to_string(),
                    })
            }
        }
    }

    fn is_match(&self, s: &str) -> bool {
        match self {
            MetaMatcher::Exact(e) => e == s,
            MetaMatcher::Regexp(re) => re.is_match(s),
        }
    }
}

fn meta_hit(
    kvps: &[(String, Option<String>)],
    key: &MetaMatcher,
    value: &Option<Option<MetaMatcher>>,
) -> bool {
    kvps.iter().any(|(k, v)| {
        key.is_match(k)
            && match (value, v) {
                (None, _) => true,
                (Some(None), v) => v.is_none(),
                (Some(Some(m)), Some(v)) => m.is_match(v),
                (Some(Some(_)), None) => false,
            }
    })
}

impl RepoMatcher {
    /// Compiles `filter` within `scope`, the resolved `context:` of the
    /// query. A `context:` other than `global` must come with the listed
    /// scope it was resolved to (query-defined contexts are substituted
    /// before planning and leave no `context:`). Repository names and
    /// descriptions match case-insensitively.
    ///
    /// Only repositories `access` may read match. Unreadable repositories are
    /// invisible: they are never searched, counted or reported as skipped. The parameter is
    /// required so that no caller can forget it.
    pub fn new(
        filter: &RepoFilter,
        scope: &ContextScope,
        access: sourcegull_types::RepoAccess,
    ) -> Result<RepoMatcher, ResolveError> {
        let context = match (filter.context.as_deref().map(ContextSpec::parse), scope) {
            (None | Some(ContextSpec::Global), _) => None,
            (Some(_), ContextScope::Listed { spec, repos })
                if filter.context.as_deref() == Some(spec.as_str()) =>
            {
                Some(repos.clone())
            }
            (Some(_), _) => {
                return Err(ResolveError::UnknownContext(
                    filter.context.clone().unwrap_or_default(),
                ));
            }
        };
        let compile = |pattern: &str| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| ResolveError::InvalidRegexp {
                    pattern: pattern.to_string(),
                    message: e.to_string(),
                })
        };
        let mut metadata = Vec::new();
        let mut deferred = Vec::new();
        for (negated, pred) in &filter.predicates {
            match pred {
                Predicate::RepoHasDescription(re) => {
                    metadata.push((*negated, MetadataPredicate::Description(compile(re)?)))
                }
                Predicate::RepoHasTopic(topic) => {
                    metadata.push((*negated, MetadataPredicate::Topic(topic.to_lowercase())))
                }
                Predicate::RepoHasMeta { key, value } => {
                    let value = match value {
                        None => None,
                        Some(MetaMatch::Exact(v)) if v.is_empty() => Some(None),
                        Some(v) => Some(Some(MetaMatcher::compile(v)?)),
                    };
                    metadata.push((
                        *negated,
                        MetadataPredicate::Meta(MetaMatcher::compile(key)?, value),
                    ))
                }
                other => deferred.push((*negated, other.clone())),
            }
        }
        Ok(RepoMatcher {
            include: filter
                .include
                .iter()
                .map(|p| compile(p))
                .collect::<Result<_, _>>()?,
            exclude: filter
                .exclude
                .iter()
                .map(|p| compile(p))
                .collect::<Result<_, _>>()?,
            revs: filter.revs.clone(),
            fork: filter.fork,
            archived: filter.archived,
            visibility: filter.visibility,
            context,
            metadata,
            deferred,
            access,
        })
    }

    /// Predicates that need a search on the repo nodes, with their negation.
    pub fn deferred(&self) -> &[(bool, Predicate)] {
        &self.deferred
    }

    pub fn matches(&self, repo: &CatalogRepo) -> bool {
        let flag = |setting: YesNoOnly, value: bool| match setting {
            YesNoOnly::Yes => true,
            YesNoOnly::No => !value,
            YesNoOnly::Only => value,
        };
        let visible = match self.visibility {
            Visibility::Any => true,
            Visibility::Public => !repo.private,
            Visibility::Private => repo.private,
        };
        !repo.blocked
            && self.access.can_read(sourcegull_types::RepoId(repo.id))
            && visible
            && self
                .context
                .as_ref()
                .is_none_or(|c| c.contains_key(&repo.id))
            && flag(self.fork, repo.fork)
            && flag(self.archived, repo.archived)
            && self.include.iter().all(|re| re.is_match(&repo.name))
            && !self.exclude.iter().any(|re| re.is_match(&repo.name))
            && self.metadata.iter().all(|(negated, pred)| {
                let hit = match pred {
                    MetadataPredicate::Description(re) => re.is_match(&repo.description),
                    MetadataPredicate::Topic(t) => {
                        repo.topics.iter().any(|topic| topic.to_lowercase() == *t)
                    }
                    MetadataPredicate::Meta(key, value) => meta_hit(&repo.kvps, key, value),
                };
                hit != *negated
            })
    }

    /// Resolves up to `page_size` repositories starting at `cursor` (0 for
    /// the first page).
    pub fn page(&self, catalog: &RepoCatalog, cursor: usize, page_size: usize) -> RepoPage {
        let mut repos = Vec::new();
        for (i, repo) in catalog.repos.iter().enumerate().skip(cursor) {
            if repos.len() == page_size {
                return RepoPage {
                    repos,
                    next: Some(i),
                };
            }
            if self.matches(repo) {
                // A listed context's revisions win over the query's.
                let revs = match self.context.as_ref().and_then(|c| c.get(&repo.id)) {
                    Some(revs) if !revs.is_empty() => revs.clone(),
                    _ => self.revs.clone(),
                };
                repos.push(RepoRevs {
                    id: repo.id,
                    name: repo.name.clone(),
                    revs,
                    stars: repo.stars,
                });
            }
        }
        RepoPage { repos, next: None }
    }

    /// All matching repositories, page by page.
    pub fn pages<'a>(
        &'a self,
        catalog: &'a RepoCatalog,
        page_size: usize,
    ) -> impl Iterator<Item = Vec<RepoRevs>> + 'a {
        let mut cursor = Some(0);
        std::iter::from_fn(move || {
            let page = self.page(catalog, cursor?, page_size.max(1));
            cursor = page.next;
            (!page.repos.is_empty() || cursor.is_some()).then_some(page.repos)
        })
        .filter(|repos| !repos.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Job, Limits, plan_query};
    use chrono::Utc;
    use sourcegull_query::PatternType;
    use sourcegull_types::RepoAccess;

    fn repo(id: i32, name: &str, stars: i32) -> CatalogRepo {
        CatalogRepo {
            id,
            name: name.into(),
            stars,
            ..Default::default()
        }
    }

    fn catalog() -> RepoCatalog {
        RepoCatalog::new(vec![
            repo(1, "gitlab.com/acme/api", 10),
            CatalogRepo {
                fork: true,
                ..repo(2, "gitlab.com/bob/api", 1)
            },
            CatalogRepo {
                archived: true,
                ..repo(3, "gitlab.com/acme/old", 5)
            },
            CatalogRepo {
                private: true,
                description: "Search engine".into(),
                topics: vec!["Rust".into()],
                kvps: vec![("go".into(), None), ("team".into(), Some("search".into()))],
                ..repo(4, "gitlab.com/acme/sourcegull", 50)
            },
            CatalogRepo {
                blocked: true,
                ..repo(5, "gitlab.com/acme/blocked", 100)
            },
            repo(6, "gitlab.com/other/web", 10),
        ])
    }

    /// The repo filter of a query's first backend job.
    fn filter(q: &str) -> RepoFilter {
        fn find(job: Job) -> RepoFilter {
            match job {
                Job::Content { repos, .. }
                | Job::Path { repos, .. }
                | Job::Repo { repos, .. }
                | Job::Symbol { repos, .. }
                | Job::Commit { repos, .. } => repos,
                Job::Parallel(jobs) => find(jobs.into_iter().next().expect("job")),
                Job::Select { child, .. }
                | Job::Limit { child, .. }
                | Job::Timeout { child, .. } => find(*child),
            }
        }
        find(plan_query(q, PatternType::Keyword, &Limits::default(), Utc::now()).unwrap())
    }

    #[test]
    fn access_hides_unreadable_repositories() {
        let unrestricted: std::sync::Arc<roaring::RoaringBitmap> =
            std::sync::Arc::new([1u32, 2, 3, 6].into_iter().collect());
        let granted = std::sync::Arc::new([4u32].into_iter().collect());
        let read = |access: sourcegull_types::RepoAccess| {
            RepoMatcher::new(&filter("repo:acme"), &ContextScope::Global, access)
                .unwrap()
                .pages(&catalog(), DEFAULT_PAGE_SIZE)
                .flatten()
                .map(|r| r.name)
                .collect::<Vec<_>>()
        };
        let anon = sourcegull_types::RepoAccess::anonymous(false, unrestricted.clone());
        // Archived repos are excluded by default, so acme/old never matches.
        assert_eq!(read(anon), ["gitlab.com/acme/api"]);
        let user = sourcegull_types::RepoAccess::Restricted {
            user_id: Some(sourcegull_types::UserId(1)),
            user_mapping: false,
            unrestricted,
            granted,
        };
        assert_eq!(
            read(user),
            ["gitlab.com/acme/sourcegull", "gitlab.com/acme/api"]
        );
    }

    fn names(q: &str) -> Vec<String> {
        let m = RepoMatcher::new(&filter(q), &ContextScope::Global, RepoAccess::All).unwrap();
        m.pages(&catalog(), DEFAULT_PAGE_SIZE)
            .flatten()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn order_and_defaults() {
        // Stars first, then name; forks, archived and blocked repos are out.
        assert_eq!(
            names("foo"),
            [
                "gitlab.com/acme/sourcegull",
                "gitlab.com/acme/api",
                "gitlab.com/other/web"
            ]
        );
    }

    #[test]
    fn name_filters() {
        assert_eq!(
            names("repo:ACME/ -repo:sourcegull foo"),
            ["gitlab.com/acme/api"]
        );
        assert_eq!(
            names("repo:/api$ fork:yes foo"),
            ["gitlab.com/acme/api", "gitlab.com/bob/api"]
        );
        assert_eq!(
            names("repo:acme archived:only foo"),
            ["gitlab.com/acme/old"]
        );
        assert_eq!(
            names("visibility:private foo"),
            ["gitlab.com/acme/sourcegull"]
        );
    }

    #[test]
    fn metadata_predicates() {
        assert_eq!(
            names("repo:has.topic(rust)"),
            ["gitlab.com/acme/sourcegull"]
        );
        assert_eq!(
            names("repo:has.description(engine$)"),
            ["gitlab.com/acme/sourcegull"]
        );
        assert_eq!(
            names("-repo:has.topic(rust) repo:acme"),
            ["gitlab.com/acme/api"]
        );
    }

    #[test]
    fn key_value_predicates() {
        let catalog = RepoCatalog::new(vec![
            CatalogRepo {
                kvps: vec![("team".into(), Some("Web".into()))],
                ..repo(1, "a", 3)
            },
            CatalogRepo {
                kvps: vec![("go".into(), None), ("team".into(), Some("search".into()))],
                ..repo(2, "b", 2)
            },
            repo(3, "c", 1),
        ]);
        let names = |q: &str| {
            RepoMatcher::new(&filter(q), &ContextScope::Global, RepoAccess::All)
                .unwrap()
                .pages(&catalog, DEFAULT_PAGE_SIZE)
                .flatten()
                .map(|r| r.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("repo:has.meta(team:search)"), ["b"]);
        assert_eq!(names("repo:has.meta(team)"), ["a", "b"]);
        assert_eq!(names("repo:has.key(team)"), ["a", "b"]);
        assert_eq!(names("repo:has(team:Web)"), ["a"]);
        // Exact matches are case-sensitive.
        assert!(names("repo:has.meta(team:web)").is_empty());
        assert_eq!(names("repo:has.meta(/^te/:/^s/)"), ["b"]);
        assert_eq!(names("repo:has.tag(go)"), ["b"]);
        assert_eq!(names("repo:has.meta(go:)"), ["b"]);
        // A tag has no value, so a value never matches it.
        assert!(names("repo:has.meta(team:)").is_empty());
        assert_eq!(names("-repo:has.meta(team)"), ["c"]);
        assert_eq!(names("-repo:has.tag(go) repo:has.key(team)"), ["a"]);
        assert_eq!(names("repo:has.tag(go) repo:has.meta(team:search)"), ["b"]);
        assert!(matches!(
            RepoMatcher::new(
                &filter("repo:has.meta(/(/:x)"),
                &ContextScope::Global,
                RepoAccess::All
            ),
            Err(ResolveError::InvalidRegexp { .. })
        ));
    }

    #[test]
    fn deferred_predicates_and_revs() {
        let m = RepoMatcher::new(
            &filter("repo:api@main:v2 repo:has.file(path:go.mod) foo"),
            &ContextScope::Global,
            RepoAccess::All,
        )
        .unwrap();
        assert_eq!(m.deferred().len(), 1);
        let page = m.page(&catalog(), 0, 10);
        assert_eq!(
            page.repos,
            [RepoRevs {
                id: 1,
                name: "gitlab.com/acme/api".into(),
                revs: vec!["main".into(), "v2".into()],
                stars: 10,
            }]
        );
    }

    #[test]
    fn paging() {
        let m = RepoMatcher::new(&filter("foo"), &ContextScope::Global, RepoAccess::All).unwrap();
        let first = m.page(&catalog(), 0, 2);
        assert_eq!(first.repos.len(), 2);
        let second = m.page(&catalog(), first.next.unwrap(), 2);
        assert_eq!(second.repos.iter().map(|r| r.id).collect::<Vec<_>>(), [6]);
        assert_eq!(second.next, None);
        assert_eq!(m.pages(&catalog(), 1).count(), 3);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            RepoMatcher::new(
                &filter("repo:(unclosed[ foo"),
                &ContextScope::Global,
                RepoAccess::All
            ),
            Err(ResolveError::InvalidRegexp { .. })
        ));
        assert!(matches!(
            RepoMatcher::new(
                &filter("context:@alice foo"),
                &ContextScope::Global,
                RepoAccess::All
            ),
            Err(ResolveError::UnknownContext(_))
        ));
        assert!(
            RepoMatcher::new(
                &filter("context:global foo"),
                &ContextScope::Global,
                RepoAccess::All
            )
            .is_ok()
        );
    }

    #[test]
    fn listed_context() {
        // api on its default branch, sourcegull on v1, and the fork bob/api
        // (the query's `fork:` still applies).
        let scope = ContextScope::listed(
            "@acme/core",
            [
                (1, vec![String::new()]),
                (4, vec!["v1".to_owned()]),
                (2, vec![]),
                (6, vec![]),
            ],
        );
        let page = |q: &str| {
            RepoMatcher::new(&filter(q), &scope, RepoAccess::All)
                .unwrap()
                .page(&catalog(), 0, 10)
                .repos
                .into_iter()
                .map(|r| (r.name, r.revs))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            page("context:@acme/core repo:acme@dev foo"),
            [
                (
                    "gitlab.com/acme/sourcegull".to_owned(),
                    vec!["v1".to_owned()]
                ),
                ("gitlab.com/acme/api".to_owned(), vec!["dev".to_owned()]),
            ]
        );
        // Another context's scope does not stand in for this one.
        assert!(matches!(
            RepoMatcher::new(&filter("context:@acme/other foo"), &scope, RepoAccess::All),
            Err(ResolveError::UnknownContext(_))
        ));
        assert_eq!(page("context:@acme/core repo:web foo").len(), 1);
        // Without a `context:` term the scope does not apply.
        assert_eq!(page("foo").len(), 3);
    }
}
