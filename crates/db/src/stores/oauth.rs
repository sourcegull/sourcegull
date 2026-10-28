//! Stores of the OAuth authorization server for MCP clients: `oauth_clients`,
//! `oauth_authorization_codes`, `oauth_grants` and `oauth_tokens`. Request handles, codes and
//! tokens are stored as SHA-256 hashes only.

use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::UserId;
use sqlx::PgPool;

/// How a client is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthClientKind {
    /// Registered through Dynamic Client Registration.
    Registered,
    /// Described by a client ID metadata document at its `client_id` URL.
    MetadataDocument,
}

impl OAuthClientKind {
    /// The `kind` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::MetadataDocument => "metadata_document",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "metadata_document" {
            Self::MetadataDocument
        } else {
            Self::Registered
        }
    }
}

/// An OAuth client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    /// Row id.
    pub id: i64,
    /// The `client_id` clients send.
    pub client_id: String,
    /// How the client is known.
    pub kind: OAuthClientKind,
    /// Display name from registration or the metadata document (may be empty).
    pub client_name: String,
    /// The client's home page, if given.
    pub client_uri: Option<String>,
    /// Allowed redirect URIs.
    pub redirect_uris: Vec<String>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
    /// Metadata document clients: when the document was last fetched.
    pub fetched_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ClientRow {
    id: i64,
    client_id: String,
    kind: String,
    client_name: String,
    client_uri: Option<String>,
    redirect_uris: Vec<String>,
    created_at: DateTime<Utc>,
    fetched_at: Option<DateTime<Utc>>,
}

impl From<ClientRow> for OAuthClient {
    fn from(r: ClientRow) -> Self {
        Self {
            id: r.id,
            client_id: r.client_id,
            kind: OAuthClientKind::parse(&r.kind),
            client_name: r.client_name,
            client_uri: r.client_uri,
            redirect_uris: r.redirect_uris,
            created_at: r.created_at,
            fetched_at: r.fetched_at,
        }
    }
}

const CLIENT_COLUMNS: &str =
    "id, client_id, kind, client_name, client_uri, redirect_uris, created_at, fetched_at";

/// Fields of a client to store.
#[derive(Debug, Clone)]
pub struct NewOAuthClient<'a> {
    /// The `client_id`.
    pub client_id: &'a str,
    /// Display name (may be empty).
    pub client_name: &'a str,
    /// Home page.
    pub client_uri: Option<&'a str>,
    /// Allowed redirect URIs.
    pub redirect_uris: &'a [String],
}

/// An authorization request, pending consent or approved (then it holds a code).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthAuthorization {
    /// Row id.
    pub id: i64,
    /// The client (row id).
    pub client_id: i64,
    /// The user who approved it.
    pub user_id: Option<UserId>,
    /// Where the code goes.
    pub redirect_uri: String,
    /// The client's `state`, returned with the code.
    pub state: Option<String>,
    /// The PKCE `S256` challenge.
    pub code_challenge: String,
    /// The resource the tokens are for.
    pub resource: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// When the request (or, once approved, the code) expires.
    pub expires_at: DateTime<Utc>,
}

const AUTHORIZATION_COLUMNS: &str =
    "id, client_id, user_id, redirect_uri, state, code_challenge, resource, scopes, expires_at";

/// Fields of a new authorization request.
#[derive(Debug, Clone)]
pub struct NewOAuthAuthorization<'a> {
    /// SHA-256 of the request handle.
    pub request_hash: &'a [u8; 32],
    /// The client (row id).
    pub client_id: i64,
    /// Where the code goes.
    pub redirect_uri: &'a str,
    /// The client's `state`.
    pub state: Option<&'a str>,
    /// The PKCE `S256` challenge.
    pub code_challenge: &'a str,
    /// The resource the tokens are for.
    pub resource: &'a str,
    /// Requested scopes.
    pub scopes: &'a [String],
    /// When the request expires if the user does not answer.
    pub expires_at: DateTime<Utc>,
}

