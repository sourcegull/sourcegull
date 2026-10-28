//! Account creation for sign-up: a user, their roles and their first email in
//! one transaction, with the first-user-becomes-site-admin rule applied
//! atomically. While the resources file declares site admins (`siteAdmins`), that rule is off:
//! a new user is site admin only with a declared, verified email.

use super::perms::{BindKind, grant_pending};
use super::system_roles;
use crate::{DbError, NewUser, Result};
use sourcegull_types::User;
use sqlx::{PgPool, Postgres, Transaction};

/// Advisory lock key serializing account creation, so that two concurrent
/// sign-ups on an empty instance cannot both become the first user.
pub(crate) const ACCOUNT_CREATION_LOCK: i64 = 0x5345_454b_4552_0001; // "SOURCEGULL" + 1

const USER_COLUMNS: &str = "id, username, display_name, avatar_url, site_admin, created_at, \
     updated_at, deleted_at, locked_until, failed_logins";

/// Fields for [`AccountStore::create`].
#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    /// The user row.
    pub user: NewUser,
    /// Primary email address, if any. It is unverified, except for the first
    /// user and with `email_verified`.
    pub email: Option<String>,
    /// Mark `email` verified (an identity provider vouched for it).
    pub email_verified: bool,
    /// Only create the account if it becomes the instance's first admin: no live users exist
    /// yet, or (with declared site admins) its verified email is declared. Otherwise fails
    /// with [`DbError::Invalid`].
    pub only_if_first: bool,
}

/// Result of [`AccountStore::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedAccount {
    /// The new user (with `site_admin` set if it is the first user or a declared site admin).
    pub user: User,
    /// Whether this was the first live user of the instance.
    pub first_user: bool,
}

/// Creates accounts atomically.
#[derive(Debug, Clone)]
pub struct AccountStore {
    pool: PgPool,
}

impl AccountStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates a user, assigns the `USER` role and adds the email, all in one
    /// transaction. If no live users exist, the new user becomes site admin
    /// (with the `SITE_ADMINISTRATOR` role) and the email is marked verified. With declared
    /// site admins, the new user instead becomes site admin when its email is verified and
    /// declared, and the first user's email is not marked verified.
    ///
    /// Concurrent calls are serialized with a transaction-scoped advisory lock,
    /// so exactly one of several simultaneous first sign-ups becomes admin.
    /// Fails with [`DbError::AlreadyExists`] when the username (constraint
    /// `users_username_lower_key`) or a verified email
    /// (`user_emails_verified_email_key`) is taken.
    pub async fn create(&self, new: &NewAccount) -> Result<CreatedAccount> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(ACCOUNT_CREATION_LOCK)
            .execute(&mut *tx)
            .await?;
        let live: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE deleted_at IS NULL")
            .fetch_one(&mut *tx)
            .await?;
        let first_user = live == 0;
        let declared: Vec<String> = sqlx::query_scalar("SELECT email FROM declared_site_admins")
            .fetch_all(&mut *tx)
            .await?;
        let declared_admin = new.email_verified
            && new
                .email
                .as_ref()
                .is_some_and(|e| declared.contains(&e.to_lowercase()));
        let site_admin = if declared.is_empty() {
            first_user
        } else {
            declared_admin
        };
        if new.only_if_first && !site_admin {
            return Err(DbError::Invalid("users already exist"));
        }
        let user: User = sqlx::query_as(&format!(
            "INSERT INTO users (username, display_name, avatar_url, password_hash, site_admin)
             VALUES ($1, $2, $3, $4, $5) RETURNING {USER_COLUMNS}"
        ))
        .bind(&new.user.username)
        .bind(&new.user.display_name)
        .bind(&new.user.avatar_url)
        .bind(&new.user.password_hash)
        .bind(site_admin)
        .fetch_one(&mut *tx)
        .await?;
        assign_system_role(&mut tx, &user, system_roles::USER).await?;
        if site_admin {
            assign_system_role(&mut tx, &user, system_roles::SITE_ADMINISTRATOR).await?;
        }
        if declared_admin {
            sqlx::query("INSERT INTO declared_grants (kind, key, user_id) VALUES ($1, '', $2)")
                .bind(super::declared::SITE_ADMIN_GRANT)
                .bind(user.id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(email) = &new.email {
            let verified = (first_user && declared.is_empty()) || new.email_verified;
            sqlx::query(
                "INSERT INTO user_emails (user_id, email, is_primary, verified_at)
                 VALUES ($1, $2, true, CASE WHEN $3 THEN now() END)",
            )
            .bind(user.id)
            .bind(email)
            .bind(verified)
            .execute(&mut *tx)
            .await?;
            if verified {
                grant_pending(&mut tx, user.id, BindKind::Email, email).await?;
            }
        }
        grant_pending(&mut tx, user.id, BindKind::Username, &user.username).await?;
        tx.commit().await?;
        Ok(CreatedAccount { user, first_user })
    }
}

async fn assign_system_role(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    role: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO user_roles (user_id, role_id)
         SELECT $1, id FROM roles WHERE name = $2 AND system AND deleted_at IS NULL
         ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(role)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
