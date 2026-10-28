//! Saved searches: the `SavedSearch` type, `savedSearches` and the
//! create, update, delete, transfer and visibility mutations.
//!
//! Who may see, list, change and publish saved searches is
//! `sourcegull_services::saved_searches`'s rule.

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_authn::Actor;
use sourcegull_db::{
    ListSavedSearchesOptions, Namespace, SavedSearch, SavedSearchOrder, SavedSearchVisibility,
};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_services::saved_searches as rules;

use crate::context::{
    access_error, codes, db_error, error, require_viewer, services, to_int, validation, viewer,
};
use crate::load_namespace;
use crate::pagination::{ConnectionPageInfo, PageArgs};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::ids::parse_namespace_id;
use sourcegull_graphql_core::repo::require_reader;
use sourcegull_graphql_core::shared::{NamespaceRef, UserRef};

/// Node kind of saved search IDs.
pub const SAVED_SEARCH_KIND: &str = "SavedSearch";

const MAX_DESCRIPTION_LEN: usize = 1024;
const MAX_QUERY_LEN: usize = 16 * 1024;

/// `SavedSearch.visibility`.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
#[graphql(name = "SavedSearchVisibility")]
pub enum Visibility {
    /// Visible to every user.
    Public,
    /// Visible only to the owner user, or the owner org's members.
    Secret,
}

impl From<SavedSearchVisibility> for Visibility {
    fn from(v: SavedSearchVisibility) -> Self {
        match v {
            SavedSearchVisibility::Public => Visibility::Public,
            SavedSearchVisibility::Secret => Visibility::Secret,
        }
    }
}

impl From<Visibility> for SavedSearchVisibility {
    fn from(v: Visibility) -> Self {
        match v {
            Visibility::Public => SavedSearchVisibility::Public,
            Visibility::Secret => SavedSearchVisibility::Secret,
        }
    }
}

/// A saved search.
#[derive(Debug, Clone)]
pub struct SavedSearchNode(pub SavedSearch);

#[Object(name = "SavedSearch")]
impl SavedSearchNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(SAVED_SEARCH_KIND, self.0.id))
    }

    async fn description(&self) -> &str {
        &self.0.description
    }

    async fn query(&self) -> &str {
        &self.0.query
    }

    async fn draft(&self) -> bool {
        self.0.draft
    }

    /// The owning user or organization.
    async fn owner(&self, ctx: &Context<'_>) -> Result<NamespaceRef> {
        load_namespace(ctx, self.0.owner)
            .await?
            .ok_or_else(|| error(codes::NOT_FOUND, "The owner no longer exists."))
    }

    async fn visibility(&self) -> Visibility {
        self.0.visibility.into()
    }

    async fn created_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.created_by).await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.updated_by).await
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }

    /// The saved search's page.
    async fn url(&self) -> String {
        format!(
            "/saved-searches/{}",
            marshal_id(SAVED_SEARCH_KIND, self.0.id)
        )
    }

    /// Whether the viewer may edit, transfer or delete it.
    async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        rules::can_administer(&services(ctx)?.db, viewer(ctx), &self.0)
            .await
            .map_err(db_error)
    }
}

async fn user(ctx: &Context<'_>, id: Option<sourcegull_types::UserId>) -> Result<Option<UserRef>> {
    let Some(id) = id else { return Ok(None) };
    Ok(services(ctx)?
        .db
        .users()
        .get_by_id(id)
        .await
        .map_err(db_error)?
        .map(UserRef))
}

fn parse_saved_search_id(id: &str) -> Result<i64> {
    unmarshal_int_id(SAVED_SEARCH_KIND, id).ok_or_else(|| {
        error(
            codes::INVALID_INPUT,
            format!("Invalid saved search ID {id:?}."),
        )
    })
}

/// Whether the viewer may see `s`.
async fn can_view(ctx: &Context<'_>, s: &SavedSearch) -> Result<bool> {
    rules::can_view(&services(ctx)?.db, viewer(ctx), s)
        .await
        .map_err(db_error)
}

async fn require_can_own(ctx: &Context<'_>, actor: &Actor, ns: Namespace) -> Result<()> {
    rules::require_can_own(&services(ctx)?.db, actor, ns)
        .await
        .map_err(access_error)
}

