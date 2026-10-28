//! `gull code-intel upload`: flags, inference of missing values and output.
//!
//! Missing values are inferred: the file is `index.scip.gz` or `index.scip`; the repository,
//! commit and root come from GitLab CI's variables in a GitLab CI job (`GITLAB_CI=true`), else
//! from git (`origin` remote, `HEAD`, the file's directory under the work tree); the indexer's
//! name and version come from the index's metadata.

use std::path::{Component, Path, PathBuf};

use clap::{ArgAction, Args};

use super::transfer::{self, Record, TransferError, TransferOptions};
use crate::env::{Credential, Env, GitLabCi, Instance};

/// `gull code-intel upload` flags. Go-style single-dash spellings (`-file=…`) work too.
#[derive(Debug, Clone, Args)]
pub struct UploadArgs {
    /// SCIP index to upload. Defaults to index.scip.gz, else index.scip.
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// The name of the repository (e.g. gitlab.com/group/project). By default, from GitLab CI
    /// (CI_PROJECT_URL) or the origin remote.
    #[arg(long)]
    pub repo: Option<String>,
    /// The 40-character hash of the commit. By default, CI_COMMIT_SHA or the checked-out
    /// commit.
    #[arg(long)]
    pub commit: Option<String>,
    /// The path in the repository that matches the SCIP projectRoot (e.g. cmd/project1).
    /// Defaults to the directory of the index file.
    #[arg(long)]
    pub root: Option<String>,
    /// The name of the indexer that generated the index. Overrides metadata.toolInfo.name.
    #[arg(long)]
    pub indexer: Option<String>,
    /// The version of the indexer. Overrides metadata.toolInfo.version.
    #[arg(long = "indexerVersion")]
    pub indexer_version: Option<String>,
    /// Index record this upload belongs to (set by the instance's auto-indexer).
    #[arg(
        long = "associated-index-id",
        hide = true,
        allow_negative_numbers = true
    )]
    pub associated_index_id: Option<i64>,
    /// Upload endpoint path on the instance (rarely needed).
    #[arg(long = "upload-route", default_value = "/.api/scip/upload")]
    pub upload_route: String,
    /// The maximum upload size in megabytes. Larger indexes are uploaded over several
    /// requests.
    #[arg(long = "max-payload-size", default_value_t = 100)]
    pub max_payload_size: u64,
    /// The maximum number of parts uploaded at once. Defaults to all parts.
    #[arg(
        long = "max-concurrency",
        default_value_t = -1,
        allow_negative_numbers = true
    )]
    pub max_concurrency: i64,
    /// Exit 0 even when the upload fails.
    #[arg(long = "ignore-upload-failure", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub ignore_upload_failure: bool,
    /// Print no progress lines.
    #[arg(long = "no-progress", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub no_progress: bool,
    /// Print the result as one JSON object.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true",
          default_value = "false", action = ArgAction::Set)]
    pub json: bool,
    /// Open the upload's page in the browser.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true",
          default_value = "false", action = ArgAction::Set)]
    pub open: bool,
    /// 0 shows no logs; 1 shows requests and responses; 2 adds headers; 3 adds bodies.
    #[arg(long, default_value_t = 0)]
    pub trace: u8,
    /// Do not verify the instance's TLS certificate.
    #[arg(long = "insecure-skip-verify", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", default_value = "false", action = ArgAction::Set)]
    pub insecure_skip_verify: bool,
}

/// Runs git commands (the real one, or a fake in tests).
pub trait Git {
    /// The trimmed standard output of `git <args>`.
    fn run(&self, args: &[&str]) -> anyhow::Result<String>;
}

/// `git` on the `PATH`, in the current directory.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = std::process::Command::new("git").args(args).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} exited with {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

/// The effective values of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub file: PathBuf,
    pub record: Record,
    pub transfer: TransferOptionsSummary,
}

/// The transfer flags, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOptionsSummary {
    pub max_payload_size: u64,
    pub max_concurrency: usize,
}

fn inference_error(name: &str, err: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "{err}\n\n-{name} could not be inferred: run the command inside the repository's working \
         tree or pass -{name}=VALUE."
    )
}

