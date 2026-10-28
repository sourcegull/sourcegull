//! Command-line interface. Every option can also be set through its `SOURCEGULL_*` variable.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use sourcegull_db::DbOptions;

use crate::roles::{Role, RoleSet};
use crate::startup::KeySource;

/// Sourcegull: self-hosted code search and intelligence.
#[derive(Debug, Parser)]
#[command(name = "sourcegull", version, about)]
pub struct Cli {
    /// Log output format (filters come from RUST_LOG, default `info`).
    #[arg(long, env = "SOURCEGULL_LOG_FORMAT", value_enum, default_value_t = LogFormatArg::Json, global = true)]
    pub log_format: LogFormatArg,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormatArg {
    /// One JSON object per line.
    Json,
    /// Human-readable output for development.
    Pretty,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one or more server roles.
    Run(RunArgs),
    /// Apply, revert or inspect database schema migrations.
    Migrate(MigrateArgs),
    /// Inspect or rotate the data encryption keys.
    Keys(KeysArgs),
    /// Check configuration files offline (for CI pipelines).
    Config(ConfigArgs),
    /// Create the key file if missing, apply migrations and run every role in one process.
    Supervise(SuperviseArgs),
    /// Run an executor: take jobs from the web role's executor queues and run them in containers.
    Executor(Box<ExecutorArgs>),
    /// Generate the Grafana dashboards, Prometheus alerting rules and their docs.
    Monitoring(MonitoringArgs),
    /// Print version information.
    Version,
}

/// `sourcegull run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Comma-separated roles (`web`, `repo`, `worker`, `blob`) or `all`.
    #[arg(long, env = "SOURCEGULL_ROLES", default_value = "all")]
    pub roles: RoleSet,

    /// Postgres URL of the main database.
    #[arg(long, env = "SOURCEGULL_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    #[command(flatten)]
    pub server: ServerArgs,
}

/// Options shared by `run` and `supervise`.
#[derive(Debug, Clone, Args)]
pub struct ServerArgs {
    /// Public listener (UI, /.api/*, /healthz, /readyz).
    #[arg(long, env = "SOURCEGULL_HTTP_ADDR", default_value = "0.0.0.0:7080")]
    pub http_addr: SocketAddr,

    /// Internal listener (/metrics, /healthz, /readyz).
    #[arg(long, env = "SOURCEGULL_INTERNAL_ADDR", default_value = "0.0.0.0:6060")]
    pub internal_addr: SocketAddr,

    /// Directory for persistent data.
    #[arg(
        long,
        env = "SOURCEGULL_DATA_DIR",
        default_value = "/var/opt/sourcegull"
    )]
    pub data_dir: PathBuf,

    /// Encryption key file (`id:base64key` per line). Default: `$SOURCEGULL_DATA_DIR/keys`. The
    /// keys can instead be given in `SOURCEGULL_KEYS` (entries separated by commas or newlines),
    /// which is read once at startup and removed from the environment.
    #[arg(long, env = "SOURCEGULL_KEYS_FILE", value_parser = optional_path)]
    pub keys_file: Option<PathBuf>,

    /// Read the site config from this file instead of the database (read-only in the UI/API).
    #[arg(long, env = "SOURCEGULL_SITE_CONFIG_FILE", value_parser = optional_path)]
    pub site_config_file: Option<PathBuf>,

    /// Resources file (connections, search contexts, roles, site admins, outbound webhooks,
    /// executor secrets, code intelligence policies, feature flags), applied by the leader
    /// worker; what it declares is read-only in the UI/API.
    #[arg(long, env = "SOURCEGULL_RESOURCES_FILE", value_parser = optional_path)]
    pub resources_file: Option<PathBuf>,

    /// Global settings file, applied by the leader worker; global settings are then read-only
    /// in the UI/API.
    #[arg(long, env = "SOURCEGULL_GLOBAL_SETTINGS_FILE", value_parser = optional_path)]
    pub global_settings_file: Option<PathBuf>,

    /// Default for `externalURL` when the site config does not set it.
    #[arg(long, env = "SOURCEGULL_EXTERNAL_URL")]
    pub external_url: Option<String>,

    /// Blob storage: `file:///path`, `http://host:port` of a `blob` role, or `s3://bucket[/prefix]`
    /// for an S3-compatible bucket (endpoint, region and credentials in `SOURCEGULL_S3_*`, see
    /// `sourcegull_blobstore::S3Config::from_url`). Defaults to `<data-dir>/blobs`, which the `blob`
    /// role also serves.
    #[arg(long, env = "SOURCEGULL_BLOB_STORE")]
    pub blob_store: Option<String>,

    /// On SIGTERM or SIGINT, how long `/readyz` reports the process as shutting down while it
    /// keeps serving, before the listeners close and the roles stop (`5s`, `500ms`, `1m`, or
    /// seconds). `0` stops at once.
    #[arg(long, env = "SOURCEGULL_SHUTDOWN_DELAY", default_value = "5s", value_parser = parse_duration)]
    pub shutdown_delay: std::time::Duration,

    #[command(flatten)]
    pub repo: RepoArgs,

    #[command(flatten)]
    pub shared: SharedStateArgs,

    #[command(flatten)]
    pub db_pool: DbPoolArgs,
}

