//! The [`Resolver`]: a bulk change spec to its workspaces (see the crate docs for the rules).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use sourcegull_authn::{Actor, AuthVia, PermissionSet};
use sourcegull_batches::BatchSpec;
use sourcegull_batches::glob::Glob;
use sourcegull_batches::spec::{ExternalId, OnQueryOrRepository};
use sourcegull_batches::store::{
    BatchSpecStore, BatchSpecWorkspaceStore, ChangesetSpecType, ExecutionCacheStore,
    NewChangesetSpec, NewWorkspace,
};
use sourcegull_db::{Db, DbError, ListReposOptions};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search::stream::Match;
use sourcegull_search_web::{SearchBackend, SearchOutcome, collect};
use sourcegull_services::git::{GitCallError, GitReader};
use sourcegull_types::{Repo, RepoAccess, RepoId, UserId};

use crate::cache::{ResolutionCache, step_cache_key};

/// `skipped_reason` of a workspace in an archived repository.
pub const ARCHIVED_REASON: &str = "The repository is archived.";
/// `skipped_reason` of a workspace on a code host bulk changes do not support.
pub const UNSUPPORTED_REASON: &str =
    "The repository is on a code host Batch Changes does not support.";
/// `skipped_reason` of a workspace in a repository with a `.batchignore` file.
pub const IGNORED_REASON: &str = "The repository has a .batchignore file.";

/// The file that opts a repository out of bulk changes.
const BATCH_IGNORE: &str = ".batchignore";
/// The code host type bulk changes support (GitLab only).
const SUPPORTED_SERVICE_TYPE: &str = "gitlab";
/// `.batchignore` checks and branch lookups run at once.
const REPO_CONCURRENCY: usize = 5;
/// `rootAtLocationOf` searches run at once.
const SEARCH_CONCURRENCY: usize = 10;

/// Why a resolution failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The spec, or what it names, is wrong; resolving again will not help.
    #[error("{0}")]
    Invalid(String),
    /// A dependency failed (database, repo node, search); worth retrying.
    #[error("{0}")]
    Unavailable(String),
}

impl From<DbError> for ResolveError {
    fn from(e: DbError) -> Self {
        Self::Unavailable(format!("database: {e}"))
    }
}

/// What a resolution produced, ready for
/// [`sourcegull_batches::store::BatchSpecWorkspaceStore::replace_resolution`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    /// Sorted by repository name, path and branch.
    pub workspaces: Vec<NewWorkspace>,
    /// One `existing` changeset spec per `importChangesets` external ID.
    pub imports: Vec<NewChangesetSpec>,
}

/// A repository on a branch at a fixed commit.
#[derive(Debug, Clone)]
struct RepoRevision {
    repo: Arc<Repo>,
    /// The branch ref (`refs/heads/…`).
    branch: String,
    commit: String,
    /// Paths of the `on` query's matches in this repository, sorted.
    file_matches: Vec<String>,
}

/// Which kind of `on` entry produced a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Query,
    Explicit,
}

/// Resolves bulk change specs' workspaces. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Resolver {
    db: Db,
    search: SearchBackend,
    cache: ResolutionCache,
}

impl Resolver {
    /// A resolver searching and reading repositories through `search`.
    pub fn new(db: Db, search: SearchBackend) -> Self {
        Self {
            db,
            search,
            cache: ResolutionCache::default(),
        }
    }

    /// Resolves the spec with id `batch_spec_id` as `initiator` and stores the result
    /// (workspaces and import specs replace the earlier ones in one transaction). Returns
    /// what was stored.
    pub async fn resolve_and_store(
        &self,
        batch_spec_id: i64,
        initiator: UserId,
    ) -> Result<Resolution, ResolveError> {
        let row = BatchSpecStore::new(&self.db)
            .get(batch_spec_id)
            .await?
            .ok_or_else(|| {
                ResolveError::Invalid(format!("bulk change spec {batch_spec_id} not found"))
            })?;
        let spec = BatchSpec::parse(&row.raw_spec)
            .map_err(|e| ResolveError::Invalid(format!("invalid bulk change spec: {e}")))?;
        let options = Options {
            allow_ignored: row.allow_ignored,
            allow_unsupported: row.allow_unsupported,
            no_cache: row.no_cache,
        };
        let mut resolution = self.resolve(&spec, initiator, options).await?;
        for import in &mut resolution.imports {
            import.batch_spec_id = Some(batch_spec_id);
        }
        BatchSpecWorkspaceStore::new(&self.db)
            .replace_resolution(batch_spec_id, &resolution.workspaces, &resolution.imports)
            .await?;
        Ok(resolution)
    }

