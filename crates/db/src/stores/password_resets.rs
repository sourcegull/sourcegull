//! Single-use password reset codes.

use crate::Result;
use sourcegull_types::UserId;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct PasswordResetStore {
    pool: PgPool,
}

impl PasswordResetStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, user: UserId, hash: &[u8], ttl_secs: u32) -> Result<()> {
        sqlx::query(
            "INSERT INTO password_reset_codes (user_id, code_hash, expires_at)
            VALUES ($1, $2, now() + make_interval(secs => $3))
            ON CONFLICT (user_id) DO UPDATE SET code_hash = excluded.code_hash,
                expires_at = excluded.expires_at",
        )
        .bind(user)
        .bind(hash)
        .bind(f64::from(ttl_secs))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn redeem(&self, user: UserId, hash: &[u8]) -> Result<bool> {
        let row = sqlx::query(
            "DELETE FROM password_reset_codes
            WHERE user_id = $1 AND code_hash = $2 AND expires_at > now()",
        )
        .bind(user)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(row.rows_affected() == 1)
    }
}
