//! Checking out a job's commit from `/.executors/git/{repo}` with the job token.

use std::path::Path;
use std::process::Stdio;

use sourcegull_executor_api::{Job, valid_tag_name};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::JobError;
use crate::client::Client;
use crate::logger::JobLogger;

/// Fetches `job.commit` (shallow unless the job asks otherwise) into a new repository at `dir`
/// and checks it out, logging each git command as a step.
pub async fn checkout(
    git: &Path,
    client: &Client,
    job: &Job,
    dir: &Path,
    logger: &JobLogger,
    cancel: &CancellationToken,
) -> Result<(), JobError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| JobError::Errored(format!("creating the workspace: {e}")))?;
    let url = client.git_url(&job.repository_name).to_string();
    let auth = format!("Authorization: Bearer {}", job.token);
    let mut fetch = vec!["fetch", "--progress", "--no-recurse-submodules"];
    if job.shallow_clone {
        fetch.push("--depth=1");
    }
    fetch.push(if job.fetch_tags {
        "--tags"
    } else {
        "--no-tags"
    });
    fetch.extend(["origin", job.commit.as_str()]);
    let mut steps: Vec<(&str, Vec<&str>)> = vec![
        ("setup.git.init", vec!["init", "--quiet"]),
        (
            "setup.git.add-remote",
            vec!["remote", "add", "origin", &url],
        ),
        ("setup.git.fetch", fetch),
    ];
    let sparse: Vec<&str> = job.sparse_checkout.iter().map(String::as_str).collect();
    if !sparse.is_empty() {
        let mut args = vec!["sparse-checkout", "set", "--cone", "--"];
        args.extend(sparse);
        steps.push(("setup.git.sparse-checkout", args));
    }
    steps.push((
        "setup.git.checkout",
        vec!["checkout", "--quiet", "--detach", job.commit.as_str()],
    ));
    let git_step = |key: &'static str, args: Vec<&str>| {
        let mut cmd = Command::new(git);
        cmd.args(&args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // The token goes through the environment, not the command line.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", &auth);
        let mut command = vec!["git".to_owned()];
        command.extend(args.iter().map(|a| (*a).to_owned()));
        (key, cmd, command)
    };
    for (key, args) in steps {
        let (key, cmd, command) = git_step(key, args);
        let code = run_logged(cmd, key, &command, logger, cancel).await?;
        if code != 0 {
            return Err(JobError::Errored(format!(
                "{} exited with status {code}",
                command.join(" ")
            )));
        }
    }
    // The commit's tags, so tools that read the version from a tag on HEAD see it. A tag
    // that cannot be created is logged and skipped.
    for tag in &job.commit_tags {
        if !valid_tag_name(tag) {
            logger
                .note(
                    "setup.git.tag",
                    &format!("skipping invalid tag name {tag:?}"),
                    0,
                )
                .await?;
            continue;
        }
        let (key, cmd, command) = git_step("setup.git.tag", vec!["tag", tag, &job.commit]);
        run_logged(cmd, key, &command, logger, cancel).await?;
    }
    Ok(())
}

/// Runs a local command as a logged step and returns its exit code.
pub async fn run_logged(
    mut cmd: Command,
    key: &str,
    command: &[String],
    logger: &JobLogger,
    cancel: &CancellationToken,
) -> Result<i32, JobError> {
    let log = logger.start(key, command).await?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log.line(&format!("failed to start: {e}"));
            log.finish(Some(-1)).await?;
            return Err(JobError::Errored(format!("{}: {e}", command[0])));
        }
    };
    let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
    let (mut out_done, mut err_done) = (false, false);
    while !(out_done && err_done) {
        tokio::select! {
            line = out.next_line(), if !out_done => match line {
                Ok(Some(l)) => log.line(&l),
                _ => out_done = true,
            },
            line = err.next_line(), if !err_done => match line {
                // git prints progress with carriage returns; keep the last state of each line.
                Ok(Some(l)) => log.line(l.rsplit('\r').next().unwrap_or(&l)),
                _ => err_done = true,
            },
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                log.line("canceled");
                log.finish(None).await?;
                return Err(JobError::Canceled);
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| JobError::Errored(e.to_string()))?;
    let code = status.code().unwrap_or(-1);
    log.finish(Some(code)).await?;
    Ok(code)
}
