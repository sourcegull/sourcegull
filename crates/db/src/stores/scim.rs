//! SCIM 2.0 provisioning: users managed by an identity provider through
//! `/.api/scim/v2/Users`.
//!
//! A user is SCIM-controlled when it has a live `user_external_accounts` row with service type
//! and id `scim` ([`SCIM_SERVICE`]). Its `account_id` is the provider's `externalId` (or the
//! lowercased `userName` when the provider sends none) and its `account_data` holds the SCIM
//! attributes the provider last wrote, so they can be returned as given. Deactivated
//! (`active: false`) users are soft-deleted but stay visible to SCIM through that account.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sourcegull_types::{User, UserEmail, UserId};
use sqlx::{PgConnection, PgPool};

use super::accounts::ACCOUNT_CREATION_LOCK;
use super::perms::{BindKind, grant_pending};
use super::system_roles;
use crate::{DbError, Result};

/// `service_type` and `service_id` of SCIM accounts.
pub const SCIM_SERVICE: &str = "scim";

/// Name reported in [`DbError::AlreadyExists`] when another SCIM user has the `userName`.
pub const SCIM_USER_NAME_CONSTRAINT: &str = "scim_user_name";

/// Suffixes tried when a username is taken by a user the provider does not manage.
const USERNAME_SUFFIXES: std::ops::RangeInclusive<u32> = 2..=9;

const USER_COLUMNS: &str = "u.id, u.username, u.display_name, u.avatar_url, u.site_admin, \
     u.created_at, u.updated_at, u.deleted_at, u.locked_until, u.failed_logins";

/// Visible to SCIM: live users, and soft-deleted users that are SCIM-controlled.
const VISIBLE: &str = "(u.deleted_at IS NULL OR a.id IS NOT NULL)";

const JOIN_ACCOUNT: &str = "LEFT JOIN user_external_accounts a ON a.user_id = u.id \
     AND a.service_type = 'scim' AND a.service_id = 'scim' AND a.deleted_at IS NULL";

/// The SCIM account of a user.
#[derive(Debug, Clone, PartialEq)]
pub struct ScimAccount {
    /// `externalId`, or the lowercased `userName` without one.
    pub account_id: String,
    /// The SCIM attributes last written by the provider.
    pub data: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A user as SCIM sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScimUser {
    /// The user row (`deleted_at` set when deactivated).
    pub user: User,
    /// All addresses, primary first.
    pub emails: Vec<UserEmail>,
    /// Set when the user is SCIM-controlled.
    pub account: Option<ScimAccount>,
}

/// A cheap SQL pre-filter for SCIM listings: every set field must match. It may keep users
/// the exact SCIM filter (evaluated by the caller) rejects, never the other way round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScimNarrow {
    /// Case-insensitive `userName` (the stored SCIM value, or the username).
    pub user_name: Option<String>,
    /// Exact `externalId`.
    pub external_id: Option<String>,
    /// Case-insensitive address among the user's emails or their SCIM `emails`.
    pub email: Option<String>,
}

/// The desired state of a SCIM-controlled user, written by [`ScimStore::write`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScimWrite {
    /// The provider's `userName` (unique among SCIM users, case-insensitively).
    pub scim_user_name: String,
    /// A valid Sourcegull username derived from it; a suffix is appended when another user has it.
    pub username: String,
    pub display_name: Option<String>,
    /// Addresses, primary first; all are stored verified (the provider vouches for them).
    pub emails: Vec<String>,
    /// Addresses of the previous SCIM state. Those no longer in `emails` are removed; other
    /// addresses the user has are left alone.
    pub previous_emails: Vec<String>,
    /// See [`ScimAccount::account_id`].
    pub account_id: String,
    /// The SCIM attributes to store.
    pub data: Value,
    /// `false` soft-deletes the user (ends sessions, revokes access tokens); `true` restores a
    /// soft-deleted one.
    pub active: bool,
}

/// The result of [`ScimStore::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScimWritten {
    pub user_id: UserId,
    /// The user was created.
    pub created: bool,
    /// The user went from active to inactive (sessions must be ended by the caller's session
    /// store too).
    pub deactivated: bool,
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    user_id: UserId,
    account_id: String,
    account_data: Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Reads and writes SCIM-controlled users.
#[derive(Debug, Clone)]
pub struct ScimStore {
    pool: PgPool,
}

