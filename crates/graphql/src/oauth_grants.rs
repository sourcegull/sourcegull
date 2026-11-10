//! Connected apps: the OAuth grants users gave MCP clients. `User.oauthGrants` lists a user's,
//! `connectedApps` pages through every user's (site admins), and `revokeOAuthGrant` ends one.
//!
//! Authorization is enforced by [`Authenticator`](sourcegull_authn::Authenticator): users see and
//! revoke their own grants, site admins anyone's.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_db::{ListOAuthGrantsOptions, OAuthGrant, OAuthGrantOrder};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

use crate::EmptyResponse;
use crate::context::{
    auth_error, codes, db_error, error, page_size, require_site_admin, require_viewer, services,
    to_int,
};
use crate::pagination::{PageInfo, decode_cursor};
use crate::users::{UserNode, load_user};

/// Node kind of OAuth grant IDs.
pub const OAUTH_GRANT_KIND: &str = "OAuthGrant";

/// An app (MCP client) a user allowed to act as them through OAuth.
pub struct OAuthGrantNode(pub OAuthGrant);

#[Object(name = "OAuthGrant")]
impl OAuthGrantNode {
    /// The opaque node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(OAUTH_GRANT_KIND, self.0.id))
    }

    /// The client's `client_id`: assigned at registration, or the URL of its metadata document.
    #[graphql(name = "clientID")]
    async fn client_id(&self) -> &str {
        &self.0.client_id
    }

    /// The name the client gave itself (may be empty).
    async fn client_name(&self) -> &str {
        &self.0.client_name
    }

    /// The client's home page, if it gave one.
    #[graphql(name = "clientURI")]
    async fn client_uri(&self) -> Option<&str> {
        self.0.client_uri.as_deref()
    }

    /// Hosts of the client's redirect URIs.
    async fn redirect_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .0
            .redirect_uris
            .iter()
            .filter_map(|u| url::Url::parse(u).ok()?.host_str().map(str::to_owned))
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// The granted scopes (`mcp`).
    async fn scopes(&self) -> &[String] {
        &self.0.scopes
    }

    /// The resource the grant's tokens work at (the MCP endpoint).
    async fn resource(&self) -> &str {
        &self.0.resource
    }

    /// When the user allowed it.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the client last used one of its tokens, if ever.
    async fn last_used_at(&self) -> Option<DateTime<Utc>> {
        self.0.last_used_at
    }

    /// The user who allowed it.
    async fn user(&self, ctx: &Context<'_>) -> Result<UserNode> {
        load_user(ctx, self.0.user_id).await.map(UserNode)
    }
}

/// Sort order of `connectedApps`.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
#[graphql(name = "OAuthGrantOrderBy")]
pub enum OAuthGrantOrderBy {
    /// By when the user allowed the app.
    #[default]
    CreatedAt,
    /// By the user's username.
    Username,
    /// By the app's name.
    App,
    /// By last use; apps never used count as the oldest.
    LastUsedAt,
}

/// A page of connected apps.
#[derive(SimpleObject)]
#[graphql(name = "OAuthGrantConnection")]
pub struct OAuthGrantConnection {
    /// The connected apps on this page.
    pub nodes: Vec<OAuthGrantNode>,
    /// All connected apps matching the query.
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Every user's connected apps.
#[derive(Default)]
pub struct OAuthGrantsQuery;

#[Object]
impl OAuthGrantsQuery {
    /// The connected apps of every user, newest first (site admins only).
    #[graphql(deprecation = "Use `connectedApps`, which pages.")]
    async fn oauth_grants(&self, ctx: &Context<'_>) -> Result<Vec<OAuthGrantNode>> {
        let actor = require_viewer(ctx)?;
        let grants = services(ctx)?
            .auth
            .list_oauth_grants(actor, None)
            .await
            .map_err(auth_error)?;
        Ok(grants.into_iter().map(OAuthGrantNode).collect())
    }

    /// A page of every user's connected apps (site admins only). `query` matches a substring of
    /// the username, the app's name or its client ID.
    async fn connected_apps(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        #[graphql(desc = "Default: `CREATED_AT`.")] order_by: Option<OAuthGrantOrderBy>,
        #[graphql(desc = "Default: true (newest first).")] descending: Option<bool>,
    ) -> Result<OAuthGrantConnection> {
        require_site_admin(ctx)?;
        // Explicit nulls mean the defaults.
        let order_by = order_by.unwrap_or_default();
        let descending = descending.unwrap_or(true);
        let db = &services(ctx)?.db;
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let opts = ListOAuthGrantsOptions {
            query: query.filter(|q| !q.trim().is_empty()),
            order: match order_by {
                OAuthGrantOrderBy::CreatedAt => OAuthGrantOrder::CreatedAt,
                OAuthGrantOrderBy::Username => OAuthGrantOrder::Username,
                OAuthGrantOrderBy::App => OAuthGrantOrder::App,
                OAuthGrantOrderBy::LastUsedAt => OAuthGrantOrder::LastUsedAt,
            },
            descending,
            limit: limit + 1,
            offset,
        };
        let grants = db.oauth().list_grants_page(&opts).await.map_err(db_error)?;
        let total = db.oauth().count_grants(&opts).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, grants.len());
        Ok(OAuthGrantConnection {
            nodes: grants
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(OAuthGrantNode)
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }
}

/// Connected app mutations.
#[derive(Default)]
pub struct OAuthGrantsMutation;

#[Object]
impl OAuthGrantsMutation {
    /// Revokes a connected app: its tokens stop working at once.
    #[graphql(name = "revokeOAuthGrant")]
    async fn revoke_oauth_grant(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let grant = unmarshal_int_id(OAUTH_GRANT_KIND, &id).ok_or_else(|| {
            error(
                codes::INVALID_INPUT,
                format!("Invalid connected app ID {:?}.", id.as_str()),
            )
        })?;
        services(ctx)?
            .auth
            .revoke_oauth_grant(actor, grant)
            .await
            .map_err(auth_error)?;
        Ok(EmptyResponse)
    }
}
