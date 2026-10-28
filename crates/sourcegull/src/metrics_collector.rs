//! Domain gauges read from Postgres: queue depths, executors, repositories and the database itself.
//! The dashboards and alerts of `sourcegull-monitoring` are built on them.
//!
//! Every worker process runs [`run`] (no lease: a few cheap queries every
//! [`COLLECT_INTERVAL`], and no stale values left behind by a former leader), so queries
//! aggregate with `max` across instances. [`run_pool_gauges`] runs in every process and
//! reports its own connection pool.

use std::time::Duration;

use sourcegull_codeintel_policies::store::CoverageStore;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_types::metric_names;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// How often the gauges are refreshed.
pub const COLLECT_INTERVAL: Duration = Duration::from_secs(30);

/// Jobs per queue table and state (`queued`, `processing`, `errored`, `failed`).
pub const QUEUE_JOBS_METRIC: &str = metric_names::QUEUE_JOBS;
/// Age in seconds of the oldest queued job per queue table (0 when none is queued).
pub const QUEUE_OLDEST_METRIC: &str = metric_names::QUEUE_OLDEST_QUEUED_SECONDS;
/// Executors that sent a heartbeat in the last [`EXECUTOR_ACTIVE_WINDOW`].
pub const EXECUTORS_ACTIVE_METRIC: &str = metric_names::EXECUTORS_ACTIVE;
/// Jobs the active executors report running.
pub const EXECUTOR_JOBS_METRIC: &str = metric_names::EXECUTOR_RUNNING_JOBS;
/// Repositories (not deleted) by clone status (`not_cloned`, `cloning`, `cloned`).
pub const REPOS_METRIC: &str = metric_names::REPOS;
/// Repositories whose last fetch failed.
pub const REPOS_ERRORED_METRIC: &str = metric_names::REPOS_FETCH_ERRORS;
/// Repositories marked corrupt by their repo node.
pub const REPOS_CORRUPT_METRIC: &str = metric_names::REPOS_CORRUPTED;
/// Repositories whose last index build failed.
pub const INDEX_ERRORS_METRIC: &str = metric_names::INDEX_REPOS_ERRORED;
/// Repositories by text index status (`indexed`, `not_indexed`).
pub const INDEX_REPOS_METRIC: &str = metric_names::INDEX_REPOS;
/// Repository moves between repo nodes by state (`queued`, `copying`, `flipped`, `failed`;
/// rebalancing).
pub const REPO_MOVES_METRIC: &str = metric_names::REPO_MOVES;
/// 1 when the last collection reached Postgres, else 0.
pub const POSTGRES_UP_METRIC: &str = metric_names::POSTGRES_UP;
/// Connections to Sourcegull's database by state (from `pg_stat_activity`).
pub const POSTGRES_CONNECTIONS_METRIC: &str = metric_names::POSTGRES_CONNECTIONS;
/// The server's `max_connections`.
pub const POSTGRES_MAX_CONNECTIONS_METRIC: &str = metric_names::POSTGRES_MAX_CONNECTIONS;
/// Size of Sourcegull's database in bytes.
pub const POSTGRES_SIZE_METRIC: &str = metric_names::POSTGRES_DATABASE_SIZE_BYTES;
/// Age in seconds of the oldest open transaction on Sourcegull's database.
pub const POSTGRES_OLDEST_TX_METRIC: &str = metric_names::POSTGRES_OLDEST_TRANSACTION_SECONDS;
/// Repositories by precise index coverage state (`up_to_date`, `stale`, `failing`, `none`,
/// `not_targeted`; see `sourcegull_codeintel_policies::store::CoverageStore`).
pub const CODEINTEL_COVERAGE_METRIC: &str = metric_names::CODEINTEL_COVERAGE_REPOS;
/// Failing repositories whose latest failure is an index job (`source="index_job"`) or an
/// upload (`source="upload"`).
pub const CODEINTEL_FAILING_METRIC: &str = metric_names::CODEINTEL_COVERAGE_FAILING_REPOS;
/// Connections of this process's pool by state (`in_use`, `idle`, `max`), labelled `pool`.
pub const DB_POOL_METRIC: &str = metric_names::DB_POOL_CONNECTIONS;

/// The coverage gauges are refreshed every this many collections (their query reads every
/// repository's uploads and index jobs).
const COVERAGE_EVERY: u32 = 10;
/// The stale window of the coverage gauges (the GraphQL report's default).
const COVERAGE_STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// Executors seen within this window count as active (they heartbeat every 5 s).
pub const EXECUTOR_ACTIVE_WINDOW: Duration = Duration::from_secs(60);