/// A token to store: its hash and expiry.
#[derive(Debug, Clone, Copy)]
pub struct NewOAuthToken<'a> {
    /// SHA-256 of the token.
    pub hash: &'a [u8; 32],
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// What a live access token stands for.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthAccessRecord {
    /// The grant.
    pub grant_id: i64,
    /// The user the token acts as.
    pub user_id: UserId,
    /// The client's `client_id`.
    pub client_id: String,
    /// The resource the token is bound to.
    pub resource: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
}

/// A grant as listed in Connected apps.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthGrant {
    /// Row id.
    pub id: i64,
    /// The user who granted it.
    pub user_id: UserId,
    /// The client's `client_id`.
    pub client_id: String,
    /// The client's display name (may be empty).
    pub client_name: String,
    /// The client's home page.
    pub client_uri: Option<String>,
    /// The client's redirect URIs.
    pub redirect_uris: Vec<String>,
    /// The resource the grant's tokens are bound to.
    pub resource: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// When the user approved it.
    pub created_at: DateTime<Utc>,
    /// When one of its tokens was last used.
    pub last_used_at: Option<DateTime<Utc>>,
}

const GRANT_COLUMNS: &str = "g.id, g.user_id, c.client_id, c.client_name, c.client_uri, \
     c.redirect_uris, g.resource, g.scopes, g.created_at, g.last_used_at";

/// The outcome of [`OAuthStore::rotate_refresh_token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The refresh token was exchanged for the new tokens.
    Rotated {
        /// The grant.
        grant_id: i64,
        /// Its user.
        user_id: UserId,
        /// Its resource.
        resource: String,
        /// Its scopes.
        scopes: Vec<String>,
    },
    /// The refresh token had been exchanged before: the grant is now revoked.
    Reused {
        /// The revoked grant.
        grant_id: i64,
        /// Its user.
        user_id: UserId,
    },
    /// Unknown, expired, revoked or another client's token.
    Invalid,
}

/// Reads and writes the OAuth server's tables.
#[derive(Debug, Clone)]
pub struct OAuthStore {
    pool: PgPool,
}

