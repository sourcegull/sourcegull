//! Runs a search for a viewer as the streaming search API streams it.
//!
//! Parses and plans the query, resolves repositories from the repo table and
//! runs content and path jobs on the repo nodes (indexed, falling back to
//! unindexed per `index:`), commit and diff jobs on the repo nodes, and repo
//! jobs from the catalog. Symbol jobs search symbol names the same way,
//! indexed or unindexed, and structural queries run their template
//! on the same paths. `select:` projects the results of every part
//! of the query through one [`Selector`], before the result limit counts
//! them. `context:` is resolved first: a query-defined context's
//! query replaces the term, a listed context limits the repositories and
//! sets their revisions. `file:has.owner()` terms are resolved to owner keys
//! here and evaluated on the repo nodes; `select:file.owners` turns file
//! results into owner results there, which are resolved to users and teams
//! here before they are sent (Code Ownership, read through [`Ownership`]). `rev:at.time()`,
//! `repo:has.commit.after()` and `repo:has.file()` (with `has.path`/`has.content`) are run on
//! the repo nodes over the resolved repositories before a job searches them
//! ([`sourcegull_search_client::repo_predicates`]), once per repository filter and request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sourcegull_authn::Actor;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_db::stores::{ContextAccess, ListReposOptions};
use sourcegull_own::OwnerTerm;
use sourcegull_query::PatternType;
use sourcegull_query::Predicate;
use sourcegull_rpc::RepoClients;
use sourcegull_search::stream::{
    Alert, Event, Match, Progress, ProposedQuery, Severity, Skipped, SkippedReason,
    SkippedSuggested,
};
use sourcegull_search::{
    CatalogRepo, ContextScope, ContextSpec, DEFAULT_PAGE_SIZE, FileFilter, Job, Limits, Matcher,
    PatternInfo, PatternSpec, RepoCatalog, RepoFilter, RepoMatcher, RepoRevs, SelectPath, Selector,
    YesNoOnly, context_of, plan_query, substitute_context,
};
use sourcegull_search_client::repo_predicates::{Filtered, RepoPredicates, filter_repos};
use sourcegull_search_client::{ContentSearch, ContributorTerm, OwnerSearch, search_repos};
use sourcegull_types::{RepoAccess, UserId};

use crate::ownership::{Owner, Ownership};

/// Repositories searched at once per request.
const REPO_CONCURRENCY: usize = 16;

/// The parameters of a search: the stream API's query string (`q`, `t`, `display`,
/// `max-line-len`, `cm`) and what other callers set.
#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub q: String,
    /// Default pattern type (`keyword`, `standard`, `literal`, `regexp`, `structural`).
    pub t: Option<String>,
    /// Display limit: results sent to the client.
    pub display: Option<usize>,
    #[serde(rename = "max-line-len")]
    pub max_line_len: Option<u32>,
    /// `cm=t` opts into chunk matches.
    pub cm: Option<String>,
    /// Set by callers other than the stream API.
    #[serde(skip)]
    pub run: RunOptions,
}

/// How a search runs besides its query, for callers other than the stream API.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Plan with [`sourcegull_search::exhaustive::plan_exhaustive`] (Search Jobs): no
    /// interactive limits, and no repository count limit for commit and diff searches.
    pub exhaustive: bool,
    /// Search only this repository (by id) at this revision (`HEAD` or `""`: the default
    /// branch), if the query covers it for the viewer.
    pub only: Option<(i32, String)>,
}

/// A job ready to run.
enum Unit {
    /// Content or path search on the repo nodes.
    Files {
        search: Box<ContentSearch>,
        repos: RepoFilter,
        /// `file:has.owner(…)` terms: (negated, argument; `None` for any owner).
        owner_terms: Vec<(bool, Option<String>)>,
        /// `file:has.contributor(…)` terms.
        contributor_terms: Vec<ContributorTerm>,
    },
    /// Repository names, answered from the catalog.
    Repos {
        pattern: Option<PatternSpec>,
        case_sensitive: bool,
        repos: RepoFilter,
    },
}

impl Unit {
    fn repos(&self) -> &RepoFilter {
        match self {
            Unit::Files { repos, .. } | Unit::Repos { repos, .. } => repos,
        }
    }

    /// The `index:` the unit's repository predicates are checked with.
    fn index(&self) -> YesNoOnly {
        match self {
            Unit::Files { search, .. } => search.index,
            Unit::Repos { .. } => YesNoOnly::Yes,
        }
    }
}

/// Repository predicate results of one request, per repository filter and `index:`: the
/// parts of a query share them (content, path and repository jobs of one filter).
#[derive(Default)]
struct PredicateCache(HashMap<String, Arc<Filtered>>);

impl PredicateCache {
    fn key(unit: &Unit) -> String {
        format!("{:?}|{:?}", unit.repos(), unit.index())
    }

    /// `repos` (all the filter's repositories) after `preds`, computed once per key.
    async fn filter(
        &mut self,
        key: String,
        clients: &Arc<RepoClients>,
        repos: Vec<RepoRevs>,
        preds: &RepoPredicates,
        index: YesNoOnly,
        deadline: Option<std::time::Instant>,
    ) -> Arc<Filtered> {
        if let Some(f) = self.0.get(&key) {
            return f.clone();
        }
        let f = Arc::new(filter_repos(clients.clone(), repos, preds, index, deadline).await);
        self.0.insert(key, f.clone());
        f
    }
}

