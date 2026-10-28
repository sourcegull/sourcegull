//! Personal access tokens: the `AccessToken` type, `createAccessToken`, `deleteAccessToken`.
//!
//! Authorization is enforced by [`Authenticator`](sourcegull_authn::Authenticator): users see and
//! manage their own tokens, site admins anyone's (subject to `auth.accessTokens`).

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_db::AccessToken;
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::AccessTokenId;

use crate::EmptyResponse;
use crate::context::{auth_error, codes, error, require_viewer, services, to_int};
use crate::users::{UserNode, load_user, parse_user_id};

/// Node kind of access token IDs.
pub const ACCESS_TOKEN_KIND: &str = "AccessToken";

/// An access token (never its secret).
pub struct AccessTokenNode(pub AccessToken);

#[Object(name = "AccessToken")]
impl AccessTokenNode {
    /// The opaque node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(ACCESS_TOKEN_KIND, self.0.id.get()))
    }

    /// The user's note describing the token.
    async fn note(&self) -> &str {
        &self.0.note
    }

    /// The token's scopes (`user:all`, `site-admin:sudo`).
    async fn scopes(&self) -> &[String] {
        &self.0.scopes
    }

    /// When the token was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the token was last used, if ever.
    async fn last_used_at(&self) -> Option<DateTime<Utc>> {
        self.0.last_used_at
    }

    /// When the token expires, if it does.
    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.0.expires_at
    }

    /// The user the token authenticates as.
    async fn subject(&self, ctx: &Context<'_>) -> Result<UserNode> {
        load_user(ctx, self.0.subject_user_id).await.map(UserNode)
    }

    /// The user who created the token.
    async fn creator(&self, ctx: &Context<'_>) -> Result<UserNode> {
        load_user(ctx, self.0.creator_user_id).await.map(UserNode)
    }
}

/// A list of access tokens.
#[derive(SimpleObject)]
pub struct AccessTokenConnection {
    /// The tokens.
    pub nodes: Vec<AccessTokenNode>,
    /// The number of tokens in total.
    pub total_count: i32,
}

impl AccessTokenConnection {
    /// Wraps a page of tokens out of `total`.
    pub fn new(tokens: Vec<AccessToken>, total: usize) -> Self {
        Self {
            nodes: tokens.into_iter().map(AccessTokenNode).collect(),
            total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        }
    }
}

/// The result of `createAccessToken`: the secret is shown only once.
#[derive(SimpleObject)]
pub struct CreateAccessTokenResult {
    /// The new token's node ID.
    pub id: ID,
    /// The token secret (`skp_…`).
    pub token: String,
}

/// Access token mutations.
#[derive(Default)]
pub struct TokensMutation;

#[Object]
impl TokensMutation {
    /// Creates an access token for `user` (yourself, or anyone as a site admin).
    async fn create_access_token(
        &self,
        ctx: &Context<'_>,
        user: ID,
        scopes: Vec<String>,
        note: String,
    ) -> Result<CreateAccessTokenResult> {
        let actor = require_viewer(ctx)?;
        let target = parse_user_id(&user)?;
        let (id, token) = services(ctx)?
            .auth
            .create_access_token(actor, target, &scopes, note.trim(), None)
            .await
            .map_err(auth_error)?;
        Ok(CreateAccessTokenResult {
            id: ID(marshal_id(ACCESS_TOKEN_KIND, id.get())),
            token,
        })
    }

    /// Deletes (revokes) an access token by its node ID.
    async fn delete_access_token(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "byID")] by_id: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let id = unmarshal_int_id(ACCESS_TOKEN_KIND, &by_id)
            .map(AccessTokenId)
            .ok_or_else(|| {
                error(
                    codes::INVALID_INPUT,
                    format!("Invalid access token ID {:?}.", by_id.as_str()),
                )
            })?;
        services(ctx)?
            .auth
            .delete_access_token(actor, id)
            .await
            .map_err(auth_error)?;
        Ok(EmptyResponse)
    }
}