impl ScimStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// A user visible to SCIM by id.
    pub async fn get(&self, id: UserId) -> Result<Option<ScimUser>> {
        let users: Vec<User> = sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users u {JOIN_ACCOUNT} WHERE u.id = $1 AND {VISIBLE}"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(self.hydrate(users).await?.into_iter().next())
    }

    /// Users visible to SCIM matching `narrow`, ordered by id, with the total count. `limit`
    /// `None` returns all of them from `offset`.
    pub async fn list(
        &self,
        narrow: &ScimNarrow,
        offset: i64,
        limit: Option<i64>,
    ) -> Result<(i64, Vec<ScimUser>)> {
        let cond = format!(
            "{VISIBLE}
             AND ($1::text IS NULL
                  OR lower(coalesce(a.account_data->>'userName', u.username)) = lower($1))
             AND ($2::text IS NULL OR a.account_data->>'externalId' = $2)
             AND ($3::text IS NULL
                  OR EXISTS (SELECT 1 FROM user_emails e
                             WHERE e.user_id = u.id AND lower(e.email) = lower($3))
                  OR EXISTS (SELECT 1 FROM jsonb_array_elements(
                                 CASE WHEN jsonb_typeof(a.account_data->'emails') = 'array'
                                      THEN a.account_data->'emails' ELSE '[]'::jsonb END) x
                             WHERE lower(x->>'value') = lower($3)))"
        );
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM users u {JOIN_ACCOUNT} WHERE {cond}"
        ))
        .bind(&narrow.user_name)
        .bind(&narrow.external_id)
        .bind(&narrow.email)
        .fetch_one(&self.pool)
        .await?;
        let users: Vec<User> = sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users u {JOIN_ACCOUNT} WHERE {cond}
             ORDER BY u.id OFFSET $4 LIMIT $5"
        ))
        .bind(&narrow.user_name)
        .bind(&narrow.external_id)
        .bind(&narrow.email)
        .bind(offset.max(0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok((total, self.hydrate(users).await?))
    }

    /// The distinct live users who verified any of `emails` (case-insensitive).
    pub async fn users_with_verified_emails(&self, emails: &[String]) -> Result<Vec<UserId>> {
        let lower: Vec<String> = emails.iter().map(|e| e.to_lowercase()).collect();
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT e.user_id FROM user_emails e JOIN users u ON u.id = e.user_id
             WHERE lower(e.email) = ANY($1) AND e.verified_at IS NOT NULL
               AND u.deleted_at IS NULL
             ORDER BY e.user_id",
        )
        .bind(&lower)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Creates (`id` `None`) or updates a user to the state `w`, making it SCIM-controlled.
    ///
    /// Fails with [`DbError::AlreadyExists`] when another SCIM user has the `userName`
    /// ([`SCIM_USER_NAME_CONSTRAINT`]) or the account id, another user verified one of the
    /// emails, or the username and all its suffixed variants are taken; with
    /// [`DbError::NotFound`] when `id` is not visible to SCIM.
    pub async fn write(&self, id: Option<UserId>, w: &ScimWrite) -> Result<ScimWritten> {
        let mut tx = self.pool.begin().await?;
        // Serializes with sign-ups and other SCIM writes, so the uniqueness checks below hold.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(ACCOUNT_CREATION_LOCK)
            .execute(&mut *tx)
            .await?;

        let taken: Option<UserId> = sqlx::query_scalar(
            "SELECT u.id FROM users u JOIN user_external_accounts a ON a.user_id = u.id
               AND a.service_type = 'scim' AND a.service_id = 'scim' AND a.deleted_at IS NULL
             WHERE lower(coalesce(a.account_data->>'userName', u.username)) = lower($1)
               AND ($2::bigint IS NULL OR u.id <> $2)
             LIMIT 1",
        )
        .bind(&w.scim_user_name)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if taken.is_some() {
            return Err(DbError::AlreadyExists {
                constraint: SCIM_USER_NAME_CONSTRAINT.into(),
            });
        }

        let (user_id, created, current_username, was_active) = match id {
            Some(id) => {
                let row: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(&format!(
                    "SELECT u.username, u.deleted_at FROM users u {JOIN_ACCOUNT}
                     WHERE u.id = $1 AND {VISIBLE} FOR UPDATE OF u"
                ))
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                let (username, deleted_at) = row.ok_or(DbError::NotFound("user"))?;
                (id, false, Some(username), deleted_at.is_none())
            }
            None => {
                let username = free_username(&mut tx, &w.username, None).await?;
                let id: UserId = sqlx::query_scalar(
                    "INSERT INTO users (username, display_name) VALUES ($1, $2) RETURNING id",
                )
                .bind(&username)
                .bind(&w.display_name)
                .fetch_one(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO user_roles (user_id, role_id)
                     SELECT $1, id FROM roles WHERE name = $2 AND system AND deleted_at IS NULL
                     ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(system_roles::USER)
                .execute(&mut *tx)
                .await?;
                grant_pending(&mut tx, id, BindKind::Username, &username).await?;
                (id, true, None, true)
            }
        };

        if let Some(current) = current_username {
            // Keep the username while it still derives from the same SCIM userName; when it is
            // renamed, or must be reclaimed on reactivation, pick a free variant.
            let same = current.eq_ignore_ascii_case(&w.username)
                || current
                    .to_ascii_lowercase()
                    .strip_prefix(&format!("{}-", w.username.to_ascii_lowercase()))
                    .is_some_and(|n| {
                        n.parse::<u32>()
                            .is_ok_and(|n| USERNAME_SUFFIXES.contains(&n))
                    });
            let username = if same
                && (was_active || !w.active || !username_taken(&mut tx, &current, user_id).await?)
            {
                current
            } else {
                free_username(&mut tx, &w.username, Some(user_id)).await?
            };
            sqlx::query(
                "UPDATE users SET username = $2, display_name = $3, updated_at = now(),
                     deleted_at = CASE WHEN $4 THEN NULL ELSE deleted_at END
                 WHERE id = $1",
            )
            .bind(user_id)
            .bind(&username)
            .bind(&w.display_name)
            .bind(w.active)
            .execute(&mut *tx)
            .await?;
        }

        sync_emails(&mut tx, user_id, &w.emails, &w.previous_emails).await?;

        sqlx::query(
            "INSERT INTO user_external_accounts
                 (user_id, service_type, service_id, account_id, account_data)
             VALUES ($1, 'scim', 'scim', $2, $3)
             ON CONFLICT (user_id, service_type, service_id) WHERE deleted_at IS NULL
             DO UPDATE SET account_id = EXCLUDED.account_id,
                 account_data = EXCLUDED.account_data, updated_at = now()",
        )
        .bind(user_id)
        .bind(&w.account_id)
        .bind(&w.data)
        .execute(&mut *tx)
        .await?;

        let deactivated = was_active && !w.active;
        if !w.active {
            soft_delete_in(&mut tx, user_id).await?;
        }
        tx.commit().await?;
        Ok(ScimWritten {
            user_id,
            created,
            deactivated,
        })
    }

    /// Permanently deletes a SCIM-controlled user and everything that belongs to them
    /// (sessions, tokens, emails, permissions cascade). Returns `false` when there is no such
    /// SCIM-controlled user.
    pub async fn hard_delete(&self, id: UserId) -> Result<bool> {
        let r = sqlx::query(
            "DELETE FROM users u WHERE u.id = $1 AND EXISTS (
                 SELECT 1 FROM user_external_accounts a WHERE a.user_id = u.id
                   AND a.service_type = 'scim' AND a.service_id = 'scim' AND a.deleted_at IS NULL)",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Whether the user is SCIM-controlled.
    pub async fn is_controlled(&self, user: UserId) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM user_external_accounts WHERE user_id = $1
                 AND service_type = 'scim' AND service_id = 'scim' AND deleted_at IS NULL)",
        )
        .bind(user)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Number of SCIM-controlled users, live and deactivated.
    pub async fn count_controlled(&self) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT count(*) FILTER (WHERE u.deleted_at IS NULL),
                    count(*) FILTER (WHERE u.deleted_at IS NOT NULL)
             FROM users u JOIN user_external_accounts a ON a.user_id = u.id
               AND a.service_type = 'scim' AND a.service_id = 'scim' AND a.deleted_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    async fn hydrate(&self, users: Vec<User>) -> Result<Vec<ScimUser>> {
        let ids: Vec<UserId> = users.iter().map(|u| u.id).collect();
        let emails: Vec<UserEmail> = sqlx::query_as(
            "SELECT user_id, email, verified_at, is_primary, created_at FROM user_emails
             WHERE user_id = ANY($1) ORDER BY user_id, is_primary DESC, created_at, email",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let accounts: Vec<AccountRow> = sqlx::query_as(
            "SELECT user_id, account_id, account_data, created_at, updated_at
                 FROM user_external_accounts
                 WHERE user_id = ANY($1) AND service_type = 'scim' AND service_id = 'scim'
                   AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_user: HashMap<UserId, Vec<UserEmail>> = HashMap::new();
        for e in emails {
            by_user.entry(e.user_id).or_default().push(e);
        }
        let mut accounts: HashMap<UserId, ScimAccount> = accounts
            .into_iter()
            .map(|r| {
                (
                    r.user_id,
                    ScimAccount {
                        account_id: r.account_id,
                        data: r.account_data,
                        created_at: r.created_at,
                        updated_at: r.updated_at,
                    },
                )
            })
            .collect();
        Ok(users
            .into_iter()
            .map(|user| ScimUser {
                emails: by_user.remove(&user.id).unwrap_or_default(),
                account: accounts.remove(&user.id),
                user,
            })
            .collect())
    }
}

async fn username_taken(tx: &mut PgConnection, username: &str, except: UserId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE lower(username) = lower($1)
                        AND deleted_at IS NULL AND id <> $2)",
    )
    .bind(username)
    .bind(except)
    .fetch_one(&mut *tx)
    .await?)
}