/// The repository name `host/path` of a git remote: the SCP-like `git@host:path` or an
/// absolute URL with a host. One trailing `.git` is dropped.
pub fn parse_remote(remote: &str) -> anyhow::Result<String> {
    if let Some(rest) = remote.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':').filter(|(_, p)| !p.contains(':')) {
            let path = path.strip_suffix(".git").unwrap_or(path);
            return Ok(format!("{host}/{}", path.trim_start_matches('/')));
        }
    }
    if let Ok(url) = url::Url::parse(remote) {
        if let Some(host) = url.host_str() {
            let path = url.path();
            return Ok(format!(
                "{host}{}",
                path.strip_suffix(".git").unwrap_or(path)
            ));
        }
    }
    anyhow::bail!("cannot derive a repository name from the git remote {remote:?}")
}

/// Cleans a root: `.` and absolute paths become empty; `a/./b/../c` becomes `a/c`; leading
/// `..` components are kept (and rejected by the caller).
pub fn sanitize_root(root: &str) -> String {
    if root.starts_with('/') {
        return String::new();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in root.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// The directory of `file` relative to `base`, as a `/`-separated path.
fn relative_dir(file: &Path, base: &Path, cwd: &Path) -> anyhow::Result<String> {
    let abs = |p: &Path| {
        if p.is_absolute() {
            p.to_owned()
        } else {
            cwd.join(p)
        }
    };
    let normalize = |p: PathBuf| -> PathBuf {
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                c => out.push(c),
            }
        }
        out
    };
    let file = normalize(abs(file));
    let base = normalize(abs(base));
    let dir = file.parent().unwrap_or(Path::new(""));
    let rel = dir
        .strip_prefix(&base)
        .map_err(|_| anyhow::anyhow!("-root must be a directory inside the repository"))?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Fills in what the flags leave out and validates the result.
pub fn resolve(
    args: &UploadArgs,
    env: &dyn Env,
    git: &dyn Git,
    cwd: &Path,
) -> anyhow::Result<Resolved> {
    let file = match &args.file {
        Some(f) => {
            if !cwd.join(f).exists() {
                anyhow::bail!("file {:?} does not exist", f.display().to_string());
            }
            f.clone()
        }
        None => {
            let gz = Path::new("index.scip.gz");
            let plain = Path::new("index.scip");
            if cwd.join(gz).is_file() {
                gz.to_owned()
            } else if cwd.join(plain).is_file() {
                plain.to_owned()
            } else {
                return Err(inference_error(
                    "file",
                    "no index.scip.gz or index.scip in the current directory",
                ));
            }
        }
    };
    let ci = GitLabCi::detect(env);
    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => match ci.as_ref().and_then(|c| c.repo.clone()) {
            Some(r) => r,
            None => git
                .run(&["remote", "get-url", "origin"])
                .and_then(|u| parse_remote(&u))
                .map_err(|e| inference_error("repo", e))?,
        },
    };
    let commit = match &args.commit {
        Some(c) => c.clone(),
        None => match ci.as_ref().and_then(|c| c.commit.clone()) {
            Some(c) => c,
            None => git
                .run(&["rev-parse", "HEAD"])
                .map_err(|e| inference_error("commit", e))?,
        },
    };
    let root = match &args.root {
        Some(r) => r.clone(),
        None => {
            let base = match ci.as_ref().and_then(|c| c.project_dir.clone()) {
                Some(dir) => dir,
                None => git
                    .run(&["rev-parse", "--show-toplevel"])
                    .map(PathBuf::from)
                    .map_err(|e| inference_error("root", e))?,
            };
            relative_dir(&file, &base, cwd).map_err(|e| inference_error("root", e))?
        }
    };
    let root = sanitize_root(&root);
    if root.starts_with("..") {
        anyhow::bail!("-root must be a directory inside the repository");
    }
    let (indexer, indexer_version) = match (&args.indexer, &args.indexer_version) {
        (Some(n), Some(v)) => (n.clone(), v.clone()),
        (name, version) => {
            let info = crate::scip::read_tool_info(&cwd.join(&file));
            let pick =
                |given: &Option<String>, flag: &str, f: fn(&crate::scip::ToolInfo) -> String| {
                    match given {
                        Some(v) => Ok(v.clone()),
                        None => info.as_ref().map(f).map_err(|e| inference_error(flag, e)),
                    }
                };
            (
                pick(name, "indexer", |i| i.name.clone())?,
                pick(version, "indexerVersion", |i| i.version.clone())?,
            )
        }
    };
    if args.max_payload_size < 25 {
        anyhow::bail!("-max-payload-size is in megabytes and must be 25 or more");
    }
    Ok(Resolved {
        file,
        record: Record {
            repo,
            commit,
            root,
            indexer,
            indexer_version,
            associated_index_id: args.associated_index_id.filter(|id| *id >= 0),
        },
        transfer: TransferOptionsSummary {
            max_payload_size: args.max_payload_size * 1000 * 1000,
            max_concurrency: usize::try_from(args.max_concurrency).unwrap_or(0),
        },
    })
}