/// Database connection budgets. `LISTEN` and leader leases use their own connections outside these
/// pools (one `LISTEN` connection per process, one per lease).
#[derive(Debug, Clone, Args)]
pub struct DbPoolArgs {
    /// Maximum connections of the process's main pool (requests of the `web` and `repo`
    /// roles; everything when the process runs only `worker`).
    #[arg(long, env = "SOURCEGULL_DB_MAX_CONNECTIONS", default_value_t = 20, value_parser = clap::value_parser!(u32).range(2..))]
    pub db_max_connections: u32,

    /// Maximum connections of the `worker` role's own pool when it shares the process with
    /// other roles (`--roles=all`, `supervise`), so background jobs cannot starve requests.
    #[arg(long, env = "SOURCEGULL_DB_WORKER_MAX_CONNECTIONS", default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
    pub db_worker_max_connections: u32,
}

/// Where state shared between processes lives: code host request budgets and the leases of
/// singleton tasks.
#[derive(Debug, Clone, Args)]
pub struct SharedStateArgs {
    /// Code host request budgets: `postgres` shares each connection's `rateLimit` between all
    /// processes, `memory` keeps a budget per process. `auto`: `memory` when this process runs
    /// the web, repo and worker roles together (a single-process install), else `postgres`.
    #[arg(long, env = "SOURCEGULL_RATE_LIMIT_BACKEND", value_enum, default_value_t = RateLimitBackend::Auto)]
    pub rate_limit_backend: RateLimitBackend,

    /// Leases of singleton tasks: Postgres advisory locks, or Kubernetes `Lease` objects
    /// (needs RBAC on `coordination.k8s.io` leases; the Helm chart grants it).
    #[arg(long, env = "SOURCEGULL_LEADER_BACKEND", value_enum, default_value_t = LeaderBackend::Postgres)]
    pub leader_backend: LeaderBackend,

    /// Namespace of the `Lease` objects. Default: the pod's own namespace.
    #[arg(long, env = "SOURCEGULL_LEADER_NAMESPACE")]
    pub leader_namespace: Option<String>,

    /// Prefix of the `Lease` object names (`<prefix>-<task>`), e.g. the Helm release name.
    #[arg(
        long,
        env = "SOURCEGULL_LEADER_LEASE_PREFIX",
        default_value = "sourcegull"
    )]
    pub leader_lease_prefix: String,

    /// This process's holder identity. Default: the host (pod) name with a random suffix.
    #[arg(long, env = "SOURCEGULL_LEADER_IDENTITY")]
    pub leader_identity: Option<String>,

    /// `leaseDurationSeconds` of the `Lease` objects: how long a crashed leader blocks a
    /// takeover. Must exceed the 5 s renewal interval.
    #[arg(long, env = "SOURCEGULL_LEADER_LEASE_DURATION", default_value_t = 15)]
    pub leader_lease_duration: u64,
}

/// Where code host request budgets live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RateLimitBackend {
    Auto,
    Memory,
    Postgres,
}

/// Where singleton leases live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LeaderBackend {
    Postgres,
    Kubernetes,
}

/// Repo node settings. The node list is needed by every process that talks to repo nodes (`web`)
/// and by repo nodes themselves.
#[derive(Debug, Clone, Args)]
pub struct RepoArgs {
    /// Comma-separated repo node addresses, e.g.
    /// `http://sourcegull-repo-0.sourcegull-repo:3178,http://sourcegull-repo-1.sourcegull-repo:3178`. Every
    /// process must list them with the same spelling. Default with the `repo` role: just this
    /// node (single node).
    #[arg(long, env = "SOURCEGULL_REPO_NODES", value_delimiter = ',')]
    pub repo_nodes: Vec<String>,