/// Adds skip reasons to `progress`, each reason once (parts of one query search the same
/// repositories).
fn add_skipped(progress: &mut Progress, skipped: impl IntoIterator<Item = Skipped>) {
    for s in skipped {
        if !progress.skipped.iter().any(|p| p.reason == s.reason) {
            progress.skipped.push(s);
        }
    }
}

/// Why a search left out repositories beyond `search.limits.maxRepos`.
fn repo_limit_skipped(max: usize) -> Skipped {
    Skipped {
        reason: SkippedReason::RepositoryLimit,
        title: "Repository limit".to_owned(),
        message: format!(
            "The search spans more than {max} repositories; only the first {max} were \
             searched. Use \"repo:\" to narrow it down."
        ),
        severity: Severity::Warn,
        suggested: None,
    }
}

/// The alert for a commit or diff search over more repositories than
/// [`Limits::commit_diff_max_repos`] allows (`RepoLimitError`).
fn repo_limit_alert(query: &str, diff: bool, max: usize, time_filtered: bool) -> Event {
    let kind = if diff { "diff" } else { "commit" };
    let title_kind = if diff { "Diff" } else { "Commit" };
    let mut proposed_queries = Vec::new();
    if !time_filtered {
        proposed_queries.push(ProposedQuery {
            description: "Search commits from the last week".to_owned(),
            query: format!("{query} after:\"1 week ago\""),
        });
    }
    Event::Alert(Alert {
        title: format!("Too many matching repositories for {kind} search to handle"),
        description: format!(
            "{title_kind} search can currently only handle searching across {max} repositories \
             at a time. Try using the \"repo:\" filter to narrow down which repositories to \
             search, or using 'after:\"1 week ago\"'."
        ),
        proposed_queries,
    })
}

fn alert(title: &str, description: String) -> Event {
    Event::Alert(Alert {
        title: title.to_owned(),
        description,
        proposed_queries: Vec::new(),
    })
}

/// What a file search's pattern matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Content,
    Path,
    Symbol,
}

/// What a plan asks for besides its jobs.
#[derive(Default)]
struct PlanOptions {
    timeout: Option<Duration>,
    count: Option<usize>,
    select: Option<SelectPath>,
    /// Parts of the query no backend runs yet.
    unsupported: Vec<&'static str>,
}

/// Collects the runnable jobs of a plan and its options.
fn units(
    job: &Job,
    max_line_len: u32,
    out: &mut Vec<Unit>,
    opts: &mut PlanOptions,
) -> Result<(), String> {
    // The pattern matches contents, paths or symbol names.
    let mut push = |pattern: Option<&PatternInfo>,
                    target: Target,
                    repos: &RepoFilter,
                    files: &FileFilter,
                    index: YesNoOnly| {
        let spec = pattern.map(PatternSpec::from_pattern).transpose();
        let spec = spec.map_err(|e| e.to_string())?;
        let (content, path_pattern) = match target {
            Target::Path => (None, spec),
            Target::Content | Target::Symbol => (spec, None),
        };
        let (owner_terms, contributor_terms) = owner_terms(files)?;
        out.push(Unit::Files {
            owner_terms,
            contributor_terms,
            search: Box::new(ContentSearch {
                pattern: content,
                path_pattern,
                case_sensitive: pattern.is_some_and(|p| p.case_sensitive),
                paths: files.clone(),
                limit: usize::MAX,
                max_line_len,
                concurrency: REPO_CONCURRENCY,
                commits: None,
                diff: false,
                index,
                symbols: target == Target::Symbol,
                bm25: pattern.is_some_and(|p| p.pattern_type == PatternType::Keyword),
                select: None,
                diff_lines: None,
                owners: None,
                deadline: None,
            }),
            repos: repos.clone(),
        });
        Ok::<(), String>(())
    };
    match job {
        Job::Timeout { timeout: t, child } => {
            opts.timeout = Some(*t);
            units(child, max_line_len, out, opts)
        }
        Job::Limit { count: c, child } => {
            opts.count = *c;
            units(child, max_line_len, out, opts)
        }
        Job::Parallel(jobs) => jobs
            .iter()
            .try_for_each(|j| units(j, max_line_len, out, opts)),
        Job::Content {
            pattern,
            repos,
            files,
            index,
        } => push(Some(pattern), Target::Content, repos, files, *index),
        Job::Path {
            pattern,
            repos,
            files,
            index,
        } => push(pattern.as_ref(), Target::Path, repos, files, *index),
        Job::Symbol {
            pattern,
            repos,
            files,
            index,
        } => push(pattern.as_ref(), Target::Symbol, repos, files, *index),
        Job::Repo { pattern, repos } => {
            let spec = pattern.as_ref().map(PatternSpec::from_pattern).transpose();
            out.push(Unit::Repos {
                pattern: spec.map_err(|e| e.to_string())?,
                case_sensitive: pattern.as_ref().is_some_and(|p| p.case_sensitive),
                repos: repos.clone(),
            });
            Ok(())
        }
        Job::Commit {
            pattern,
            repos,
            files,
            filter,
            diff,
        } => {
            let spec = pattern.as_ref().map(PatternSpec::from_pattern).transpose();
            let (owner_terms, contributor_terms) = owner_terms(files)?;
            if !owner_terms.is_empty() {
                opts.unsupported
                    .push("`file:has.owner()` in commit and diff searches");
            }
            if !contributor_terms.is_empty() {
                opts.unsupported
                    .push("`file:has.contributor()` in commit and diff searches");
            }
            out.push(Unit::Files {
                owner_terms: Vec::new(),
                contributor_terms: Vec::new(),
                search: Box::new(ContentSearch {
                    pattern: spec.map_err(|e| e.to_string())?,
                    path_pattern: None,
                    case_sensitive: pattern.as_ref().is_some_and(|p| p.case_sensitive),
                    paths: files.clone(),
                    limit: usize::MAX,
                    max_line_len,
                    concurrency: REPO_CONCURRENCY,
                    commits: Some(filter.into()),
                    diff: *diff,
                    index: YesNoOnly::No,
                    symbols: false,
                    bm25: false,
                    select: None,
                    diff_lines: None,
                    owners: None,
                    deadline: None,
                }),
                repos: repos.clone(),
            });
            Ok(())
        }
        Job::Select { path, child } => {
            match SelectPath::parse(path) {
                Some(p) => opts.select = Some(p),
                None => opts.unsupported.push("this `select:` value"),
            }
            units(child, max_line_len, out, opts)
        }
    }
}