/// The page of the repository's precise indexes.
pub fn upload_page(instance: &Instance, repo: &str) -> String {
    instance.url(&format!("/{repo}/-/code-graph"))
}

/// Hints for an authentication failure or a refused upload size.
fn auth_hint(instance: &Instance, err: &TransferError) -> Option<String> {
    let TransferError::Status { status, .. } = err else {
        return None;
    };
    let hint = match (status.as_u16(), &instance.credential) {
        (401, Credential::None) => {
            "No access token was provided. Set SOURCEGULL_ACCESS_TOKEN (or SRC_ACCESS_TOKEN) to a \
             valid token; in GitLab CI the job's CI_JOB_TOKEN is used when the instance enables \
             codeIntel.gitlabJobTokenUploads."
        }
        (401, Credential::AccessToken(_)) => {
            "Check that the access token is still valid and that SOURCEGULL_ENDPOINT is the right \
             instance."
        }
        (401, Credential::GitLabJobToken(_)) => {
            "The instance does not accept GitLab CI job tokens (site config \
             codeIntel.gitlabJobTokenUploads), or set SOURCEGULL_ACCESS_TOKEN instead."
        }
        (404, Credential::GitLabJobToken(_)) => {
            "The repository is unknown to the instance, or the job's project is not the \
             repository's GitLab project (check -repo)."
        }
        (403, _) => "Your account is not allowed to upload indexes for this repository.",
        (413, _) => {
            "The index is larger than the instance accepts. A site admin can raise the limit \
             (site config codeIntel.maxUploadBytes); a proxy in front of the instance may have \
             its own request size limit, which applies to each request (see -max-payload-size)."
        }
        _ => return None,
    };
    Some(format!("upload failed: {err}\n\n{hint}"))
}

/// Runs the command; `Ok` also when the upload failed with `-ignore-upload-failure`.
pub async fn run(args: &UploadArgs, env: &dyn Env, git: &dyn Git) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let instance = Instance::from_env(env)?;
    let resolved = resolve(args, env, git, &cwd)?;
    let r = &resolved.record;
    if !args.json {
        println!("Upload values:");
        println!("  repo: {}", r.repo);
        println!("  commit: {}", r.commit);
        println!("  root: {}", r.root);
        println!("  file: {}", resolved.file.display());
        println!("  indexer: {}", r.indexer);
        println!("  indexerVersion: {}", r.indexer_version);
    }
    let opts = TransferOptions {
        route: args.upload_route.clone(),
        max_payload_size: resolved.transfer.max_payload_size,
        max_concurrency: resolved.transfer.max_concurrency,
        insecure_skip_verify: args.insecure_skip_verify,
        trace: args.trace,
        ..TransferOptions::default()
    };
    let quiet = args.json || args.no_progress || args.trace > 0;
    let report = move |line: &str| {
        if !quiet {
            eprintln!("{line}");
        }
    };
    let id = match transfer::upload(&instance, r, &cwd.join(&resolved.file), &opts, &report).await {
        Ok(id) => id,
        Err(e) => {
            let message = auth_hint(&instance, &e).unwrap_or_else(|| format!("upload failed: {e}"));
            if args.ignore_upload_failure {
                println!("{message}");
                return Ok(());
            }
            anyhow::bail!(message);
        }
    };
    let page = upload_page(&instance, &r.repo);
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "repo": r.repo,
                "commit": r.commit,
                "root": r.root,
                "file": resolved.file.display().to_string(),
                "indexer": r.indexer,
                "indexerVersion": r.indexer_version,
                "uploadId": id,
                "uploadUrl": page,
            })
        );
    } else {
        println!("Processing status: {page}");
    }
    if args.open {
        open_browser(&page)?;
    }
    Ok(())
}

