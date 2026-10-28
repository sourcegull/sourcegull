//! `gull`, the Sourcegull command-line client.
//!
//! `gull code-intel upload` uploads SCIP indexes. Flags are accepted in single-dash form
//! (`-flag=value`, see [`args`]), and the instance comes from `SOURCEGULL_ENDPOINT` /
//! `SOURCEGULL_ACCESS_TOKEN` or the alternative names `SRC_ENDPOINT` / `SRC_ACCESS_TOKEN`. In
//! GitLab CI it reads the job's `CI_*` variables and can authenticate with `CI_JOB_TOKEN`.
//! `gull bulk new|validate|preview|apply` ([`batch`]; `gull batch` is a hidden alias) runs
//! bulk change spec steps on the local Docker. Each command group is a variant of [`Command`]
//! with a module.

pub mod args;
pub mod batch;
pub mod codeintel;
pub mod env;
pub mod scip;

use clap::{Parser, Subcommand};

/// The command line.
#[derive(Debug, Parser)]
#[command(name = "gull", version, about = "The Sourcegull command-line client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level command groups.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Code intelligence: upload SCIP indexes.
    #[command(name = "code-intel")]
    CodeIntel {
        #[command(subcommand)]
        command: CodeIntelCommand,
    },
    /// Bulk changes: run bulk change specs locally and upload the result.
    #[command(name = "bulk", alias = "batch")]
    Batch {
        #[command(subcommand)]
        command: batch::BatchCommand,
    },
}

/// `gull code-intel …`.
#[derive(Debug, Subcommand)]
pub enum CodeIntelCommand {
    /// Upload a SCIP index.
    #[command(after_help = UPLOAD_EXAMPLES)]
    Upload(codeintel::UploadArgs),
}

const UPLOAD_EXAMPLES: &str = "\
Examples:
  Set SOURCEGULL_ENDPOINT and SOURCEGULL_ACCESS_TOKEN (or SRC_ENDPOINT and SRC_ACCESS_TOKEN) first.
  Inside the project, the repository, commit and root are inferred from git:

    $ gull code-intel upload                 # uploads ./index.scip

  Otherwise give them explicitly:

    $ gull code-intel upload -root='' -repo=FOO -commit=BAR -file=index.scip

  In GitLab CI they come from the job (CI_PROJECT_URL, CI_COMMIT_SHA), and without an access
  token the job's CI_JOB_TOKEN authenticates the upload when the instance enables
  codeIntel.gitlabJobTokenUploads.";

/// Runs the command line `argv` (including the program name).
pub async fn run(argv: Vec<String>) -> anyhow::Result<()> {
    let cli = Cli::parse_from(args::normalize(argv));
    match cli.command {
        Command::CodeIntel {
            command: CodeIntelCommand::Upload(a),
        } => codeintel::upload(&a, &env::ProcessEnv, &codeintel::upload::SystemGit).await,
        Command::Batch { command } => match command {
            batch::BatchCommand::New(a) => batch::new_spec(&a, &codeintel::upload::SystemGit),
            batch::BatchCommand::Validate(a) => batch::validate(&a),
            batch::BatchCommand::Preview(a) => {
                let runtime = docker_runtime(&a)?;
                batch::run::execute(&a, false, &env::ProcessEnv, runtime)
                    .await
                    .map(drop)
            }
            batch::BatchCommand::Apply(a) => {
                let runtime = docker_runtime(&a)?;
                batch::run::execute(&a, true, &env::ProcessEnv, runtime)
                    .await
                    .map(drop)
            }
        },
    }
}

/// The local Docker daemon (`DOCKER_HOST`, else the default socket) as the step runtime.
fn docker_runtime(
    args: &batch::ExecuteArgs,
) -> anyhow::Result<std::sync::Arc<dyn sourcegull_batches_steps::ContainerRuntime>> {
    use sourcegull_batches_steps::docker::{DockerOptions, DockerRuntime, WorkspaceMode};
    let workspace_mode = match args.workspace {
        batch::WorkspaceKind::Auto | batch::WorkspaceKind::Bind => WorkspaceMode::Bind,
        batch::WorkspaceKind::Volume => WorkspaceMode::Copy,
    };
    let mut runtime = DockerRuntime::connect(DockerOptions {
        workspace_mode,
        ..DockerOptions::default()
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(tmp) = &args.tmp {
        runtime = runtime.with_scratch_dir(tmp);
    }
    Ok(std::sync::Arc::new(runtime))
}