const QUEUE_STATES: &[&str] = &["queued", "processing", "errored", "failed"];

/// Refreshes the domain gauges every [`COLLECT_INTERVAL`] (the coverage gauges every
/// [`COVERAGE_EVERY`] collections) until `cancel` fires.
pub async fn run(db: Db, config: SiteConfigHandle, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(COLLECT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: u32 = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let mut ok = match collect(db.pool()).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "collecting metrics from Postgres failed");
                false
            }
        };
        if n % COVERAGE_EVERY == 0 {
            let (enabled, allow_global) = {
                let site = &config.load().value;
                (
                    site.code_intel_auto_indexing_enabled,
                    site.code_intel_auto_indexing_allow_global_policies,
                )
            };
            if let Err(e) = collect_coverage(&db, enabled, allow_global).await {
                tracing::warn!(error = %e, "collecting precise index coverage failed");
                ok = false;
            }
        }
        n = n.wrapping_add(1);
        metrics::gauge!(POSTGRES_UP_METRIC).set(if ok { 1.0 } else { 0.0 });
    }
}

/// Sets the precise index coverage gauges; `enabled` and `allow_global` are the site's
/// auto-indexing settings, which decide which repositories are targeted.
pub async fn collect_coverage(db: &Db, enabled: bool, allow_global: bool) -> anyhow::Result<()> {
    let store = CoverageStore::new(db);
    let targets = store.targets(enabled, allow_global).await?;
    let counts = store
        .counts(&sourcegull_codeintel_policies::store::CoverageOptions {
            targets,
            stale_after: COVERAGE_STALE_AFTER,
        })
        .await?;
    for state in sourcegull_codeintel_policies::store::CoverageState::ALL {
        metrics::gauge!(CODEINTEL_COVERAGE_METRIC, "state" => state.as_str())
            .set(counts.get(state) as f64);
    }
    metrics::gauge!(CODEINTEL_FAILING_METRIC, "source" => "index_job")
        .set(counts.failing_index_jobs as f64);
    metrics::gauge!(CODEINTEL_FAILING_METRIC, "source" => "upload")
        .set((counts.failing - counts.failing_index_jobs) as f64);
    Ok(())
}

/// Reports a connection pool of this process (`main`, or `worker` when the worker role has its
/// own) every [`COLLECT_INTERVAL`] until `cancel` fires.
pub async fn run_pool_gauges(db: Db, pool: &'static str, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(COLLECT_INTERVAL);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => set_pool_gauges(pool, db.pool()),
        }
    }
}

fn set_pool_gauges(pool: &'static str, p: &PgPool) {
    let size = f64::from(p.size());
    let idle = p.num_idle() as f64;
    let max = f64::from(p.options().get_max_connections());
    metrics::gauge!(DB_POOL_METRIC, "pool" => pool, "state" => "in_use").set(size - idle);
    metrics::gauge!(DB_POOL_METRIC, "pool" => pool, "state" => "idle").set(idle);
    metrics::gauge!(DB_POOL_METRIC, "pool" => pool, "state" => "max").set(max);
}

/// One collection pass.
pub async fn collect(pool: &PgPool) -> anyhow::Result<()> {
    collect_queues(pool).await?;
    collect_executors(pool).await?;
    collect_repos(pool).await?;
    collect_postgres(pool).await?;
    Ok(())
}

async fn collect_queues(pool: &PgPool) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    for table in sourcegull_pgqueue::admin::queue_tables(pool).await? {
        let s = sourcegull_pgqueue::admin::summarize(pool, &table, 0).await?;
        for (state, n) in QUEUE_STATES
            .iter()
            .zip([s.queued, s.processing, s.errored, s.failed])
        {
            metrics::gauge!(QUEUE_JOBS_METRIC, "queue" => table.clone(), "state" => *state)
                .set(n as f64);
        }
        let oldest = s
            .oldest_queued_at
            .map_or(0.0, |t| (now - t).num_milliseconds().max(0) as f64 / 1000.0);
        metrics::gauge!(QUEUE_OLDEST_METRIC, "queue" => table).set(oldest);
    }
    Ok(())
}

