//! Error type shared by all stores.

/// Result alias for this crate.
pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Errors returned by the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The requested row does not exist (or is soft-deleted).
    #[error("{0} not found")]
    NotFound(&'static str),
    /// A unique constraint was violated, e.g. a username is taken.
    #[error("already exists (constraint {constraint})")]
    AlreadyExists {
        /// Name of the violated constraint or index.
        constraint: String,
    },
    /// An optimistic-concurrency check failed: someone else wrote a newer
    /// version first. The caller should reload and retry.
    #[error("conflict: expected latest version {expected:?}, found {actual:?}")]
    Conflict {
        /// The version id the caller based its change on.
        expected: Option<i64>,
        /// The actual latest version id.
        actual: Option<i64>,
    },
    /// A singleton's write was refused because another process took its lease since this
    /// one did (see [`crate::Fence`]).
    #[error("lease {name} moved on from epoch {epoch} (now {current:?})")]
    Fenced {
        /// The lease name.
        name: String,
        /// The epoch the writer holds.
        epoch: i64,
        /// The current epoch.
        current: Option<i64>,
    },
    /// The operation is not allowed in the current state.
    #[error("invalid operation: {0}")]
    Invalid(&'static str),
    /// Encrypting or decrypting a stored secret failed, or no keyring is configured.
    #[error(transparent)]
    Crypto(#[from] sourcegull_crypto::CryptoError),
    /// A queue operation failed.
    #[error(transparent)]
    Queue(#[from] sourcegull_pgqueue::Error),
    /// Applying or reverting migrations failed.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// Any other database error.
    #[error(transparent)]
    Sqlx(sqlx::Error),
}

impl From<sqlx::Error> for DbError {
    fn from(err: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &err
            && db.is_unique_violation()
        {
            return DbError::AlreadyExists {
                constraint: db.constraint().unwrap_or_default().to_owned(),
            };
        }
        DbError::Sqlx(err)
    }
}

impl DbError {
    /// Whether this is [`DbError::NotFound`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, DbError::NotFound(_))
    }

    /// Whether this is [`DbError::Fenced`].
    pub fn is_fenced(&self) -> bool {
        matches!(self, DbError::Fenced { .. })
    }

    /// Whether this is [`DbError::Conflict`].
    pub fn is_conflict(&self) -> bool {
        matches!(self, DbError::Conflict { .. })
    }
}