/// The `file:has.owner()` and `file:has.contributor()` terms of a file filter. A
/// contributor pattern is a regular expression, matched case-insensitively against the
/// names and emails of the authors of the commits that changed a file.
#[allow(clippy::type_complexity)]
fn owner_terms(
    files: &FileFilter,
) -> Result<(Vec<(bool, Option<String>)>, Vec<ContributorTerm>), String> {
    let mut owners = Vec::new();
    let mut contributors = Vec::new();
    for (negated, p) in &files.predicates {
        match p {
            Predicate::FileHasOwner(arg) => owners.push((*negated, arg.clone())),
            Predicate::FileHasContributor(pattern) => {
                regex::RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| {
                        format!("invalid regular expression in file:has.contributor(): {e}")
                    })?;
                contributors.push(ContributorTerm {
                    negated: *negated,
                    pattern: pattern.clone(),
                });
            }
            _ => {}
        }
    }
    Ok((owners, contributors))
}

/// Resolves the owner terms of a search and loads the ingested CODEOWNERS files and
/// assignments of `repos`. Term keys are cached per request.
async fn owner_search(
    ownership: &dyn Ownership,
    terms: &[(bool, Option<String>)],
    report: bool,
    repos: &[i32],
    keys: &mut HashMap<String, std::collections::HashSet<String>>,
) -> Result<OwnerSearch, sourcegull_db::DbError> {
    let mut out = OwnerSearch {
        report,
        ..OwnerSearch::default()
    };
    for (negated, arg) in terms {
        let term_keys = match arg {
            None => None,
            Some(a) => Some(match keys.get(a) {
                Some(k) => k.clone(),
                None => {
                    let k = ownership.term_keys(a).await?;
                    keys.insert(a.clone(), k.clone());
                    k
                }
            }),
        };
        out.terms.push(OwnerTerm {
            negated: *negated,
            keys: term_keys,
        });
    }
    out.repos = ownership.repo_owner_data(repos).await?;
    Ok(out)
}

/// Resolves the owner results of `select:file.owners` to users and teams before they are
/// sent; owners reached through several references (a GitLab handle and an email) are sent
/// once.
fn resolve_owner_events(
    ownership: Arc<dyn Ownership>,
    out: tokio::sync::mpsc::Sender<Event>,
) -> tokio::sync::mpsc::Sender<Event> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        while let Some(event) = rx.recv().await {
            let event = match event {
                Event::Matches(ms) => {
                    let texts: Vec<String> = ms.iter().filter_map(owner_text).collect();
                    let resolved = match ownership.resolve(&texts).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!(error = %e, "resolving owners");
                            HashMap::new()
                        }
                    };
                    let ms: Vec<Match> = ms
                        .into_iter()
                        .filter_map(|m| {
                            let Some(text) = owner_text(&m) else {
                                return Some(m);
                            };
                            let owner = resolved.get(&text)?;
                            seen.insert(owner.identity.clone())
                                .then(|| owner_match(&m, &owner.owner))
                        })
                        .collect();
                    if ms.is_empty() {
                        continue;
                    }
                    Event::Matches(ms)
                }
                other => other,
            };
            if out.send(event).await.is_err() {
                return;
            }
        }
    });
    tx
}

/// The reference an unresolved owner result names (`@handle` or an email).
fn owner_text(m: &Match) -> Option<String> {
    match m {
        Match::Person { handle, email, .. } if !handle.is_empty() => Some(format!("@{handle}")),
        Match::Person { email, .. } => Some(email.clone()),
        _ => None,
    }
}

/// The streamed form of a resolved owner (`person` and `team` matches).
fn owner_match(m: &Match, owner: &Owner) -> Match {
    let (handle, email) = match m {
        Match::Person { handle, email, .. } => (handle.clone(), email.clone()),
        _ => (String::new(), String::new()),
    };
    match owner {
        Owner::User(u) => Match::Person {
            handle: if handle.is_empty() {
                u.username.clone()
            } else {
                handle
            },
            email,
            user: Some(u.clone()),
        },
        Owner::Team { name, display_name } => Match::Team {
            handle: name.clone(),
            email,
            name: name.clone(),
            display_name: display_name.clone(),
        },
        Owner::ExternalTeam { handle } => Match::Team {
            handle: handle.clone(),
            email,
            name: String::new(),
            display_name: String::new(),
        },
        Owner::Person => Match::Person {
            handle,
            email,
            user: None,
        },
    }
}

