//! Lease epochs, which fence the writes of leader-elected singletons.
//!
//! Each time a process takes a singleton's lease it advances the singleton's epoch in
//! `leader_epochs` ([`Fence::advance`]) and hands the resulting [`Fence`] to the task. A fenced
//! write checks the fence inside its transaction ([`Fence::check`], or [`Fence::begin`]): the
//! check takes a share lock on the epoch row, so a successor's advance waits for the write to
//! commit, and a write that starts after the advance fails with [`DbError::Fenced`]. A leader
//! that lost its lease (a paused process, a network partition) therefore cannot overwrite the
//! state its successor writes.

use std::sync::Arc;

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{DbError, Result};

/// A singleton's lease epoch as of when this process took the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    name: Arc<str>,
    /// `None` for work outside a lease, which is not fenced.
    epoch: Option<i64>,
}

impl Fence {
    /// Advances the epoch of lease `name` (just taken by this process) and returns its fence.
    pub async fn advance(pool: &PgPool, name: &str) -> Result<Self> {
        let epoch: i64 = sqlx::query_scalar(
            "INSERT INTO leader_epochs (name, epoch) VALUES ($1, 1)
             ON CONFLICT (name) DO UPDATE
                 SET epoch = leader_epochs.epoch + 1, acquired_at = now()
             RETURNING epoch",
        )
        .bind(name)
        .fetch_one(pool)
        .await?;
        Ok(Self {
            name: name.into(),
            epoch: Some(epoch),
        })
    }

    /// A fence that every check passes, for work run outside a lease (tests, admin commands).
    pub fn none(name: &str) -> Self {
        Self {
            name: name.into(),
            epoch: None,
        }
    }

    /// The lease name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The epoch, `None` for an unfenced [`Fence::none`].
    pub fn epoch(&self) -> Option<i64> {
        self.epoch
    }

    /// Fails with [`DbError::Fenced`] when another process took the lease since this fence was
    /// made. Call it inside the write's transaction: it share-locks the epoch row until the
    /// transaction ends.
    pub async fn check(&self, conn: &mut PgConnection) -> Result<()> {
        let Some(epoch) = self.epoch else {
            return Ok(());
        };
        let current: Option<i64> =
            sqlx::query_scalar("SELECT epoch FROM leader_epochs WHERE name = $1 FOR SHARE")
                .bind(&*self.name)
                .fetch_optional(conn)
                .await?;
        if current == Some(epoch) {
            Ok(())
        } else {
            Err(DbError::Fenced {
                name: self.name.to_string(),
                epoch,
                current,
            })
        }
    }

    /// Begins a transaction on `pool` and [checks](Fence::check) the fence in it.
    pub async fn begin(&self, pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
        let mut tx = pool.begin().await?;
        self.check(&mut tx).await?;
        Ok(tx)
    }
}
