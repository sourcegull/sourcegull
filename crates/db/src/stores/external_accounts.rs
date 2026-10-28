//! `user_external_accounts` and `oauth_flows`: users' code host accounts,
//! their OAuth tokens (encrypted with the `oauth_token` data key), and the OAuth
//! authorization-code flows in progress.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_types::{ExternalAccountId, UserId};
use sqlx::PgPool;

use crate::{DbError, Result};

const COLUMNS: &str = "id, user_id, service_type, service_id, client_id, account_id, \
     account_data, auth_data_enc, expired_at, created_at, updated_at";

/// A linked account. `auth_data` is the decrypted token JSON, when the account has tokens.
#[derive(Clone, PartialEq)]
pub struct ExternalAccount {
    pub id: ExternalAccountId,
    pub user_id: UserId,
    pub service_type: String,
    pub service_id: String,
    pub client_id: String,
    pub account_id: String,
    pub account_data: Value,
    pub auth_data: Option<Vec<u8>>,
    pub expired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for ExternalAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccount")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("service_type", &self.service_type)
            .field("service_id", &self.service_id)
            .field("account_id", &self.account_id)
            .field("auth_data", &self.auth_data.as_ref().map(|_| "<redacted>"))
            .field("expired_at", &self.expired_at)
            .finish_non_exhaustive()
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: ExternalAccountId,
    user_id: UserId,
    service_type: String,
    service_id: String,
    client_id: String,
    account_id: String,
    account_data: Value,
    auth_data_enc: Option<Vec<u8>>,
    expired_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// A linked account without its tokens (for listings that need no keyring).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ExternalAccountSummary {
    pub id: ExternalAccountId,
    pub user_id: UserId,
    pub service_type: String,
    pub service_id: String,
    pub client_id: String,
    pub account_id: String,
    pub account_data: Value,
    /// Whether tokens are stored.
    pub has_token: bool,
    pub expired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Identifies a code host account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountSpec<'a> {
    /// `gitlab`.
    pub service_type: &'a str,
    /// The code host instance (normalized base URL).
    pub service_id: &'a str,
    /// The account id on the code host.
    pub account_id: &'a str,
}

/// Fields for [`ExternalAccountStore::link`].
#[derive(Clone)]
pub struct LinkAccount<'a> {
    pub spec: AccountSpec<'a>,
    /// The OAuth application of `auth_data` ('' without tokens).
    pub client_id: &'a str,
    pub account_data: Value,
    /// Token JSON (plaintext; encrypted here). `None` keeps the stored tokens.
    pub auth_data: Option<&'a [u8]>,
}

/// An OAuth flow in progress.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthFlow {
    pub provider: String,
    pub pkce_verifier: String,
    pub return_to: String,
    pub nonce: Option<String>,
}

