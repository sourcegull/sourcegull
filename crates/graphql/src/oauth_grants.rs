//! Connected apps: the OAuth grants users gave MCP clients. `User.oauthGrants` lists a user's,
//! `oauthGrants` every user's (site admins), and `revokeOAuthGrant` ends one.
//!
//! Authorization is enforced by [`Authenticator`](sourcegull_authn::Authenticator): users see and
//! revoke their own grants, site admins anyone's.

use async_graphql::{Context, ID, Object, Result};
use chrono::{DateTime, Utc};
use sourcegull_db::OAuthGrant;
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

use crate::EmptyResponse;
use crate::context::{auth_error, codes, error, require_viewer, services};
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

/// Every user's connected apps.
#[derive(Default)]
pub struct OAuthGrantsQuery;

#[Object]
impl OAuthGrantsQuery {
    /// The connected apps of every user, newest first (site admins only).
    async fn oauth_grants(&self, ctx: &Context<'_>) -> Result<Vec<OAuthGrantNode>> {
        let actor = require_viewer(ctx)?;
        let grants = services(ctx)?
            .auth
            .list_oauth_grants(actor, None)
            .await
            .map_err(auth_error)?;
        Ok(grants.into_iter().map(OAuthGrantNode).collect())
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
