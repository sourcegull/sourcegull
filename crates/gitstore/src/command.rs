//! Running the `git` CLI: timeouts, cancellation, progress capture, redaction.
//!
//! Network operations (clone, fetch) and maintenance use the CLI; reads use `gix` in-process.
//! Each command runs in its own process group so a timeout also kills helpers git spawned
//! (`git-remote-https`, `ssh`, `index-pack`).

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use crate::error::{GitError, Result};
use crate::remote::Redactor;

/// Receives each progress line (redacted) as git prints it.
pub type ProgressSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Lines of stderr kept for error messages and `progress_tail`.
const STDERR_TAIL_LINES: usize = 20;

/// Environment variables that would redirect git away from the repo we chose.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
];

/// A git invocation.
#[derive(Clone)]
pub struct GitCommand {
    git: PathBuf,
    op: String,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(String, OsString)>,
    timeout: Duration,
    cancel: CancellationToken,
    redactor: Redactor,
    progress: Option<ProgressSink>,
    max_stdout: usize,
    stdin: Option<bytes::Bytes>,
}

/// Output of a finished command.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// Standard output (truncated at the configured maximum).
    pub stdout: Vec<u8>,
    /// Whether stdout was truncated.
    pub stdout_truncated: bool,
    /// The last lines of stderr, redacted.
    pub stderr_tail: String,
}

impl GitCommand {
    /// A command running `git <op> …` with a 10 minute default timeout.
    pub fn new(git: impl Into<PathBuf>, op: &str) -> Self {
        Self {
            git: git.into(),
            op: op.to_owned(),
            args: vec![op.into()],
            cwd: None,
            env: Vec::new(),
            timeout: Duration::from_secs(600),
            cancel: CancellationToken::new(),
            redactor: Redactor::default(),
            progress: None,
            max_stdout: 64 * 1024 * 1024,
            stdin: None,
        }
    }

    /// Runs against the repository at `dir` (`git -C <dir>`), inserted before the subcommand.
    pub fn repo(mut self, dir: &Path) -> Self {
        self.args.splice(0..0, [OsString::from("-C"), dir.into()]);
        self
    }

    /// Adds a `-c key=value` config override before the subcommand.
    pub fn config(mut self, kv: &str) -> Self {
        let pos = self
            .args
            .iter()
            .position(|a| a == self.op.as_str())
            .unwrap_or(0);
        self.args
            .splice(pos..pos, [OsString::from("-c"), OsString::from(kv)]);
        self
    }

    /// Appends an argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Appends arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Sets the working directory.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Adds environment variables.
    pub fn envs(mut self, env: impl IntoIterator<Item = (String, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Sets the timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Kills the command when `token` is cancelled.
    pub fn cancel_on(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Redacts output with `redactor`.
    pub fn redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Streams progress lines to `sink`.
    pub fn progress(mut self, sink: Option<ProgressSink>) -> Self {
        self.progress = sink;
        self
    }

    /// Caps captured stdout.
    pub fn max_stdout(mut self, bytes: usize) -> Self {
        self.max_stdout = bytes;
        self
    }

    /// Writes `input` to the command's stdin (closed afterwards); stdin is empty otherwise.
    pub fn stdin(mut self, input: bytes::Bytes) -> Self {
        self.stdin = Some(input);
        self
    }

    /// The arguments, for logging (never contains credentials by construction).
    pub fn display_args(&self) -> String {
        self.args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn spawn(&self, stdout: Stdio) -> Result<Child> {
        let mut cmd = Command::new(&self.git);
        cmd.args(&self.args)
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(stdout)
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for k in SCRUBBED_ENV {
            cmd.env_remove(k);
        }
        cmd.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
        cmd.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())));
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        tracing::debug!(op = %self.op, args = %self.display_args(), "running git");
        let mut child = cmd.spawn()?;
        if let (Some(input), Some(mut stdin)) = (self.stdin.clone(), child.stdin.take()) {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                // A command that exits early closes the pipe; its status reports why.
                let _ = stdin.write_all(&input).await;
            });
        }
        Ok(child)
    }

    /// Runs the command to completion and returns its output. A non-zero exit
    /// status is an error carrying the redacted stderr tail.
    pub async fn run(self) -> Result<CommandOutput> {
        let mut child = self.spawn(Stdio::piped())?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = tokio::spawn(read_stderr(
            stderr,
            tail.clone(),
            self.redactor.clone(),
            self.progress.clone(),
        ));
        let max = self.max_stdout;
        let stdout_task = tokio::spawn(async move { read_capped(stdout, max).await });

        let status = self.wait(&mut child).await?;
        let (stdout, stdout_truncated) = stdout_task
            .await
            .map_err(|e| GitError::Io(std::io::Error::other(e)))??;
        let _ = stderr_task.await;
        let stderr_tail = join_tail(&tail);
        if !status.success() {
            return Err(GitError::Command {
                op: self.op,
                status: status.to_string(),
                stderr: stderr_tail,
            });
        }
        Ok(CommandOutput {
            stdout,
            stdout_truncated,
            stderr_tail,
        })
    }

    /// Spawns the command with stdout available for streaming (blame, archive).
    /// Call [`StreamingCommand::finish`] after draining stdout to check the exit status.
    pub fn spawn_streaming(self) -> Result<StreamingCommand> {
        let mut child = self.spawn(Stdio::piped())?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = tokio::spawn(read_stderr(
            stderr,
            tail.clone(),
            self.redactor.clone(),
            None,
        ));
        let deadline = tokio::time::Instant::now() + self.timeout;
        Ok(StreamingCommand {
            stdout: Some(stdout),
            child,
            tail,
            stderr_task,
            deadline,
            cmd: self,
        })
    }

    async fn wait(&self, child: &mut Child) -> Result<std::process::ExitStatus> {
        tokio::select! {
            status = child.wait() => Ok(status?),
            _ = tokio::time::sleep(self.timeout) => {
                kill_group(child).await;
                Err(GitError::Timeout { op: self.op.clone(), after: self.timeout })
            }
            _ = self.cancel.cancelled() => {
                kill_group(child).await;
                Err(GitError::Cancelled(self.op.clone()))
            }
        }
    }
}

