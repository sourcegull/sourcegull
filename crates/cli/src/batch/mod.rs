//! `gull bulk new|validate|preview|apply`: bulk changes run on the local machine. Flags take
//! the single-dash form `-flag=value` as well.
//!
//! `preview` and `apply` ([`run::execute`]):
//!
//! 1. Read the spec (`-f FILE`, a positional file, or `-f -` for standard input), parse and
//!    validate it (`sourcegull_batches::parse_batch_spec`, then the template dry run), and check
//!    that mounted paths exist next to it.
//! 2. Resolve the namespace (`-namespace`/`-n`, else the current user) and the workspaces on
//!    the instance (`resolveWorkspacesForBatchSpec`). Workspaces in repositories on
//!    unsupported code hosts or with a `.batchignore` are left out unless `-allow-unsupported`
//!    / `-allow-ignored`.
//! 3. Run every workspace's steps with `sourcegull-batches-steps` on Docker, `-j` at a time: the
//!    repository is downloaded as an archive at the resolved commit ([`workspace`]), steps
//!    reuse cached results from the local cache directory ([`cache`], `-clear-cache` runs
//!    everything again), each workspace has `-timeout`, and its log goes to a file in the temp
//!    directory that is kept on failure or with `-keep-logs`. Failed workspaces fail the run
//!    unless `-skip-errors` (then their changes are left out); `-fail-fast` stops at the
//!    first failure.
//! 4. Build the changeset specs, plus one per `importChangesets` ID, check that no two
//!    target the same branch of a repository, upload them (`createChangesetSpecs`) and create
//!    the bulk change spec (`createBatchSpec`). `preview` prints the preview URL; `apply` applies
//!    the spec (`applyBatchChange`) and prints the bulk change's URL.
//!
//! Limitations: there is no `bulk remote`, `repos` or interactive UI; repository archives are
//! not cached between runs, and the working copy is not chowned to the image's user (non-root
//! images may be unable to write in `volume` mode).

pub mod cache;
pub mod client;
pub mod run;
pub mod workspace;

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{ArgAction, Args, Subcommand};

/// `gull bulk …`.
#[derive(Debug, Subcommand)]
pub enum BatchCommand {
    /// Write a starter bulk change spec.
    New(NewArgs),
    /// Validate a bulk change spec.
    Validate(ValidateArgs),
    /// Run a bulk change spec's steps locally and upload the result for preview.
    #[command(after_help = PREVIEW_EXAMPLES)]
    Preview(ExecuteArgs),
    /// Run a bulk change spec's steps locally, upload the result and apply it.
    #[command(after_help = APPLY_EXAMPLES)]
    Apply(ExecuteArgs),
}

const PREVIEW_EXAMPLES: &str = "\
Examples:
  Set SOURCEGULL_ENDPOINT and SOURCEGULL_ACCESS_TOKEN (or SRC_ENDPOINT and SRC_ACCESS_TOKEN) first.

    $ gull bulk preview -f batch.yaml
    $ gull bulk preview -f batch.yaml -namespace my-org -j 8";

const APPLY_EXAMPLES: &str = "\
Examples:
    $ gull bulk apply -f batch.yaml
    $ gull bulk apply -f batch.yaml -clear-cache -skip-errors";

/// `gull bulk new`.
#[derive(Debug, Clone, Args)]
pub struct NewArgs {
    /// The file to write.
    #[arg(short = 'f', default_value = "batch.yaml")]
    pub file: PathBuf,
    /// Accepted for compatibility and ignored.
    #[arg(long = "skip-errors", hide = true, num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub skip_errors: bool,
}

/// `gull bulk validate`.
#[derive(Debug, Clone, Args)]
pub struct ValidateArgs {
    /// The bulk change spec file to read, or - for standard input.
    #[arg(short = 'f')]
    pub file: Option<String>,
    /// The bulk change spec file (instead of -f).
    #[arg(value_name = "FILE")]
    pub positional: Option<String>,
    /// Accepted for compatibility and ignored.
    #[arg(long = "allow-unsupported", hide = true, num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub allow_unsupported: bool,
    /// Accepted for compatibility and ignored.
    #[arg(long = "allow-ignored", hide = true, num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub allow_ignored: bool,
}

/// How the working copy gets into step containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkspaceKind {
    /// `bind` (bind mounts work everywhere).
    Auto,
    /// Bind-mount the working copy.
    Bind,
    /// Copy the working copy into each step's container and back.
    Volume,
}

