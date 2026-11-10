//! Personal access tokens: the `AccessToken` type, `accessTokens` and `accessTokenCounts` (every
//! user's, site admins only), `createAccessToken`, `deleteAccessToken`.
//!
//! Authorization is enforced by [`Authenticator`](sourcegull_authn::Authenticator): users see and
//! manage their own tokens, site admins anyone's (subject to `auth.accessTokens`).

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_db::{AccessToken, AccessTokenOrder, AccessTokenState, ListAccessTokensOptions};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::AccessTokenId;

use crate::EmptyResponse;
use crate::context::{
    auth_error, codes, db_error, error, page_size, require_site_admin, require_viewer, services,
    to_int,
};
use crate::pagination::{PageInfo, decode_cursor};
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

    /// The first characters of the token after `skp_`, to tell tokens apart (not secret).
    async fn prefix(&self) -> &str {
        &self.0.prefix
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
    pub page_info: PageInfo,
}

impl AccessTokenConnection {
    /// Wraps the first tokens out of `total`; there is no next page cursor.
    pub fn new(tokens: Vec<AccessToken>, total: usize) -> Self {
        Self {
            page_info: PageInfo {
                has_next_page: tokens.len() < total,
                end_cursor: None,
            },
            nodes: tokens.into_iter().map(AccessTokenNode).collect(),
            total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        }
    }
}

/// Days without use after which `accessTokens(state: UNUSED)` lists a token.
pub const UNUSED_TOKEN_DAYS: i32 = 90;

/// Sort order of `accessTokens`.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum AccessTokenOrderBy {
    /// By when the token was created.
    #[default]
    CreatedAt,
    /// By the username of the user the token acts as.
    Username,
    /// By last use; tokens never used count as the oldest.
    LastUsedAt,
    /// By expiry; tokens that never expire count as the latest.
    ExpiresAt,
}

/// Which tokens `accessTokens` lists.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
#[graphql(name = "AccessTokenState")]
pub enum AccessTokenStateGql {
    /// Every token, expired ones included.
    #[default]
    All,
    /// Tokens past their expiry.
    Expired,
    /// Unexpired tokens not used in the last 90 days (counted from creation if never used).
    Unused,
}

/// Token totals for the whole site (tokens of deactivated users are not counted).
#[derive(SimpleObject)]
pub struct AccessTokenCounts {
    /// Every token, expired ones included.
    pub total: i32,
    /// Tokens past their expiry.
    pub expired: i32,
    /// Unexpired tokens not used in the last 90 days.
    pub unused: i32,
}

/// Every user's access tokens.
#[derive(Default)]
pub struct TokensQuery;

#[Object]
impl TokensQuery {
    /// A page of every user's access tokens (site admins only). `query` matches a substring of
    /// the username of the user a token acts as, or of its note.
    #[allow(clippy::too_many_arguments)]
    async fn access_tokens(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        #[graphql(desc = "Default: `ALL`.")] state: Option<AccessTokenStateGql>,
        #[graphql(desc = "Default: `CREATED_AT`.")] order_by: Option<AccessTokenOrderBy>,
        #[graphql(desc = "Default: true (newest first).")] descending: Option<bool>,
    ) -> Result<AccessTokenConnection> {
        require_site_admin(ctx)?;
        // Explicit nulls mean the defaults.
        let (state, order_by) = (state.unwrap_or_default(), order_by.unwrap_or_default());
        let descending = descending.unwrap_or(true);
        let db = &services(ctx)?.db;
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let opts = ListAccessTokensOptions {
            query: query.filter(|q| !q.trim().is_empty()),
            state: match state {
                AccessTokenStateGql::All => AccessTokenState::All,
                AccessTokenStateGql::Expired => AccessTokenState::Expired,
                AccessTokenStateGql::Unused => AccessTokenState::Unused,
            },
            unused_days: UNUSED_TOKEN_DAYS,
            order: match order_by {
                AccessTokenOrderBy::CreatedAt => AccessTokenOrder::CreatedAt,
                AccessTokenOrderBy::Username => AccessTokenOrder::Username,
                AccessTokenOrderBy::LastUsedAt => AccessTokenOrder::LastUsedAt,
                AccessTokenOrderBy::ExpiresAt => AccessTokenOrder::ExpiresAt,
            },
            descending,
            limit: limit + 1,
            offset,
        };
        let tokens = db.access_tokens().list(&opts).await.map_err(db_error)?;
        let total = db
            .access_tokens()
            .count_listed(&opts)
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, tokens.len());
        Ok(AccessTokenConnection {
            nodes: tokens
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(AccessTokenNode)
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }

    /// Numbers of every user's tokens: all, expired and unused (site admins only).
    async fn access_token_counts(&self, ctx: &Context<'_>) -> Result<AccessTokenCounts> {
        require_site_admin(ctx)?;
        let c = services(ctx)?
            .db
            .access_tokens()
            .counts(UNUSED_TOKEN_DAYS)
            .await
            .map_err(db_error)?;
        Ok(AccessTokenCounts {
            total: to_int(c.total),
            expired: to_int(c.expired),
            unused: to_int(c.unused),
        })
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
