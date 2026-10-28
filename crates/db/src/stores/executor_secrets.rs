//! `executor_secrets` and `executor_secret_access_logs`: secrets injected into
//! executor jobs as environment variables.
//!
//! A secret is global or belongs to a user or an organization ([`Namespace`]) and is usable
//! in one [`ExecutorSecretScope`]. Values are encrypted with the `executor_secret` data key and
//! only decrypted by [`ExecutorSecretStore::value`], which records an access log entry first.
//! Listing a namespace also returns the global secrets it does not overwrite. Who may see which
//! namespace is the API's concern.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_types::{OrgId, UserId};
use sqlx::PgPool;

use crate::{DbError, Namespace, Result};

/// The jobs a secret is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutorSecretScope {
    /// Bulk changes executions.
    Batches,
    /// Auto-indexing jobs.
    CodeIntel,
}

impl ExecutorSecretScope {
    /// The stored name (`executor_secrets.scope`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Batches => "batches",
            Self::CodeIntel => "codeintel",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "batches" => Some(Self::Batches),
            "codeintel" => Some(Self::CodeIntel),
            _ => None,
        }
    }
}

/// A secret, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorSecret {
    pub id: i64,
    pub scope: ExecutorSecretScope,
    /// The environment variable name.
    pub key: String,
    /// `None`: a global secret.
    pub namespace: Option<Namespace>,
    /// `None` once the creator has been deleted.
    pub creator_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// A namespace's secret with the key of a global secret, which it overwrites.
    pub overwrites_global_secret: bool,
}

/// In whose name a secret value is decrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretAccessor<'a> {
    /// A signed-in user.
    User(UserId),
    /// An internal procedure, e.g. `codeintel-autoindexing`, for an executor job.
    Machine { name: &'a str, job_id: Option<i64> },
}

/// One decryption of a secret value.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExecutorSecretAccessLog {
    pub id: i64,
    pub executor_secret_id: i64,
    pub user_id: Option<UserId>,
    /// Empty when a user accessed the secret.
    pub machine_user: String,
    pub job_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// Options of [`ExecutorSecretStore::list`].
#[derive(Debug, Clone, Default)]
pub struct ListExecutorSecrets {
    /// The secrets usable in this namespace (its own and the global ones it does not
    /// overwrite); only the global secrets when `None`.
    pub namespace: Option<Namespace>,
    /// Only these keys.
    pub keys: Option<Vec<String>>,
    /// Page size; no limit when 0.
    pub limit: i64,
    pub offset: i64,
}

#[derive(sqlx::FromRow)]
struct SecretRow {
    id: i64,
    scope: String,
    key: String,
    namespace_user_id: Option<UserId>,
    namespace_org_id: Option<OrgId>,
    creator_id: Option<UserId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    overwrites_global_secret: bool,
}

impl TryFrom<SecretRow> for ExecutorSecret {
    type Error = DbError;

    fn try_from(r: SecretRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            scope: ExecutorSecretScope::parse(&r.scope)
                .ok_or(DbError::Invalid("unknown executor secret scope"))?,
            key: r.key,
            namespace: Namespace::from_columns(r.namespace_user_id, r.namespace_org_id),
            creator_id: r.creator_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
            overwrites_global_secret: r.overwrites_global_secret,
        })
    }
}

/// The columns of a secret row `s`, with whether it overwrites a global secret.
const COLUMNS: &str = "s.id, s.scope, s.key, s.namespace_user_id, s.namespace_org_id, \
     s.creator_id, s.created_at, s.updated_at, \
     (s.namespace_user_id IS NOT NULL OR s.namespace_org_id IS NOT NULL) AND EXISTS ( \
         SELECT 1 FROM executor_secrets g WHERE g.key = s.key AND g.scope = s.scope \
         AND g.namespace_user_id IS NULL AND g.namespace_org_id IS NULL \
     ) AS overwrites_global_secret";

/// The secrets of scope `$1` usable in the namespace (`$2` user, `$3` org, both NULL for the
/// global namespace), limited to keys `$4` unless NULL: a namespace's own secret wins over the
/// global one with the same key.
const VISIBLE: &str = "SELECT * FROM (
         SELECT s.*, rank() OVER (
             PARTITION BY s.key ORDER BY s.namespace_user_id NULLS LAST, s.namespace_org_id NULLS LAST
         ) AS rank
         FROM executor_secrets s
         WHERE s.scope = $1
           AND ((s.namespace_user_id IS NULL AND s.namespace_org_id IS NULL)
                OR s.namespace_user_id = $2::bigint OR s.namespace_org_id = $3::bigint)
           AND ($4::text[] IS NULL OR s.key = ANY($4))
     ) s WHERE s.rank = 1";