/// `gull bulk preview|apply` flags.
#[derive(Debug, Clone, Args)]
pub struct ExecuteArgs {
    /// The bulk change spec file to read, or - for standard input.
    #[arg(short = 'f')]
    pub file: Option<String>,
    /// The bulk change spec file (instead of -f).
    #[arg(value_name = "FILE")]
    pub positional: Option<String>,
    /// The user or organization namespace to place the bulk change within. Defaults to the
    /// current user.
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,
    /// Include workspaces on code hosts that bulk changes do not support.
    #[arg(long = "allow-unsupported", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub allow_unsupported: bool,
    /// Allow repositories with a .batchignore file.
    #[arg(long = "allow-ignored", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub allow_ignored: bool,
    /// Ignore cached step results and run every step again (results are still cached).
    #[arg(long = "clear-cache", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub clear_cache: bool,
    /// Upload the changes of the workspaces that succeeded when others fail.
    #[arg(long = "skip-errors", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub skip_errors: bool,
    /// Stop at the first failing workspace.
    #[arg(long = "fail-fast", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub fail_fast: bool,
    /// Keep the log files of successful workspaces too.
    #[arg(long = "keep-logs", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub keep_logs: bool,
    /// How many workspaces run at once. Defaults to the number of CPUs.
    #[arg(short = 'j')]
    pub parallelism: Option<usize>,
    /// How long one workspace may take (Go duration, e.g. 60m, 1h30m, 90s).
    #[arg(long, default_value = "60m", value_parser = parse_go_duration)]
    pub timeout: Duration,
    /// The step result cache directory. Defaults to ~/.cache/sourcegull/batch.
    #[arg(long)]
    pub cache: Option<PathBuf>,
    /// The directory for working copies and logs. Defaults to the system temp directory.
    #[arg(long)]
    pub tmp: Option<PathBuf>,
    /// How the working copy gets into step containers.
    #[arg(long, value_enum, default_value = "auto")]
    pub workspace: WorkspaceKind,
    /// Accepted for compatibility and ignored; output is always plain text.
    #[arg(long = "text-only", hide = true, num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub text_only: bool,
    /// Do not verify the instance's TLS certificate.
    #[arg(long = "insecure-skip-verify", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub insecure_skip_verify: bool,
}

impl ExecuteArgs {
    /// `-f`, else the positional file.
    pub fn spec_file(&self) -> anyhow::Result<&str> {
        spec_file(self.file.as_deref(), self.positional.as_deref())
    }
}

fn spec_file<'a>(flag: Option<&'a str>, positional: Option<&'a str>) -> anyhow::Result<&'a str> {
    match (flag, positional) {
        (Some(f), None) | (None, Some(f)) => Ok(f),
        (Some(_), Some(_)) => {
            anyhow::bail!("give the bulk change spec either with -f or as an argument")
        }
        (None, None) => {
            anyhow::bail!("no bulk change spec given: use -f FILE (or -f - to read standard input)")
        }
    }
}

/// Parses a Go duration (`1h30m`, `60m`, `90s`, `500ms`, `1.5h`).
pub fn parse_go_duration(s: &str) -> Result<Duration, String> {
    let err = || format!("invalid duration {s:?}");
    let mut rest = s.trim();
    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    if rest.is_empty() {
        return Err(err());
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let n = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(err)?;
        let value: f64 = rest[..n].parse().map_err(|_| err())?;
        rest = &rest[n..];
        let u = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = match &rest[..u] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(err()),
        };
        total += value * unit;
        rest = &rest[u..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// The starter spec of `gull bulk new`.
pub const NEW_SPEC_TEMPLATE: &str = r#"# A bulk change spec describes one bulk change: where it runs, what it does in each repository,
# and the merge requests it opens. Check it with `gull bulk validate`, run it with
# `gull bulk preview` and publish it with `gull bulk apply`.

# The bulk change's name: letters, digits, '-', '_' and '.' only.
name: add-editorconfig

# A sentence or two shown on the bulk change's page.
description: Add a shared .editorconfig to repositories that do not have one yet.

# The repositories to run in, chosen by a search query.
on:
  - repositoriesMatchingQuery: type:repo repo:^gitlab\.example\.com/my-group/

# The commands run in each repository's working copy, one container per step. Repositories
# where the steps change nothing get no merge request.
steps:
  - run: |
      [ -f .editorconfig ] || printf 'root = true\n\n[*]\nend_of_line = lf\ninsert_final_newline = true\ncharset = utf-8\n' > .editorconfig
    container: alpine:3.20

# The merge request opened in each repository that the steps changed.
changesetTemplate:
  title: Add an .editorconfig
  body: |
    This merge request adds an `.editorconfig` so editors agree on line endings,
    the final newline and the character set.
  # The branch the commit is pushed to.
  branch: bulk/add-editorconfig
  commit:
    message: Add an .editorconfig
    author:
      name: AUTHOR-NAME
      email: AUTHOR-EMAIL
"#;

/// `gull bulk new`: writes [`NEW_SPEC_TEMPLATE`] with the author from git's configuration;
/// refuses to overwrite a file.
pub fn new_spec(args: &NewArgs, git: &dyn crate::codeintel::upload::Git) -> anyhow::Result<()> {
    if args.file.exists() {
        anyhow::bail!("{} already exists; not overwriting it", args.file.display());
    }
    let name = git
        .run(&["config", "user.name"])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Your Name".into());
    let email = git
        .run(&["config", "user.email"])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "you@example.com".into());
    let text = NEW_SPEC_TEMPLATE
        .replace("AUTHOR-NAME", &yaml_quote(&name))
        .replace("AUTHOR-EMAIL", &yaml_quote(&email));
    if let Some(dir) = args.file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&args.file, text)?;
    println!("Bulk change spec written to {}.", args.file.display());
    Ok(())
}

