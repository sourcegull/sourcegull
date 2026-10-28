//! `users` store.

use super::perms::{BindKind, grant_pending};
use super::system_roles;
use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{User, UserId};
use sqlx::{PgExecutor, PgPool};
use std::time::Duration;

const USER_COLUMNS: &str = "id, username, display_name, avatar_url, site_admin, created_at, \
     updated_at, deleted_at, locked_until, failed_logins";

/// Fields for [`UserStore::create`].
#[derive(Debug, Clone, Default)]
pub struct NewUser {
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// Already-hashed password (hashing is the authn crate's job), if any.
    pub password_hash: Option<String>,
}

/// Fields for [`UserStore::update_profile`]; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    pub username: Option<String>,
    pub display_name: Option<Option<String>>,
    pub avatar_url: Option<Option<String>>,
}

/// Filters for [`UserStore::list`].
#[derive(Debug, Clone)]
pub struct ListUsersOptions {
    /// Case-insensitive substring match on username or display name.
    pub query: Option<String>,
    /// Include soft-deleted users.
    pub include_deleted: bool,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListUsersOptions {
    fn default() -> Self {
        Self {
            query: None,
            include_deleted: false,
            limit: 100,
            offset: 0,
        }
    }
}

/// Account lockout rule applied by [`UserStore::record_failed_login`], per account and client
/// address.
#[derive(Debug, Clone, Copy)]
pub struct LockoutPolicy {
    /// Lock the account (for the address) once this many consecutive failures are reached.
    pub max_failures: i32,
    /// Failures count together only while they fall within this period of the
    /// first failure of the run; a later failure starts a new run at 1.
    pub window: Duration,
    /// How long the lock lasts.
    pub lockout: Duration,
}

/// The state after recording a failed login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginFailure {
    /// Consecutive failures since the last reset (0 right after a lock).
    pub failed_logins: i32,
    /// Set when this failure locked the account for the address (or it was already locked).
    pub locked_until: Option<DateTime<Utc>>,
}

/// Reads and writes `users`. Soft-deleted users are invisible to lookups.
#[derive(Debug, Clone)]
pub struct UserStore {
    pool: PgPool,
}