fn open_browser(url: &str) -> anyhow::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(url)
        .spawn()
        .map_err(|e| anyhow::anyhow!("opening {url} with {program}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clap::Parser;

    use super::*;
    use crate::Cli;

    fn parse(args: &[&str]) -> UploadArgs {
        let mut argv = vec!["gull", "code-intel", "upload"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(crate::args::normalize(argv)).unwrap();
        match cli.command {
            crate::Command::CodeIntel {
                command: crate::CodeIntelCommand::Upload(a),
            } => a,
            other => panic!("{other:?}"),
        }
    }

    struct FakeGit(HashMap<&'static str, &'static str>);

    impl Git for FakeGit {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            self.0
                .get(args.join(" ").as_str())
                .map(|s| (*s).to_owned())
                .ok_or_else(|| anyhow::anyhow!("not a git repository"))
        }
    }

    fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn flags_parse_like_src_cli() {
        let a = parse(&[]);
        assert_eq!(a.file, None);
        assert_eq!(a.upload_route, "/.api/scip/upload");
        assert_eq!((a.max_payload_size, a.max_concurrency), (100, -1));
        assert!(!a.json && !a.no_progress && !a.open && !a.ignore_upload_failure);

        let a = parse(&[
            "-file=sub/index.scip",
            "-repo",
            "gitlab.com/a/b",
            "-commit=0123456789abcdef0123456789abcdef01234567",
            "-root=sub",
            "-indexer=scip-go",
            "-indexerVersion=v1",
            "-max-payload-size=30",
            "-max-concurrency",
            "2",
            "-no-progress",
            "--json",
            "-open=false",
            "-ignore-upload-failure=true",
            "-trace=2",
            "-insecure-skip-verify",
        ]);
        assert_eq!(a.file.as_deref(), Some(Path::new("sub/index.scip")));
        assert_eq!(a.repo.as_deref(), Some("gitlab.com/a/b"));
        assert_eq!(a.root.as_deref(), Some("sub"));
        assert_eq!(a.indexer.as_deref(), Some("scip-go"));
        assert_eq!(a.indexer_version.as_deref(), Some("v1"));
        assert_eq!((a.max_payload_size, a.max_concurrency), (30, 2));
        assert!(a.no_progress && a.json && !a.open && a.ignore_upload_failure);
        assert!(a.insecure_skip_verify);
        assert_eq!(a.trace, 2);
    }

    #[test]
    fn remotes_and_roots() {
        assert_eq!(
            parse_remote("git@gitlab.com:acme/demo.git").unwrap(),
            "gitlab.com/acme/demo"
        );
        assert_eq!(
            parse_remote("https://gitlab.example.com/acme/sub/demo.git").unwrap(),
            "gitlab.example.com/acme/sub/demo"
        );
        assert!(parse_remote("not a url").is_err());
        for (remote, name) in [
            (
                "git@gitlab.internal:platform/tools/cli.git",
                "gitlab.internal/platform/tools/cli",
            ),
            (
                "git@code.example.org:/team/app",
                "code.example.org/team/app",
            ),
            (
                "ssh://git@code.example.org:2222/team/app.git",
                "code.example.org/team/app",
            ),
            (
                "https://ci-bot:secret@gitlab.example.net/grp/proj.git",
                "gitlab.example.net/grp/proj",
            ),
            (
                "https://GitLab.Example.NET/Grp/Proj",
                "gitlab.example.net/Grp/Proj",
            ),
        ] {
            assert_eq!(parse_remote(remote).unwrap(), name, "{remote}");
        }
        for remote in [
            "git@host:a:b",
            "file:///srv/repos/x.git",
            "../relative/path",
        ] {
            let err = parse_remote(remote).unwrap_err().to_string();
            assert_eq!(
                err,
                format!("cannot derive a repository name from the git remote {remote:?}")
            );
        }
        assert_eq!(sanitize_root("."), "");
        assert_eq!(sanitize_root("/abs"), "");
        assert_eq!(sanitize_root("a/./b/../c/"), "a/c");
        assert_eq!(sanitize_root("../x"), "../x");
    }

    #[test]
    fn inference_in_gitlab_ci_and_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir(cwd.join("sub")).unwrap();
        std::fs::write(
            cwd.join("sub/index.scip"),
            crate::scip::tests::index("scip-typescript", "0.3.14"),
        )
        .unwrap();
        let no_git = FakeGit(HashMap::new());

        // No file given and none in the directory.
        let err = resolve(&parse(&[]), &env(&[]), &no_git, cwd).unwrap_err();
        assert!(
            err.to_string().contains("no index.scip.gz or index.scip"),
            "{err}"
        );

        // GitLab CI: repository, commit and root from the job's variables.
        let ci = env(&[
            ("GITLAB_CI", "true"),
            ("CI_PROJECT_URL", "https://gitlab.example.com/acme/demo"),
            ("CI_COMMIT_SHA", "0123456789abcdef0123456789abcdef01234567"),
            ("CI_PROJECT_DIR", cwd.to_str().unwrap()),
        ]);
        let r = resolve(&parse(&["-file=sub/index.scip"]), &ci, &no_git, cwd).unwrap();
        assert_eq!(
            r.record,
            Record {
                repo: "gitlab.example.com/acme/demo".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                root: "sub".into(),
                indexer: "scip-typescript".into(),
                indexer_version: "0.3.14".into(),
                associated_index_id: None,
            }
        );
        assert_eq!(r.transfer.max_payload_size, 100_000_000);
        assert_eq!(r.transfer.max_concurrency, 0);

        // Flags win over everything.
        let r = resolve(
            &parse(&[
                "-file=sub/index.scip",
                "-repo=x/y",
                "-commit=abc",
                "-root=./",
                "-indexer=mine",
            ]),
            &ci,
            &no_git,
            cwd,
        )
        .unwrap();
        assert_eq!(
            (
                r.record.repo.as_str(),
                r.record.commit.as_str(),
                r.record.root.as_str()
            ),
            ("x/y", "abc", "")
        );
        assert_eq!(
            (r.record.indexer.as_str(), r.record.indexer_version.as_str()),
            ("mine", "0.3.14")
        );

        // Outside CI: git.
        let toplevel = cwd.to_str().unwrap().to_owned();
        let git = FakeGit(HashMap::from([
            ("remote get-url origin", "git@gitlab.com:acme/demo.git"),
            ("rev-parse HEAD", "89abcdef0123456789abcdef0123456789abcdef"),
            (
                "rev-parse --show-toplevel",
                Box::leak(toplevel.into_boxed_str()) as &str,
            ),
        ]));
        let r = resolve(&parse(&["-file", "sub/index.scip"]), &env(&[]), &git, cwd).unwrap();
        assert_eq!(r.record.repo, "gitlab.com/acme/demo");
        assert_eq!(r.record.commit, "89abcdef0123456789abcdef0123456789abcdef");
        assert_eq!(r.record.root, "sub");
        let err = resolve(&parse(&["-file=sub/index.scip"]), &env(&[]), &no_git, cwd)
            .unwrap_err()
            .to_string();
        assert!(err.contains("-repo could not be inferred"), "{err}");

        // Validation.
        let err = resolve(&parse(&["-file=nope.scip"]), &ci, &no_git, cwd).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        let err = resolve(
            &parse(&["-file=sub/index.scip", "-root=../x"]),
            &ci,
            &no_git,
            cwd,
        )
        .unwrap_err();
        assert!(err.to_string().contains("inside the repository"), "{err}");
        let err = resolve(
            &parse(&["-file=sub/index.scip", "-max-payload-size=10"]),
            &ci,
            &no_git,
            cwd,
        )
        .unwrap_err();
        assert!(err.to_string().contains("25 or more"), "{err}");
        // The default file, gzip preferred.
        std::fs::copy(cwd.join("sub/index.scip"), cwd.join("index.scip")).unwrap();
        let r = resolve(&parse(&[]), &ci, &no_git, cwd).unwrap();
        assert_eq!(
            (r.file.to_str(), r.record.root.as_str()),
            (Some("index.scip"), "")
        );
    }
}
