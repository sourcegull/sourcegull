//! The `sourcegull` binary's logic. `main.rs` only parses the command line and calls [`main`].
//!
//! - `sourcegull run --roles=web,repo,worker|all`: connect to Postgres (retrying for up to 60 s),
//!   refuse to start `web`/`repo`/`worker` with pending migrations, load the site config
//!   (database or `SOURCEGULL_SITE_CONFIG_FILE`) and keep it current, then serve until
//!   SIGTERM/SIGINT. The `repo` role is in `sourcegull_repo_node`; the web side's routing table to repo
//!   nodes in [`routing`].
//! - `sourcegull migrate up|down --to V|status|validate`.
//! - `sourcegull config check`: the configuration files, offline ([`config_cmd`]).
//! - `sourcegull supervise`: key file, migrations and every role in one process.
//!
//! `main.rs` reads `SOURCEGULL_KEYS` and removes it from the environment before the async runtime
//! starts ([`startup::capture_inline_keys`]).
//! - `sourcegull monitoring generate --out DIR|--repo ROOT`: dashboards, alerts and their docs.
//! - `sourcegull version`.

// The GraphQL schema with every feature ([`features::Graphql`]) is deep enough to need more
// than the default 128.
#![recursion_limit = "256"]

pub mod admin_logs;
pub mod cli;
pub mod config_cmd;
pub mod executor;
pub mod features;
pub mod keys;
pub mod metrics_collector;
pub mod migrate_cmd;
pub mod roles;
pub mod routing;
pub mod server;
pub mod site_config;
pub mod startup;
pub mod supervise;
pub mod users_cmd;
pub mod worker;

use std::process::ExitCode;

use anyhow::Context as _;
use sourcegull_observability::{LogConfig, LogFormat};
use tokio_util::sync::CancellationToken;

use crate::cli::{Cli, Command, LogFormatArg};

/// Runs the parsed command line and returns the process exit code.
pub async fn main(cli: Cli) -> ExitCode {
    let log = LogConfig {
        format: match cli.log_format {
            LogFormatArg::Json => LogFormat::Json,
            LogFormatArg::Pretty => LogFormat::Pretty,
        },
        filter: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
        service: "sourcegull",
    };
    if let Err(e) = sourcegull_observability::init(&log) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    match run(cli.command, cancel).await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "fatal error");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command, cancel: CancellationToken) -> anyhow::Result<ExitCode> {
    match command {
        Command::Version => {
            println!("{}", version_text());
            Ok(ExitCode::SUCCESS)
        }
        Command::Migrate(args) => {
            let opts = sourcegull_db::DbOptions {
                max_connections: 2,
                application_name: "sourcegull:migrate".to_owned(),
                ..Default::default()
            };
            let db = startup::connect_with_retry(
                &args.database_url,
                &opts,
                startup::DB_CONNECT_DEADLINE,
                &cancel,
            )
            .await?;
            let outcome = migrate_cmd::run(db.pool(), args.action).await?;
            println!("{}", outcome.output);
            Ok(if outcome.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Keys(args) => {
            let opts = sourcegull_db::DbOptions {
                max_connections: 2,
                application_name: "sourcegull:keys".to_owned(),
                ..Default::default()
            };
            let db = startup::connect_with_retry(
                &args.database_url,
                &opts,
                startup::DB_CONNECT_DEADLINE,
                &cancel,
            )
            .await?;
            startup::ensure_schema_current(db.pool()).await?;
            println!(
                "{}",
                keys::run_command(&db, &args.key_source()?, args.action).await?
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Users(args) => {
            let opts = sourcegull_db::DbOptions {
                max_connections: 2,
                application_name: "sourcegull:users".to_owned(),
                ..Default::default()
            };
            let db = startup::connect_with_retry(
                &args.database_url,
                &opts,
                startup::DB_CONNECT_DEADLINE,
                &cancel,
            )
            .await?;
            startup::ensure_schema_current(db.pool()).await?;
            println!("{}", users_cmd::run(&db, &args).await?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Config(args) => {
            let crate::cli::ConfigAction::Check {
                site_config,
                resources,
                global_settings,
            } = args.action;
            let outcome = config_cmd::check(
                cli::non_empty(&site_config).as_deref(),
                cli::non_empty(&resources).as_deref(),
                cli::non_empty(&global_settings).as_deref(),
            );
            println!("{}", outcome.output);
            Ok(if outcome.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Run(args) => {
            let metrics = sourcegull_observability::install_metrics()?;
            let db = startup::connect_with_retry(
                &args.database_url,
                &args.server.db_options(&args.roles),
                startup::DB_CONNECT_DEADLINE,
                &cancel,
            )
            .await?;
            server::run_roles(&args.server, &args.roles, db, metrics, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Monitoring(args) => {
            let crate::cli::MonitoringAction::Generate { out, repo } = args.action;
            let output = sourcegull_monitoring::Output::render(
                &sourcegull_monitoring::definitions::dashboards(),
            );
            let dir = if let Some(root) = repo {
                output.write_repo(&root)?;
                root
            } else {
                let out = out.context("--out or --repo is required")?;
                output.write(&out)?;
                out
            };
            println!("wrote {} files to {}", output.files.len(), dir.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Executor(args) => {
            executor::run(*args, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Supervise(args) => {
            let metrics = sourcegull_observability::install_metrics()?;
            supervise::supervise(&args, metrics, cancel)
                .await
                .context("supervise")?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `sourcegull version` output.
pub fn version_text() -> String {
    format!(
        "sourcegull {}\nschema version: {}\nweb UI: {}",
        env!("CARGO_PKG_VERSION"),
        sourcegull_db::migrate::latest_version().unwrap_or(0),
        if sourcegull_web_assets::ui_embedded() {
            "embedded"
        } else {
            "not built (placeholder page)"
        }
    )
}

/// Cancels `cancel` on the first SIGTERM or SIGINT; a second signal exits immediately.
fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "cannot install the SIGTERM handler");
                    return;
                }
            };
        let mut signals = 0;
        loop {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = tokio::signal::ctrl_c() => "SIGINT",
            };
            signals += 1;
            if signals > 1 {
                tracing::warn!(signal = name, "second signal; exiting immediately");
                std::process::exit(130);
            }
            tracing::info!(signal = name, "received signal; shutting down gracefully");
            cancel.cancel();
        }
    });
}
