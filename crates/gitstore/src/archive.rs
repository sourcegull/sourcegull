//! Tar/zip archives of a tree.
//!
//! Uses `git archive`, which streams as it walks the tree and honours
//! `export-ignore`/`export-subst` attributes the way users expect from
//! GitLab's own archive downloads.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use gix::ObjectId;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::command::{GitCommand, StreamingCommand};
use crate::error::{GitError, Result};
use crate::read::normalize_path;

/// Archive format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Uncompressed tar.
    Tar,
    /// Zip (deflate).
    Zip,
}

impl ArchiveFormat {
    fn as_arg(self) -> &'static str {
        match self {
            ArchiveFormat::Tar => "--format=tar",
            ArchiveFormat::Zip => "--format=zip",
        }
    }
}

const CHUNK: usize = 64 * 1024;

/// Streams an archive of `commit`, limited to `paths` (repo-relative files or
/// directories; empty for everything). Entries keep their full paths.
/// The stream ends with an error item if git fails. Dropping it kills git.
pub fn archive_stream(
    git_bin: &Path,
    repo_dir: &Path,
    commit: ObjectId,
    format: ArchiveFormat,
    paths: &[String],
    timeout: Duration,
) -> Result<ReceiverStream<Result<Bytes>>> {
    let mut specs = Vec::with_capacity(paths.len());
    for p in paths {
        let p = normalize_path(p)?;
        if !p.is_empty() {
            // Literal pathspecs: no globbing or magic from user input.
            specs.push(format!(":(literal){p}"));
        }
    }
    let running = GitCommand::new(git_bin, "archive")
        .repo(repo_dir)
        .arg(format.as_arg())
        .arg(commit.to_string())
        .arg("--")
        .args(&specs)
        .timeout(timeout)
        .spawn_streaming()?;
    Ok(stream_stdout(running, |e| match e {
        GitError::Command { stderr, .. } if stderr.contains("did not match any files") => {
            GitError::PathNotFound(stderr)
        }
        e => e,
    }))
}

/// Streams the stdout of a running command in chunks until it exits or its deadline passes;
/// a failed exit ends the stream with `map_err` of the error. Dropping the stream kills the
/// command.
pub(crate) fn stream_stdout(
    mut running: StreamingCommand,
    map_err: impl FnOnce(GitError) -> GitError + Send + 'static,
) -> ReceiverStream<Result<Bytes>> {
    let mut stdout = running.take_stdout().expect("stdout is piped");
    let deadline = running.deadline();
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = tokio::select! {
                n = stdout.read(&mut buf) => n,
                _ = tokio::time::sleep_until(deadline) => break,
            };
            match n {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(GitError::Io(e))).await;
                    return;
                }
            }
        }
        if let Err(e) = running.finish().await {
            let _ = tx.send(Err(map_err(e))).await;
        }
    });
    ReceiverStream::new(rx)
}