/// Loads a saved search for `node(id:)`, honouring the read rule.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<SavedSearchNode>> {
    require_reader(ctx)?;
    let Some(s) = services(ctx)?
        .db
        .saved_searches()
        .get(id)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    Ok(can_view(ctx, &s).await?.then_some(SavedSearchNode(s)))
}

/// Loads a saved search the viewer may administer, or fails: `NOT_FOUND`
/// when they may not even see it, `FORBIDDEN` when they may only see it.
async fn load_for_admin(ctx: &Context<'_>, actor: &Actor, id: &ID) -> Result<SavedSearch> {
    let db = &services(ctx)?.db;
    let s = db
        .saved_searches()
        .get(parse_saved_search_id(id)?)
        .await
        .map_err(db_error)?
        .ok_or_else(|| access_error(rules::not_found()))?;
    rules::require_can_administer(db, actor, &s)
        .await
        .map_err(access_error)?;
    Ok(s)
}

fn require_publisher(actor: &Actor) -> Result<()> {
    rules::require_publisher(actor).map_err(access_error)
}

/// Checks a description and query; the query must name its pattern type,
/// so it runs the same whatever the defaults.
fn check(description: &str, query: &str) -> Result<()> {
    let mut problems = Vec::new();
    if description.trim().is_empty() {
        problems.push("The description is empty.".to_owned());
    }
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        problems.push(format!(
            "The description is longer than {MAX_DESCRIPTION_LEN} characters."
        ));
    }
    if query.trim().is_empty() {
        problems.push("The query is empty.".to_owned());
    } else if query.len() > MAX_QUERY_LEN {
        problems.push(format!("The query is longer than {MAX_QUERY_LEN} bytes."));
    } else if !has_pattern_type(query) {
        problems.push(
            "A `patternType:` filter is required in the query for all saved searches. \
             `patternType` can be \"keyword\", \"standard\", \"literal\", or \"regexp\"."
                .to_owned(),
        );
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(validation(problems))
    }
}

fn has_pattern_type(query: &str) -> bool {
    sourcegull_query::parse(query, sourcegull_query::PatternType::Standard).is_ok_and(|q| {
        q.parameters().iter().any(|p| {
            p.field == "patterntype"
                && matches!(
                    p.value.to_ascii_lowercase().as_str(),
                    "keyword" | "standard" | "literal" | "regexp" | "regex" | "structural"
                )
        })
    })
}

/// `savedSearches(orderBy:)`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Default)]
pub enum SavedSearchesOrderBy {
    SavedSearchDescription,
    #[default]
    SavedSearchUpdatedAt,
}

/// A page of saved searches.
#[derive(SimpleObject)]
#[graphql(name = "SavedSearchesConnection")]
pub struct SavedSearchConnection {
    pub nodes: Vec<SavedSearchNode>,
    pub total_count: i32,
    pub page_info: ConnectionPageInfo,
}

/// A new saved search.
#[derive(InputObject)]
pub struct SavedSearchInput {
    /// The owning user or organization.
    pub owner: ID,
    pub description: String,
    pub query: String,
    pub draft: bool,
    pub visibility: Visibility,
}

/// The editable fields of a saved search.
#[derive(InputObject)]
pub struct SavedSearchUpdateInput {
    pub description: String,
    pub query: String,
    pub draft: bool,
}

/// Saved search queries.
#[derive(Default)]
pub struct SavedSearchesQuery;

