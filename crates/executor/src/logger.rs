//! Streaming a job's step logs to the server: each step is one execution log entry,
//! added when it starts and updated as output arrives (at most once per flush interval) and
//! when it ends. Every injected secret is replaced before anything leaves the executor.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use sourcegull_executor_api::LogEntry;
use tokio::task::JoinHandle;

use crate::client::{Client, ClientError};

/// Output kept per step; older output is dropped from the front.
const MAX_OUTPUT: usize = 1024 * 1024;

/// How often a running step's growing output is sent.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Exact-match replacement of secret values.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    values: Vec<(String, String)>,
}

impl Redactor {
    /// Replaces each key of `values` with its value. Short values (under 4 bytes) are ignored,
    /// they would mangle unrelated output.
    pub fn new(values: &BTreeMap<String, String>) -> Self {
        let mut values: Vec<(String, String)> = values
            .iter()
            .filter(|(k, _)| k.len() >= 4)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Longest first, so a secret containing another is replaced whole.
        values.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        Self { values }
    }

    /// The redactions of `job`: its `redacted_values`, plus the value of every executor secret
    /// (`${{ secrets.NAME }}`) in case the server did not list it.
    pub fn for_job(job: &sourcegull_executor_api::Job) -> Self {
        let mut values = job.redacted_values.clone();
        for entry in &job.secret_env {
            if let Some((key, value)) = entry.split_once('=') {
                values
                    .entry(value.to_owned())
                    .or_insert_with(|| format!("${{{{ secrets.{key} }}}}"));
            }
        }
        if let Some(config) = &job.docker_auth_config {
            for (_, password) in config.auths.values().filter_map(|a| decode_auth(&a.auth)) {
                values
                    .entry(password)
                    .or_insert_with(|| "${{ secrets.DOCKER_AUTH_CONFIG }}".to_owned());
            }
        }
        Self::new(&values)
    }

    /// `s` with every secret replaced.
    pub fn redact(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (secret, replacement) in &self.values {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), replacement);
            }
        }
        out
    }
}

/// A line longer than this is logged without waiting for its end.
const MAX_PENDING_LINE: usize = 64 * 1024;

/// Holds back the unfinished last line of a stream, so a secret split across two reads is
/// still redacted whole and a multi-byte character split across them stays intact.
#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    /// Adds `data` and returns the lines it completed (with their newlines), if any.
    pub fn push(&mut self, data: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(data);
        let end = match self.pending.iter().rposition(|b| *b == b'\n') {
            Some(i) => i + 1,
            None if self.pending.len() > MAX_PENDING_LINE => self.pending.len(),
            None => return None,
        };
        let complete: Vec<u8> = self.pending.drain(..end).collect();
        Some(String::from_utf8_lossy(&complete).into_owned())
    }

    /// The unfinished last line, if any.
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let rest = std::mem::take(&mut self.pending);
        Some(String::from_utf8_lossy(&rest).into_owned())
    }
}

/// `(username, password)` of a Docker config `auth` value (base64 of `username:password`).
pub fn decode_auth(auth: &str) -> Option<(String, String)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(auth.trim())
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

/// Logs of one job.
#[derive(Clone)]
pub struct JobLogger {
    client: Client,
    queue: String,
    job_id: i64,
    redactor: Arc<Redactor>,
}

impl JobLogger {
    pub fn new(client: Client, queue: &str, job_id: i64, redactor: Redactor) -> Self {
        Self {
            client,
            queue: queue.to_owned(),
            job_id,
            redactor: Arc::new(redactor),
        }
    }

    /// Starts the log entry of a step.
    pub async fn start(&self, key: &str, command: &[String]) -> Result<StepLog, ClientError> {
        let entry = LogEntry {
            key: key.to_owned(),
            command: command.iter().map(|c| self.redactor.redact(c)).collect(),
            start_time: Some(Utc::now()),
            ..Default::default()
        };
        let index = self
            .client
            .add_log(&self.queue, self.job_id, entry.clone())
            .await?;
        let state = Arc::new(Mutex::new(StepState {
            entry,
            dirty: false,
        }));
        let flusher = {
            let logger = self.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(FLUSH_INTERVAL);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let entry = {
                        let mut s = state.lock().expect("log state");
                        if !s.dirty {
                            continue;
                        }
                        s.dirty = false;
                        s.entry.clone()
                    };
                    if let Err(e) = logger
                        .client
                        .update_log(&logger.queue, logger.job_id, index, entry)
                        .await
                    {
                        tracing::debug!(error = %e, "updating a log entry failed");
                    }
                }
            })
        };
        Ok(StepLog {
            logger: self.clone(),
            index,
            state,
            started: Instant::now(),
            flusher,
        })
    }

    /// Logs a step that consists of a message only.
    pub async fn note(&self, key: &str, message: &str, exit_code: i32) -> Result<(), ClientError> {
        let log = self.start(key, &[]).await?;
        log.append(message);
        log.finish(Some(exit_code)).await
    }
}