/// The alert for parts of a query that are not supported.
fn unsupported_alert(parts: &[&str]) -> Event {
    alert(
        "Unsupported Query",
        format!("{} is not supported yet.", parts.join(" and ")),
    )
}

/// Repositories passing the filters whose name matches `name`.
fn repo_matches(
    catalog: &RepoCatalog,
    filter: &RepoMatcher,
    name: Option<&Matcher>,
    limit: usize,
) -> Vec<Match> {
    catalog
        .repos()
        .iter()
        .filter(|r| filter.matches(r))
        .filter(|r| name.is_none_or(|m| m.is_match(r.name.as_bytes())))
        .take(limit)
        .map(|r| Match::Repo {
            repository_id: r.id,
            repository: r.name.clone(),
            description: (!r.description.is_empty()).then(|| r.description.clone()),
            fork: r.fork,
            archived: r.archived,
            private: r.private,
        })
        .collect()
}

/// [`repo_matches`] for a filter with repository predicates: candidates are checked page by
/// page until `limit` pass, or taken from `known` (the filter's whole result, when another
/// part of the query computed it).
#[allow(clippy::too_many_arguments)]
async fn repo_matches_checked(
    clients: &Arc<RepoClients>,
    catalog: &RepoCatalog,
    filter: &RepoMatcher,
    name: Option<&Matcher>,
    limit: usize,
    preds: &RepoPredicates,
    known: Option<Arc<Filtered>>,
    deadline: Option<std::time::Instant>,
) -> (Vec<Match>, Vec<Skipped>) {
    let to_match = |id: i32| {
        catalog.get(id).map(|r| Match::Repo {
            repository_id: r.id,
            repository: r.name.clone(),
            description: (!r.description.is_empty()).then(|| r.description.clone()),
            fork: r.fork,
            archived: r.archived,
            private: r.private,
        })
    };
    let named = |r: &RepoRevs| name.is_none_or(|m| m.is_match(r.name.as_bytes()));
    if let Some(f) = known {
        let matches = f
            .repos
            .iter()
            .filter(|r| named(r))
            .take(limit)
            .filter_map(|r| to_match(r.id))
            .collect();
        return (matches, f.skipped.clone());
    }
    let pages: Vec<Vec<RepoRevs>> = filter.pages(catalog, DEFAULT_PAGE_SIZE).collect();
    let mut matches = Vec::new();
    let mut skipped = Vec::new();
    for page in pages {
        if matches.len() >= limit {
            break;
        }
        let page: Vec<RepoRevs> = page.into_iter().filter(|r| named(r)).collect();
        let f = filter_repos(clients.clone(), page, preds, YesNoOnly::Yes, deadline).await;
        skipped.extend(f.skipped);
        let room = limit - matches.len();
        matches.extend(f.repos.iter().take(room).filter_map(|r| to_match(r.id)));
    }
    (matches, skipped)
}

/// How long a loaded catalog serves searches before it is reloaded.
const CATALOG_TTL: Duration = Duration::from_secs(5);

/// How old a catalog may be and still serve a search while a reload runs in
/// the background; an older one is reloaded before the search goes on.
const CATALOG_STALE: Duration = Duration::from_secs(30);

/// The repo catalog, reloaded from the repo table at most every
/// [`CATALOG_TTL`]. Concurrent searches share one reload. Once the TTL has
/// passed, searches keep using the loaded catalog (up to [`CATALOG_STALE`])
/// while one reload runs in the background: reading many thousands of repositories
/// takes tens of milliseconds, too long for a search to wait for its first event.
#[derive(Debug, Default)]
pub struct CatalogCache {
    loaded: std::sync::Mutex<Option<(std::time::Instant, Arc<RepoCatalog>)>>,
    /// Held while the catalog is reloaded.
    reload: Arc<tokio::sync::Mutex<()>>,
}

impl CatalogCache {
    /// The catalog, reloaded when older than [`CATALOG_TTL`].
    pub async fn get(self: &Arc<Self>, db: &Db) -> Result<Arc<RepoCatalog>, String> {
        let current = self.current();
        if let Some((at, catalog)) = &current {
            if at.elapsed() < CATALOG_TTL {
                return Ok(catalog.clone());
            }
            if at.elapsed() < CATALOG_STALE {
                // One background reload at a time; this search goes on with
                // the catalog it has.
                if let Ok(guard) = self.reload.clone().try_lock_owned() {
                    let (cache, db) = (self.clone(), db.clone());
                    tokio::spawn(async move {
                        let _guard = guard;
                        if cache
                            .current()
                            .is_some_and(|(at, _)| at.elapsed() < CATALOG_TTL)
                        {
                            return;
                        }
                        match load_catalog(&db).await {
                            Ok(c) => cache.store(Arc::new(c)),
                            Err(e) => tracing::warn!(error = %e, "reloading the repo catalog"),
                        }
                    });
                }
                return Ok(catalog.clone());
            }
        }
        let _guard = self.reload.lock().await;
        // Another search may have reloaded it while this one waited.
        if let Some((at, catalog)) = self.current()
            && at.elapsed() < CATALOG_TTL
        {
            return Ok(catalog);
        }
        let catalog = Arc::new(load_catalog(db).await?);
        self.store(catalog.clone());
        Ok(catalog)
    }

    fn current(&self) -> Option<(std::time::Instant, Arc<RepoCatalog>)> {
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store(&self, catalog: Arc<RepoCatalog>) {
        *self.loaded.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), catalog));
    }
}

