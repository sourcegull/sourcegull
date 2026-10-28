//! `user_credentials` (domain `batches`) and `site_credentials`.
//!
//! A credential is a code-host token (a personal access token, or an OAuth token from the
//! GitLab login) with an optional SSH keypair, stored as encrypted JSON: user credentials
//! with the `user_credential` data key, site credentials with the `site_credential` key. Only
//! [`BatchesCredentialStore::secret`] decrypts. Resolution order is user, then site
//! ([`BatchesCredentialStore::resolve`]).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_crypto::{Keyring, RecordClass, ciphertext_dek_version};
use sourcegull_types::UserId;
use sqlx::PgPool;

use sourcegull_db::{Db, DbError, Result};

/// The `user_credentials.domain` of bulk changes credentials.
const DOMAIN: &str = "batches";

/// The kind of token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A personal (or project/group) access token.
    PersonalAccessToken,
    /// An OAuth bearer token.
    OAuthBearer,
}

/// An SSH keypair for pushing over SSH.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeypair {
    pub public_key: String,
    pub private_key: String,
    #[serde(default)]
    pub passphrase: String,
}

impl std::fmt::Debug for SshKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshKeypair")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// The secret part of a credential (decrypted).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSecret {
    pub kind: CredentialKind,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshKeypair>,
}

impl std::fmt::Debug for CredentialSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSecret")
            .field("kind", &self.kind)
            .field("ssh", &self.ssh)
            .finish_non_exhaustive()
    }
}