/// Reads and writes executor secrets. Writing or reading a value needs the keyring; without
/// one (no key file configured) those calls fail with [`DbError::Invalid`].
#[derive(Debug, Clone)]
pub struct ExecutorSecretStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl ExecutorSecretStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool, keyring: Option<Arc<Keyring>>) -> Self {
        Self { pool, keyring }
    }

    fn keyring(&self) -> Result<&Keyring> {
        self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)",
        ))
    }

    fn encrypt(&self, value: &str) -> Result<Vec<u8>> {
        Ok(self
            .keyring()?
            .encrypt(RecordClass::ExecutorSecret, value.as_bytes())?)
    }

    /// Creates a secret. A second secret with the same scope and key in the namespace fails
    /// with [`DbError::AlreadyExists`].
    pub async fn create(
        &self,
        scope: ExecutorSecretScope,
        key: &str,
        value: &str,
        namespace: Option<Namespace>,
        creator: Option<UserId>,
    ) -> Result<ExecutorSecret> {
        let enc = self.encrypt(value)?;
        let (user, org) = Namespace::columns(namespace);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO executor_secrets
                 (scope, key, value_enc, namespace_user_id, namespace_org_id, creator_id)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(scope.as_str())
        .bind(key)
        .bind(enc)
        .bind(user)
        .bind(org)
        .bind(creator)
        .fetch_one(&self.pool)
        .await?;
        self.get(scope, id)
            .await?
            .ok_or(DbError::NotFound("executor secret"))
    }

    /// Looks up a secret of `scope`.
    pub async fn get(&self, scope: ExecutorSecretScope, id: i64) -> Result<Option<ExecutorSecret>> {
        let row: Option<SecretRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM executor_secrets s WHERE s.id = $1 AND s.scope = $2"
        ))
        .bind(id)
        .bind(scope.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Replaces a secret's value; `None` if there is no such secret.
    pub async fn update_value(
        &self,
        scope: ExecutorSecretScope,
        id: i64,
        value: &str,
    ) -> Result<Option<ExecutorSecret>> {
        let enc = self.encrypt(value)?;
        let updated = sqlx::query(
            "UPDATE executor_secrets SET value_enc = $3, updated_at = now()
             WHERE id = $1 AND scope = $2",
        )
        .bind(id)
        .bind(scope.as_str())
        .bind(enc)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Ok(None);
        }
        self.get(scope, id).await
    }

    /// Deletes a secret and its access logs; false if there was none.
    pub async fn delete(&self, scope: ExecutorSecretScope, id: i64) -> Result<bool> {
        Ok(
            sqlx::query("DELETE FROM executor_secrets WHERE id = $1 AND scope = $2")
                .bind(id)
                .bind(scope.as_str())
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// The secrets usable in a namespace, ordered by key.
    pub async fn list(
        &self,
        scope: ExecutorSecretScope,
        opts: &ListExecutorSecrets,
    ) -> Result<Vec<ExecutorSecret>> {
        let (user, org) = Namespace::columns(opts.namespace);
        let rows: Vec<SecretRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM ({VISIBLE}) s ORDER BY s.key
             LIMIT CASE WHEN $5::bigint > 0 THEN $5::bigint END OFFSET $6"
        ))
        .bind(scope.as_str())
        .bind(user)
        .bind(org)
        .bind(&opts.keys)
        .bind(opts.limit)
        .bind(opts.offset.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// The number of secrets [`list`](Self::list) returns without a limit.
    pub async fn count(
        &self,
        scope: ExecutorSecretScope,
        opts: &ListExecutorSecrets,
    ) -> Result<i64> {
        let (user, org) = Namespace::columns(opts.namespace);
        Ok(
            sqlx::query_scalar(&format!("SELECT count(*) FROM ({VISIBLE}) s"))
                .bind(scope.as_str())
                .bind(user)
                .bind(org)
                .bind(&opts.keys)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Decrypts a secret's value, recording the access in its log first (each call is one
    /// entry).
    pub async fn value(&self, secret: &ExecutorSecret, by: SecretAccessor<'_>) -> Result<String> {
        let keyring = self.keyring()?;
        let (user, machine, job) = match by {
            SecretAccessor::User(u) => (Some(u), "", None),
            SecretAccessor::Machine { name, job_id } => (None, name, job_id),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO executor_secret_access_logs
                 (executor_secret_id, user_id, machine_user, job_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(secret.id)
        .bind(user)
        .bind(machine)
        .bind(job)
        .execute(&mut *tx)
        .await?;
        let enc: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value_enc FROM executor_secrets WHERE id = $1")
                .bind(secret.id)
                .fetch_optional(&mut *tx)
                .await?;
        let enc = enc.ok_or(DbError::NotFound("executor secret"))?;
        let plain = keyring.decrypt(RecordClass::ExecutorSecret, &enc)?;
        tx.commit().await?;
        String::from_utf8(plain.to_vec()).map_err(|_| DbError::Invalid("secret is not UTF-8"))
    }

    /// A page of a secret's access logs, newest first.
    pub async fn access_logs(
        &self,
        secret_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ExecutorSecretAccessLog>> {
        Ok(sqlx::query_as(
            "SELECT id, executor_secret_id, user_id, machine_user, job_id, created_at
             FROM executor_secret_access_logs WHERE executor_secret_id = $1
             ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(secret_id)
        .bind(limit)
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of a secret's access logs.
    pub async fn count_access_logs(&self, secret_id: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM executor_secret_access_logs WHERE executor_secret_id = $1",
        )
        .bind(secret_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Looks up an access log entry.
    pub async fn access_log(&self, id: i64) -> Result<Option<ExecutorSecretAccessLog>> {
        Ok(sqlx::query_as(
            "SELECT id, executor_secret_id, user_id, machine_user, job_id, created_at
             FROM executor_secret_access_logs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }
}