    /// This repo node's own address as listed in `SOURCEGULL_REPO_NODES`. Default: the only listed
    /// node, or the one whose host is `$HOSTNAME` (StatefulSet pods).
    #[arg(long, env = "SOURCEGULL_REPO_NODE_ADDR")]
    pub repo_node_addr: Option<String>,

    /// gRPC listener of the `repo` role.
    #[arg(
        long,
        env = "SOURCEGULL_REPO_GRPC_ADDR",
        default_value = "0.0.0.0:3178"
    )]
    pub repo_grpc_addr: SocketAddr,

    /// Directory for bare repos, index shards and caches. Default: `$SOURCEGULL_DATA_DIR`.
    #[arg(long, env = "SOURCEGULL_REPO_DATA_DIR")]
    pub repo_data_dir: Option<PathBuf>,

    /// Concurrent fetches per repo node (clones use the site config `gitMaxConcurrentClones`).
    #[arg(
        long,
        env = "SOURCEGULL_REPO_MAX_CONCURRENT_FETCHES",
        default_value_t = 10
    )]
    pub repo_max_concurrent_fetches: usize,

    /// Private SSH key for code host connections that clone over SSH.
    #[arg(long, env = "SOURCEGULL_REPO_SSH_KEY")]
    pub repo_ssh_key: Option<PathBuf>,

    /// `known_hosts` file for SSH clones (host keys are then checked strictly).
    #[arg(long, env = "SOURCEGULL_REPO_SSH_KNOWN_HOSTS")]
    pub repo_ssh_known_hosts: Option<PathBuf>,

    /// Do not keep a search index on this node; content search then runs unindexed. The
    /// variable accepts `1`/`0`, `true`/`false`, `yes`/`no` and `on`/`off`.
    #[arg(long, env = "SOURCEGULL_REPO_DISABLE_INDEX", value_parser = clap::builder::BoolishValueParser::new())]
    pub repo_disable_index: bool,

    /// Reindex debounce window in milliseconds. After a fetch that moved refs, a repository
    /// not built within the window is reindexed at once; pushes that arrive within the window
    /// of a build coalesce into one build at its end, so the last push is always indexed.
    #[arg(
        long,
        env = "SOURCEGULL_REPO_INDEX_DEBOUNCE_MS",
        default_value_t = 5_000
    )]
    pub repo_index_debounce_ms: u64,

    /// Clones pause when free space on the data volume is below this percentage of it AND
    /// below `SOURCEGULL_REPO_PAUSE_CLONES_FREE_GIB` (the smaller threshold applies, so a large
    /// disk is not held to a ratio of its size). 0 turns the pause off.
    #[arg(
        long,
        env = "SOURCEGULL_REPO_PAUSE_CLONES_FREE_PERCENT",
        default_value_t = 10.0
    )]
    pub repo_pause_clones_free_percent: f64,

    /// Absolute cap, in GiB, of the clone pause threshold (see
    /// `SOURCEGULL_REPO_PAUSE_CLONES_FREE_PERCENT`).
    #[arg(
        long,
        env = "SOURCEGULL_REPO_PAUSE_CLONES_FREE_GIB",
        default_value_t = 20
    )]
    pub repo_pause_clones_free_gib: u64,

    /// Caches and index shards of inactive repositories are evicted when free space is below
    /// this percentage of the data volume AND below `SOURCEGULL_REPO_EVICT_FREE_GIB`, until free
    /// space is 20% above the clone pause threshold. 0 turns eviction off.
    #[arg(
        long,
        env = "SOURCEGULL_REPO_EVICT_FREE_PERCENT",
        default_value_t = 7.0
    )]
    pub repo_evict_free_percent: f64,

    /// Absolute cap, in GiB, of the eviction threshold (see `SOURCEGULL_REPO_EVICT_FREE_PERCENT`).
    #[arg(long, env = "SOURCEGULL_REPO_EVICT_FREE_GIB", default_value_t = 10)]
    pub repo_evict_free_gib: u64,

    /// Repositories moved to this node at once when repo nodes are added or removed
    /// (rebalancing); 0 turns moves to this node off.
    #[arg(
        long,
        env = "SOURCEGULL_REPO_REBALANCE_CONCURRENCY",
        default_value_t = 2
    )]
    pub repo_rebalance_concurrency: usize,
}