    /// Resolves `spec` as `user` without storing anything.
    pub async fn resolve(
        &self,
        spec: &BatchSpec,
        user: UserId,
        options: Options,
    ) -> Result<Resolution, ResolveError> {
        let actor = self.actor(user).await?;
        let access = self
            .search
            .authz
            .access(Some(&actor))
            .await
            .map_err(ResolveError::from)?;
        let revisions = self.determine_repositories(spec, &actor, &access).await?;
        let ignored = self.find_ignored(&revisions).await?;
        let mut workspaces = self.find_workspaces(spec, &actor, revisions).await?;
        for (w, repo) in &mut workspaces {
            w.unsupported = repo.external_service_type != SUPPORTED_SERVICE_TYPE;
            w.ignored = ignored.contains(&(w.repo_id, w.commit.clone()));
            let reason = if repo.archived {
                Some(ARCHIVED_REASON)
            } else if w.unsupported && !options.allow_unsupported {
                Some(UNSUPPORTED_REASON)
            } else if w.ignored && !options.allow_ignored {
                Some(IGNORED_REASON)
            } else {
                None
            };
            w.skipped = reason.is_some();
            w.skipped_reason = reason.map(str::to_owned);
        }
        if !options.no_cache {
            self.find_cached(spec, user, &mut workspaces).await?;
        }
        let imports = self.imports(spec, user, &access, options).await?;
        Ok(Resolution {
            workspaces: workspaces.into_iter().map(|(w, _)| w).collect(),
            imports,
        })
    }

    /// The creator as a search actor (only identity and site admin status matter here).
    async fn actor(&self, user: UserId) -> Result<Actor, ResolveError> {
        let u = self.db.users().get_by_id(user).await?.ok_or_else(|| {
            ResolveError::Invalid(
                "the user who created the bulk change spec no longer exists".into(),
            )
        })?;
        Ok(Actor {
            user_id: u.id,
            username: u.username,
            site_admin: u.site_admin,
            via: AuthVia::Session,
            permissions: PermissionSet::default(),
        })
    }

    fn git(&self, repo: &Repo) -> GitReader {
        GitReader::from_parts(self.search.repo_clients.clone(), repo.id, &repo.name)
    }