/// A running command whose stdout the caller reads incrementally.
pub struct StreamingCommand {
    stdout: Option<ChildStdout>,
    child: Child,
    tail: Arc<Mutex<VecDeque<String>>>,
    stderr_task: tokio::task::JoinHandle<()>,
    deadline: tokio::time::Instant,
    cmd: GitCommand,
}

impl StreamingCommand {
    /// Takes stdout. Returns `None` on the second call.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// The deadline after which [`StreamingCommand::finish`] kills the process.
    pub fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    /// Waits for exit (killing the process at the deadline) and checks the status.
    pub async fn finish(mut self) -> Result<CommandOutput> {
        drop(self.stdout.take());
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let cmd = self.cmd.clone().timeout(remaining);
        let status = cmd.wait(&mut self.child).await?;
        let _ = (&mut self.stderr_task).await;
        let stderr_tail = join_tail(&self.tail);
        if !status.success() {
            return Err(GitError::Command {
                op: self.cmd.op.clone(),
                status: status.to_string(),
                stderr: stderr_tail,
            });
        }
        Ok(CommandOutput {
            stderr_tail,
            ..Default::default()
        })
    }
}

async fn kill_group(child: &mut Child) {
    if let Some(pid) = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

async fn read_capped(mut r: impl AsyncRead + Unpin, max: usize) -> Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = max.saturating_sub(out.len());
        if room < n {
            truncated = true;
        }
        out.extend_from_slice(&buf[..n.min(room)]);
    }
    Ok((out, truncated))
}

/// Reads stderr, splitting on `\n` and `\r` (git redraws progress with `\r`),
/// keeping the last lines and forwarding each to the progress sink.
async fn read_stderr(
    mut r: impl AsyncRead + Unpin,
    tail: Arc<Mutex<VecDeque<String>>>,
    redactor: Redactor,
    progress: Option<ProgressSink>,
) {
    let mut buf = vec![0u8; 8 * 1024];
    let mut line = Vec::new();
    let emit = |line: &mut Vec<u8>| {
        if line.is_empty() {
            return;
        }
        let text = redactor.redact(&String::from_utf8_lossy(line));
        line.clear();
        if let Some(sink) = &progress {
            sink(&text);
        }
        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
        // Consecutive progress updates of the same phase replace each other.
        if let Some(last) = t.back_mut() {
            if same_phase(last, &text) {
                *last = text;
                return;
            }
        }
        if t.len() == STDERR_TAIL_LINES {
            t.pop_front();
        }
        t.push_back(text);
    };
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                emit(&mut line);
            } else {
                line.push(b);
            }
        }
    }
    emit(&mut line);
}

/// Whether two progress lines are updates of the same phase
/// (`Receiving objects:  10% …` → `Receiving objects:  20% …`).
fn same_phase(a: &str, b: &str) -> bool {
    match (a.split_once(':'), b.split_once(':')) {
        (Some((pa, ra)), Some((pb, _))) => pa == pb && ra.contains('%'),
        _ => false,
    }
}

fn join_tail(tail: &Mutex<VecDeque<String>>) -> String {
    let t = tail.lock().unwrap_or_else(|e| e.into_inner());
    t.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> GitCommand {
        GitCommand::new("git", "version")
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let out = git().run().await.unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("git version"));
    }

    #[tokio::test]
    async fn failure_carries_redacted_stderr() {
        let err = GitCommand::new("git", "ls-remote")
            .arg("/nonexistent/secret-token-xyz")
            .redactor(Redactor::new(["secret-token-xyz".to_owned()]))
            .run()
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, GitError::Command { .. }), "{msg}");
        assert!(!msg.contains("secret-token-xyz"), "{msg}");
        assert!(msg.contains("[REDACTED]"), "{msg}");
    }

    #[tokio::test]
    async fn timeout_and_cancel_kill_the_process() {
        // `git -c alias.slow='!sleep 5' slow` runs a shell command through git.
        let slow = || GitCommand::new("git", "slow").config("alias.slow=!sleep 5");
        let start = std::time::Instant::now();
        let err = slow()
            .timeout(Duration::from_millis(100))
            .run()
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Timeout { .. }), "{err}");
        assert!(start.elapsed() < Duration::from_secs(3));

        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel();
        });
        let err = slow().cancel_on(token).run().await.unwrap_err();
        assert!(matches!(err, GitError::Cancelled(_)), "{err}");
    }

    #[tokio::test]
    async fn progress_lines_are_split_on_carriage_returns() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        let out = GitCommand::new("git", "p")
            .config("alias.p=!printf 'Counting: 10%%\\rCounting: 100%%\\nDone\\n' >&2")
            .progress(Some(Arc::new(move |l: &str| {
                l2.lock().unwrap().push(l.to_owned())
            })))
            .run()
            .await
            .unwrap();
        assert_eq!(
            *lines.lock().unwrap(),
            ["Counting: 10%", "Counting: 100%", "Done"]
        );
        assert_eq!(out.stderr_tail, "Counting: 100%\nDone");
    }

    #[test]
    fn config_goes_before_the_subcommand() {
        let c = GitCommand::new("git", "fetch")
            .repo(Path::new("/r"))
            .config("credential.helper=")
            .arg("origin");
        assert_eq!(c.display_args(), "-C /r -c credential.helper= fetch origin");
    }
}
