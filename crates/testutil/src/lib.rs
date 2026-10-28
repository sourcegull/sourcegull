//! Test helpers shared across crates (dev-dependency only).
//!
//! Database tests read `SOURCEGULL_TEST_DATABASE_URL` (start one with
//! `scripts/dev-postgres.sh`). When it is unset, [`TestDb::new`] returns
//! `None` and the test should return early, so `cargo test` still passes on
//! machines without Postgres. With `SOURCEGULL_REQUIRE_TEST_DATABASE=1` (set by
//! `scripts/dev test`) a missing URL panics instead, so no test skips silently;
//! an unreachable server always panics.

#[cfg(feature = "git")]
pub mod git;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection};
use std::str::FromStr;

pub const ENV: &str = "SOURCEGULL_TEST_DATABASE_URL";
/// When set to `1`, a missing [`ENV`] fails the test instead of skipping it.
pub const REQUIRE_ENV: &str = "SOURCEGULL_REQUIRE_TEST_DATABASE";

/// The test server URL, or `None` when database tests should be skipped.
///
/// # Panics
///
/// When [`ENV`] is unset and [`REQUIRE_ENV`] is `1`.
pub fn database_url() -> Option<String> {
    match std::env::var(ENV) {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var(REQUIRE_ENV).is_ok_and(|v| v == "1") => {
            panic!("{ENV} is not set and {REQUIRE_ENV}=1: database tests must not be skipped")
        }
        _ => None,
    }
}

/// A fresh, uniquely named database, dropped when the value is dropped.
pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
    name: String,
    admin: PgConnectOptions,
}

impl TestDb {
    /// Creates an empty database. Returns `None` when no test server is configured
    /// (see [`database_url`]).
    pub async fn new() -> Option<Self> {
        let base = database_url()?;
        let admin = PgConnectOptions::from_str(&base).expect("valid SOURCEGULL_TEST_DATABASE_URL");
        let name = format!("sourcegull_test_{}", uuid::Uuid::new_v4().simple());
        let mut conn = admin.connect().await.expect("connect to test postgres");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();
        let opts = admin.clone().database(&name);
        let url = opts.to_url_lossy().to_string();
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .expect("connect to test database");
        Some(Self {
            pool,
            url,
            name,
            admin,
        })
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Drop the database from a separate thread with its own runtime: the
        // test's runtime may be single-threaded and is blocked here. The pool
        // is not closed gracefully (that would need the blocked runtime);
        // `WITH (FORCE)` terminates its connections instead. Failures are ignored.
        let admin = self.admin.clone();
        let name = std::mem::take(&mut self.name);
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut c) = PgConnection::connect_with(&admin).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&mut c)
                        .await;
                }
            });
        })
        .join();
    }
}
