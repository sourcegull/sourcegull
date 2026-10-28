//! Database access for Sourcegull: the Postgres pool, schema migrations and the
//! per-domain stores.
//!
//! # Migration sets and schemas
//!
//! Migrations live in `crates/db/migrations/<set>/` as reversible sqlx
//! migrations (`<version>_<name>.up.sql` / `.down.sql`) and are embedded in the
//! binary. The only set today is **`main`**. Its tables live in the default
//! `public` schema; the set is named `main` so that the `codeintel` and
//! `insights` sets can be added later, each with its own schema
//! and history table, without renaming anything.
//!
//! Migrations must follow expand → migrate → contract: a release's
//! schema stays compatible with the previous minor version's code.
//!
//! # Stores
//!
//! Stores are cheap handles around a cloned [`sqlx::PgPool`] and use runtime
//! (not compile-time checked) queries. Get them from [`Db`], e.g. `db.users()`.
//!
//! Queue tables (`external_service_sync_jobs`, `repo_update_jobs`) have the standard
//! [`sourcegull_pgqueue`] shape; their stores enqueue and list, and workers run them with
//! `sourcegull_pgqueue::Queue`.

mod db;
mod error;
mod fence;
pub mod migrate;
pub mod notify;
pub mod schema;
pub mod stores;
pub mod upgrade;

pub use db::{Db, DbOptions};
pub use error::{DbError, Result};
pub use fence::Fence;
pub use stores::*;

/// Re-export so dependents use the same sqlx version.
pub use sqlx;