/// Reads and writes `user_external_accounts` and `oauth_flows`. Token reads and writes need
/// the keyring; without one they fail with [`DbError::Invalid`].
#[derive(Debug, Clone)]
pub struct ExternalAccountStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl ExternalAccountStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool, keyring: Option<Arc<Keyring>>) -> Self {
        Self { pool, keyring }
    }

    fn keyring(&self) -> Result<&Keyring> {
        self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)",
        ))
    }

    fn decode(&self, r: Row) -> Result<ExternalAccount> {
        let auth_data = match r.auth_data_enc {
            Some(blob) => Some(
                self.keyring()?
                    .decrypt(RecordClass::OAuthToken, &blob)?
                    .to_vec(),
            ),
            None => None,
        };
        Ok(ExternalAccount {
            id: r.id,
            user_id: r.user_id,
            service_type: r.service_type,
            service_id: r.service_id,
            client_id: r.client_id,
            account_id: r.account_id,
            account_data: r.account_data,
            auth_data,
            expired_at: r.expired_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }

    /// The live user who owns the account, if it is linked.
    pub async fn user_for(&self, spec: AccountSpec<'_>) -> Result<Option<UserId>> {
        Ok(sqlx::query_scalar(
            "SELECT a.user_id FROM user_external_accounts a
             JOIN users u ON u.id = a.user_id AND u.deleted_at IS NULL
             WHERE a.service_type = $1 AND a.service_id = $2 AND a.account_id = $3
               AND a.deleted_at IS NULL",
        )
        .bind(spec.service_type)
        .bind(spec.service_id)
        .bind(spec.account_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Links the account to `user` (or updates it when already linked to them), storing
    /// `account_data` and, when given, new tokens (which also clears `expired_at`). An account
    /// linked to a deleted user moves to `user`; one linked to another live user fails with
    /// [`DbError::AlreadyExists`].
    pub async fn link(&self, user: UserId, a: &LinkAccount<'_>) -> Result<ExternalAccountId> {
        let enc = match a.auth_data {
            Some(plain) => Some(self.keyring()?.encrypt(RecordClass::OAuthToken, plain)?),
            None => None,
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE user_external_accounts a SET deleted_at = now()
             FROM users u
             WHERE u.id = a.user_id AND u.deleted_at IS NOT NULL
               AND a.service_type = $1 AND a.service_id = $2 AND a.account_id = $3
               AND a.deleted_at IS NULL",
        )
        .bind(a.spec.service_type)
        .bind(a.spec.service_id)
        .bind(a.spec.account_id)
        .execute(&mut *tx)
        .await?;
        let id: Option<ExternalAccountId> = sqlx::query_scalar(
            "INSERT INTO user_external_accounts
                 (user_id, service_type, service_id, client_id, account_id, account_data,
                  auth_data_enc)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (service_type, service_id, account_id) WHERE deleted_at IS NULL
             DO UPDATE SET account_data = EXCLUDED.account_data, updated_at = now(),
                 client_id = CASE WHEN EXCLUDED.auth_data_enc IS NULL
                     THEN user_external_accounts.client_id ELSE EXCLUDED.client_id END,
                 auth_data_enc = coalesce(EXCLUDED.auth_data_enc,
                     user_external_accounts.auth_data_enc),
                 expired_at = CASE WHEN EXCLUDED.auth_data_enc IS NULL
                     THEN user_external_accounts.expired_at END
             WHERE user_external_accounts.user_id = EXCLUDED.user_id
             RETURNING id",
        )
        .bind(user)
        .bind(a.spec.service_type)
        .bind(a.spec.service_id)
        .bind(a.client_id)
        .bind(a.spec.account_id)
        .bind(&a.account_data)
        .bind(enc)
        .fetch_optional(&mut *tx)
        .await?;
        let id = id.ok_or_else(|| DbError::AlreadyExists {
            constraint: "user_external_accounts_account_key".into(),
        })?;
        tx.commit().await?;
        Ok(id)
    }

    /// The live accounts of `user`, oldest first.
    pub async fn list_for_user(&self, user: UserId) -> Result<Vec<ExternalAccount>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM user_external_accounts
             WHERE user_id = $1 AND deleted_at IS NULL ORDER BY id"
        ))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| self.decode(r)).collect()
    }

    /// The live accounts of `user` without their tokens, oldest first.
    pub async fn list_summaries(&self, user: UserId) -> Result<Vec<ExternalAccountSummary>> {
        Ok(sqlx::query_as(
            "SELECT id, user_id, service_type, service_id, client_id, account_id, account_data,
                    auth_data_enc IS NOT NULL AS has_token, expired_at, created_at, updated_at
             FROM user_external_accounts
             WHERE user_id = $1 AND deleted_at IS NULL ORDER BY id",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The live account of `user` on a code host instance.
    pub async fn get_for_user(
        &self,
        user: UserId,
        service_type: &str,
        service_id: &str,
    ) -> Result<Option<ExternalAccount>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM user_external_accounts
             WHERE user_id = $1 AND service_type = $2 AND service_id = $3 AND deleted_at IS NULL"
        ))
        .bind(user)
        .bind(service_type)
        .bind(service_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| self.decode(r)).transpose()
    }

    /// The live user owning an account on a code host instance, found by its account ID or
    /// by its login (`account_data.username`, case-insensitive); `None` when neither is given.
    pub async fn user_by_account_or_login(
        &self,
        service_type: &str,
        service_id: &str,
        account_id: Option<&str>,
        login: Option<&str>,
    ) -> Result<Option<UserId>> {
        if account_id.is_none() && login.is_none() {
            return Ok(None);
        }
        Ok(sqlx::query_scalar(
            "SELECT a.user_id FROM user_external_accounts a
             JOIN users u ON u.id = a.user_id AND u.deleted_at IS NULL
             WHERE a.service_type = $1 AND a.service_id = $2 AND a.deleted_at IS NULL
               AND (a.account_id = $3 OR lower(a.account_data->>'username') = lower($4))
             LIMIT 1",
        )
        .bind(service_type)
        .bind(service_id)
        .bind(account_id)
        .bind(login)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The live users owning `account_ids` on a code host instance, as `(account_id, user)`.
    pub async fn users_for_accounts(
        &self,
        service_type: &str,
        service_id: &str,
        account_ids: &[String],
    ) -> Result<Vec<(String, UserId)>> {
        Ok(sqlx::query_as(
            "SELECT a.account_id, a.user_id FROM user_external_accounts a
             JOIN users u ON u.id = a.user_id AND u.deleted_at IS NULL
             WHERE a.service_type = $1 AND a.service_id = $2 AND a.account_id = ANY($3)
               AND a.deleted_at IS NULL",
        )
        .bind(service_type)
        .bind(service_id)
        .bind(account_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replaces the tokens of an account (after a refresh) and clears `expired_at`.
    pub async fn set_auth_data(&self, id: ExternalAccountId, auth_data: &[u8]) -> Result<()> {
        let enc = self
            .keyring()?
            .encrypt(RecordClass::OAuthToken, auth_data)?;
        sqlx::query(
            "UPDATE user_external_accounts
             SET auth_data_enc = $2, expired_at = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(enc)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks an account's tokens as no longer working.
    pub async fn mark_expired(&self, id: ExternalAccountId) -> Result<()> {
        sqlx::query(
            "UPDATE user_external_accounts SET expired_at = now(), updated_at = now()
             WHERE id = $1 AND expired_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Starts an OAuth flow: stores the PKCE verifier under the hash of the state.
    pub async fn create_flow(
        &self,
        state_hash: &[u8],
        flow: &OAuthFlow,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_flows (state_hash, provider, pkce_verifier, return_to, nonce, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(state_hash)
        .bind(&flow.provider)
        .bind(&flow.pkce_verifier)
        .bind(&flow.return_to)
        .bind(&flow.nonce)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ends the unexpired flow with this state hash and returns it (each flow can be taken
    /// once).
    pub async fn take_flow(&self, state_hash: &[u8]) -> Result<Option<OAuthFlow>> {
        Ok(sqlx::query_as(
            "DELETE FROM oauth_flows WHERE state_hash = $1 AND expires_at > now()
             RETURNING provider, pkce_verifier, return_to, nonce",
        )
        .bind(state_hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Deletes expired flows. Returns the number deleted.
    pub async fn delete_expired_flows(&self) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM oauth_flows WHERE expires_at <= now()")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}
