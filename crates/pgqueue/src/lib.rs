//! Postgres-backed job queues.
//!
//! Each domain owns a queue table with the standard columns from
//! [`queue_columns_sql`] plus its own payload columns. [`Queue`] moves rows
//! through `queued → processing → completed | failed | canceled` using
//! `FOR UPDATE SKIP LOCKED`; [`Worker`] runs a [`Handler`] over a queue with
//! `LISTEN/NOTIFY` wake-ups, a polling fallback and heartbeats; [`Resetter`]
//! returns jobs whose worker stopped heartbeating.

pub mod admin;
mod queue;
mod schema;
mod worker;

pub use queue::{
    ExecutionLogEntry, Heartbeat, Job, JobState, Queue, QueueConfig, Record, StalledReset,
    notify_enqueued,
};
pub use schema::{
    MAX_TABLE_NAME_LEN, channel_name, create_queue_table_sql, queue_columns_sql, queue_indexes_sql,
    validate_table_name,
};
pub use worker::{
    Handler, JobContext, JobError, Resetter, Worker, WorkerOptions, default_worker_id,
};

/// Errors from queue operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The table name is not a plain lowercase identifier.
    #[error("invalid queue table name {0:?}")]
    InvalidTableName(String),
    /// A `state` value outside the known set.
    #[error("unknown job state {0:?}")]
    UnknownState(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
