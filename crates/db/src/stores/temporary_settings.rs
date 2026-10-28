//! `temporary_settings` store: one JSON object of small UI state per user, such as
//! recent searches. Unlike `settings` it has no versions and no cascade.

use crate::{DbError, Result};
use serde_json::{Map, Value};
use sourcegull_types::UserId;
use sqlx::PgPool;
use sqlx::types::Json;

/// Reads and writes `temporary_settings`.
#[derive(Debug, Clone)]
pub struct TemporarySettingsStore {
    pool: PgPool,
}

fn require_object(contents: &Value) -> Result<()> {
    if contents.is_object() {
        Ok(())
    } else {
        Err(DbError::Invalid("temporary settings must be a JSON object"))
    }
}

impl TemporarySettingsStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The user's temporary settings; an empty object when none were saved.
    pub async fn get(&self, user: UserId) -> Result<Value> {
        let row: Option<Json<Value>> =
            sqlx::query_scalar("SELECT contents FROM temporary_settings WHERE user_id = $1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|j| j.0)
            .unwrap_or_else(|| Value::Object(Map::new())))
    }

    /// Replaces the user's temporary settings with `contents` (a JSON object).
    pub async fn overwrite(&self, user: UserId, contents: &Value) -> Result<()> {
        require_object(contents)?;
        sqlx::query(
            "INSERT INTO temporary_settings (user_id, contents) VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE SET contents = EXCLUDED.contents, updated_at = now()",
        )
        .bind(user)
        .bind(Json(contents))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Merges the top-level keys of `edits` (a JSON object) into the user's temporary
    /// settings, replacing the values of keys that exist; other keys are kept. Atomic, so
    /// concurrent edits of different keys do not lose each other's changes.
    pub async fn edit(&self, user: UserId, edits: &Value) -> Result<()> {
        require_object(edits)?;
        sqlx::query(
            "INSERT INTO temporary_settings (user_id, contents) VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE
             SET contents = temporary_settings.contents || EXCLUDED.contents, updated_at = now()",
        )
        .bind(user)
        .bind(Json(edits))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