async fn collect_executors(pool: &PgPool) -> anyhow::Result<()> {
    let (active, jobs): (i64, i64) = sqlx::query_as(
        "SELECT count(*), coalesce(sum(jsonb_array_length(active_jobs)), 0)::bigint
         FROM executor_heartbeats WHERE last_seen_at >= now() - make_interval(secs => $1)",
    )
    .bind(EXECUTOR_ACTIVE_WINDOW.as_secs_f64())
    .fetch_one(pool)
    .await?;
    metrics::gauge!(EXECUTORS_ACTIVE_METRIC).set(active as f64);
    metrics::gauge!(EXECUTOR_JOBS_METRIC).set(jobs as f64);
    Ok(())
}

async fn collect_repos(pool: &PgPool) -> anyhow::Result<()> {
    let (not_cloned, cloning, cloned, errored, corrupt): (i64, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT count(*) FILTER (WHERE g.clone_status = 'not_cloned'),
                    count(*) FILTER (WHERE g.clone_status = 'cloning'),
                    count(*) FILTER (WHERE g.clone_status = 'cloned'),
                    count(*) FILTER (WHERE g.last_error IS NOT NULL),
                    count(*) FILTER (WHERE g.corrupted_at IS NOT NULL)
             FROM gitserver_repos g JOIN repo r ON r.id = g.repo_id
             WHERE r.deleted_at IS NULL",
        )
        .fetch_one(pool)
        .await?;
    for (status, n) in [
        ("not_cloned", not_cloned),
        ("cloning", cloning),
        ("cloned", cloned),
    ] {
        metrics::gauge!(REPOS_METRIC, "clone_status" => status).set(n as f64);
    }
    metrics::gauge!(REPOS_ERRORED_METRIC).set(errored as f64);
    metrics::gauge!(REPOS_CORRUPT_METRIC).set(corrupt as f64);

    let (indexed, not_indexed, index_errors): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE s.index_status = 'indexed'),
                count(*) FILTER (WHERE s.index_status = 'not_indexed'),
                count(*) FILTER (WHERE s.last_error IS NOT NULL)
         FROM search_index_repos s JOIN repo r ON r.id = s.repo_id
         WHERE r.deleted_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    metrics::gauge!(INDEX_REPOS_METRIC, "status" => "indexed").set(indexed as f64);
    metrics::gauge!(INDEX_REPOS_METRIC, "status" => "not_indexed").set(not_indexed as f64);
    metrics::gauge!(INDEX_ERRORS_METRIC).set(index_errors as f64);

    let moves = sourcegull_db::RepoMoveStore::new(pool.clone())
        .counts()
        .await?;
    for (state, n) in [
        ("queued", moves.queued),
        ("copying", moves.copying),
        ("flipped", moves.flipped),
        ("failed", moves.failed),
    ] {
        metrics::gauge!(REPO_MOVES_METRIC, "state" => state).set(n as f64);
    }
    Ok(())
}

async fn collect_postgres(pool: &PgPool) -> anyhow::Result<()> {
    let states: Vec<(String, i64)> = sqlx::query_as(
        "SELECT coalesce(state, 'unknown'), count(*) FROM pg_stat_activity
         WHERE datname = current_database() GROUP BY 1",
    )
    .fetch_all(pool)
    .await?;
    for state in ["active", "idle", "idle in transaction"] {
        let n = states
            .iter()
            .find(|(s, _)| s == state)
            .map_or(0, |(_, n)| *n);
        metrics::gauge!(POSTGRES_CONNECTIONS_METRIC, "state" => state).set(n as f64);
    }
    let other: i64 = states
        .iter()
        .filter(|(s, _)| !matches!(s.as_str(), "active" | "idle" | "idle in transaction"))
        .map(|(_, n)| n)
        .sum();
    metrics::gauge!(POSTGRES_CONNECTIONS_METRIC, "state" => "other").set(other as f64);

    let (max_conns, size, oldest_tx): (i64, i64, f64) = sqlx::query_as(
        "SELECT current_setting('max_connections')::bigint,
                pg_database_size(current_database()),
                coalesce(extract(epoch FROM max(now() - xact_start)), 0)::float8
         FROM pg_stat_activity
         WHERE datname = current_database() AND pid <> pg_backend_pid()",
    )
    .fetch_one(pool)
    .await?;
    metrics::gauge!(POSTGRES_MAX_CONNECTIONS_METRIC).set(max_conns as f64);
    metrics::gauge!(POSTGRES_SIZE_METRIC).set(size as f64);
    metrics::gauge!(POSTGRES_OLDEST_TX_METRIC).set(oldest_tx);
    Ok(())
}
