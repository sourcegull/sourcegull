//! Search contexts: the `SearchContext` type, `searchContexts`,
//! `searchContextBySpec`, `isSearchContextAvailable`, `defaultSearchContext`
//! and the create, update, delete, star and default mutations.
//!
//! Who may read and write a context is `sourcegull_services::search_contexts`'s
//! rule.

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_authn::Actor;
use sourcegull_db::{
    ContextAccess, ContextOrder, ListContextsOptions, Namespace, SearchContext, SearchContextInput,
};
use sourcegull_search::{ContextSpec, GLOBAL_CONTEXT, check_context_fields};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_services::search_contexts as rules;
use sourcegull_types::{RepoId, UserId};

use crate::context::{
    access_error, codes, db_error, error, page_size, require_viewer, services, to_int, validation,
    viewer,
};
use crate::load_namespace;
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::declared;
use sourcegull_graphql_core::ids::{parse_namespace_id, parse_repository_id, parse_user_id};
use sourcegull_graphql_core::repo::require_reader;
use sourcegull_graphql_core::shared::{NamespaceRef, RepositoryRef};

/// Node kind of search context IDs. The global context is id 0.
pub const SEARCH_CONTEXT_KIND: &str = "SearchContext";

const MAX_REVISION_LEN: usize = 255;

/// A search context, or the built-in `global` one.
#[derive(Debug, Clone)]
pub enum SearchContextNode {
    Global { viewer_default: bool },
    Stored(SearchContext),
}

impl SearchContextNode {
    fn stored(&self) -> Option<&SearchContext> {
        match self {
            SearchContextNode::Global { .. } => None,
            SearchContextNode::Stored(c) => Some(c),
        }
    }
}

/// A repository of a context and the revisions searched.
#[derive(SimpleObject)]
pub struct SearchContextRepositoryRevisions {
    pub repository: RepositoryRef,
    pub revisions: Vec<String>,
}