/// Parses a path that may be empty, so `SOURCEGULL_*_FILE=` (as Compose passes `${VAR:-}`) counts
/// as unset instead of failing the parse; see [`non_empty`].
/// A duration: a number with an `ms`, `s` or `m` suffix, or a plain number of seconds.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?}"))?;
    let secs = match unit.trim() {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        other => return Err(format!("unknown unit {other:?} in {s:?} (use ms, s or m)")),
    };
    std::time::Duration::try_from_secs_f64(secs).map_err(|_| format!("invalid duration {s:?}"))
}

fn optional_path(s: &str) -> Result<PathBuf, std::convert::Infallible> {
    Ok(PathBuf::from(s))
}

/// `path` unless it is empty (a variable set to an empty string counts as unset).
pub fn non_empty(path: &Option<PathBuf>) -> Option<PathBuf> {
    path.clone().filter(|p| !p.as_os_str().is_empty())
}

impl ServerArgs {
    /// The blob store URL (an empty `SOURCEGULL_BLOB_STORE` means the default).
    pub fn blob_store_url(&self) -> String {
        self.blob_store
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("file://{}", self.data_dir.join("blobs").display()))
    }

    /// Options of the main pool for `roles`; its `application_name` is `sourcegull:<roles>`
    /// (without `worker` when the worker has its own pool, see [`Self::worker_db_options`]).
    pub fn db_options(&self, roles: &RoleSet) -> DbOptions {
        let separate_worker = self.worker_db_options(roles).is_some();
        let names: Vec<&str> = roles
            .iter()
            .filter(|r| !(separate_worker && *r == Role::Worker))
            .map(Role::as_str)
            .collect();
        DbOptions {
            max_connections: self.db_pool.db_max_connections,
            application_name: format!("sourcegull:{}", names.join(",")),
            ..DbOptions::default()
        }
    }

    /// Options of the worker role's own pool (`application_name` `sourcegull:worker`), when it
    /// runs alongside other roles; `None` when the process runs only `worker` (it then uses the
    /// main pool) or no worker.
    pub fn worker_db_options(&self, roles: &RoleSet) -> Option<DbOptions> {
        (roles.contains(Role::Worker) && roles.iter().any(|r| r != Role::Worker)).then(|| {
            DbOptions {
                max_connections: self.db_pool.db_worker_max_connections,
                application_name: "sourcegull:worker".to_owned(),
                ..DbOptions::default()
            }
        })
    }

    /// Where the encryption keys come from: `SOURCEGULL_KEYS`, `SOURCEGULL_KEYS_FILE` or the default
    /// key file `$SOURCEGULL_DATA_DIR/keys`.
    pub fn key_source(&self) -> anyhow::Result<KeySource> {
        KeySource::resolve(
            crate::startup::inline_keys(),
            self.keys_file.as_deref(),
            self.data_dir.join("keys"),
        )
    }
}

/// `sourcegull migrate`.
#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Postgres URL of the main database.
    #[arg(long, env = "SOURCEGULL_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// The migration action.
    #[command(subcommand)]
    pub action: MigrateAction,
}

/// Migration actions.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum MigrateAction {
    /// Apply every pending migration.
    Up,
    /// Revert migrations newer than `--to` (0 reverts everything).
    Down {
        /// Target version: migrations with a greater version are reverted.
        #[arg(long)]
        to: i64,
    },
    /// List migrations and whether each is applied.
    Status,
    /// Check applied migrations against this binary (exit 1 on problems).
    Validate,
    /// Compare the live schema with a version's expected schema; prints each difference with
    /// SQL to fix it (exit 1 on drift).
    Drift {
        /// Version to compare with (`0.1.0`). Default: the version the database is at.
        #[arg(long)]
        version: Option<String>,
    },
    /// Upgrade across versions: check that the database is at `--from` and has no drift, then
    /// apply each version's migrations in order and check the result. Stop Sourcegull first.
    Upgrade {
        /// The version the database is at (`0.1.0`, or a migration number; `0`: empty).
        #[arg(long)]
        from: String,
        /// The version to upgrade to. Default: this binary's.
        #[arg(long)]
        to: Option<String>,
        /// Print the plan and run the checks without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not compare the schema with `--from`'s before upgrading.
        #[arg(long)]
        skip_drift_check: bool,
    },
    /// List the versions this binary can upgrade between and where the database is.
    Versions,
    /// Print the live schema as JSON (the format of `crates/db/schema/*.json`).
    Describe,
}

