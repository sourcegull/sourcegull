//! `settings` store: the versioned settings cascade.

use crate::notify::{channels, notify};
use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{OrgId, SettingsId, SettingsSubject, UserId};
use sqlx::PgPool;

/// One version of a subject's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub id: SettingsId,
    pub subject: SettingsSubject,
    /// JSONC text, verbatim.
    pub contents: String,
    pub author_user_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: SettingsId,
    org_id: Option<OrgId>,
    user_id: Option<UserId>,
    contents: String,
    author_user_id: Option<UserId>,
    created_at: DateTime<Utc>,
}

impl Row {
    fn into_settings(self) -> Settings {
        let subject = match (self.org_id, self.user_id) {
            (Some(org), _) => SettingsSubject::Org(org),
            (None, Some(user)) => SettingsSubject::User(user),
            (None, None) => SettingsSubject::Global,
        };
        Settings {
            id: self.id,
            subject,
            contents: self.contents,
            author_user_id: self.author_user_id,
            created_at: self.created_at,
        }
    }
}

fn subject_ids(subject: SettingsSubject) -> (Option<OrgId>, Option<UserId>) {
    match subject {
        SettingsSubject::Global => (None, None),
        SettingsSubject::Org(id) => (Some(id), None),
        SettingsSubject::User(id) => (None, Some(id)),
    }
}

// Matches one subject; `IS NOT DISTINCT FROM` handles the NULL ids.
const SUBJECT_FILTER: &str = "subject_type = $1 AND org_id IS NOT DISTINCT FROM $2 \
     AND user_id IS NOT DISTINCT FROM $3";

/// Reads and writes `settings`.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    pool: PgPool,
}

impl SettingsStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The newest version for `subject`, if any.
    pub async fn latest(&self, subject: SettingsSubject) -> Result<Option<Settings>> {
        let (org, user) = subject_ids(subject);
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT id, org_id, user_id, contents, author_user_id, created_at FROM settings
             WHERE {SUBJECT_FILTER} ORDER BY id DESC LIMIT 1"
        ))
        .bind(subject.subject_type())
        .bind(org)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Row::into_settings))
    }

    /// Writes a new version for `subject`, provided its latest version is still
    /// `expected_last_id` (`None` = none yet). Otherwise fails with
    /// [`DbError::Conflict`]. Sends a `settings` notification on commit.
    pub async fn create(
        &self,
        subject: SettingsSubject,
        contents: &str,
        author: Option<UserId>,
        expected_last_id: Option<SettingsId>,
    ) -> Result<Settings> {
        let (org, user) = subject_ids(subject);
        let mut tx = self.pool.begin().await?;
        // Serialize writers of this subject only: a transaction-scoped advisory
        // lock keyed by the subject.
        let key = match subject {
            SettingsSubject::Global => "settings:global".to_owned(),
            SettingsSubject::Org(id) => format!("settings:org:{id}"),
            SettingsSubject::User(id) => format!("settings:user:{id}"),
        };
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        let actual: Option<SettingsId> = sqlx::query_scalar(&format!(
            "SELECT max(id) FROM settings WHERE {SUBJECT_FILTER}"
        ))
        .bind(subject.subject_type())
        .bind(org)
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        if actual != expected_last_id {
            return Err(DbError::Conflict {
                expected: expected_last_id.map(SettingsId::get),
                actual: actual.map(SettingsId::get),
            });
        }
        let row: Row = sqlx::query_as(
            "INSERT INTO settings (subject_type, org_id, user_id, contents, author_user_id)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, org_id, user_id, contents, author_user_id, created_at",
        )
        .bind(subject.subject_type())
        .bind(org)
        .bind(user)
        .bind(contents)
        .bind(author)
        .fetch_one(&mut *tx)
        .await?;
        let payload = match subject {
            SettingsSubject::Global => "global:".to_owned(),
            SettingsSubject::Org(id) => format!("org:{id}"),
            SettingsSubject::User(id) => format!("user:{id}"),
        };
        notify(&mut *tx, channels::SETTINGS, &payload).await?;
        tx.commit().await?;
        Ok(row.into_settings())
    }
}