/// `base`, or `base-2` … `base-9`, whichever no other live user has.
async fn free_username(tx: &mut PgConnection, base: &str, user: Option<UserId>) -> Result<String> {
    let candidates =
        std::iter::once(base.to_owned()).chain(USERNAME_SUFFIXES.map(|n| format!("{base}-{n}")));
    for candidate in candidates {
        if !username_taken(tx, &candidate, user.unwrap_or(UserId(0))).await? {
            return Ok(candidate);
        }
    }
    Err(DbError::AlreadyExists {
        constraint: "users_username_lower_key".into(),
    })
}

/// Makes `emails` (primary first) verified addresses of the user and removes the
/// `previous` ones that are gone.
async fn sync_emails(
    tx: &mut PgConnection,
    user: UserId,
    emails: &[String],
    previous: &[String],
) -> Result<()> {
    let current: Vec<(String, bool)> =
        sqlx::query_as("SELECT email, verified_at IS NOT NULL FROM user_emails WHERE user_id = $1")
            .bind(user)
            .fetch_all(&mut *tx)
            .await?;
    let find = |email: &str| {
        current
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(email))
            .cloned()
    };
    let mut stored = Vec::with_capacity(emails.len());
    for email in emails {
        match find(email) {
            Some((existing, true)) => stored.push(existing),
            Some((existing, false)) => {
                sqlx::query(
                    "UPDATE user_emails SET verified_at = now(), verification_code_hash = NULL
                     WHERE user_id = $1 AND email = $2",
                )
                .bind(user)
                .bind(&existing)
                .execute(&mut *tx)
                .await?;
                grant_pending(tx, user, BindKind::Email, &existing).await?;
                stored.push(existing);
            }
            None => {
                sqlx::query(
                    "INSERT INTO user_emails (user_id, email, verified_at) VALUES ($1, $2, now())",
                )
                .bind(user)
                .bind(email)
                .execute(&mut *tx)
                .await?;
                grant_pending(tx, user, BindKind::Email, email).await?;
                stored.push(email.clone());
            }
        }
    }
    let removed: Vec<String> = previous
        .iter()
        .filter(|p| !emails.iter().any(|e| e.eq_ignore_ascii_case(p)))
        .map(|p| p.to_lowercase())
        .collect();
    if !removed.is_empty() {
        sqlx::query("DELETE FROM user_emails WHERE user_id = $1 AND lower(email) = ANY($2)")
            .bind(user)
            .bind(&removed)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(primary) = stored.first() {
        sqlx::query("UPDATE user_emails SET is_primary = false WHERE user_id = $1 AND email <> $2")
            .bind(user)
            .bind(primary)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE user_emails SET is_primary = true WHERE user_id = $1 AND email = $2")
            .bind(user)
            .bind(primary)
            .execute(&mut *tx)
            .await?;
    } else {
        // Keep exactly one primary address when the provider's ones were all removed.
        sqlx::query(
            "UPDATE user_emails SET is_primary = true WHERE user_id = $1
               AND NOT EXISTS (SELECT 1 FROM user_emails WHERE user_id = $1 AND is_primary)
               AND email = (SELECT email FROM user_emails WHERE user_id = $1
                            ORDER BY verified_at IS NULL, created_at, email LIMIT 1)",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Soft-deletes a user in `tx` (no-op when already deleted): sets `deleted_at`, ends their
/// sessions and revokes their access tokens.
async fn soft_delete_in(tx: &mut PgConnection, id: UserId) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE users SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE access_tokens SET deleted_at = now() WHERE subject_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    Ok(r.rows_affected() > 0)
}