impl UserStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates a user and assigns the `USER` role. Never makes the user a site
    /// admin (the first-user rule belongs to authn).
    /// Fails with [`DbError::AlreadyExists`] if the username is taken.
    pub async fn create(&self, new: &NewUser) -> Result<User> {
        let mut tx = self.pool.begin().await?;
        let user: User = sqlx::query_as(&format!(
            "INSERT INTO users (username, display_name, avatar_url, password_hash)
             VALUES ($1, $2, $3, $4) RETURNING {USER_COLUMNS}"
        ))
        .bind(&new.username)
        .bind(&new.display_name)
        .bind(&new.avatar_url)
        .bind(&new.password_hash)
        .fetch_one(&mut *tx)
        .await?;
        assign_role(&mut *tx, user.id, system_roles::USER).await?;
        grant_pending(&mut tx, user.id, BindKind::Username, &user.username).await?;
        tx.commit().await?;
        Ok(user)
    }

    /// Looks up a live user by id.
    pub async fn get_by_id(&self, id: UserId) -> Result<Option<User>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks up a user by id, live or soft-deleted.
    pub async fn get_by_id_including_deleted(&self, id: UserId) -> Result<Option<User>> {
        Ok(
            sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Changes a live user's profile: each `Some` field is written (`Some(None)` clears the
    /// display name or avatar). A new username is granted the explicit permissions pending for
    /// it. Fails with [`DbError::AlreadyExists`] if the username is taken.
    pub async fn update_profile(&self, id: UserId, update: &ProfileUpdate) -> Result<User> {
        let mut tx = self.pool.begin().await?;
        let user: Option<User> = sqlx::query_as(&format!(
            "UPDATE users SET
                 username = coalesce($2, username),
                 display_name = CASE WHEN $3 THEN $4 ELSE display_name END,
                 avatar_url = CASE WHEN $5 THEN $6 ELSE avatar_url END,
                 updated_at = now()
             WHERE id = $1 AND deleted_at IS NULL
             RETURNING {USER_COLUMNS}"
        ))
        .bind(id)
        .bind(&update.username)
        .bind(update.display_name.is_some())
        .bind(update.display_name.clone().flatten())
        .bind(update.avatar_url.is_some())
        .bind(update.avatar_url.clone().flatten())
        .fetch_optional(&mut *tx)
        .await?;
        let user = user.ok_or(DbError::NotFound("user"))?;
        if update.username.is_some() {
            grant_pending(&mut tx, user.id, BindKind::Username, &user.username).await?;
        }
        tx.commit().await?;
        Ok(user)
    }

    /// Restores a soft-deleted user. Their sessions and access tokens stay revoked. Fails with
    /// [`DbError::NotFound`] if there is no such deactivated user and with
    /// [`DbError::AlreadyExists`] if a live user took the username meanwhile.
    pub async fn reactivate(&self, id: UserId) -> Result<User> {
        let user: Option<User> = sqlx::query_as(&format!(
            "UPDATE users SET deleted_at = NULL, updated_at = now()
             WHERE id = $1 AND deleted_at IS NOT NULL RETURNING {USER_COLUMNS}"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        user.ok_or(DbError::NotFound("deactivated user"))
    }

    /// Looks up a live user by username, case-insensitively.
    pub async fn get_by_username(&self, username: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE lower(username) = lower($1) AND deleted_at IS NULL"
        ))
        .bind(username)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Lists users ordered by id.
    pub async fn list(&self, opts: &ListUsersOptions) -> Result<Vec<User>> {
        let pattern = opts
            .query
            .as_deref()
            .map(|q| format!("%{}%", escape_like(q)));
        Ok(sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users
             WHERE ($1 OR deleted_at IS NULL)
               AND ($2::text IS NULL OR username ILIKE $2 OR display_name ILIKE $2)
             ORDER BY id LIMIT $3 OFFSET $4"
        ))
        .bind(opts.include_deleted)
        .bind(pattern)
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Number of live users.
    pub async fn count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM users WHERE deleted_at IS NULL")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Number of users whose username or display name contains `query`
    /// (case-insensitive), or of all users; live users only unless `include_deleted`.
    pub async fn count_matching(&self, query: Option<&str>, include_deleted: bool) -> Result<i64> {
        let pattern = query.map(|q| format!("%{}%", escape_like(q)));
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM users WHERE ($2 OR deleted_at IS NULL)
               AND ($1::text IS NULL OR username ILIKE $1 OR display_name ILIKE $1)",
        )
        .bind(pattern)
        .bind(include_deleted)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Number of live site admins.
    pub async fn count_site_admins(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar(
                "SELECT count(*) FROM users WHERE site_admin AND deleted_at IS NULL",
            )
            .fetch_one(&self.pool)
            .await?,
        )
    }

    /// Sets the site-admin flag and keeps the `SITE_ADMINISTRATOR` role in sync.
    pub async fn set_site_admin(&self, id: UserId, site_admin: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE users SET site_admin = $2, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(site_admin)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(DbError::NotFound("user"));
        }
        if site_admin {
            assign_role(&mut *tx, id, system_roles::SITE_ADMINISTRATOR).await?;
        } else {
            sqlx::query(
                "DELETE FROM user_roles WHERE user_id = $1
                   AND role_id = (SELECT id FROM roles WHERE name = $2 AND system)",
            )
            .bind(id)
            .bind(system_roles::SITE_ADMINISTRATOR)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Replaces (or clears, with `None`) the stored password hash.
    pub async fn set_password_hash(&self, id: UserId, hash: Option<&str>) -> Result<()> {
        let r = sqlx::query(
            "UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        not_found_if_zero(r.rows_affected())
    }

    /// The stored password hash of a live user (`Ok(None)` if they have none).
    pub async fn password_hash(&self, id: UserId) -> Result<Option<String>> {
        let row: Option<Option<String>> = sqlx::query_scalar(
            "SELECT password_hash FROM users WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(DbError::NotFound("user"))
    }

    /// Atomically counts a failed login of user `id` from client `address` (`""` when unknown).
    /// Failures are counted per address, so failures from one address never lock out another.
    /// A failure more than `policy.window` after the first failure of the current run starts a
    /// new run at 1. When the count reaches `policy.max_failures`, the account is locked for
    /// that address for `policy.lockout` and the counter restarts at 0.
    pub async fn record_failed_login(
        &self,
        id: UserId,
        address: &str,
        policy: LockoutPolicy,
    ) -> Result<LoginFailure> {
        // The failure count of the existing row after this failure.
        const N: &str = "(CASE WHEN f.failures > 0 AND f.since IS NOT NULL
                                AND f.since + make_interval(secs => $5) >= now()
                           THEN f.failures + 1 ELSE 1 END)";
        let sql = format!(
            "INSERT INTO sign_in_failures AS f (user_id, address, failures, since, locked_until)
             SELECT $1, $2,
                    CASE WHEN 1 >= $3 THEN 0 ELSE 1 END,
                    CASE WHEN 1 >= $3 THEN NULL ELSE now() END,
                    CASE WHEN 1 >= $3 THEN now() + make_interval(secs => $4) END
             FROM users WHERE id = $1 AND deleted_at IS NULL
             ON CONFLICT (user_id, address) DO UPDATE SET
                 failures = CASE WHEN {N} >= $3 THEN 0 ELSE {N} END,
                 since = CASE WHEN {N} >= $3 THEN NULL WHEN {N} = 1 THEN now() ELSE f.since END,
                 locked_until = CASE WHEN {N} >= $3 THEN now() + make_interval(secs => $4)
                                     ELSE f.locked_until END
             RETURNING f.failures, f.locked_until"
        );
        let row: (i32, Option<DateTime<Utc>>) = sqlx::query_as(&sql)
            .bind(id)
            .bind(address)
            .bind(policy.max_failures)
            .bind(policy.lockout.as_secs_f64())
            .bind(policy.window.as_secs_f64())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(DbError::NotFound("user"))?;
        Ok(LoginFailure {
            failed_logins: row.0,
            locked_until: row.1,
        })
    }

    /// When the lock of user `id` for client `address` ends, if it is locked now.
    pub async fn login_locked_until(
        &self,
        id: UserId,
        address: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar(
            "SELECT locked_until FROM sign_in_failures
             WHERE user_id = $1 AND address = $2 AND locked_until > now()",
        )
        .bind(id)
        .bind(address)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Clears the failure counter and lock of user `id` for client `address` (after a
    /// successful login from it).
    pub async fn clear_failed_logins_from(&self, id: UserId, address: &str) -> Result<()> {
        sqlx::query("DELETE FROM sign_in_failures WHERE user_id = $1 AND address = $2")
            .bind(id)
            .bind(address)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Clears every failure counter and lock of user `id` (an admin unlock).
    pub async fn reset_failed_logins(&self, id: UserId) -> Result<()> {
        let r =
            sqlx::query("UPDATE users SET failed_logins = 0, failed_logins_since = NULL, locked_until = NULL WHERE id = $1")
                .bind(id)
                .execute(&self.pool)
                .await?;
        not_found_if_zero(r.rows_affected())?;
        sqlx::query("DELETE FROM sign_in_failures WHERE user_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Soft-deletes a user: sets `deleted_at`, ends their sessions and revokes
    /// their access tokens. Frees the username.
    pub async fn soft_delete(&self, id: UserId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE users SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        not_found_if_zero(r.rows_affected())?;
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
        tx.commit().await?;
        Ok(())
    }
}

async fn assign_role<'e, E: PgExecutor<'e>>(executor: E, user: UserId, role: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO user_roles (user_id, role_id)
         SELECT $1, id FROM roles WHERE name = $2 AND system AND deleted_at IS NULL
         ON CONFLICT DO NOTHING",
    )
    .bind(user)
    .bind(role)
    .execute(executor)
    .await?;
    Ok(())
}

fn not_found_if_zero(rows: u64) -> Result<()> {
    if rows == 0 {
        Err(DbError::NotFound("user"))
    } else {
        Ok(())
    }
}

/// Escapes `%`, `_` and `\` for use inside a `LIKE` pattern.
pub(crate) fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