#[Object(name = "SearchContext")]
impl SearchContextNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(
            SEARCH_CONTEXT_KIND,
            self.stored().map_or(0, |c| c.id),
        ))
    }

    /// The name, without the owner.
    async fn name(&self) -> &str {
        self.stored().map_or(GLOBAL_CONTEXT, |c| c.name.as_str())
    }

    /// The owning user or organization; null for instance-level contexts.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<Option<NamespaceRef>> {
        match self.stored().and_then(|c| c.namespace) {
            Some(ns) => load_namespace(ctx, ns).await,
            None => Ok(None),
        }
    }

    async fn description(&self) -> &str {
        self.stored()
            .map_or("All repositories", |c| c.description.as_str())
    }

    /// What `context:` takes: `name` or `@owner/name`.
    async fn spec(&self) -> String {
        self.stored()
            .map_or_else(|| GLOBAL_CONTEXT.to_owned(), SearchContext::spec)
    }

    /// Whether this is the built-in `global` context.
    async fn auto_defined(&self) -> bool {
        self.stored().is_none()
    }

    /// The repository filters of a query-defined context; empty for listed
    /// ones.
    async fn query(&self) -> &str {
        self.stored().and_then(|c| c.query.as_deref()).unwrap_or("")
    }

    /// The repositories of a listed context, with revisions (`HEAD`: the
    /// default branch).
    async fn repositories(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<SearchContextRepositoryRevisions>> {
        let Some(c) = self.stored() else {
            return Ok(Vec::new());
        };
        let db = &services(ctx)?.db;
        let access = crate::context::repo_access(ctx).await?;
        let mut out = Vec::new();
        for r in db.search_contexts().repos(c.id).await.map_err(db_error)? {
            let repo = db
                .repos()
                .get_authorized(r.repo_id, &access)
                .await
                .map_err(db_error)?;
            let Some(repo) = repo else {
                continue;
            };
            out.push(SearchContextRepositoryRevisions {
                repository: RepositoryRef(repo),
                revisions: r
                    .revisions
                    .into_iter()
                    .map(|rev| {
                        if rev.is_empty() {
                            "HEAD".to_owned()
                        } else {
                            rev
                        }
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    /// Public contexts are available to every user; private ones to the
    /// owning user, the owning org's members, or (instance-level) site
    /// admins.
    async fn public(&self) -> bool {
        self.stored().is_none_or(|c| c.public)
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.stored().map_or(DateTime::UNIX_EPOCH, |c| c.updated_at)
    }

    /// Whether the viewer may edit or delete it (never when the resources file manages it).
    async fn viewer_can_manage(&self, ctx: &Context<'_>) -> Result<bool> {
        match (self.stored(), viewer(ctx)) {
            (Some(c), Some(actor)) => {
                if c.namespace.is_none() && managed(ctx, c).await?.is_some() {
                    return Ok(false);
                }
                can_write(ctx, actor, c.namespace, c.public).await
            }
            _ => Ok(false),
        }
    }

    /// The resources file entry that manages this instance-level context
    /// (`searchContexts[<key>]`), or `searchContexts` when the file manages all of them.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        match self.stored() {
            Some(c) if c.namespace.is_none() => managed(ctx, c).await,
            _ => Ok(None),
        }
    }

    /// Whether it is the viewer's default context.
    async fn viewer_has_as_default(&self) -> bool {
        match self {
            SearchContextNode::Global { viewer_default } => *viewer_default,
            SearchContextNode::Stored(c) => c.viewer_default,
        }
    }

    async fn viewer_has_starred(&self) -> bool {
        self.stored().is_some_and(|c| c.viewer_starred)
    }
}

/// Instance-level contexts the resources file manages are read-only.
async fn ensure_not_declared(ctx: &Context<'_>, c: &SearchContext) -> Result<()> {
    if c.namespace.is_some() {
        return Ok(());
    }
    declared::ensure_not_declared(
        services(ctx)?,
        declared::kinds::SEARCH_CONTEXTS,
        &c.id.to_string(),
    )
    .await
}

async fn managed(ctx: &Context<'_>, c: &SearchContext) -> Result<Option<String>> {
    declared::managed_by(
        services(ctx)?,
        declared::kinds::SEARCH_CONTEXTS,
        &c.id.to_string(),
    )
    .await
}

async fn can_write(
    ctx: &Context<'_>,
    actor: &Actor,
    ns: Option<Namespace>,
    public: bool,
) -> Result<bool> {
    rules::can_write(&services(ctx)?.db, actor, ns, public)
        .await
        .map_err(db_error)
}

async fn require_write(
    ctx: &Context<'_>,
    actor: &Actor,
    ns: Option<Namespace>,
    public: bool,
) -> Result<()> {
    rules::require_write(&services(ctx)?.db, actor, ns, public)
        .await
        .map_err(access_error)
}

fn access(ctx: &Context<'_>) -> ContextAccess {
    rules::access(viewer(ctx))
}

/// The id of a `SearchContext` node ID (0: global).
fn parse_context_id(id: &str) -> Result<i64> {
    unmarshal_int_id(SEARCH_CONTEXT_KIND, id).ok_or_else(|| {
        error(
            codes::INVALID_INPUT,
            format!("Invalid search context ID {id:?}."),
        )
    })
}

/// Loads a stored context the viewer may read, or fails with `NOT_FOUND`.
async fn load_readable(ctx: &Context<'_>, id: i64) -> Result<SearchContext> {
    services(ctx)?
        .db
        .search_contexts()
        .get(id, access(ctx))
        .await
        .map_err(db_error)?
        .ok_or_else(|| access_error(rules::not_found()))
}

/// Loads a context for `node(id:)`, honouring the read rule.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<SearchContextNode>> {
    require_reader(ctx)?;
    if id == 0 {
        return Ok(Some(global(ctx).await?));
    }
    Ok(services(ctx)?
        .db
        .search_contexts()
        .get(id, access(ctx))
        .await
        .map_err(db_error)?
        .map(SearchContextNode::Stored))
}

async fn global(ctx: &Context<'_>) -> Result<SearchContextNode> {
    let viewer_default = match viewer(ctx) {
        Some(a) => services(ctx)?
            .db
            .search_contexts()
            .default_for(a.user_id)
            .await
            .map_err(db_error)?
            .is_none(),
        None => true,
    };
    Ok(SearchContextNode::Global { viewer_default })
}

/// `searchContexts(orderBy:)`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Default)]
pub enum SearchContextsOrderBy {
    #[default]
    SearchContextSpec,
    SearchContextUpdatedAt,
}

/// A page of search contexts.
#[derive(SimpleObject)]
pub struct SearchContextConnection {
    pub nodes: Vec<SearchContextNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// The name, description, visibility and query of a new context.
#[derive(InputObject)]
#[graphql(name = "SearchContextInput")]
pub struct SearchContextInputGql {
    /// The name, without the owner (`ctx` in `@alice/ctx`).
    pub name: String,
    pub description: String,
    pub public: bool,
    /// The owning user or organization; null for an instance-level context.
    pub namespace: Option<ID>,
    /// Repository filters for a query-defined context; empty for a listed one.
    pub query: String,
}

/// The name, description, visibility and query of an existing context.
#[derive(InputObject)]
pub struct SearchContextEditInput {
    pub name: String,
    pub description: String,
    pub public: bool,
    pub query: String,
}

/// A repository and the revisions to search in it.
#[derive(InputObject)]
pub struct SearchContextRepositoryRevisionsInput {
    #[graphql(name = "repositoryID")]
    pub repository_id: ID,
    /// Branches, tags or commits; empty or `HEAD` is the default branch.
    pub revisions: Vec<String>,
}

/// Validates a context's fields and resolves its repositories.
async fn context_input(
    ctx: &Context<'_>,
    namespace: Option<Namespace>,
    name: &str,
    description: &str,
    public: bool,
    query: &str,
    repositories: &[SearchContextRepositoryRevisionsInput],
) -> Result<SearchContextInput> {
    let query = query.trim();
    let mut problems = check_context_fields(
        namespace.is_none(),
        name,
        description,
        query,
        repositories.len(),
    );
    let mut repos: Vec<(RepoId, Vec<String>)> = Vec::new();
    let db = &services(ctx)?.db;
    let access = crate::context::repo_access(ctx).await?;
    for r in repositories {
        let id = parse_repository_id(&r.repository_id)?;
        let readable = db.repos().get_authorized(id, &access).await;
        if readable.map_err(db_error)?.is_none() {
            problems.push(format!(
                "Repository {} not found.",
                r.repository_id.as_str()
            ));
            continue;
        }
        let mut revs = Vec::new();
        for rev in &r.revisions {
            let rev = rev.trim();
            if rev.len() > MAX_REVISION_LEN {
                problems.push(format!(
                    "The revision {rev:?} is longer than {MAX_REVISION_LEN} bytes."
                ));
            }
            let rev = if rev == "HEAD" { "" } else { rev };
            if !revs.iter().any(|r: &String| r == rev) {
                revs.push(rev.to_owned());
            }
        }
        match repos.iter_mut().find(|(r, _)| *r == id) {
            Some((_, existing)) => existing.extend(revs),
            None => repos.push((id, revs)),
        }
    }
    if !problems.is_empty() {
        return Err(validation(problems));
    }
    Ok(SearchContextInput {
        name: name.to_owned(),
        description: description.to_owned(),
        public,
        query: (!query.is_empty()).then(|| query.to_owned()),
        repos,
    })
}

fn already_exists(e: sourcegull_db::DbError) -> async_graphql::Error {
    if matches!(e, sourcegull_db::DbError::AlreadyExists { .. }) {
        error(
            codes::ALREADY_EXISTS,
            "A search context with this name already exists.",
        )
    } else {
        db_error(e)
    }
}

fn require_same_user(actor: &Actor, user: UserId) -> Result<()> {
    rules::require_same_user(actor, user).map_err(access_error)
}

/// Search context queries.
#[derive(Default)]
pub struct SearchContextsQuery;

#[Object]
impl SearchContextsQuery {
    /// Contexts the viewer may read: `global` first, then the viewer's
    /// default, starred ones, and the rest in `orderBy` order.
    #[allow(clippy::too_many_arguments)]
    async fn search_contexts(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
        query: Option<String>,
        #[graphql(desc = "User or org IDs, or null for instance-level contexts; empty: all.")]
        namespaces: Option<Vec<Option<ID>>>,
        #[graphql(default)] order_by: SearchContextsOrderBy,
        #[graphql(default)] descending: bool,
    ) -> Result<SearchContextConnection> {
        require_reader(ctx)?;
        let limit = page_size(Some(first), 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let namespaces = namespaces
            .unwrap_or_default()
            .iter()
            .map(|id| id.as_ref().map(|id| parse_namespace_id(id)).transpose())
            .collect::<Result<Vec<_>>>()?;
        let query = query.map(|q| q.trim().to_owned()).filter(|q| !q.is_empty());
        let include_global = (namespaces.is_empty() || namespaces.contains(&None))
            && query
                .as_deref()
                .is_none_or(|q| GLOBAL_CONTEXT.contains(&q.to_lowercase()));
        // `global` is item 0 of the list when included.
        let skip = i64::from(include_global);
        let store_offset = (offset - skip).max(0);
        let store_limit = limit + 1 - if include_global && offset == 0 { 1 } else { 0 };
        let (contexts, total) = services(ctx)?
            .db
            .search_contexts()
            .list(&ListContextsOptions {
                access: access(ctx),
                query,
                namespaces: (!namespaces.is_empty()).then_some(namespaces),
                order: match order_by {
                    SearchContextsOrderBy::SearchContextSpec => ContextOrder::Spec,
                    SearchContextsOrderBy::SearchContextUpdatedAt => ContextOrder::UpdatedAt,
                },
                descending,
                limit: store_limit,
                offset: store_offset,
            })
            .await
            .map_err(db_error)?;
        let mut nodes = Vec::new();
        if include_global && offset == 0 {
            nodes.push(global(ctx).await?);
        }
        nodes.extend(contexts.into_iter().map(SearchContextNode::Stored));
        let page_info = PageInfo::new(offset, limit, nodes.len());
        nodes.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(SearchContextConnection {
            nodes,
            total_count: to_int(total + skip),
            page_info,
        })
    }

    /// The context with this spec (`global`, `name` or `@owner/name`), if
    /// the viewer may read it.
    async fn search_context_by_spec(
        &self,
        ctx: &Context<'_>,
        spec: String,
    ) -> Result<Option<SearchContextNode>> {
        require_reader(ctx)?;
        match ContextSpec::parse(&spec) {
            ContextSpec::Global => Ok(Some(global(ctx).await?)),
            ContextSpec::Owner(_) => Ok(None),
            _ => Ok(services(ctx)?
                .db
                .search_contexts()
                .get_by_spec(&spec, access(ctx))
                .await
                .map_err(db_error)?
                .map(SearchContextNode::Stored)),
        }
    }

    /// Whether `context:<spec>` would resolve for the viewer.
    async fn is_search_context_available(&self, ctx: &Context<'_>, spec: String) -> Result<bool> {
        Ok(self.search_context_by_spec(ctx, spec).await?.is_some())
    }

    /// The viewer's default context: the one they set, else `global`.
    async fn default_search_context(&self, ctx: &Context<'_>) -> Result<Option<SearchContextNode>> {
        require_reader(ctx)?;
        if let Some(a) = viewer(ctx)
            && let Some(c) = services(ctx)?
                .db
                .search_contexts()
                .default_for(a.user_id)
                .await
                .map_err(db_error)?
        {
            return Ok(Some(SearchContextNode::Stored(c)));
        }
        Ok(Some(global(ctx).await?))
    }
}

/// Search context mutations.
#[derive(Default)]
pub struct SearchContextsMutation;

#[Object]
impl SearchContextsMutation {
    /// Creates a context, listed (`repositories`) or query-defined
    /// (`searchContext.query`).
    async fn create_search_context(
        &self,
        ctx: &Context<'_>,
        search_context: SearchContextInputGql,
        repositories: Vec<SearchContextRepositoryRevisionsInput>,
    ) -> Result<SearchContextNode> {
        let actor = require_viewer(ctx)?;
        let ns = search_context
            .namespace
            .as_ref()
            .map(|id| parse_namespace_id(id))
            .transpose()?;
        if let Some(ns) = ns
            && load_namespace(ctx, ns).await?.is_none()
        {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        require_write(ctx, actor, ns, search_context.public).await?;
        if ns.is_none() {
            declared::ensure_creatable(services(ctx)?, declared::kinds::SEARCH_CONTEXTS).await?;
        }
        let input = context_input(
            ctx,
            ns,
            search_context.name.trim(),
            &search_context.description,
            search_context.public,
            &search_context.query,
            &repositories,
        )
        .await?;
        let created = services(ctx)?
            .db
            .search_contexts()
            .create(ns, &input)
            .await
            .map_err(already_exists)?;
        Ok(SearchContextNode::Stored(
            load_readable(ctx, created.id).await?,
        ))
    }

    /// Replaces a context's fields and repositories.
    async fn update_search_context(
        &self,
        ctx: &Context<'_>,
        id: ID,
        search_context: SearchContextEditInput,
        repositories: Vec<SearchContextRepositoryRevisionsInput>,
    ) -> Result<SearchContextNode> {
        let actor = require_viewer(ctx)?;
        let old = load_readable(ctx, parse_context_id(&id)?).await?;
        // Both the current and the new visibility must be writable.
        require_write(ctx, actor, old.namespace, old.public).await?;
        ensure_not_declared(ctx, &old).await?;
        require_write(ctx, actor, old.namespace, search_context.public).await?;
        let input = context_input(
            ctx,
            old.namespace,
            search_context.name.trim(),
            &search_context.description,
            search_context.public,
            &search_context.query,
            &repositories,
        )
        .await?;
        services(ctx)?
            .db
            .search_contexts()
            .update(old.id, &input)
            .await
            .map_err(already_exists)?;
        Ok(SearchContextNode::Stored(load_readable(ctx, old.id).await?))
    }

    async fn delete_search_context(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let old = load_readable(ctx, parse_context_id(&id)?).await?;
        require_write(ctx, actor, old.namespace, old.public).await?;
        ensure_not_declared(ctx, &old).await?;
        services(ctx)?
            .db
            .search_contexts()
            .delete(old.id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    async fn create_search_context_star(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "searchContextID")] search_context_id: ID,
        #[graphql(name = "userID")] user_id: ID,
    ) -> Result<EmptyResponse> {
        star(ctx, &search_context_id, &user_id, true).await
    }

    async fn delete_search_context_star(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "searchContextID")] search_context_id: ID,
        #[graphql(name = "userID")] user_id: ID,
    ) -> Result<EmptyResponse> {
        star(ctx, &search_context_id, &user_id, false).await
    }

    /// Sets the user's default context (the global context's ID clears it).
    async fn set_default_search_context(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "searchContextID")] search_context_id: ID,
        #[graphql(name = "userID")] user_id: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let user = parse_user_id(&user_id)?;
        require_same_user(actor, user)?;
        let id = parse_context_id(&search_context_id)?;
        let store = services(ctx)?.db.search_contexts();
        let target = if id == 0 {
            None
        } else {
            // The user (not just the caller) must be able to read it.
            store
                .get(id, ContextAccess::Viewer(Some(user)))
                .await
                .map_err(db_error)?
                .ok_or_else(|| access_error(rules::not_found()))?;
            Some(id)
        };
        store.set_default(user, target).await.map_err(db_error)?;
        Ok(EmptyResponse)
    }
}

async fn star(ctx: &Context<'_>, context_id: &ID, user_id: &ID, on: bool) -> Result<EmptyResponse> {
    let actor = require_viewer(ctx)?;
    let user = parse_user_id(user_id)?;
    require_same_user(actor, user)?;
    let id = parse_context_id(context_id)?;
    if id == 0 {
        return Err(error(
            codes::INVALID_INPUT,
            "The global context cannot be starred.",
        ));
    }
    let store = services(ctx)?.db.search_contexts();
    store
        .get(id, ContextAccess::Viewer(Some(user)))
        .await
        .map_err(db_error)?
        .ok_or_else(|| access_error(rules::not_found()))?;
    store.set_starred(id, user, on).await.map_err(db_error)?;
    Ok(EmptyResponse)
}