/// `sourcegull monitoring`.
#[derive(Debug, Args)]
pub struct MonitoringArgs {
    /// The action.
    #[command(subcommand)]
    pub action: MonitoringAction,
}

/// Monitoring actions.
#[derive(Debug, Clone, Subcommand)]
pub enum MonitoringAction {
    /// Write `grafana/sourcegull-*.json`, `prometheus/sourcegull-alerts.yml`, `alerts.md` and
    /// `dashboards.md` into `--out`, or the checked-in copies of a Sourcegull source tree with
    /// `--repo`.
    Generate {
        /// Output directory.
        #[arg(long, required_unless_present = "repo", conflicts_with = "repo")]
        out: Option<PathBuf>,
        /// Root of a Sourcegull source checkout: update the files under `deploy/` and
        /// `docs/observability/`.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

/// `sourcegull keys`.
#[derive(Debug, Args)]
pub struct KeysArgs {
    /// Postgres URL of the main database.
    #[arg(long, env = "SOURCEGULL_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// Encryption key file (`id:base64key` per line; the last line is the current key).
    /// Required unless the keys are given in `SOURCEGULL_KEYS`.
    #[arg(long, env = "SOURCEGULL_KEYS_FILE", value_parser = optional_path)]
    pub keys_file: Option<PathBuf>,

    /// The action.
    #[command(subcommand)]
    pub action: KeysAction,
}

impl KeysArgs {
    /// Where the keys come from; unlike the server, there is no default key file.
    pub fn key_source(&self) -> anyhow::Result<KeySource> {
        let inline = crate::startup::inline_keys();
        if inline.is_none() && non_empty(&self.keys_file).is_none() {
            anyhow::bail!(
                "no encryption keys: set SOURCEGULL_KEYS_FILE (--keys-file) or SOURCEGULL_KEYS"
            );
        }
        KeySource::resolve(inline, self.keys_file.as_deref(), PathBuf::new())
    }
}

/// Key actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum KeysAction {
    /// List the data keys and how many secrets are still on retired keys.
    Status,
    /// Give every record class a new data key (wrapped with the current key-encryption key)
    /// and retire the old ones; secrets are then re-encrypted in the background by the
    /// workers. Appending a new key to the key file and restarting rotates the same way.
    Rotate,
}

/// `sourcegull config`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The action.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// Configuration actions.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConfigAction {
    /// Validate the site config, resources and global settings files without a database.
    /// Secret references are resolved from this environment and reported when missing, but
    /// their values are never printed. Exits with 1 when a file has problems.
    Check {
        /// Site config file.
        #[arg(long, env = "SOURCEGULL_SITE_CONFIG_FILE", value_parser = optional_path)]
        site_config: Option<PathBuf>,
        /// Resources file.
        #[arg(long, env = "SOURCEGULL_RESOURCES_FILE", value_parser = optional_path)]
        resources: Option<PathBuf>,
        /// Global settings file.
        #[arg(long, env = "SOURCEGULL_GLOBAL_SETTINGS_FILE", value_parser = optional_path)]
        global_settings: Option<PathBuf>,
    },
}

/// `sourcegull supervise`.
#[derive(Debug, Args)]
pub struct SuperviseArgs {
    /// PostgreSQL URL (required).
    #[arg(long, env = "SOURCEGULL_DATABASE_URL", hide_env_values = true)]
    pub database_url: Option<String>,

    #[command(flatten)]
    pub server: ServerArgs,
}

/// `sourcegull executor`.
#[derive(Debug, Args)]
pub struct ExecutorArgs {
    /// URL of the web role, e.g. `http://sourcegull-web:7080` (jobs fetch from and upload to it).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_FRONTEND_URL")]
    pub url: url::Url,

    /// The shared executor token (site config `executors.accessToken`).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_TOKEN", hide_env_values = true)]
    pub token: String,

