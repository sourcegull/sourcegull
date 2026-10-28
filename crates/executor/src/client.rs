//! HTTP client of the executor queue API and of the job-token endpoints.

use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sourcegull_executor_api::{
    self as api, AddLogEntryRequest, AddLogEntryResponse, BatchesCacheEntryRequest,
    BatchesResultRequest, BatchesResultResponse, DequeueRequest, HeartbeatRequest,
    HeartbeatResponse, Job, LogEntry, MarkCompleteRequest, MarkErroredRequest,
    UpdateLogEntryRequest,
};
use url::Url;

/// Errors talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server no longer gives this executor the job (reset, canceled, finished).
    #[error("the job is no longer held by this executor")]
    NotHeld,
    /// The server answered with an error status.
    #[error("{status}: {message}")]
    Status { status: StatusCode, message: String },
    /// Network or decoding error.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Reading a local file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Talks to the web role on behalf of one executor.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: String,
    name: String,
}

impl Client {
    /// A client for the server at `base` (e.g. `http://sourcegull-web:7080`), authenticating with
    /// the shared executor `token`, for the executor called `name`.
    pub fn new(base: Url, token: impl Into<String>, name: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("sourcegull-executor/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base,
            token: token.into(),
            name: name.into(),
        }
    }

    /// The executor's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The server's base URL.
    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        let base_path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{base_path}{path}"));
        url
    }

    /// The git remote of `repo` for checkouts with a job token.
    pub fn git_url(&self, repo: &str) -> Url {
        self.url(&format!("/.executors/git/{repo}"))
    }

    async fn post<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<Option<R>, ClientError> {
        let resp = self
            .http
            .post(self.url(path))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("{} {}", api::EXECUTOR_TOKEN_SCHEME, self.token),
            )
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(None),
            s if s.is_success() => Ok(Some(resp.json().await?)),
            StatusCode::NOT_FOUND if path.contains("/queue/") && !path.ends_with("/dequeue") => {
                Err(ClientError::NotHeld)
            }
            status => Err(ClientError::Status {
                status,
                message: error_message(resp).await,
            }),
        }
    }

    /// Asks `queue` for a job, for an executor that can run `capabilities` (see
    /// [`api::capability`]).
    pub async fn dequeue(
        &self,
        queue: &str,
        capabilities: &[&str],
    ) -> Result<Option<Job>, ClientError> {
        let req = DequeueRequest {
            executor_name: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        };
        self.post(
            &format!("/.executors/queue/{queue}/dequeue"),
            &req,
            Duration::from_secs(30),
        )
        .await
    }

    /// Sends a heartbeat.
    pub async fn heartbeat(
        &self,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ClientError> {
        Ok(self
            .post("/.executors/queue/heartbeat", req, Duration::from_secs(30))
            .await?
            .unwrap_or_default())
    }

    /// Appends a log entry; returns its index.
    pub async fn add_log(
        &self,
        queue: &str,
        job_id: i64,
        entry: LogEntry,
    ) -> Result<i32, ClientError> {
        let req = AddLogEntryRequest {
            executor_name: self.name.clone(),
            job_id,
            entry,
        };
        let resp: Option<AddLogEntryResponse> = self
            .post(
                &format!("/.executors/queue/{queue}/addExecutionLogEntry"),
                &req,
                Duration::from_secs(30),
            )
            .await?;
        Ok(resp.unwrap_or_default().index)
    }

    /// Replaces log entry `index`.
    pub async fn update_log(
        &self,
        queue: &str,
        job_id: i64,
        index: i32,
        entry: LogEntry,
    ) -> Result<(), ClientError> {
        let req = UpdateLogEntryRequest {
            executor_name: self.name.clone(),
            job_id,
            index,
            entry,
        };
        self.post::<_, serde_json::Value>(
            &format!("/.executors/queue/{queue}/updateExecutionLogEntry"),
            &req,
            Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Marks the job completed.
    pub async fn mark_complete(&self, queue: &str, job_id: i64) -> Result<(), ClientError> {
        let req = MarkCompleteRequest {
            executor_name: self.name.clone(),
            job_id,
        };
        self.post::<_, serde_json::Value>(
            &format!("/.executors/queue/{queue}/markComplete"),
            &req,
            Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Marks the job errored (retried while attempts remain) or, with `fatal`, failed.
    pub async fn mark_errored(
        &self,
        queue: &str,
        job_id: i64,
        message: &str,
        fatal: bool,
    ) -> Result<(), ClientError> {
        let req = MarkErroredRequest {
            executor_name: self.name.clone(),
            job_id,
            error_message: message.to_owned(),
        };
        let action = if fatal { "markFailed" } else { "markErrored" };
        self.post::<_, serde_json::Value>(
            &format!("/.executors/queue/{queue}/{action}"),
            &req,
            Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Uploads the SCIP index at `file` for `job` with its job token. Returns the upload id.
    pub async fn upload_scip(
        &self,
        job: &Job,
        root: &str,
        file: &Path,
    ) -> Result<String, ClientError> {
        let mut url = self.url("/.api/scip/upload");
        url.query_pairs_mut()
            .append_pair("repository", &job.repository_name)
            .append_pair("commit", &job.commit)
            .append_pair("root", root)
            .append_pair("associatedIndexId", &job.id.to_string());
        let f = tokio::fs::File::open(file).await?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let resp = self
            .http
            .post(url)
            .bearer_auth(&job.token)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf+scip")
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ClientError::Status {
                status: resp.status(),
                message: error_message(resp).await,
            });
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v["id"].as_str().unwrap_or_default().to_owned())
    }

    /// Posts `body` to a batches upload path with `job`'s token. A job that is no longer
    /// running answers 404 ([`ClientError::NotHeld`]).
    async fn post_with_job_token<B: Serialize>(
        &self,
        job: &Job,
        path: &str,
        body: &B,
    ) -> Result<reqwest::Response, ClientError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&job.token)
            .timeout(Duration::from_secs(60))
            .json(body)
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            StatusCode::NOT_FOUND => Err(ClientError::NotHeld),
            status => Err(ClientError::Status {
                status,
                message: error_message(resp).await,
            }),
        }
    }

    /// Stores a step result in the execution cache of a batches job's user
    /// ([`api::BATCHES_ADD_CACHE_ENTRY`]).
    pub async fn batches_add_cache_entry(
        &self,
        job: &Job,
        key: &str,
        value: &str,
    ) -> Result<(), ClientError> {
        let req = BatchesCacheEntryRequest {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        self.post_with_job_token(job, api::BATCHES_ADD_CACHE_ENTRY, &req)
            .await?;
        Ok(())
    }

    /// Downloads a file of a batches job's spec (a [`api::BatchesWorkspaceFile::url`], a path
    /// on the server) with the job token to `dest`. A job that is no longer running answers
    /// 401 ([`ClientError::NotHeld`]).
    pub async fn batches_download_file(
        &self,
        job: &Job,
        url: &str,
        dest: &Path,
    ) -> Result<(), ClientError> {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let mut url = self.url(path);
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        let mut resp = self
            .http
            .get(url)
            .bearer_auth(&job.token)
            .timeout(Duration::from_secs(300))
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED => return Err(ClientError::NotHeld),
            status => {
                return Err(ClientError::Status {
                    status,
                    message: error_message(resp).await,
                });
            }
        }
        let mut f = tokio::fs::File::create(dest).await?;
        while let Some(chunk) = resp.chunk().await? {
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut f).await?;
        Ok(())
    }

    /// Uploads the changeset specs of a batches job ([`api::BATCHES_UPLOAD_RESULT`]).
    pub async fn batches_upload_result(
        &self,
        job: &Job,
        changeset_specs: Vec<String>,
    ) -> Result<BatchesResultResponse, ClientError> {
        let req = BatchesResultRequest { changeset_specs };
        Ok(self
            .post_with_job_token(job, api::BATCHES_UPLOAD_RESULT, &req)
            .await?
            .json()
            .await?)
    }
}

async fn error_message(resp: reqwest::Response) -> String {
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or(text)
}