impl OAuthStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The client with `client_id`.
    pub async fn client(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let row: Option<ClientRow> = sqlx::query_as(&format!(
            "SELECT {CLIENT_COLUMNS} FROM oauth_clients WHERE client_id = $1"
        ))
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// The client with row id `id`.
    pub async fn client_by_id(&self, id: i64) -> Result<Option<OAuthClient>> {
        let row: Option<ClientRow> = sqlx::query_as(&format!(
            "SELECT {CLIENT_COLUMNS} FROM oauth_clients WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Stores a client registered from `ip` through Dynamic Client Registration.
    pub async fn register_client(
        &self,
        new: &NewOAuthClient<'_>,
        ip: Option<&str>,
    ) -> Result<OAuthClient> {
        let row: ClientRow = sqlx::query_as(&format!(
            "INSERT INTO oauth_clients
                 (client_id, kind, client_name, client_uri, redirect_uris, registered_ip)
             VALUES ($1, 'registered', $2, $3, $4, $5::inet)
             RETURNING {CLIENT_COLUMNS}"
        ))
        .bind(new.client_id)
        .bind(new.client_name)
        .bind(new.client_uri)
        .bind(new.redirect_uris)
        .bind(ip)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    /// Registrations since `since`: from `ip`, and in total.
    pub async fn registrations_since(
        &self,
        since: DateTime<Utc>,
        ip: Option<&str>,
    ) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT count(*) FILTER (WHERE registered_ip = $2::inet), count(*)
             FROM oauth_clients WHERE kind = 'registered' AND created_at > $1",
        )
        .bind(since)
        .bind(ip)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Deletes registered clients created before `before` that never got a grant. Returns how
    /// many were deleted.
    pub async fn delete_unused_registered_clients(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM oauth_clients c
             WHERE c.kind = 'registered' AND c.created_at < $1
               AND NOT EXISTS (SELECT 1 FROM oauth_grants g WHERE g.client_id = c.id)",
        )
        .bind(before)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Deletes metadata document clients last fetched before `before` that have no live
    /// grant (their revoked grants go with them). Returns how many were deleted.
    pub async fn delete_unused_metadata_clients(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM oauth_clients c
             WHERE c.kind = 'metadata_document' AND c.fetched_at < $1
               AND NOT EXISTS (SELECT 1 FROM oauth_grants g
                               WHERE g.client_id = c.id AND g.revoked_at IS NULL)",
        )
        .bind(before)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Deletes authorization requests and codes that expired before `before`: requests never
    /// answered and codes used or not. Returns how many were deleted.
    pub async fn delete_expired_authorizations(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query("DELETE FROM oauth_authorization_codes WHERE expires_at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Creates or refreshes the client described by the metadata document at `client_id`.
    pub async fn upsert_metadata_client(&self, new: &NewOAuthClient<'_>) -> Result<OAuthClient> {
        let row: ClientRow = sqlx::query_as(&format!(
            "INSERT INTO oauth_clients
                 (client_id, kind, client_name, client_uri, redirect_uris, fetched_at)
             VALUES ($1, 'metadata_document', $2, $3, $4, now())
             ON CONFLICT (client_id) DO UPDATE SET
                 client_name = excluded.client_name, client_uri = excluded.client_uri,
                 redirect_uris = excluded.redirect_uris, fetched_at = now()
             WHERE oauth_clients.kind = 'metadata_document'
             RETURNING {CLIENT_COLUMNS}"
        ))
        .bind(new.client_id)
        .bind(new.client_name)
        .bind(new.client_uri)
        .bind(new.redirect_uris)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DbError::Invalid(
            "client_id is taken by a registered client",
        ))?;
        Ok(row.into())
    }

    /// Stores a new authorization request.
    pub async fn create_authorization(&self, new: &NewOAuthAuthorization<'_>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO oauth_authorization_codes
                 (request_sha256, client_id, redirect_uri, state, code_challenge, resource,
                  scopes, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(&new.request_hash[..])
        .bind(new.client_id)
        .bind(new.redirect_uri)
        .bind(new.state)
        .bind(new.code_challenge)
        .bind(new.resource)
        .bind(new.scopes)
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The request with handle hash `request_hash`, while it waits for consent.
    pub async fn pending_authorization(
        &self,
        request_hash: &[u8; 32],
    ) -> Result<Option<OAuthAuthorization>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {AUTHORIZATION_COLUMNS} FROM oauth_authorization_codes
             WHERE request_sha256 = $1 AND approved_at IS NULL AND expires_at > now()"
        ))
        .bind(&request_hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Approves a pending request as `user`, attaching the code with hash `code_hash` that
    /// expires at `code_expires_at`. `None` if the request is not pending (any more).
    pub async fn approve_authorization(
        &self,
        request_hash: &[u8; 32],
        user: UserId,
        code_hash: &[u8; 32],
        code_expires_at: DateTime<Utc>,
    ) -> Result<Option<OAuthAuthorization>> {
        Ok(sqlx::query_as(&format!(
            "UPDATE oauth_authorization_codes
             SET user_id = $2, code_sha256 = $3, approved_at = now(), expires_at = $4
             WHERE request_sha256 = $1 AND approved_at IS NULL AND expires_at > now()
             RETURNING {AUTHORIZATION_COLUMNS}"
        ))
        .bind(&request_hash[..])
        .bind(user)
        .bind(&code_hash[..])
        .bind(code_expires_at)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Deletes a pending request the user denied, returning it.
    pub async fn deny_authorization(
        &self,
        request_hash: &[u8; 32],
    ) -> Result<Option<OAuthAuthorization>> {
        Ok(sqlx::query_as(&format!(
            "DELETE FROM oauth_authorization_codes
             WHERE request_sha256 = $1 AND approved_at IS NULL AND expires_at > now()
             RETURNING {AUTHORIZATION_COLUMNS}"
        ))
        .bind(&request_hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Marks the live, unused code with hash `code_hash` used and returns its authorization.
    pub async fn take_code(&self, code_hash: &[u8; 32]) -> Result<Option<OAuthAuthorization>> {
        Ok(sqlx::query_as(&format!(
            "UPDATE oauth_authorization_codes SET used_at = now()
             WHERE code_sha256 = $1 AND used_at IS NULL AND user_id IS NOT NULL
               AND expires_at > now()
             RETURNING {AUTHORIZATION_COLUMNS}"
        ))
        .bind(&code_hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Handles a code with hash `code_hash` that was already used: marks it replayed and
    /// revokes the grants created from it (RFC 6749 §4.1.2). Returns the revoked grants with
    /// their users; empty if the code is unknown or no grant came from it.
    pub async fn revoke_replayed_code(&self, code_hash: &[u8; 32]) -> Result<Vec<(i64, UserId)>> {
        let mut tx = self.pool.begin().await?;
        let id: Option<i64> = sqlx::query_scalar(
            "UPDATE oauth_authorization_codes SET replayed_at = coalesce(replayed_at, now())
             WHERE code_sha256 = $1 AND used_at IS NOT NULL
             RETURNING id",
        )
        .bind(&code_hash[..])
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(Vec::new());
        };
        // A grant being created from this code commits before this statement runs (it locks
        // the code's row, which the update above waits for), or sees `replayed_at` and fails.
        let revoked = sqlx::query_as(
            "UPDATE oauth_grants SET revoked_at = now()
             WHERE authorization_id = $1 AND revoked_at IS NULL
             RETURNING id, user_id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revoked)
    }

    /// Creates a grant with its first access and refresh tokens, returning the grant id. Fails
    /// with [`DbError::NotFound`] if the code was replayed meanwhile
    /// ([`Self::revoke_replayed_code`]).
    pub async fn create_grant(
        &self,
        authorization: &OAuthAuthorization,
        user: UserId,
        access: NewOAuthToken<'_>,
        refresh: NewOAuthToken<'_>,
    ) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let live: Option<bool> = sqlx::query_scalar(
            "SELECT replayed_at IS NULL FROM oauth_authorization_codes WHERE id = $1 FOR UPDATE",
        )
        .bind(authorization.id)
        .fetch_optional(&mut *tx)
        .await?;
        if live != Some(true) {
            return Err(DbError::NotFound("OAuth authorization code"));
        }
        let grant: i64 = sqlx::query_scalar(
            "INSERT INTO oauth_grants (client_id, user_id, resource, scopes, authorization_id)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(authorization.client_id)
        .bind(user)
        .bind(&authorization.resource)
        .bind(&authorization.scopes)
        .bind(authorization.id)
        .fetch_one(&mut *tx)
        .await?;
        insert_tokens(&mut tx, grant, access, refresh).await?;
        tx.commit().await?;
        Ok(grant)
    }

    /// Exchanges the refresh token with hash `old` of client `client_id` (row id) for new
    /// tokens. A refresh token exchanged before revokes its grant (the token was copied).
    pub async fn rotate_refresh_token(
        &self,
        old: &[u8; 32],
        client_id: i64,
        access: NewOAuthToken<'_>,
        refresh: NewOAuthToken<'_>,
    ) -> Result<RefreshOutcome> {
        let mut tx = self.pool.begin().await?;
        let row: Option<RefreshRow> = sqlx::query_as(
            "SELECT t.id, g.id, g.user_id, g.resource, g.scopes, t.used_at IS NOT NULL,
                        t.expires_at > now(), g.client_id = $2
                 FROM oauth_tokens t
                 JOIN oauth_grants g ON g.id = t.grant_id
                 JOIN users u ON u.id = g.user_id
                 WHERE t.value_sha256 = $1 AND t.kind = 'refresh' AND g.revoked_at IS NULL
                   AND u.deleted_at IS NULL
                 FOR UPDATE OF t, g",
        )
        .bind(&old[..])
        .bind(client_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((token, grant, user, resource, scopes, used, live, same_client)) = row else {
            return Ok(RefreshOutcome::Invalid);
        };
        if !same_client || !live {
            return Ok(RefreshOutcome::Invalid);
        }
        if used {
            sqlx::query("UPDATE oauth_grants SET revoked_at = now() WHERE id = $1")
                .bind(grant)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(RefreshOutcome::Reused {
                grant_id: grant,
                user_id: user,
            });
        }
        sqlx::query("UPDATE oauth_tokens SET used_at = now() WHERE id = $1")
            .bind(token)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM oauth_tokens WHERE grant_id = $1 AND expires_at < now()")
            .bind(grant)
            .execute(&mut *tx)
            .await?;
        insert_tokens(&mut tx, grant, access, refresh).await?;
        tx.commit().await?;
        Ok(RefreshOutcome::Rotated {
            grant_id: grant,
            user_id: user,
            resource,
            scopes,
        })
    }

    /// The live access token with hash `hash`: not expired, its grant not revoked and its user
    /// not deleted.
    pub async fn access_token(&self, hash: &[u8; 32]) -> Result<Option<OAuthAccessRecord>> {
        Ok(sqlx::query_as(
            "SELECT g.id AS grant_id, g.user_id, c.client_id, g.resource, g.scopes
             FROM oauth_tokens t
             JOIN oauth_grants g ON g.id = t.grant_id
             JOIN oauth_clients c ON c.id = g.client_id
             JOIN users u ON u.id = g.user_id
             WHERE t.value_sha256 = $1 AND t.kind = 'access' AND t.expires_at > now()
               AND g.revoked_at IS NULL AND u.deleted_at IS NULL",
        )
        .bind(&hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records use of a grant. Debounced: only writes when the stored value is unset or older
    /// than one minute.
    pub async fn touch_grant(&self, grant: i64) -> Result<()> {
        sqlx::query(
            "UPDATE oauth_grants SET last_used_at = now()
             WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(grant)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revokes the grant of the token with hash `hash` (access or refresh), if the token
    /// belongs to client `client_id` (row id). Returns the grant id and its user.
    pub async fn revoke_by_token(
        &self,
        hash: &[u8; 32],
        client_id: i64,
    ) -> Result<Option<(i64, UserId)>> {
        Ok(sqlx::query_as(
            "UPDATE oauth_grants g SET revoked_at = now()
             FROM oauth_tokens t
             WHERE t.value_sha256 = $1 AND t.grant_id = g.id AND g.client_id = $2
               AND g.revoked_at IS NULL
             RETURNING g.id, g.user_id",
        )
        .bind(&hash[..])
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// A live grant by id.
    pub async fn grant(&self, id: i64) -> Result<Option<OAuthGrant>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {GRANT_COLUMNS} FROM oauth_grants g JOIN oauth_clients c ON c.id = g.client_id
             WHERE g.id = $1 AND g.revoked_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Live grants of `user` (every user's with `None`), newest first.
    pub async fn list_grants(&self, user: Option<UserId>) -> Result<Vec<OAuthGrant>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {GRANT_COLUMNS} FROM oauth_grants g JOIN oauth_clients c ON c.id = g.client_id
             JOIN users u ON u.id = g.user_id
             WHERE g.revoked_at IS NULL AND u.deleted_at IS NULL
               AND ($1::bigint IS NULL OR g.user_id = $1)
             ORDER BY g.created_at DESC, g.id DESC"
        ))
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Revokes a live grant. Fails with [`DbError::NotFound`] if nothing matched.
    pub async fn revoke_grant(&self, id: i64) -> Result<()> {
        let r = sqlx::query(
            "UPDATE oauth_grants SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("OAuth grant"));
        }
        Ok(())
    }

    /// Revokes every live grant of `user`, returning how many were revoked.
    pub async fn revoke_grants_for_user(&self, user: UserId) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE oauth_grants SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

/// A refresh token being exchanged: token id, grant id, user, resource, scopes, whether it
/// was used, whether it is live, and whether it belongs to the client.
type RefreshRow = (i64, i64, UserId, String, Vec<String>, bool, bool, bool);

async fn insert_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    grant: i64,
    access: NewOAuthToken<'_>,
    refresh: NewOAuthToken<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO oauth_tokens (grant_id, kind, value_sha256, expires_at)
         VALUES ($1, 'access', $2, $3), ($1, 'refresh', $4, $5)",
    )
    .bind(grant)
    .bind(&access.hash[..])
    .bind(access.expires_at)
    .bind(&refresh.hash[..])
    .bind(refresh.expires_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