    /// Unique executor name. Default: the host name.
    #[arg(long, env = "SOURCEGULL_EXECUTOR_NAME")]
    pub name: Option<String>,

    /// Comma-separated queues to take jobs from: `codeintel` (auto-indexing) and `batches`
    /// (bulk change spec executions; Docker runtime only).
    #[arg(
        long,
        env = "SOURCEGULL_EXECUTOR_QUEUES",
        value_delimiter = ',',
        default_value = "codeintel"
    )]
    pub queues: Vec<String>,

    /// Jobs run at the same time.
    #[arg(long, env = "SOURCEGULL_EXECUTOR_MAX_JOBS", default_value_t = 1)]
    pub max_jobs: usize,

    /// Seconds between dequeue attempts while the queues are empty.
    #[arg(long, env = "SOURCEGULL_EXECUTOR_POLL_INTERVAL", default_value_t = 1.0)]
    pub poll_interval: f64,

    /// Directory for job checkouts before they are copied into a job's volume.
    #[arg(
        long,
        env = "SOURCEGULL_EXECUTOR_SCRATCH_DIR",
        default_value = "/tmp/sourcegull-executor"
    )]
    pub scratch_dir: PathBuf,

    /// The git binary used for checkouts.
    #[arg(long, env = "SOURCEGULL_EXECUTOR_GIT", default_value = "git")]
    pub git: PathBuf,

    /// Network of step containers: `bridge`, `none` (no network) or a named network (e.g. one
    /// whose only way out is an allowlisting proxy).
    #[arg(
        long,
        env = "SOURCEGULL_EXECUTOR_DOCKER_NETWORK",
        default_value = "bridge"
    )]
    pub docker_network: String,

    /// Memory limit per step container, e.g. `4g` or `512m` (default: none).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_DOCKER_MEMORY")]
    pub docker_memory: Option<String>,

    /// CPU limit per step container, e.g. `2` or `0.5` (default: none).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_DOCKER_CPUS")]
    pub docker_cpus: Option<f64>,

    /// Pull step images even when they are present (to follow moving tags).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_DOCKER_ALWAYS_PULL", value_parser = clap::builder::BoolishValueParser::new())]
    pub docker_always_pull: bool,

    /// Where jobs run: `docker` (a container per step) or `kubernetes` (a Job per job).
    #[arg(long, env = "SOURCEGULL_EXECUTOR_RUNTIME", value_enum, default_value_t = ExecutorRuntime::Docker)]
    pub runtime: ExecutorRuntime,

    #[command(flatten)]
    pub kubernetes: KubernetesArgs,
}

/// The executor's runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExecutorRuntime {
    Docker,
    Kubernetes,
}