/// A YAML double-quoted scalar.
fn yaml_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A spec read from a file or standard input, with the directory its `mount` paths are
/// relative to.
#[derive(Debug, Clone)]
pub struct SpecSource {
    pub raw: String,
    pub dir: PathBuf,
}

/// Reads the spec (`-` is standard input, whose mounts are relative to the current
/// directory).
pub fn read_spec(file: &str) -> anyhow::Result<SpecSource> {
    if file == "-" {
        let mut raw = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
        return Ok(SpecSource {
            raw,
            dir: std::env::current_dir()?,
        });
    }
    let path = Path::new(file);
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading bulk change spec {file}: {e}"))?;
    let dir = path
        .canonicalize()?
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(SpecSource { raw, dir })
}

/// Parses and validates a spec as `preview`, `apply` and `validate` do: the format, the
/// templates, and the mounted paths (relative to `dir`, which they must not leave).
pub fn validate_spec(source: &SpecSource) -> anyhow::Result<sourcegull_batches::ParsedBatchSpec> {
    let parsed = sourcegull_batches::parse_batch_spec(&source.raw)
        .map_err(|e| anyhow::anyhow!("bulk change spec is invalid: {e}"))?;
    sourcegull_batches_template::validate_batch_spec_template(&source.raw)
        .map_err(|e| anyhow::anyhow!("bulk change spec is invalid: {e}"))?;
    let root = source
        .dir
        .canonicalize()
        .unwrap_or_else(|_| source.dir.clone());
    for (i, step) in parsed.spec.steps.iter().enumerate() {
        for m in &step.mount {
            let path = root.join(&m.path);
            let resolved = path.canonicalize().map_err(|_| {
                anyhow::anyhow!("step {}: the mounted path {} does not exist", i + 1, m.path)
            })?;
            if !resolved.starts_with(&root) {
                anyhow::bail!(
                    "step {}: the mounted path {} must be in the bulk change spec's directory or below \
                     it (same directory or a subdirectory)",
                    i + 1,
                    m.path
                );
            }
        }
    }
    Ok(parsed)
}