async fn load_catalog(db: &Db) -> Result<RepoCatalog, String> {
    let mut kvps: HashMap<i32, Vec<(String, Option<String>)>> = HashMap::new();
    for (id, key, value) in db.repos().all_kvps().await.map_err(|e| e.to_string())? {
        kvps.entry(id.get()).or_default().push((key, value));
    }
    let mut all = Vec::new();
    let mut opts = ListReposOptions {
        limit: 10_000,
        ..ListReposOptions::default()
    };
    loop {
        // The shared catalog holds every repository; each search filters it with the viewer's
        // RepoAccess (RepoMatcher::new requires one).
        #[allow(clippy::disallowed_methods)]
        let page = db.repos().list(&opts).await.map_err(|e| e.to_string())?;
        let n = page.len();
        all.extend(page.into_iter().map(|r| CatalogRepo {
            id: r.id.get(),
            name: r.name,
            description: r.description,
            fork: r.fork,
            archived: r.archived,
            private: r.private,
            stars: r.stars,
            topics: r.topics,
            kvps: kvps.remove(&r.id.get()).unwrap_or_default(),
            blocked: r.blocked.is_some(),
        }));
        if (n as i64) < opts.limit {
            break;
        }
        opts.offset += opts.limit;
    }
    Ok(RepoCatalog::new(all))
}

/// What a query's `context:` resolved to.
pub enum ResolvedContext {
    /// No context, or `global`.
    Global,
    /// A query-defined context's query.
    Query(String),
    Listed(ContextScope),
}

/// Resolves the `context:` of `query` as `viewer` sees contexts; `Ok(None)`
/// when the context does not exist or the viewer may not read it.
pub async fn resolve_context(
    db: &Db,
    query: &str,
    pattern_type: PatternType,
    viewer: Option<UserId>,
) -> Result<Option<ResolvedContext>, sourcegull_db::DbError> {
    let Some(spec) = context_of(query, pattern_type) else {
        return Ok(Some(ResolvedContext::Global));
    };
    if matches!(ContextSpec::parse(&spec), ContextSpec::Global) {
        return Ok(Some(ResolvedContext::Global));
    }
    let store = db.search_contexts();
    let Some(context) = store
        .get_by_spec(&spec, ContextAccess::Viewer(viewer))
        .await?
    else {
        return Ok(None);
    };
    if let Some(q) = context.query.filter(|q| !q.trim().is_empty()) {
        return Ok(Some(ResolvedContext::Query(q)));
    }
    let repos = store.repos(context.id).await?;
    Ok(Some(ResolvedContext::Listed(ContextScope::listed(
        spec,
        repos.into_iter().map(|r| (r.repo_id.get(), r.revisions)),
    ))))
}

/// What running a search needs besides the query: the repo table (catalog and contexts),
/// repository permissions, the repo nodes, the site config (`search.limits`) and code
/// ownership. The web role builds it from its shared state (sharing one catalog cache);
/// roles without one (the worker's bulk changes workspace resolution) build their own with
/// [`SearchBackend::new`].
#[derive(Debug, Clone)]
pub struct SearchBackend {
    pub db: Db,
    pub authz: sourcegull_authz::Checker,
    pub repo_clients: Arc<RepoClients>,
    pub catalog: Arc<CatalogCache>,
    pub config: SiteConfigHandle,
    pub ownership: Arc<dyn Ownership>,
}

impl SearchBackend {
    /// A backend with its own repo catalog cache, reading the site config of `authz`.
    pub fn new(
        db: Db,
        authz: sourcegull_authz::Checker,
        repo_clients: Arc<RepoClients>,
        ownership: Arc<dyn Ownership>,
    ) -> Self {
        Self {
            config: authz.config().clone(),
            db,
            authz,
            repo_clients,
            catalog: Arc::default(),
            ownership,
        }
    }
}

/// A query resolved and planned for a viewer, ready to run.
struct Prepared {
    list: Vec<Unit>,
    timeout: Option<Duration>,
    count: Option<usize>,
    select: Option<SelectPath>,
    scope: ContextScope,
    access: RepoAccess,
    catalog: Arc<RepoCatalog>,
    limits: Limits,
}