    /// The `on` entries' revisions, by precedence: a later entry replaces an earlier one's
    /// revisions of the same repository, unless the earlier one was a
    /// `repository` entry and the later one a query.
    async fn determine_repositories(
        &self,
        spec: &BatchSpec,
        actor: &Actor,
        access: &RepoAccess,
    ) -> Result<Vec<RepoRevision>, ResolveError> {
        let mut by_repo: HashMap<RepoId, (Rule, Vec<RepoRevision>)> = HashMap::new();
        let mut errors = Vec::new();
        for on in &spec.on {
            let (rule, revisions) = match on {
                OnQueryOrRepository::Query {
                    repositories_matching_query: q,
                } => (
                    Rule::Query,
                    self.resolve_query(q, spec.version, actor, access).await,
                ),
                OnQueryOrRepository::Repository(r) => (
                    Rule::Explicit,
                    self.resolve_repository(&r.repository, &r.branches(), access)
                        .await,
                ),
            };
            let revisions = match revisions {
                Ok(r) => r,
                Err(ResolveError::Invalid(e)) => {
                    errors.push(format!("resolving {:?}: {e}", on.to_string()));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut this: HashMap<RepoId, Vec<RepoRevision>> = HashMap::new();
            for rev in revisions {
                this.entry(rev.repo.id).or_default().push(rev);
            }
            for (id, revs) in this {
                if let Some((Rule::Explicit, _)) = by_repo.get(&id)
                    && rule == Rule::Query
                {
                    continue;
                }
                by_repo.insert(id, (rule, revs));
            }
        }
        if !errors.is_empty() {
            return Err(ResolveError::Invalid(errors.join("; ")));
        }
        let mut seen = HashSet::new();
        let mut out: Vec<RepoRevision> = by_repo
            .into_values()
            .flat_map(|(_, revs)| revs)
            .filter(|r| seen.insert((r.repo.id, r.branch.clone())))
            .collect();
        out.sort_by(|a, b| (&a.repo.name, &a.branch).cmp(&(&b.repo.name, &b.branch)));
        Ok(out)
    }

    /// `repositoriesMatchingQuery`: the repositories with results, on their default branch.
    async fn resolve_query(
        &self,
        query: &str,
        version: u32,
        actor: &Actor,
        access: &RepoAccess,
    ) -> Result<Vec<RepoRevision>, ResolveError> {
        let out = self
            .run_search(&with_default_count(query), version, actor)
            .await?;
        let mut ids = Vec::new();
        let mut paths: HashMap<i32, BTreeSet<String>> = HashMap::new();
        for m in &out.matches {
            let (id, path) = match m {
                Match::Repo { repository_id, .. } => (*repository_id, None),
                Match::Content {
                    repository_id,
                    path,
                    ..
                }
                | Match::Path {
                    repository_id,
                    path,
                    ..
                }
                | Match::Symbol {
                    repository_id,
                    path,
                    ..
                } => (*repository_id, Some(path)),
                // Commit and diff matches name no workspace.
                _ => continue,
            };
            let entry = paths.entry(id).or_insert_with(|| {
                ids.push(RepoId(id));
                BTreeSet::new()
            });
            if let Some(p) = path {
                entry.insert(p.clone());
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Search already applied the viewer's permissions; the repo table checks again.
        #[allow(clippy::disallowed_methods)]
        let repos = self
            .db
            .repos()
            .list(&ListReposOptions {
                ids: Some(ids.clone()),
                access: Some(access.clone()),
                limit: ids.len() as i64,
                ..ListReposOptions::default()
            })
            .await?;
        futures::stream::iter(repos)
            .map(|repo| {
                let matches = paths
                    .get(&repo.id.get())
                    .map(|p| p.iter().cloned().collect())
                    .unwrap_or_default();
                async move {
                    match self.default_branch(&repo).await {
                        Ok(Some((branch, commit))) => Ok(Some(RepoRevision {
                            repo: Arc::new(repo),
                            branch,
                            commit,
                            file_matches: matches,
                        })),
                        // Empty, or not cloned yet: nothing to change.
                        Ok(None)
                        | Err(GitCallError::RepoNotCloned | GitCallError::CloneInProgress) => {
                            Ok(None)
                        }
                        Err(e) => Err(ResolveError::Unavailable(format!(
                            "resolving the default branch of {}: {e}",
                            repo.name
                        ))),
                    }
                }
            })
            .buffered(REPO_CONCURRENCY)
            .try_filter_map(|r| async move { Ok(r) })
            .try_collect()
            .await
    }

    /// `repository` (+ `branch`/`branches`).
    async fn resolve_repository(
        &self,
        name: &str,
        branches: &[String],
        access: &RepoAccess,
    ) -> Result<Vec<RepoRevision>, ResolveError> {
        let repo = self
            .db
            .repos()
            .get_by_name_authorized(name, access)
            .await?
            .ok_or_else(|| ResolveError::Invalid(format!("repository {name:?} not found")))?;
        let repo = Arc::new(repo);
        let unavailable = |e: GitCallError| match e {
            GitCallError::RepoNotCloned | GitCallError::CloneInProgress => {
                ResolveError::Unavailable(format!("repository {name} is not cloned yet"))
            }
            e => ResolveError::Unavailable(format!("reading {name}: {e}")),
        };
        if branches.is_empty() {
            return match self.default_branch(&repo).await.map_err(unavailable)? {
                Some((branch, commit)) => Ok(vec![RepoRevision {
                    repo,
                    branch,
                    commit,
                    file_matches: Vec::new(),
                }]),
                None => Ok(Vec::new()),
            };
        }
        let git = self.git(&repo);
        let mut out = Vec::with_capacity(branches.len());
        for b in branches {
            let commit = match git.resolve_revision(b).await {
                Ok(c) => c,
                Err(GitCallError::RevisionNotFound(_)) => {
                    return Err(ResolveError::Invalid(format!(
                        "no branch matching {b:?} found for repository {name}"
                    )));
                }
                Err(e) => return Err(unavailable(e)),
            };
            out.push(RepoRevision {
                repo: repo.clone(),
                branch: branch_ref(b),
                commit,
                file_matches: Vec::new(),
            });
        }
        Ok(out)
    }

    /// The default branch ref and its head commit; `None` for an empty repository.
    async fn default_branch(&self, repo: &Repo) -> Result<Option<(String, String)>, GitCallError> {
        let git = self.git(repo);
        let head = git.list_refs(false, false, "").await?.head_ref;
        if head.is_empty() {
            return Ok(None);
        }
        match git.resolve_revision(&head).await {
            Ok(commit) => Ok(Some((head, commit))),
            Err(GitCallError::RevisionNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Runs `query` as `actor`; a rejected query, an error or a timeout fails the resolution
    /// rather than silently resolving fewer workspaces.
    async fn run_search(
        &self,
        query: &str,
        version: u32,
        actor: &Actor,
    ) -> Result<SearchOutcome, ResolveError> {
        let pattern_type = if version <= 1 { "standard" } else { "keyword" };
        let out = collect(
            &self.search,
            Some(actor),
            query,
            Some(pattern_type),
            usize::MAX,
        )
        .await;
        if let Some(e) = &out.error {
            return Err(ResolveError::Unavailable(format!("search failed: {e}")));
        }
        if out.progress.is_none() {
            let why = out
                .alerts
                .iter()
                .map(|a| format!("{}: {}", a.title, a.description))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ResolveError::Invalid(format!("invalid query: {why}")));
        }
        if out.timed_out() {
            return Err(ResolveError::Invalid(
                "the search timed out; narrow the query or raise its timeout: (up to 1m)".into(),
            ));
        }
        Ok(out)
    }

    /// The `(repository, commit)` pairs with a `.batchignore` at the root.
    async fn find_ignored(
        &self,
        revisions: &[RepoRevision],
    ) -> Result<HashSet<(RepoId, String)>, ResolveError> {
        let mut unique: Vec<(Arc<Repo>, String)> = Vec::new();
        let mut seen = HashSet::new();
        for r in revisions {
            if seen.insert((r.repo.id, r.commit.as_str())) {
                unique.push((r.repo.clone(), r.commit.clone()));
            }
        }
        let flags: Vec<(RepoId, String, bool)> = futures::stream::iter(unique)
            .map(|(repo, commit)| async move {
                if let Some(hit) = self.cache.ignored(repo.id, &commit).await {
                    return Ok((repo.id, commit, hit));
                }
                let ignored = match self.git(&repo).stat(&commit, BATCH_IGNORE).await {
                    Ok(e) => matches!(
                        pb::EntryKind::try_from(e.kind),
                        Ok(pb::EntryKind::File | pb::EntryKind::Executable)
                    ),
                    Err(GitCallError::PathNotFound(_)) => false,
                    Err(e) => {
                        return Err(ResolveError::Unavailable(format!(
                            "checking {} for {BATCH_IGNORE}: {e}",
                            repo.name
                        )));
                    }
                };
                self.cache.set_ignored(repo.id, &commit, ignored).await;
                Ok((repo.id, commit, ignored))
            })
            .buffer_unordered(REPO_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(flags
            .into_iter()
            .filter(|(_, _, ignored)| *ignored)
            .map(|(id, commit, _)| (id, commit))
            .collect())
    }

    /// Workspaces of the revisions per `workspaces[]`, sorted by repository name, path and
    /// branch, each with its repository.
    async fn find_workspaces(
        &self,
        spec: &BatchSpec,
        actor: &Actor,
        revisions: Vec<RepoRevision>,
    ) -> Result<Vec<(NewWorkspace, Arc<Repo>)>, ResolveError> {
        let globs = spec
            .workspaces
            .iter()
            .map(|c| match c.in_glob() {
                None => Glob::new("*"),
                Some(g) => g,
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ResolveError::Invalid(format!("invalid workspaces.in glob: {e}")))?;
        // Each revision with the index of the workspace config matching its repository.
        let mut errors = Vec::new();
        let mut matched: Vec<(RepoRevision, Option<usize>)> = Vec::new();
        for rev in revisions {
            let mut found = None;
            for (i, g) in globs.iter().enumerate() {
                if !g.is_match(&rev.repo.name) {
                    continue;
                }
                if found.is_some() {
                    errors.push(format!(
                        "repository {} matches multiple workspaces.in globs in the bulk change spec. \
                         glob: {:?}",
                        rev.repo.name,
                        spec.workspaces[i].in_.as_deref().unwrap_or("")
                    ));
                    continue;
                }
                found = Some(i);
            }
            matched.push((rev, found));
        }
        if !errors.is_empty() {
            return Err(ResolveError::Invalid(errors.join("; ")));
        }
        if !has_steps(spec) {
            return Ok(Vec::new());
        }
        let located: Vec<(RepoRevision, Vec<(String, bool)>)> = futures::stream::iter(matched)
            .map(|(rev, conf)| async move {
                let Some(i) = conf else {
                    return Ok((rev, vec![(String::new(), false)]));
                };
                let conf = &spec.workspaces[i];
                let dirs = self
                    .find_directories(&conf.root_at_location_of, spec.version, actor, &rev)
                    .await?;
                // An empty path is the root, where the whole repository is fetched anyway.
                let paths = dirs
                    .iter()
                    .map(|d| (d.clone(), conf.only_fetch_workspace && !d.is_empty()))
                    .collect();
                Ok::<_, ResolveError>((rev, paths))
            })
            .buffered(SEARCH_CONCURRENCY)
            .try_collect()
            .await?;
        let mut out = Vec::new();
        // Static `if:` evaluation only depends on the repository name.
        let mut runnable_of: HashMap<String, bool> = HashMap::new();
        for (rev, paths) in located {
            let any = match runnable_of.get(&rev.repo.name) {
                Some(&any) => any,
                None => {
                    let any = !runnable_steps(spec, &rev.repo.name)?.is_empty();
                    runnable_of.insert(rev.repo.name.clone(), any);
                    any
                }
            };
            // A workspace whose steps are all statically skipped would do nothing.
            if !any {
                continue;
            }
            for (path, only_fetch) in paths {
                // Matched paths within the workspace only.
                let file_matches: Vec<String> = rev
                    .file_matches
                    .iter()
                    .filter(|p| within(p, &path))
                    .cloned()
                    .collect();
                let mut w = NewWorkspace::new(rev.repo.id, &rev.branch, &rev.commit);
                w.path = path;
                w.file_matches = file_matches;
                w.only_fetch_workspace = only_fetch;
                out.push((w, rev.repo.clone()));
            }
        }
        out.sort_by(|(a, ra), (b, rb)| {
            (&ra.name, &a.path, &a.branch).cmp(&(&rb.name, &b.path, &b.branch))
        });
        Ok(out)
    }

    /// Directories (`""` for the root) holding a file named `file` at the revision.
    async fn find_directories(
        &self,
        file: &str,
        version: u32,
        actor: &Actor,
        rev: &RepoRevision,
    ) -> Result<Arc<Vec<String>>, ResolveError> {
        if let Some(hit) = self.cache.dirs(rev.repo.id, &rev.commit, file).await {
            return Ok(hit);
        }
        let query = format!(
            "file:(^|/){}$ repo:^{}$@{} type:path count:all patterntype:keyword",
            quote_regex(file),
            quote_regex(&rev.repo.name),
            rev.commit
        );
        let out = self.run_search(&query, version, actor).await?;
        let mut dirs: Vec<String> = out
            .matches
            .iter()
            .filter_map(|m| match m {
                Match::Path {
                    repository_id,
                    path,
                    ..
                }
                | Match::Content {
                    repository_id,
                    path,
                    ..
                } if *repository_id == rev.repo.id.get() => Some(parent_dir(path)),
                _ => None,
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        let dirs = Arc::new(dirs);
        self.cache
            .set_dirs(rev.repo.id, &rev.commit, file, dirs.clone())
            .await;
        Ok(dirs)
    }

    /// Records execution cache hits on the workspaces that will run.
    async fn find_cached(
        &self,
        spec: &BatchSpec,
        user: UserId,
        workspaces: &mut [(NewWorkspace, Arc<Repo>)],
    ) -> Result<(), ResolveError> {
        let mut runnable_of: HashMap<String, Vec<usize>> = HashMap::new();
        let mut keys_of: Vec<Vec<(usize, String)>> = Vec::with_capacity(workspaces.len());
        let mut all = Vec::new();
        for (w, repo) in workspaces.iter() {
            if !runnable_of.contains_key(&repo.name) {
                runnable_of.insert(repo.name.clone(), runnable_steps(spec, &repo.name)?);
            }
            let runnable = &runnable_of[&repo.name];
            let keys: Vec<(usize, String)> = if w.skipped {
                Vec::new()
            } else {
                runnable
                    .iter()
                    .map(|&i| {
                        let key = step_cache_key(
                            spec,
                            &repo.name,
                            &w.commit,
                            &w.file_matches,
                            &w.path,
                            w.only_fetch_workspace,
                            i,
                        );
                        (i, key)
                    })
                    .collect()
            };
            all.extend(keys.iter().map(|(_, k)| k.clone()));
            keys_of.push(keys);
        }
        if all.is_empty() {
            return Ok(());
        }
        all.sort();
        all.dedup();
        let entries = ExecutionCacheStore::new(&self.db)
            .get_many(user, &all)
            .await?;
        let values: HashMap<&str, &str> = entries
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        for ((w, repo), keys) in workspaces.iter_mut().zip(keys_of) {
            // The last step that may run in this workspace.
            let last = runnable_of[&repo.name].last().copied();
            let mut results = serde_json::Map::new();
            // Leading hits only: a step's result is useless once an earlier one must run.
            for (i, key) in keys {
                let Some(value) = values.get(key.as_str()) else {
                    break;
                };
                let value = serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String((*value).to_owned()));
                // Keyed by step number (1-based).
                results.insert(
                    (i + 1).to_string(),
                    serde_json::json!({ "key": key, "value": value }),
                );
                if Some(i) == last {
                    w.cached_result_found = true;
                }
            }
            w.step_cache_results = serde_json::Value::Object(results);
        }
        Ok(())
    }

    /// `importChangesets`: one `existing` changeset spec per external ID.
    async fn imports(
        &self,
        spec: &BatchSpec,
        user: UserId,
        access: &RepoAccess,
        options: Options,
    ) -> Result<Vec<NewChangesetSpec>, ResolveError> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for import in &spec.import_changesets {
            let Some(repo) = self
                .db
                .repos()
                .get_by_name_authorized(&import.repository, access)
                .await?
            else {
                errors.push(format!("repository {:?} not found", import.repository));
                continue;
            };
            if repo.external_service_type != SUPPORTED_SERVICE_TYPE && !options.allow_unsupported {
                errors.push(format!(
                    "repository {:?} is on a code host Batch Changes does not support",
                    import.repository
                ));
                continue;
            }
            let mut seen = HashSet::new();
            for id in &import.external_ids {
                let id = match external_id(id) {
                    Ok(id) => id,
                    Err(e) => {
                        errors.push(format!("{}: {e}", import.repository));
                        continue;
                    }
                };
                if !seen.insert(id.clone()) {
                    continue;
                }
                let mut s = NewChangesetSpec::new(repo.id, ChangesetSpecType::Existing);
                s.user_id = Some(user);
                s.external_id = Some(id);
                out.push(s);
            }
        }
        if !errors.is_empty() {
            return Err(ResolveError::Invalid(format!(
                "importChangesets: {}",
                errors.join("; ")
            )));
        }
        Ok(out)
    }
}

/// Per-spec options of a resolution (the spec's server-side flags).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub allow_ignored: bool,
    pub allow_unsupported: bool,
    /// Ignore the execution cache.
    pub no_cache: bool,
}

/// `query` with ` count:all` appended unless it has a `count:`, so a query finds every
/// repository and not the first 500 results.
pub fn with_default_count(query: &str) -> String {
    static COUNT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bcount:(\d+|all)\b").expect("valid regex")
    });
    if COUNT.is_match(query) {
        query.to_owned()
    } else {
        format!("{query} count:all")
    }
}

/// A GitLab merge request's external ID: its project-level number (iid).
fn external_id(id: &ExternalId) -> Result<String, String> {
    let s = id.to_string();
    match s.trim().parse::<u64>() {
        Ok(n) if n > 0 => Ok(n.to_string()),
        _ => Err(format!(
            "invalid external ID {s:?}: GitLab merge requests are numbered from 1"
        )),
    }
}

/// `branch` as a full ref.
fn branch_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

/// The directory of `path`, `""` for the root.
fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(d, _)| d.to_owned())
        .unwrap_or_default()
}

/// Whether `path` is in directory `dir` (`""` is the root).
fn within(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `s` as a regex matching it literally, with spaces escaped so it stays one query term.
fn quote_regex(s: &str) -> String {
    regex::escape(s).replace(' ', r"\x20")
}

/// The steps that may run in a workspace of repository `repo` (by index): those without an
/// `if:` and those whose `if:` is not statically false. A condition is partially evaluated
/// with what is known before any step runs (the repository name, the bulk change's name and
/// description, literals and functions of them); conditions that need more (outputs, previous
/// steps, search result paths) are evaluated when the step runs.
fn runnable_steps(spec: &BatchSpec, repo: &str) -> Result<Vec<usize>, ResolveError> {
    let ctx = sourcegull_batches_template::StepContext {
        batch_change: sourcegull_batches_template::BatchChangeAttributes {
            name: spec.name.clone(),
            description: spec.description.clone(),
        },
        repository: sourcegull_batches_template::Repository {
            name: repo.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut out = Vec::new();
    for (i, step) in spec.steps.iter().enumerate() {
        let Some(cond) = step.if_condition() else {
            out.push(i);
            continue;
        };
        let (is_static, value) = ctx
            .is_static_bool(&cond)
            .map_err(|e| ResolveError::Invalid(format!("steps[{i}].if: {e}")))?;
        if !is_static || value {
            out.push(i);
        }
    }
    Ok(out)
}

/// A step whose `if:` is a literal false (`false`, `"false"`). Conditions with templates are
/// evaluated per repository ([`runnable_steps`]) or when the step runs.
fn statically_skipped(step: &sourcegull_batches::spec::Step) -> bool {
    step.if_condition()
        .is_some_and(|c| c.trim().eq_ignore_ascii_case("false"))
}

/// Whether any step may run (workspaces of a spec without such steps are dropped).
fn has_steps(spec: &BatchSpec) -> bool {
    spec.steps.iter().any(|s| !statically_skipped(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_count() {
        assert_eq!(with_default_count("foo"), "foo count:all");
        assert_eq!(with_default_count("foo count:10"), "foo count:10");
        assert_eq!(with_default_count("count:all foo"), "count:all foo");
        assert_eq!(
            with_default_count("foo xcount:10"),
            "foo xcount:10 count:all"
        );
    }

    #[test]
    fn paths() {
        assert_eq!(parent_dir("a/b/go.mod"), "a/b");
        assert_eq!(parent_dir("go.mod"), "");
        assert!(within("a/b/x.go", "a"));
        assert!(within("a/b/x.go", ""));
        assert!(!within("ab/x.go", "a"));
        assert_eq!(branch_ref("main"), "refs/heads/main");
        assert_eq!(branch_ref("refs/heads/x"), "refs/heads/x");
        assert_eq!(quote_regex("a.b c"), r"a\.b\x20c");
    }

    #[test]
    fn external_ids() {
        assert_eq!(external_id(&ExternalId::Number(7)), Ok("7".into()));
        assert_eq!(
            external_id(&ExternalId::String("12".into())),
            Ok("12".into())
        );
        assert!(external_id(&ExternalId::String("abc".into())).is_err());
        assert!(external_id(&ExternalId::Number(0)).is_err());
    }

    #[test]
    fn directory_query_parses() {
        let q = format!(
            "file:(^|/){}$ repo:^{}$@{} type:path count:all patterntype:keyword",
            quote_regex("package.json"),
            quote_regex("gitlab.example.com/a/b"),
            "0123456789abcdef0123456789abcdef01234567"
        );
        sourcegull_search::plan_query(
            &q,
            sourcegull_query::PatternType::Standard,
            &sourcegull_search::Limits::default(),
            chrono::Utc::now(),
        )
        .unwrap();
    }
}