#[Object]
impl SavedSearchesQuery {
    /// Saved searches of `owner`, or those the viewer is affiliated with
    /// (their own, their orgs', and public ones). Listing all of them
    /// needs a site admin.
    #[allow(clippy::too_many_arguments)]
    async fn saved_searches(
        &self,
        ctx: &Context<'_>,
        query: Option<String>,
        owner: Option<ID>,
        viewer_is_affiliated: Option<bool>,
        #[graphql(default = true)] include_drafts: bool,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        before: Option<String>,
        #[graphql(default)] order_by: SavedSearchesOrderBy,
    ) -> Result<SavedSearchConnection> {
        require_reader(ctx)?;
        let page = PageArgs {
            first,
            last,
            after,
            before,
        };
        page.check()?;
        let mut opts = ListSavedSearchesOptions {
            query: query.filter(|q| !q.trim().is_empty()),
            include_drafts,
            order: match order_by {
                SavedSearchesOrderBy::SavedSearchDescription => SavedSearchOrder::Description,
                SavedSearchesOrderBy::SavedSearchUpdatedAt => SavedSearchOrder::UpdatedAt,
            },
            ..Default::default()
        };
        let owner = owner.map(|id| parse_namespace_id(&id)).transpose()?;
        rules::scope_list(
            &services(ctx)?.db,
            viewer(ctx),
            owner,
            viewer_is_affiliated == Some(true),
            &mut opts,
        )
        .await
        .map_err(access_error)?;
        let store = services(ctx)?.db.saved_searches();
        // The total first (`last` counts back from the end), then the page.
        let (_, total) = store.list(&opts).await.map_err(db_error)?;
        let (start, end) = page.window(total, 50, 1000)?;
        let rows = if end > start {
            opts.offset = start;
            opts.limit = end - start;
            store.list(&opts).await.map_err(db_error)?.0
        } else {
            Vec::new()
        };
        let end = start + rows.len() as i64;
        Ok(SavedSearchConnection {
            nodes: rows.into_iter().map(SavedSearchNode).collect(),
            total_count: to_int(total),
            page_info: ConnectionPageInfo::new(start, end, total),
        })
    }
}

/// Saved search mutations.
#[derive(Default)]
pub struct SavedSearchesMutation;

#[Object]
impl SavedSearchesMutation {
    async fn create_saved_search(
        &self,
        ctx: &Context<'_>,
        input: SavedSearchInput,
    ) -> Result<SavedSearchNode> {
        let actor = require_viewer(ctx)?;
        let owner = parse_namespace_id(&input.owner)?;
        if load_namespace(ctx, owner).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        require_can_own(ctx, actor, owner).await?;
        check(&input.description, &input.query)?;
        if input.visibility == Visibility::Public {
            require_publisher(actor)?;
        }
        let store = services(ctx)?.db.saved_searches();
        let mut s = store
            .create(
                owner,
                input.description.trim(),
                input.query.trim(),
                input.draft,
                actor.user_id,
            )
            .await
            .map_err(db_error)?;
        if input.visibility == Visibility::Public {
            s = store
                .set_visibility(s.id, SavedSearchVisibility::Public, actor.user_id)
                .await
                .map_err(db_error)?;
        }
        Ok(SavedSearchNode(s))
    }

    async fn update_saved_search(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: SavedSearchUpdateInput,
    ) -> Result<SavedSearchNode> {
        let actor = require_viewer(ctx)?;
        let s = load_for_admin(ctx, actor, &id).await?;
        check(&input.description, &input.query)?;
        Ok(SavedSearchNode(
            services(ctx)?
                .db
                .saved_searches()
                .update(
                    s.id,
                    input.description.trim(),
                    input.query.trim(),
                    input.draft,
                    actor.user_id,
                )
                .await
                .map_err(db_error)?,
        ))
    }

    async fn delete_saved_search(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let s = load_for_admin(ctx, actor, &id).await?;
        services(ctx)?
            .db
            .saved_searches()
            .delete(s.id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Moves a saved search to another user or organization; the viewer
    /// must be able to act for both.
    async fn transfer_saved_search_ownership(
        &self,
        ctx: &Context<'_>,
        id: ID,
        new_owner: ID,
    ) -> Result<SavedSearchNode> {
        let actor = require_viewer(ctx)?;
        let s = load_for_admin(ctx, actor, &id).await?;
        let owner = parse_namespace_id(&new_owner)?;
        if load_namespace(ctx, owner).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        require_can_own(ctx, actor, owner).await?;
        Ok(SavedSearchNode(
            services(ctx)?
                .db
                .saved_searches()
                .transfer(s.id, owner, actor.user_id)
                .await
                .map_err(db_error)?,
        ))
    }

    /// Makes a saved search public or secret (site admins only).
    async fn change_saved_search_visibility(
        &self,
        ctx: &Context<'_>,
        id: ID,
        new_visibility: Visibility,
    ) -> Result<SavedSearchNode> {
        let actor = require_viewer(ctx)?;
        require_publisher(actor)?;
        let s = load_for_admin(ctx, actor, &id).await?;
        Ok(SavedSearchNode(
            services(ctx)?
                .db
                .saved_searches()
                .set_visibility(s.id, new_visibility.into(), actor.user_id)
                .await
                .map_err(db_error)?,
        ))
    }
}
