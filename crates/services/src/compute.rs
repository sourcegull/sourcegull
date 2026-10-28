//! Compute over search: the compute stream API (`sourcegull-http-api`) and GraphQL `compute`.
//!
//! Parses the compute query ([`sourcegull_compute::ComputeQuery`]), runs its search through
//! [`sourcegull_search_web::spawn_search`] with the viewer's repository permissions, and runs the
//! command on each result as it arrives (reading whole files from the repo nodes for
//! `replace`). Events ([`ComputeEvent`]): `results` (a JSON array of
//! `MatchContext` / `Text` objects), `progress` (the search's), `alert` and `error`. A
//! compute query runs for at most [`MAX_DURATION`], then ends with an "Incomplete data"
//! alert.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use sourcegull_authn::Actor;
use sourcegull_compute::{Command, ComputeError, ComputeQuery, ComputeResult};
use sourcegull_query::PatternType;
use sourcegull_search::stream::{Alert, Event, Match, Progress};
use sourcegull_search_web::{RunOptions, SearchBackend, spawn_search};
use sourcegull_types::RepoId;

use crate::git::GitReader;

/// How long a compute query runs at most (`maxRequestDuration`).
pub const MAX_DURATION: Duration = Duration::from_secs(60);

/// Search results computed at once.
const CONCURRENCY: usize = 8;

/// One event of a compute run.
#[derive(Debug, Clone)]
pub enum ComputeEvent {
    Results(Vec<ComputeResult>),
    Progress(Progress),
    Alert(Alert),
    Error(String),
}

impl ComputeEvent {
    /// The event name on the stream API.
    pub fn name(&self) -> &'static str {
        match self {
            ComputeEvent::Results(_) => "results",
            ComputeEvent::Progress(_) => "progress",
            ComputeEvent::Alert(_) => "alert",
            ComputeEvent::Error(_) => "error",
        }
    }

    /// The event data on the stream API.
    pub fn data(&self) -> String {
        match self {
            ComputeEvent::Results(r) => serde_json::to_string(r).expect("results serialize"),
            ComputeEvent::Progress(p) => Event::Progress(p.clone()).data(),
            ComputeEvent::Alert(a) => Event::Alert(a.clone()).data(),
            ComputeEvent::Error(e) => Event::Error(e.clone()).data(),
        }
    }
}

/// Runs `cmd` on one search result, reading the file when the command needs it.
async fn compute_one(
    backend: &SearchBackend,
    cmd: &Command,
    m: Match,
) -> Result<Vec<ComputeResult>, String> {
    if !cmd.needs_file(&m) {
        return Ok(cmd.run(&m, None));
    }
    let Match::Content {
        path,
        repository_id,
        repository,
        commit,
        ..
    } = &m
    else {
        return Ok(Vec::new());
    };
    let git = GitReader::from_parts(
        backend.repo_clients.clone(),
        RepoId(*repository_id),
        repository,
    );
    let file = git
        .read_file(commit, path, 0)
        .await
        .map_err(|e| format!("reading {repository}/{path}: {e}"))?;
    if file.truncated {
        // Rewriting a truncated file would drop its end.
        return Ok(Vec::new());
    }
    let content = String::from_utf8_lossy(&file.content);
    Ok(cmd.run(&m, Some(&content)))
}

/// The pattern type name of the search a compute query runs.
fn type_name(t: PatternType) -> &'static str {
    match t {
        PatternType::Structural => "structural",
        _ => "regexp",
    }
}

/// Starts computing `query` for `actor` and returns its events. Repository permissions apply
/// as in search; the caller checks that `actor` may search at all.
pub fn spawn_compute(
    backend: &SearchBackend,
    actor: Option<&Actor>,
    query: Arc<ComputeQuery>,
) -> tokio::sync::mpsc::Receiver<ComputeEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let (search_query, ty) = query.search_query();
    let (mut search, _) = spawn_search(
        backend,
        actor,
        &search_query,
        Some(type_name(ty)),
        None,
        RunOptions::default(),
    );
    let backend = backend.clone();
    tokio::spawn(async move {
        let work = async {
            while let Some(event) = search.recv().await {
                let out = match event {
                    Event::Matches(ms) => {
                        let results: Vec<_> = futures::stream::iter(ms)
                            .map(|m| compute_one(&backend, &query.command, m))
                            .buffered(CONCURRENCY)
                            .collect()
                            .await;
                        let mut all = Vec::new();
                        for r in results {
                            match r {
                                Ok(r) => all.extend(r),
                                Err(e) => {
                                    tracing::warn!(error = %e, "compute");
                                    let _ = tx.send(ComputeEvent::Error(e)).await;
                                    return;
                                }
                            }
                        }
                        if all.is_empty() {
                            continue;
                        }
                        ComputeEvent::Results(all)
                    }
                    Event::Progress(p) => ComputeEvent::Progress(p),
                    Event::Alert(a) => ComputeEvent::Alert(a),
                    Event::Error(e) => ComputeEvent::Error(e),
                    Event::Filters(_) | Event::Done => continue,
                };
                if tx.send(out).await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(MAX_DURATION, work).await.is_err() {
            let _ = tx
                .send(ComputeEvent::Alert(Alert {
                    title: "Incomplete data".to_owned(),
                    description: format!(
                        "This data is incomplete! The query ran for {} seconds, the most a \
                         compute query may run. Narrow the query to compute all results.",
                        MAX_DURATION.as_secs()
                    ),
                    proposed_queries: Vec::new(),
                }))
                .await;
        }
    });
    rx
}

/// A compute query run to the end ([`collect`]).
#[derive(Debug, Default)]
pub struct ComputeOutcome {
    pub results: Vec<ComputeResult>,
    pub alerts: Vec<Alert>,
    /// The search's final progress; `None` when the search rejected the query.
    pub progress: Option<Progress>,
    pub error: Option<String>,
}

/// Parses `input`, computes it for `actor` and collects the results (GraphQL `compute`).
pub async fn collect(
    backend: &SearchBackend,
    actor: Option<&Actor>,
    input: &str,
) -> Result<ComputeOutcome, ComputeError> {
    let query = Arc::new(ComputeQuery::parse(input)?);
    let mut rx = spawn_compute(backend, actor, query);
    let mut out = ComputeOutcome::default();
    while let Some(event) = rx.recv().await {
        match event {
            ComputeEvent::Results(r) => out.results.extend(r),
            ComputeEvent::Progress(p) => out.progress = Some(p),
            ComputeEvent::Alert(a) => out.alerts.push(a),
            ComputeEvent::Error(e) => out.error = Some(e),
        }
    }
    Ok(out)
}