/// `gull bulk validate`.
pub fn validate(args: &ValidateArgs) -> anyhow::Result<()> {
    let file = spec_file(args.file.as_deref(), args.positional.as_deref())?;
    let source = read_spec(file)?;
    let parsed = validate_spec(&source)?;
    for w in &parsed.warnings {
        eprintln!("warning: {w}");
    }
    println!("The bulk change spec is valid.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{Cli, Command};

    fn parse(args: &[&str]) -> BatchCommand {
        parse_as("bulk", args)
    }

    fn parse_as(group: &str, args: &[&str]) -> BatchCommand {
        let mut argv = vec!["gull", group];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(crate::args::normalize(argv))
            .unwrap()
            .command
        {
            Command::Batch { command } => command,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn src_cli_flags() {
        let BatchCommand::Preview(a) = parse(&[
            "preview",
            "-f",
            "batch.yaml",
            "-namespace=acme",
            "-allow-unsupported",
            "-allow-ignored=true",
            "-clear-cache",
            "-skip-errors",
            "-keep-logs",
            "-j",
            "3",
            "-timeout",
            "1h30m",
        ]) else {
            panic!()
        };
        assert_eq!(a.spec_file().unwrap(), "batch.yaml");
        assert_eq!(a.namespace.as_deref(), Some("acme"));
        assert!(a.allow_unsupported && a.allow_ignored && a.clear_cache);
        assert!(a.skip_errors && a.keep_logs && !a.fail_fast);
        assert_eq!(a.parallelism, Some(3));
        assert_eq!(a.timeout, Duration::from_secs(5400));
        assert_eq!(a.workspace, WorkspaceKind::Auto);

        let BatchCommand::Apply(a) = parse(&["apply", "-n", "me", "spec.yml", "-workspace=volume"])
        else {
            panic!()
        };
        assert_eq!(a.spec_file().unwrap(), "spec.yml");
        assert_eq!(a.namespace.as_deref(), Some("me"));
        assert_eq!(a.timeout, Duration::from_secs(3600));
        assert_eq!(a.workspace, WorkspaceKind::Volume);
        assert!(!a.allow_unsupported && !a.clear_cache);

        let BatchCommand::Apply(a) = parse(&["apply"]) else {
            panic!()
        };
        assert!(a.spec_file().is_err());
        let BatchCommand::New(n) = parse(&["new"]) else {
            panic!()
        };
        assert_eq!(n.file, Path::new("batch.yaml"));
        let BatchCommand::Validate(v) = parse(&["validate", "-f", "-"]) else {
            panic!()
        };
        assert_eq!(v.file.as_deref(), Some("-"));
    }

    #[test]
    fn batch_is_a_hidden_alias_of_bulk() {
        let BatchCommand::Validate(v) = parse_as("batch", &["validate", "-f", "x.yaml"]) else {
            panic!()
        };
        assert_eq!(v.file.as_deref(), Some("x.yaml"));
        let help = <Cli as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        assert!(help.contains("bulk"), "{help}");
        assert!(!help.contains("batch"), "{help}");
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("60m"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_go_duration("1h30m10s"), Ok(Duration::from_secs(5410)));
        assert_eq!(parse_go_duration("1.5h"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("0"), Ok(Duration::ZERO));
        assert!(parse_go_duration("60").is_err());
        assert!(parse_go_duration("1d").is_err());
        assert!(parse_go_duration("").is_err());
    }

    struct FakeGit;
    impl crate::codeintel::upload::Git for FakeGit {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            Ok(match args {
                ["config", "user.name"] => "Ada \"L\" Lovelace".into(),
                ["config", "user.email"] => "ada@example.com".into(),
                _ => anyhow::bail!("unexpected"),
            })
        }
    }

    #[test]
    fn new_writes_a_valid_spec_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/batch.yaml");
        let args = NewArgs {
            file: file.clone(),
            skip_errors: false,
        };
        new_spec(&args, &FakeGit).unwrap();
        let source = read_spec(file.to_str().unwrap()).unwrap();
        let parsed = validate_spec(&source).unwrap();
        let author = parsed
            .spec
            .changeset_template
            .unwrap()
            .commit
            .author
            .unwrap();
        assert_eq!(author.name, "Ada \"L\" Lovelace");
        assert_eq!(author.email, "ada@example.com");
        assert!(
            new_spec(&args, &FakeGit)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
    }

    #[test]
    fn validation_checks_templates_and_mounts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fix.sh"), "echo hi\n").unwrap();
        let source = |raw: &str| SpecSource {
            raw: raw.to_owned(),
            dir: dir.path().to_owned(),
        };
        let spec = |extra: &str| {
            format!(
                "name: x\non:\n  - repository: gitlab.example.com/a/b\nsteps:\n  - run: sh /fix.sh\n    container: alpine:3\n{extra}changesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n"
            )
        };
        validate_spec(&source(&spec(
            "    mount:\n      - path: ./fix.sh\n        mountpoint: /fix.sh\n",
        )))
        .unwrap();
        let e = validate_spec(&source(&spec(
            "    mount:\n      - path: ./missing.sh\n        mountpoint: /m\n",
        )))
        .unwrap_err();
        assert!(e.to_string().contains("does not exist"), "{e}");
        let e = validate_spec(&source(&spec(
            "    mount:\n      - path: ../\n        mountpoint: /m\n",
        )))
        .unwrap_err();
        assert!(e.to_string().contains("same directory"), "{e}");
        let e = validate_spec(&source("name: x\nsteps: 3\n")).unwrap_err();
        assert!(
            e.to_string().starts_with("bulk change spec is invalid"),
            "{e}"
        );
        let e = validate_spec(&source(&spec("").replace("title: t", "title: ${{ nope }}")))
            .unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
    }
}