/// Resolves the viewer's permissions and the query's `context:`, plans the query and loads
/// the repo catalog. Errors are the event to send instead of results.
async fn prepare(
    state: &SearchBackend,
    params: &StreamParams,
    actor: Option<&Actor>,
) -> Result<Prepared, Event> {
    let viewer = actor.map(|a| a.user_id);
    let access: RepoAccess = match state.authz.access(actor).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "resolving repository permissions");
            return Err(Event::Error("internal error".to_owned()));
        }
    };
    let pattern_type = params
        .t
        .as_deref()
        .and_then(PatternType::from_name)
        .unwrap_or_default();
    let limits = Limits::from_site(&state.config);
    let mut query = params.q.clone();
    let mut scope = ContextScope::Global;
    match resolve_context(&state.db, &query, pattern_type, viewer).await {
        Ok(Some(ResolvedContext::Global)) => {}
        Ok(Some(ResolvedContext::Query(q))) => {
            query = substitute_context(&query, pattern_type, &q);
        }
        Ok(Some(ResolvedContext::Listed(s))) => scope = s,
        Ok(None) => {
            let spec = context_of(&query, pattern_type).unwrap_or_default();
            return Err(alert(
                "Search context not found",
                format!("The search context {spec:?} does not exist or you may not use it."),
            ));
        }
        Err(e) => {
            tracing::error!(error = %e, "resolving a search context");
            return Err(Event::Error("internal error".to_owned()));
        }
    }
    let now = chrono::Utc::now();
    let planned = if params.run.exhaustive {
        sourcegull_search::exhaustive::plan_exhaustive(&query, pattern_type, &limits, now)
            .map_err(|e| e.to_string())
    } else {
        plan_query(&query, pattern_type, &limits, now).map_err(|e| e.to_string())
    };
    let job = planned.map_err(|e| alert("Unable To Process Query", e))?;
    let mut list = Vec::new();
    let mut opts = PlanOptions::default();
    if let Err(e) = units(&job, params.max_line_len.unwrap_or(0), &mut list, &mut opts) {
        return Err(alert("Unable To Process Query", e));
    }
    let PlanOptions {
        timeout,
        count,
        select,
        mut unsupported,
    } = opts;
    unsupported.sort();
    unsupported.dedup();
    if !unsupported.is_empty() {
        // An unsupported filter or projection would change every result: run nothing.
        return Err(unsupported_alert(&unsupported));
    }
    if list.is_empty() {
        return Err(alert(
            "Unsupported Query",
            "This kind of search is not supported yet.".to_owned(),
        ));
    }
    for unit in &list {
        RepoPredicates::compile(&unit.repos().predicates, now)
            .map_err(|e| alert("Unable To Process Query", e))?;
    }
    let catalog = match state.catalog.get(&state.db).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "loading repositories for search");
            return Err(Event::Error("internal error".to_owned()));
        }
    };
    Ok(Prepared {
        list,
        timeout,
        count,
        select,
        scope,
        access,
        catalog,
        limits,
    })
}

/// The repositories `query` searches for `actor`, with their revision specifiers (empty: the
/// default branch), resolved as the stream API resolves them. Search Jobs splits a job into
/// them. Errors are the alert or error a search would show.
pub async fn resolve_repositories(
    backend: &SearchBackend,
    actor: Option<&Actor>,
    query: &str,
    pattern_type: Option<&str>,
    run: RunOptions,
) -> Result<Vec<RepoRevs>, String> {
    let params = StreamParams {
        q: query.to_owned(),
        t: pattern_type.map(str::to_owned),
        display: None,
        max_line_len: None,
        cm: None,
        run,
    };
    let p = prepare(backend, &params, actor)
        .await
        .map_err(|e| match e {
            Event::Alert(a) => format!("{}: {}", a.title, a.description),
            Event::Error(e) => e,
            other => other.data(),
        })?;
    let mut out: Vec<RepoRevs> = Vec::new();
    let mut at = std::collections::HashMap::new();
    for unit in &p.list {
        let Unit::Files { repos, .. } = unit else {
            continue;
        };
        let matcher = RepoMatcher::new(repos, &p.scope, p.access.clone())
            .map_err(|e| format!("Invalid Repository Filter: {e}"))?;
        let mut resolved: Vec<RepoRevs> = matcher
            .pages(&p.catalog, DEFAULT_PAGE_SIZE)
            .flatten()
            .collect();
        let preds = RepoPredicates::compile(matcher.deferred(), chrono::Utc::now())
            .map_err(|e| format!("Unable To Process Query: {e}"))?;
        if !preds.is_empty() {
            let f = filter_repos(
                backend.repo_clients.clone(),
                resolved,
                &preds,
                unit.index(),
                None,
            )
            .await;
            resolved = f.repos;
        }
        for repo in resolved {
            match at.get(&repo.id) {
                Some(&i) => {
                    let known: &mut RepoRevs = &mut out[i];
                    for rev in repo.revs {
                        if !known.revs.contains(&rev) {
                            known.revs.push(rev);
                        }
                    }
                }
                None => {
                    at.insert(repo.id, out.len());
                    out.push(repo);
                }
            }
        }
    }
    Ok(out)
}