struct StepState {
    entry: LogEntry,
    dirty: bool,
}

/// The log entry of a running step.
pub struct StepLog {
    logger: JobLogger,
    index: i32,
    state: Arc<Mutex<StepState>>,
    started: Instant,
    flusher: JoinHandle<()>,
}

impl StepLog {
    /// Appends output (redacted).
    pub fn append(&self, text: &str) {
        let text = self.logger.redactor.redact(text);
        let mut s = self.state.lock().expect("log state");
        let out = &mut s.entry.out;
        out.push_str(&text);
        if out.len() > MAX_OUTPUT {
            let mut cut = out.len() - MAX_OUTPUT;
            while !out.is_char_boundary(cut) {
                cut += 1;
            }
            out.replace_range(..cut, "");
        }
        s.dirty = true;
    }

    /// Appends a line of output.
    pub fn line(&self, text: &str) {
        self.append(&format!("{text}\n"));
    }

    /// Ends the step with `exit_code` and sends the final entry.
    pub async fn finish(self, exit_code: Option<i32>) -> Result<(), ClientError> {
        self.flusher.abort();
        let entry = {
            let mut s = self.state.lock().expect("log state");
            s.entry.exit_code = exit_code;
            s.entry.duration_ms =
                Some(i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX));
            s.entry.clone()
        };
        self.logger
            .client
            .update_log(&self.logger.queue, self.logger.job_id, self.index, entry)
            .await
    }
}

impl Drop for StepLog {
    fn drop(&mut self) {
        self.flusher.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_longest_first() {
        let r = Redactor::new(
            &[
                ("secret".to_owned(), "A".to_owned()),
                ("secret-token".to_owned(), "B".to_owned()),
                ("x".to_owned(), "C".to_owned()),
            ]
            .into(),
        );
        assert_eq!(r.redact("a secret-token and a secret, x"), "a B and a A, x");
    }

    #[test]
    fn redacts_secret_env_and_registry_passwords() {
        let job = sourcegull_executor_api::Job {
            secret_env: vec!["API_KEY=hunter2-value".into()],
            docker_auth_config: Some(sourcegull_executor_api::DockerAuthConfig {
                // `bot:registry-password`
                auths: [(
                    "registry.example.com".to_owned(),
                    sourcegull_executor_api::DockerAuthConfigAuth {
                        auth: "Ym90OnJlZ2lzdHJ5LXBhc3N3b3Jk".into(),
                    },
                )]
                .into(),
            }),
            ..Default::default()
        };
        let r = Redactor::for_job(&job);
        assert_eq!(
            r.redact("key hunter2-value pw registry-password"),
            "key ${{ secrets.API_KEY }} pw ${{ secrets.DOCKER_AUTH_CONFIG }}"
        );
        assert_eq!(
            decode_auth("dXNlcjpwYXNz"),
            Some(("user".into(), "pass".into()))
        );
        assert_eq!(decode_auth("not base64!"), None);
    }

    #[test]
    fn secret_split_across_reads_is_redacted() {
        let r = Redactor::new(&[("hunter2-value".to_owned(), "***".to_owned())].into());
        let mut buf = LineBuffer::default();
        let mut out = String::new();
        for chunk in [
            &b"token: hunt"[..],
            b"er2-va",
            b"lue\nnext \xc3",
            b"\xa9 tail",
        ] {
            if let Some(text) = buf.push(chunk) {
                out.push_str(&r.redact(&text));
            }
        }
        assert_eq!(out, "token: ***\n");
        assert_eq!(buf.finish().as_deref(), Some("next \u{e9} tail"));
        assert_eq!(buf.finish(), None);

        let long = vec![b'a'; MAX_PENDING_LINE + 1];
        assert_eq!(buf.push(&long).map(|s| s.len()), Some(MAX_PENDING_LINE + 1));
    }
}