/// A credential without its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchesCredential {
    pub id: i64,
    /// The owner; `None` for a site credential.
    pub user_id: Option<UserId>,
    pub external_service_type: String,
    /// The code host's base URL.
    pub external_service_id: String,
    pub ssh_public_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BatchesCredential {
    /// Whether this is a site credential.
    pub fn is_site_credential(&self) -> bool {
        self.user_id.is_none()
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    user_id: Option<UserId>,
    external_service_type: String,
    external_service_id: String,
    ssh_public_key: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<Row> for BatchesCredential {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            external_service_type: r.external_service_type,
            external_service_id: r.external_service_id,
            ssh_public_key: r.ssh_public_key,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const USER_COLUMNS: &str = "id, user_id, external_service_type, external_service_id, ssh_public_key, created_at, updated_at";
const SITE_COLUMNS: &str = "id, NULL::bigint AS user_id, external_service_type, external_service_id, \
     ssh_public_key, created_at, updated_at";

/// Reads and writes bulk changes credentials. Writing or reading a secret needs the keyring;
/// without one those calls fail with [`DbError::Invalid`].
#[derive(Debug, Clone)]
pub struct BatchesCredentialStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl BatchesCredentialStore {
    pub fn new(db: &Db, keyring: Option<Arc<Keyring>>) -> Self {
        Self {
            pool: db.pool().clone(),
            keyring,
        }
    }

    fn keyring(&self) -> Result<&Keyring> {
        self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)",
        ))
    }

    fn encrypt(&self, class: RecordClass, secret: &CredentialSecret) -> Result<(Vec<u8>, i32)> {
        let json = sourcegull_crypto::Zeroizing::new(
            serde_json::to_vec(secret)
                .map_err(|_| DbError::Invalid("unserializable credential"))?,
        );
        let enc = self.keyring()?.encrypt(class, &json)?;
        let version = ciphertext_dek_version(&enc)?;
        Ok((enc, version))
    }

    /// Stores a user's credential for a code host, replacing an existing one.
    pub async fn set_user_credential(
        &self,
        user: UserId,
        external_service_type: &str,
        external_service_id: &str,
        secret: &CredentialSecret,
    ) -> Result<BatchesCredential> {
        let (enc, version) = self.encrypt(RecordClass::UserCredential, secret)?;
        let row: Row = sqlx::query_as(&format!(
            "INSERT INTO user_credentials (domain, user_id, external_service_type,
                 external_service_id, credential_enc, encryption_key_version, ssh_public_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (domain, user_id, external_service_type, external_service_id)
             DO UPDATE SET credential_enc = EXCLUDED.credential_enc,
                 encryption_key_version = EXCLUDED.encryption_key_version,
                 ssh_public_key = EXCLUDED.ssh_public_key, updated_at = now()
             RETURNING {USER_COLUMNS}"
        ))
        .bind(DOMAIN)
        .bind(user)
        .bind(external_service_type)
        .bind(external_service_id)
        .bind(enc)
        .bind(version)
        .bind(secret.ssh.as_ref().map(|k| &k.public_key))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    /// A user's credential for a code host.
    pub async fn user_credential(
        &self,
        user: UserId,
        external_service_type: &str,
        external_service_id: &str,
    ) -> Result<Option<BatchesCredential>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM user_credentials
             WHERE domain = $1 AND user_id = $2 AND external_service_type = $3
               AND external_service_id = $4"
        ))
        .bind(DOMAIN)
        .bind(user)
        .bind(external_service_type)
        .bind(external_service_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Looks up a user credential by id.
    pub async fn user_credential_by_id(&self, id: i64) -> Result<Option<BatchesCredential>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM user_credentials WHERE domain = $1 AND id = $2"
        ))
        .bind(DOMAIN)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// A user's credentials, by code host.
    pub async fn user_credentials(&self, user: UserId) -> Result<Vec<BatchesCredential>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM user_credentials WHERE domain = $1 AND user_id = $2
             ORDER BY external_service_type, external_service_id"
        ))
        .bind(DOMAIN)
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Deletes a user credential; false if there was none.
    pub async fn delete_user_credential(&self, id: i64) -> Result<bool> {
        Ok(
            sqlx::query("DELETE FROM user_credentials WHERE domain = $1 AND id = $2")
                .bind(DOMAIN)
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// Stores the site credential for a code host, replacing an existing one.
    pub async fn set_site_credential(
        &self,
        external_service_type: &str,
        external_service_id: &str,
        secret: &CredentialSecret,
    ) -> Result<BatchesCredential> {
        let (enc, version) = self.encrypt(RecordClass::SiteCredential, secret)?;
        let row: Row = sqlx::query_as(&format!(
            "INSERT INTO site_credentials (external_service_type, external_service_id,
                 credential_enc, encryption_key_version, ssh_public_key)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (external_service_type, external_service_id)
             DO UPDATE SET credential_enc = EXCLUDED.credential_enc,
                 encryption_key_version = EXCLUDED.encryption_key_version,
                 ssh_public_key = EXCLUDED.ssh_public_key, updated_at = now()
             RETURNING {SITE_COLUMNS}"
        ))
        .bind(external_service_type)
        .bind(external_service_id)
        .bind(enc)
        .bind(version)
        .bind(secret.ssh.as_ref().map(|k| &k.public_key))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    /// The site credential for a code host.
    pub async fn site_credential(
        &self,
        external_service_type: &str,
        external_service_id: &str,
    ) -> Result<Option<BatchesCredential>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {SITE_COLUMNS} FROM site_credentials
             WHERE external_service_type = $1 AND external_service_id = $2"
        ))
        .bind(external_service_type)
        .bind(external_service_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Looks up a site credential by id.
    pub async fn site_credential_by_id(&self, id: i64) -> Result<Option<BatchesCredential>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {SITE_COLUMNS} FROM site_credentials WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// All site credentials, by code host.
    pub async fn site_credentials(&self) -> Result<Vec<BatchesCredential>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {SITE_COLUMNS} FROM site_credentials
             ORDER BY external_service_type, external_service_id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Deletes a site credential; false if there was none.
    pub async fn delete_site_credential(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM site_credentials WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Decrypts a credential's secret.
    pub async fn secret(&self, credential: &BatchesCredential) -> Result<CredentialSecret> {
        let keyring = self.keyring()?;
        let (class, sql) = if credential.is_site_credential() {
            (
                RecordClass::SiteCredential,
                "SELECT credential_enc FROM site_credentials WHERE id = $1",
            )
        } else {
            (
                RecordClass::UserCredential,
                "SELECT credential_enc FROM user_credentials WHERE id = $1",
            )
        };
        let enc: Option<Vec<u8>> = sqlx::query_scalar(sql)
            .bind(credential.id)
            .fetch_optional(&self.pool)
            .await?;
        let enc = enc.ok_or(DbError::NotFound("credential"))?;
        let plain = keyring.decrypt(class, &enc)?;
        serde_json::from_slice(&plain).map_err(|_| DbError::Invalid("malformed stored credential"))
    }

    /// The credential to publish with for `user` on a code host: the user's own, else the
    /// site's.
    pub async fn resolve(
        &self,
        user: UserId,
        external_service_type: &str,
        external_service_id: &str,
    ) -> Result<Option<(BatchesCredential, CredentialSecret)>> {
        let credential = match self
            .user_credential(user, external_service_type, external_service_id)
            .await?
        {
            Some(c) => Some(c),
            None => {
                self.site_credential(external_service_type, external_service_id)
                    .await?
            }
        };
        match credential {
            Some(c) => {
                let secret = self.secret(&c).await?;
                Ok(Some((c, secret)))
            }
            None => Ok(None),
        }
    }

    /// Re-encrypts credentials not encrypted with the current data keys (after a key
    /// rotation). Returns how many were re-encrypted.
    pub async fn reencrypt(&self) -> Result<u64> {
        let keyring = self.keyring()?;
        let mut n = 0;
        for (class, table) in [
            (RecordClass::UserCredential, "user_credentials"),
            (RecordClass::SiteCredential, "site_credentials"),
        ] {
            let Some(current) = keyring.current_version(class) else {
                continue;
            };
            let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(&format!(
                "SELECT id, credential_enc FROM {table} WHERE encryption_key_version <> $1"
            ))
            .bind(current)
            .fetch_all(&self.pool)
            .await?;
            for (id, enc) in rows {
                let plain = keyring.decrypt(class, &enc)?;
                let new = keyring.encrypt(class, &plain)?;
                n += sqlx::query(&format!(
                    "UPDATE {table} SET credential_enc = $2, encryption_key_version = $3
                     WHERE id = $1 AND credential_enc = $4"
                ))
                .bind(id)
                .bind(new)
                .bind(current)
                .bind(enc)
                .execute(&self.pool)
                .await?
                .rows_affected();
            }
        }
        Ok(n)
    }
}