/// Runs a search for `actor` and sends its events to `tx`: matches, alerts, errors and the
/// final progress (the stream API adds filters and `done`). Repository permissions apply;
/// the caller checks that `actor` may search at all. The search stops when `tx` is closed.
pub async fn run(
    state: SearchBackend,
    params: StreamParams,
    actor: Option<Actor>,
    tx: tokio::sync::mpsc::Sender<Event>,
) {
    let started = std::time::Instant::now();
    let Prepared {
        mut list,
        timeout,
        count,
        select,
        scope,
        access,
        catalog,
        limits,
    } = match prepare(&state, &params, actor.as_ref()).await {
        Ok(p) => p,
        Err(event) => {
            let _ = tx.send(event).await;
            return;
        }
    };
    let deadline = timeout.map(|t| started + t);
    let report_owners = select == Some(SelectPath::FileOwners);
    let tx = if report_owners {
        resolve_owner_events(state.ownership.clone(), tx)
    } else {
        tx
    };
    let mut owner_keys = HashMap::new();
    let selector = select.map(|p| {
        let diff = match p {
            SelectPath::CommitDiff(sign) => Some(sign),
            _ => None,
        };
        (
            Arc::new(std::sync::Mutex::new(Selector::new(p, catalog.clone()))),
            diff,
        )
    });
    if let Some((sel, diff)) = &selector {
        for unit in &mut list {
            if let Unit::Files { search, .. } = unit {
                search.select = Some(sel.clone());
                search.diff_lines = *diff;
            }
        }
    }
    let mut limit = count.unwrap_or(usize::MAX);
    if let Some(d) = params.display {
        limit = limit.min(d);
    }
    let mut progress = Progress::default();
    let mut searched = std::collections::HashSet::new();
    let mut predicate_cache = PredicateCache::default();
    let work = async {
        for unit in list {
            let remaining = limit.saturating_sub(progress.match_count as usize);
            if remaining == 0 {
                break;
            }
            let matcher = match RepoMatcher::new(unit.repos(), &scope, access.clone()) {
                Ok(m) => m,
                Err(e) => {
                    let _ = tx
                        .send(alert("Invalid Repository Filter", e.to_string()))
                        .await;
                    continue;
                }
            };
            // Validated when the query was prepared.
            let preds = match RepoPredicates::compile(matcher.deferred(), chrono::Utc::now()) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(alert("Unable To Process Query", e)).await;
                    continue;
                }
            };
            let cache_key = PredicateCache::key(&unit);
            let index = unit.index();
            match unit {
                // Repository names are not results of a one-repository search.
                Unit::Repos { .. } if params.run.only.is_some() => {}
                Unit::Repos {
                    pattern,
                    case_sensitive,
                    ..
                } => {
                    let name_matcher = match pattern
                        .map(|p| Matcher::from_spec(&p, case_sensitive))
                        .transpose()
                    {
                        Ok(m) => m,
                        Err(e) => {
                            let _ = tx
                                .send(alert("Unable To Process Query", e.to_string()))
                                .await;
                            continue;
                        }
                    };
                    let mut matches = if preds.is_empty() {
                        repo_matches(&catalog, &matcher, name_matcher.as_ref(), remaining)
                    } else {
                        let (matches, skipped) = repo_matches_checked(
                            &state.repo_clients,
                            &catalog,
                            &matcher,
                            name_matcher.as_ref(),
                            remaining,
                            &preds,
                            predicate_cache.0.get(&cache_key).cloned(),
                            deadline,
                        )
                        .await;
                        add_skipped(&mut progress, skipped);
                        matches
                    };
                    if let Some((sel, _)) = &selector {
                        let mut sel = sel.lock().unwrap_or_else(|e| e.into_inner());
                        matches = sel.apply(matches);
                    }
                    progress.match_count += matches.len() as u64;
                    if !matches.is_empty() {
                        let _ = tx.send(Event::Matches(matches)).await;
                    }
                }
                Unit::Files {
                    search,
                    owner_terms,
                    contributor_terms,
                    ..
                } => {
                    let mut repos: Vec<_> = matcher
                        .pages(&catalog, DEFAULT_PAGE_SIZE)
                        .flatten()
                        .collect();
                    if let Some((id, rev)) = &params.run.only {
                        repos.retain(|r| r.id == *id);
                        for r in &mut repos {
                            r.revs = match rev.as_str() {
                                "" | "HEAD" => Vec::new(),
                                rev => vec![rev.to_owned()],
                            };
                        }
                        // The revision was resolved through `rev:at.time()` already.
                        let preds = preds.without_at_time();
                        if !preds.is_empty() {
                            let f = filter_repos(
                                state.repo_clients.clone(),
                                repos,
                                &preds,
                                index,
                                deadline,
                            )
                            .await;
                            add_skipped(&mut progress, f.skipped);
                            repos = f.repos;
                        }
                    } else if !preds.is_empty() {
                        let f = predicate_cache
                            .filter(
                                cache_key,
                                &state.repo_clients,
                                repos,
                                &preds,
                                index,
                                deadline,
                            )
                            .await;
                        add_skipped(&mut progress, f.skipped.iter().cloned());
                        repos = f.repos.clone();
                    }
                    if let Some(max) = limits.max_repos.filter(|&m| repos.len() > m) {
                        repos.truncate(max);
                        add_skipped(&mut progress, [repo_limit_skipped(max)]);
                    }
                    if let Some(f) = search.commits.as_ref().filter(|_| !params.run.exhaustive) {
                        let time_filtered = f.before.is_some() || f.after.is_some();
                        let max = if time_filtered {
                            limits.commit_diff_with_time_filter_max_repos
                        } else {
                            limits.commit_diff_max_repos
                        };
                        if repos.len() > max {
                            let a = repo_limit_alert(&params.q, search.diff, max, time_filtered);
                            let _ = tx.send(a).await;
                            continue;
                        }
                    }
                    searched.extend(repos.iter().map(|r| r.id));
                    let owners = if !owner_terms.is_empty()
                        || !contributor_terms.is_empty()
                        || report_owners
                    {
                        let ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
                        match owner_search(
                            &*state.ownership,
                            &owner_terms,
                            report_owners,
                            &ids,
                            &mut owner_keys,
                        )
                        .await
                        {
                            Ok(o) => Some(Arc::new(OwnerSearch {
                                contributors: contributor_terms,
                                ..o
                            })),
                            Err(e) => {
                                tracing::error!(error = %e, "loading code ownership for search");
                                let _ = tx.send(Event::Error("internal error".to_owned())).await;
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    let search = ContentSearch {
                        limit: remaining,
                        owners,
                        deadline,
                        ..*search
                    };
                    let p =
                        search_repos(state.repo_clients.clone(), repos, search, tx.clone()).await;
                    progress.match_count += p.match_count;
                    // Parts of one query search the same repos: report each reason once.
                    add_skipped(&mut progress, p.skipped);
                }
            }
        }
    };
    let timed_out = match timeout {
        Some(t) => tokio::time::timeout(t, work).await.is_err(),
        None => {
            work.await;
            false
        }
    };
    if timed_out {
        let _ = tx
            .send(alert(
                "Timed out",
                "The search timed out before finishing. Try `timeout:` or narrow the query."
                    .to_owned(),
            ))
            .await;
        progress.skipped.push(Skipped {
            reason: SkippedReason::ShardTimeout,
            title: "Timed out".to_owned(),
            message: "The search timed out before finishing.".to_owned(),
            severity: Severity::Warn,
            suggested: Some(SkippedSuggested {
                title: "increase timeout".to_owned(),
                query_expression: "timeout:2m".to_owned(),
            }),
        });
    }
    progress.done = true;
    progress.repositories_count = Some(u32::try_from(searched.len()).unwrap_or(u32::MAX));
    progress.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let _ = tx.send(Event::Progress(progress)).await;
}

/// A search run to the end ([`collect`]).
#[derive(Debug, Default)]
pub struct SearchOutcome {
    pub matches: Vec<Match>,
    pub alerts: Vec<Alert>,
    /// The final progress; `None` when the query was rejected.
    pub progress: Option<Progress>,
    pub error: Option<String>,
}

impl SearchOutcome {
    /// The search stopped at its timeout.
    pub fn timed_out(&self) -> bool {
        self.skipped(SkippedReason::ShardTimeout)
    }

    /// The result limit cut the search short.
    pub fn limit_hit(&self) -> bool {
        self.skipped(SkippedReason::DisplayLimit)
    }

    fn skipped(&self, reason: SkippedReason) -> bool {
        self.progress
            .as_ref()
            .is_some_and(|p| p.skipped.iter().any(|s| s.reason == reason))
    }
}

/// Runs a search as the stream API does for `actor` and collects its
/// events, keeping at most `max` matches (aggregations). Repository
/// permissions apply as in the stream API; the caller checks that `actor`
/// may search at all.
pub async fn collect(
    backend: &SearchBackend,
    actor: Option<&Actor>,
    query: &str,
    pattern_type: Option<&str>,
    max: usize,
) -> SearchOutcome {
    let params = StreamParams {
        q: query.to_owned(),
        t: pattern_type.map(str::to_owned),
        display: Some(max),
        max_line_len: None,
        cm: None,
        run: RunOptions::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    tokio::spawn(run(backend.clone(), params, actor.cloned(), tx));
    let mut out = SearchOutcome::default();
    while let Some(event) = rx.recv().await {
        match event {
            Event::Matches(m) => out.matches.extend(m),
            Event::Alert(a) => out.alerts.push(a),
            Event::Progress(p) => out.progress = Some(p),
            Event::Error(e) => out.error = Some(e),
            Event::Filters(_) | Event::Done => {}
        }
    }
    out
}

/// Starts a search as the stream API runs it for `actor` and returns its events as they
/// arrive (matches, alerts, the final progress, errors), for callers that consume results
/// without holding them all (Search Jobs). `limit` caps the results. The search stops
/// when the receiver is dropped or the returned handle is aborted.
pub fn spawn_search(
    backend: &SearchBackend,
    actor: Option<&Actor>,
    query: &str,
    pattern_type: Option<&str>,
    limit: Option<usize>,
    run_options: RunOptions,
) -> (tokio::sync::mpsc::Receiver<Event>, tokio::task::AbortHandle) {
    let params = StreamParams {
        q: query.to_owned(),
        t: pattern_type.map(str::to_owned),
        display: limit,
        max_line_len: None,
        cm: None,
        run: run_options,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(16);
    let handle = tokio::spawn(run(backend.clone(), params, actor.cloned(), tx));
    (rx, handle.abort_handle())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes the loaded catalog `age` old.
    fn backdate(cache: &CatalogCache, age: Duration) {
        let mut loaded = cache.loaded.lock().unwrap();
        let (at, _) = loaded.as_mut().unwrap();
        *at = std::time::Instant::now() - age;
    }

    #[tokio::test]
    async fn an_expired_catalog_serves_while_it_reloads() {
        let Some(t) = sourcegull_testutil::TestDb::new().await else {
            eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
            return;
        };
        sourcegull_db::migrate::up(&t.pool).await.unwrap();
        let db = Db::from_pool(t.pool.clone());
        let cache = Arc::new(CatalogCache::default());
        let first = cache.get(&db).await.unwrap();
        assert_eq!(first.len(), 0);
        #[allow(clippy::disallowed_methods)]
        sqlx::query(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
             VALUES ('gitlab.example.com/a/b', '1', 'gitlab', 'https://gitlab.example.com/')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Fresh: served as loaded.
        assert!(Arc::ptr_eq(&cache.get(&db).await.unwrap(), &first));
        // Expired: still served, while a reload runs in the background.
        backdate(&cache, CATALOG_TTL + Duration::from_secs(1));
        assert!(Arc::ptr_eq(&cache.get(&db).await.unwrap(), &first));
        let _reloaded = cache.reload.lock().await;
        assert_eq!(cache.current().unwrap().1.len(), 1);
        drop(_reloaded);
        assert_eq!(cache.get(&db).await.unwrap().len(), 1);
        // Too old to serve: reloaded before the search goes on.
        #[allow(clippy::disallowed_methods)]
        sqlx::query("DELETE FROM repo")
            .execute(db.pool())
            .await
            .unwrap();
        backdate(&cache, CATALOG_STALE);
        assert_eq!(cache.get(&db).await.unwrap().len(), 0);
    }
}
