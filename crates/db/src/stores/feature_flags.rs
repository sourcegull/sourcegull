//! `feature_flags` and `feature_flag_overrides`: boolean and rollout flags with
//! per-user and per-org overrides. Every write sends `NOTIFY feature_flags` with the flag name.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::notify::{channels, notify};
use crate::{DbError, Result};

/// The value of a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureFlagValue {
    /// The same value for everyone.
    Bool(bool),
    /// On for this share of users, in basis points (0..=10000).
    Rollout(i32),
}

/// A stored flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFeatureFlag {
    pub name: String,
    pub value: FeatureFlagValue,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Who an override applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideNamespace {
    User(i64),
    Org(i64),
}

/// A stored override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFlagOverride {
    pub flag_name: String,
    pub namespace: OverrideNamespace,
    pub value: bool,
}

type FlagRow = (
    String,
    String,
    Option<bool>,
    Option<i32>,
    DateTime<Utc>,
    DateTime<Utc>,
);

fn flag(row: FlagRow) -> Result<StoredFeatureFlag> {
    let (name, flag_type, bool_value, rollout, created_at, updated_at) = row;
    let value = match (flag_type.as_str(), bool_value, rollout) {
        ("bool", Some(v), _) => FeatureFlagValue::Bool(v),
        ("rollout", _, Some(r)) => FeatureFlagValue::Rollout(r),
        _ => return Err(DbError::Invalid("malformed feature flag row")),
    };
    Ok(StoredFeatureFlag {
        name,
        value,
        created_at,
        updated_at,
    })
}

type OverrideRow = (String, Option<i64>, Option<i64>, bool);

fn override_of(row: OverrideRow) -> Result<StoredFlagOverride> {
    let (flag_name, user_id, org_id, value) = row;
    let namespace = match (user_id, org_id) {
        (Some(u), None) => OverrideNamespace::User(u),
        (None, Some(o)) => OverrideNamespace::Org(o),
        _ => return Err(DbError::Invalid("malformed feature flag override row")),
    };
    Ok(StoredFlagOverride {
        flag_name,
        namespace,
        value,
    })
}

fn split(value: FeatureFlagValue) -> Result<(&'static str, Option<bool>, Option<i32>)> {
    Ok(match value {
        FeatureFlagValue::Bool(v) => ("bool", Some(v), None),
        FeatureFlagValue::Rollout(r) if (0..=10_000).contains(&r) => ("rollout", None, Some(r)),
        FeatureFlagValue::Rollout(_) => {
            return Err(DbError::Invalid("rollout must be between 0 and 10000"));
        }
    })
}

fn ns_ids(ns: OverrideNamespace) -> (Option<i64>, Option<i64>) {
    match ns {
        OverrideNamespace::User(u) => (Some(u), None),
        OverrideNamespace::Org(o) => (None, Some(o)),
    }
}

const FLAG_COLUMNS: &str = "name, flag_type, bool_value, rollout, created_at, updated_at";

/// Reads and writes feature flags and overrides.
#[derive(Debug, Clone)]
pub struct FeatureFlagStore {
    pool: PgPool,
}