/// Options of the executor's Kubernetes runtime (in-cluster configuration, else `KUBECONFIG`).
#[derive(Debug, Args)]
pub struct KubernetesArgs {
    /// Namespace of job pods. Default: the executor's own namespace, else `default`.
    #[arg(
        long = "kubernetes-namespace",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_NAMESPACE"
    )]
    pub namespace: Option<String>,

    /// Image of the checkout and upload containers (needs sh, git, cat and sleep).
    #[arg(
        long = "kubernetes-helper-image",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_HELPER_IMAGE",
        default_value = concat!("sourcegull/sourcegull:", env!("CARGO_PKG_VERSION"))
    )]
    pub helper_image: String,

    /// The web role's URL as job pods reach it, when it differs from `--url`.
    #[arg(
        long = "kubernetes-pod-frontend-url",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_POD_FRONTEND_URL"
    )]
    pub pod_frontend_url: Option<url::Url>,

    /// `runtimeClassName` of job pods (e.g. gVisor or Kata).
    #[arg(
        long = "kubernetes-runtime-class",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_RUNTIME_CLASS"
    )]
    pub runtime_class: Option<String>,

    /// `nodeSelector` of job pods, as comma-separated `key=value` pairs.
    #[arg(
        long = "kubernetes-node-selector",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_NODE_SELECTOR",
        value_delimiter = ','
    )]
    pub node_selector: Vec<String>,

    /// Tolerations of job pods, as a JSON array of Kubernetes tolerations.
    #[arg(
        long = "kubernetes-tolerations",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_TOLERATIONS"
    )]
    pub tolerations: Option<String>,

    /// Service account of job pods (its token is not mounted).
    #[arg(
        long = "kubernetes-service-account",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_SERVICE_ACCOUNT"
    )]
    pub service_account: Option<String>,

    /// Comma-separated image pull secrets of job pods.
    #[arg(
        long = "kubernetes-image-pull-secrets",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_IMAGE_PULL_SECRETS",
        value_delimiter = ','
    )]
    pub image_pull_secrets: Vec<String>,

    /// CPU request of each step container (a Kubernetes quantity, e.g. `500m`).
    #[arg(
        long = "kubernetes-cpu-request",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_CPU_REQUEST"
    )]
    pub cpu_request: Option<String>,

    /// Memory request of each step container (e.g. `2Gi`).
    #[arg(
        long = "kubernetes-memory-request",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_MEMORY_REQUEST"
    )]
    pub memory_request: Option<String>,

    /// CPU limit of each step container.
    #[arg(
        long = "kubernetes-cpu-limit",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_CPU_LIMIT"
    )]
    pub cpu_limit: Option<String>,

    /// Memory limit of each step container.
    #[arg(
        long = "kubernetes-memory-limit",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_MEMORY_LIMIT"
    )]
    pub memory_limit: Option<String>,

    /// Size limit of the job's workspace volume (e.g. `20Gi`).
    #[arg(
        long = "kubernetes-workspace-size",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_WORKSPACE_SIZE"
    )]
    pub workspace_size: Option<String>,

    /// Seconds a job may run before Kubernetes stops it.
    #[arg(
        long = "kubernetes-deadline",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_DEADLINE",
        default_value_t = 4 * 3600
    )]
    pub deadline: u64,

    /// Seconds a job pod may take to be scheduled and to start each container (image pulls
    /// included) before the job is handed back for a retry.
    #[arg(
        long = "kubernetes-start-timeout",
        env = "SOURCEGULL_EXECUTOR_KUBERNETES_START_TIMEOUT",
        default_value_t = 600
    )]
    pub start_timeout: u64,

    /// Keep Jobs after they end, for debugging (they are removed 10 minutes later).
    #[arg(long = "kubernetes-keep-jobs", env = "SOURCEGULL_EXECUTOR_KUBERNETES_KEEP_JOBS", value_parser = clap::builder::BoolishValueParser::new())]
    pub keep_jobs: bool,
}

/// Parses a memory size such as `512m`, `4g`, `1.5G` or `1048576` into bytes.
pub fn parse_memory(s: &str) -> Result<i64, String> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (num, mult) = match s.chars().last() {
        Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('m') => (&s[..s.len() - 1], 1 << 20),
        Some('g') => (&s[..s.len() - 1], 1 << 30),
        Some('t') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid memory size {s:?}"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("invalid memory size {s:?}"));
    }
    Ok((n * mult as f64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory("512m"), Ok(512 << 20));
        assert_eq!(parse_memory("4G"), Ok(4 << 30));
        assert_eq!(parse_memory("1.5g"), Ok(3 << 29));
        assert_eq!(parse_memory("1024"), Ok(1024));
        assert!(parse_memory("lots").is_err());
        assert!(parse_memory("0").is_err());
    }

    fn run_args(args: &[&str]) -> RunArgs {
        let mut argv = vec!["sourcegull", "run", "--database-url", "postgres://x/y"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Run(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn database_pools_per_role() {
        let all = run_args(&["--roles=all", "--db-max-connections=30"]);
        let main = all.server.db_options(&all.roles);
        assert_eq!(main.max_connections, 30);
        assert_eq!(main.application_name, "sourcegull:web,repo,blob");
        let worker = all.server.worker_db_options(&all.roles).unwrap();
        assert_eq!(worker.max_connections, 10);
        assert_eq!(worker.application_name, "sourcegull:worker");

        let worker_only = run_args(&["--roles=worker"]);
        assert!(
            worker_only
                .server
                .worker_db_options(&worker_only.roles)
                .is_none()
        );
        let main = worker_only.server.db_options(&worker_only.roles);
        assert_eq!(main.application_name, "sourcegull:worker");
        assert_eq!(main.max_connections, 20);

        let web = run_args(&["--roles=web"]);
        assert!(web.server.worker_db_options(&web.roles).is_none());
        assert!(
            Cli::try_parse_from([
                "sourcegull",
                "run",
                "--database-url=x",
                "--db-max-connections=1"
            ])
            .is_err()
        );
    }
}
