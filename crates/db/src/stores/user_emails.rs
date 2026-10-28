//! `user_emails` store.

use super::perms::{BindKind, grant_pending};
use crate::{DbError, Result};
use sourcegull_types::{UserEmail, UserId};
use sqlx::PgPool;

const COLUMNS: &str = "user_id, email, verified_at, is_primary, created_at";

/// Reads and writes `user_emails`.
#[derive(Debug, Clone)]
pub struct UserEmailStore {
    pool: PgPool,
}

impl UserEmailStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Adds an unverified address. It becomes primary if the user has no
    /// other address. Fails with [`DbError::AlreadyExists`] if the user already
    /// has it.
    pub async fn add(&self, user_id: UserId, email: &str) -> Result<UserEmail> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO user_emails (user_id, email, is_primary)
             VALUES ($1, $2, NOT EXISTS (SELECT 1 FROM user_emails WHERE user_id = $1))
             RETURNING {COLUMNS}"
        ))
        .bind(user_id)
        .bind(email)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Marks an address verified. Fails with [`DbError::AlreadyExists`] if
    /// another user already verified the same address.
    /// Explicit permissions pending for the address are granted to the user.
    pub async fn verify(&self, user_id: UserId, email: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE user_emails SET verified_at = coalesce(verified_at, now()),
                 verification_code_hash = NULL
             WHERE user_id = $1 AND email = $2",
        )
        .bind(user_id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("user email"));
        }
        grant_pending(&mut tx, user_id, BindKind::Email, email).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Saves a fresh verification code for an unverified address.
    pub async fn set_verification_code(
        &self,
        user_id: UserId,
        email: &str,
        hash: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE user_emails SET verification_code_hash = $3,
            last_verification_sent_at = now() WHERE user_id = $1 AND lower(email) = lower($2)
            AND verified_at IS NULL",
        )
        .bind(user_id)
        .bind(email)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Consumes a code and grants permissions pending for this address atomically.
    pub async fn redeem_verification_code(&self, email: &str, hash: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let user: Option<UserId> = sqlx::query_scalar(
            "UPDATE user_emails SET verified_at = now(),
            verification_code_hash = NULL WHERE lower(email) = lower($1)
            AND verification_code_hash = $2 AND verified_at IS NULL RETURNING user_id",
        )
        .bind(email)
        .bind(hash)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(user) = user {
            grant_pending(&mut tx, user, BindKind::Email, email).await?;
            tx.commit().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Revokes verification and any unused code.
    pub async fn unverify(&self, user_id: UserId, email: &str) -> Result<()> {
        let result = sqlx::query(
            "UPDATE user_emails SET verified_at = NULL,
            verification_code_hash = NULL WHERE user_id = $1 AND lower(email) = lower($2)",
        )
        .bind(user_id)
        .bind(email)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound("user email"));
        }
        Ok(())
    }

    /// Makes a verified address the user's primary one.
    pub async fn set_primary(&self, user_id: UserId, email: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let verified: Option<bool> = sqlx::query_scalar(
            "SELECT verified_at IS NOT NULL FROM user_emails WHERE user_id = $1 AND email = $2 FOR UPDATE",
        )
        .bind(user_id)
        .bind(email)
        .fetch_optional(&mut *tx)
        .await?;
        match verified {
            None => return Err(DbError::NotFound("user email")),
            Some(false) => return Err(DbError::Invalid("primary email must be verified")),
            Some(true) => {}
        }
        sqlx::query("UPDATE user_emails SET is_primary = false WHERE user_id = $1 AND is_primary")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE user_emails SET is_primary = true WHERE user_id = $1 AND email = $2")
            .bind(user_id)
            .bind(email)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes an address. Fails with [`DbError::NotFound`] if the user does not have it and
    /// with [`DbError::Invalid`] if it is the primary or the only address.
    pub async fn remove(&self, user_id: UserId, email: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT email, is_primary FROM user_emails WHERE user_id = $1 FOR UPDATE",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
        let Some((_, is_primary)) = rows.iter().find(|(e, _)| e == email) else {
            return Err(DbError::NotFound("user email"));
        };
        if *is_primary {
            return Err(DbError::Invalid("the primary email cannot be removed"));
        }
        if rows.len() == 1 {
            return Err(DbError::Invalid("the only email cannot be removed"));
        }
        sqlx::query("DELETE FROM user_emails WHERE user_id = $1 AND email = $2")
            .bind(user_id)
            .bind(email)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The user's addresses, primary first, then oldest first.
    pub async fn list(&self, user_id: UserId) -> Result<Vec<UserEmail>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM user_emails WHERE user_id = $1
             ORDER BY is_primary DESC, created_at, email"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The live user who has verified `email` (case-insensitive), if any.
    pub async fn user_by_verified_email(&self, email: &str) -> Result<Option<UserId>> {
        Ok(sqlx::query_scalar(
            "SELECT e.user_id FROM user_emails e JOIN users u ON u.id = e.user_id
             WHERE lower(e.email) = lower($1) AND e.verified_at IS NOT NULL AND u.deleted_at IS NULL",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?)
    }
}