impl FeatureFlagStore {
    /// A store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every flag, by name.
    pub async fn list(&self) -> Result<Vec<StoredFeatureFlag>> {
        let rows: Vec<FlagRow> = sqlx::query_as(&format!(
            "SELECT {FLAG_COLUMNS} FROM feature_flags WHERE deleted_at IS NULL ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(flag).collect()
    }

    /// One flag.
    pub async fn get(&self, name: &str) -> Result<Option<StoredFeatureFlag>> {
        let row: Option<FlagRow> = sqlx::query_as(&format!(
            "SELECT {FLAG_COLUMNS} FROM feature_flags WHERE name = $1 AND deleted_at IS NULL"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(flag).transpose()
    }

    /// Creates a flag. A deleted flag of the same name is replaced; a live one is
    /// [`DbError::AlreadyExists`].
    pub async fn create(&self, name: &str, value: FeatureFlagValue) -> Result<StoredFeatureFlag> {
        if name.trim().is_empty() {
            return Err(DbError::Invalid("the flag name is empty"));
        }
        let (flag_type, bool_value, rollout) = split(value)?;
        let mut tx = self.pool.begin().await?;
        let row: Option<FlagRow> = sqlx::query_as(&format!(
            "INSERT INTO feature_flags (name, flag_type, bool_value, rollout)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (name) DO UPDATE SET flag_type = EXCLUDED.flag_type,
                 bool_value = EXCLUDED.bool_value, rollout = EXCLUDED.rollout,
                 created_at = now(), updated_at = now(), deleted_at = NULL
             WHERE feature_flags.deleted_at IS NOT NULL
             RETURNING {FLAG_COLUMNS}"
        ))
        .bind(name)
        .bind(flag_type)
        .bind(bool_value)
        .bind(rollout)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(DbError::AlreadyExists {
                constraint: "feature_flags_pkey".to_owned(),
            });
        };
        notify(&mut *tx, channels::FEATURE_FLAGS, name).await?;
        tx.commit().await?;
        flag(row)
    }

    /// Changes a flag's value (and possibly its type).
    pub async fn update(&self, name: &str, value: FeatureFlagValue) -> Result<StoredFeatureFlag> {
        let (flag_type, bool_value, rollout) = split(value)?;
        let mut tx = self.pool.begin().await?;
        let row: Option<FlagRow> = sqlx::query_as(&format!(
            "UPDATE feature_flags SET flag_type = $2, bool_value = $3, rollout = $4,
                 updated_at = now()
             WHERE name = $1 AND deleted_at IS NULL RETURNING {FLAG_COLUMNS}"
        ))
        .bind(name)
        .bind(flag_type)
        .bind(bool_value)
        .bind(rollout)
        .fetch_optional(&mut *tx)
        .await?;
        let row = row.ok_or(DbError::NotFound("feature flag"))?;
        notify(&mut *tx, channels::FEATURE_FLAGS, name).await?;
        tx.commit().await?;
        flag(row)
    }

    /// Deletes a flag and its overrides.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query("DELETE FROM feature_flags WHERE name = $1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("feature flag"));
        }
        notify(&mut *tx, channels::FEATURE_FLAGS, name).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The overrides of `flag`, users first.
    pub async fn overrides(&self, flag: &str) -> Result<Vec<StoredFlagOverride>> {
        let rows: Vec<OverrideRow> = sqlx::query_as(
            "SELECT flag_name, user_id, org_id, value FROM feature_flag_overrides
             WHERE flag_name = $1 ORDER BY user_id NULLS LAST, org_id",
        )
        .bind(flag)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(override_of).collect()
    }

    /// The overrides that apply to a user (theirs and their orgs'), of every flag.
    pub async fn overrides_for(
        &self,
        user_id: Option<i64>,
        org_ids: &[i64],
    ) -> Result<Vec<StoredFlagOverride>> {
        let rows: Vec<OverrideRow> = sqlx::query_as(
            "SELECT o.flag_name, o.user_id, o.org_id, o.value FROM feature_flag_overrides o
             JOIN feature_flags f ON f.name = o.flag_name AND f.deleted_at IS NULL
             WHERE o.user_id = $1 OR o.org_id = ANY($2)",
        )
        .bind(user_id)
        .bind(org_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(override_of).collect()
    }

    /// One override.
    pub async fn get_override(
        &self,
        flag: &str,
        ns: OverrideNamespace,
    ) -> Result<Option<StoredFlagOverride>> {
        let (user_id, org_id) = ns_ids(ns);
        let row: Option<OverrideRow> = sqlx::query_as(
            "SELECT flag_name, user_id, org_id, value FROM feature_flag_overrides
             WHERE flag_name = $1 AND user_id IS NOT DISTINCT FROM $2
               AND org_id IS NOT DISTINCT FROM $3",
        )
        .bind(flag)
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(override_of).transpose()
    }

    /// Creates an override; [`DbError::AlreadyExists`] if the namespace has one for the flag,
    /// [`DbError::NotFound`] if the flag does not exist.
    pub async fn create_override(
        &self,
        flag: &str,
        ns: OverrideNamespace,
        value: bool,
    ) -> Result<StoredFlagOverride> {
        if self.get(flag).await?.is_none() {
            return Err(DbError::NotFound("feature flag"));
        }
        let (user_id, org_id) = ns_ids(ns);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO feature_flag_overrides (flag_name, user_id, org_id, value)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(flag)
        .bind(user_id)
        .bind(org_id)
        .bind(value)
        .execute(&mut *tx)
        .await?;
        notify(&mut *tx, channels::FEATURE_FLAGS, flag).await?;
        tx.commit().await?;
        Ok(StoredFlagOverride {
            flag_name: flag.to_owned(),
            namespace: ns,
            value,
        })
    }

    /// Changes an override's value.
    pub async fn update_override(
        &self,
        flag: &str,
        ns: OverrideNamespace,
        value: bool,
    ) -> Result<StoredFlagOverride> {
        let (user_id, org_id) = ns_ids(ns);
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE feature_flag_overrides SET value = $4, updated_at = now()
             WHERE flag_name = $1 AND user_id IS NOT DISTINCT FROM $2
               AND org_id IS NOT DISTINCT FROM $3",
        )
        .bind(flag)
        .bind(user_id)
        .bind(org_id)
        .bind(value)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("feature flag override"));
        }
        notify(&mut *tx, channels::FEATURE_FLAGS, flag).await?;
        tx.commit().await?;
        Ok(StoredFlagOverride {
            flag_name: flag.to_owned(),
            namespace: ns,
            value,
        })
    }

    /// Deletes an override.
    pub async fn delete_override(&self, flag: &str, ns: OverrideNamespace) -> Result<()> {
        let (user_id, org_id) = ns_ids(ns);
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "DELETE FROM feature_flag_overrides
             WHERE flag_name = $1 AND user_id IS NOT DISTINCT FROM $2
               AND org_id IS NOT DISTINCT FROM $3",
        )
        .bind(flag)
        .bind(user_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("feature flag override"));
        }
        notify(&mut *tx, channels::FEATURE_FLAGS, flag).await?;
        tx.commit().await?;
        Ok(())
    }
}
